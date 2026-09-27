//! **The hardware seam**: everything that touches an audio device goes through [`AudioIo`] — the
//! device scan, the microphone (peaks for the level meter + 16 kHz mono frames for the voice
//! pipeline) and the voice output stream. [`crate::cpal_io::CpalIo`] is the real one,
//! [`crate::wav::WavFileIo`] plays a WAV file as the microphone, [`NullIo`] has no devices, and
//! tests use fakes — no test ever opens a device.
//!
//! Threads talk to the app only through atomics and BOUNDED channels (`sync_channel`, fixed-size
//! array messages: no allocation on the audio thread, a full queue drops the frame and counts it).

use crate::codec::{MonoFrame, FRAME};
use crate::dsp::PullResampler;
use crate::mixer::StereoFrame;
use crate::DeviceState;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};

/// The input + output device names of one scan.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceLists {
    /// Input device names.
    pub inputs: Vec<String>,
    /// Output device names.
    pub outputs: Vec<String>,
}

/// A scan in flight: the worker fills it once.
#[derive(Clone, Default)]
pub struct ScanJob(pub Arc<Mutex<Option<DeviceLists>>>);

impl ScanJob {
    /// The result, once (`None` while the worker is still busy).
    pub fn take(&self) -> Option<DeviceLists> {
        self.0.lock().ok().and_then(|mut g| g.take())
    }

    /// The worker's side: deliver the lists.
    pub fn deliver(&self, lists: DeviceLists) {
        if let Ok(mut g) = self.0.lock() {
            *g = Some(lists);
        }
    }
}

/// Device state (`MicShared::state` / `OutputShared::state`): being opened.
pub const DEVICE_OPENING: u8 = 0;
/// The device is running.
pub const DEVICE_LIVE: u8 = 1;
/// The device failed or is missing.
pub const DEVICE_FAILED: u8 = 2;

/// Frames the capture thread may queue for the app (500 ms): more = the app stalled, drop.
pub const MIC_QUEUE: usize = 25;
/// Stereo frames the app may queue for the output thread.
pub const OUTPUT_QUEUE: usize = 16;

/// What the capture thread and the app share: the peak since the last frame (f32 bits — for
/// non-negative floats the bit order is the value order, so `fetch_max` works), the state, the
/// device really opened and its format, frames dropped because the app did not keep up.
#[derive(Debug, Default)]
pub struct MicShared {
    /// The loudest sample since the last take (f32 bits).
    pub peak_bits: AtomicU32,
    /// `DEVICE_OPENING` / `DEVICE_LIVE` / `DEVICE_FAILED`.
    pub state: AtomicU8,
    /// The device really opened.
    pub device: Mutex<String>,
    /// Its sample rate (0 until known).
    pub rate: AtomicU32,
    /// Its channel count (0 until known).
    pub channels: AtomicU32,
    /// Frames dropped because the app did not keep up.
    pub overflow: AtomicU32,
    /// Capture -> callback delay reported by the driver (µs; 0 = unknown).
    pub latency_us: AtomicU32,
}

impl MicShared {
    /// The audio callback's side: remember the loudest sample (absolute, `0..=1`).
    pub fn publish_peak(&self, peak: f32) {
        let p = if peak.is_finite() { peak.abs().min(1.0) } else { 0.0 };
        self.peak_bits.fetch_max(p.to_bits(), Ordering::Relaxed);
    }

    /// The app's side: the peak since the last call.
    pub fn take_peak(&self) -> f32 {
        let v = f32::from_bits(self.peak_bits.swap(0, Ordering::Relaxed));
        if v.is_finite() {
            v.clamp(0.0, 1.0)
        } else {
            0.0
        }
    }

    /// Set the state (a `DEVICE_*` value).
    pub fn set_state(&self, s: u8) {
        self.state.store(s, Ordering::Relaxed);
    }

    /// The state (a `DEVICE_*` value).
    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    /// The device name (empty until opened).
    pub fn device_name(&self) -> String {
        self.device.lock().map(|d| d.clone()).unwrap_or_default()
    }

    /// The state as the public [`DeviceState`].
    pub fn status(&self) -> DeviceState {
        state_of(self.state(), || self.device_name())
    }
}

/// An open microphone. **Dropping it closes the microphone**: the stop sender is dropped, the
/// capture thread wakes up, drops the cpal stream and ends (never joined — a stuck driver can
/// never freeze the game).
pub struct MicSession {
    /// The device asked for (`None` = system default).
    pub requested: Option<String>,
    /// The device's shared state.
    pub shared: Arc<MicShared>,
    /// Dropping this closes the device.
    pub stop: Option<mpsc::Sender<()>>,
    /// 16 kHz mono frames from the capture thread (`None` = a meter-only fake).
    pub frames: Option<Mutex<Receiver<MonoFrame>>>,
}

impl MicSession {
    /// A session plus the sender its producer thread pushes frames into.
    pub fn new(requested: Option<String>) -> (Self, SyncSender<MonoFrame>, mpsc::Receiver<()>) {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::sync_channel::<MonoFrame>(MIC_QUEUE);
        let s = Self { requested, shared: Arc::new(MicShared::default()), stop: Some(stop_tx), frames: Some(Mutex::new(rx)) };
        (s, tx, stop_rx)
    }

    /// Every frame captured since the last call (in order).
    pub fn drain(&self, out: &mut Vec<MonoFrame>) {
        let Some(rx) = &self.frames else { return };
        let Ok(rx) = rx.lock() else { return };
        while let Ok(f) = rx.try_recv() {
            out.push(f);
        }
    }
}

/// What the output thread and the app share.
#[derive(Debug, Default)]
pub struct OutputShared {
    /// Stereo frames the device has taken from the queue (the app keeps `produced - consumed` at
    /// the target depth).
    pub consumed: AtomicU64,
    /// `DEVICE_OPENING` / `DEVICE_LIVE` / `DEVICE_FAILED`.
    pub state: AtomicU8,
    /// The device really opened.
    pub device: Mutex<String>,
    /// Its sample rate (0 until known).
    pub rate: AtomicU32,
    /// Callback -> speaker delay reported by the driver (µs; 0 = unknown).
    pub latency_us: AtomicU32,
}

impl OutputShared {
    /// The state (a `DEVICE_*` value).
    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    /// Set the state (a `DEVICE_*` value).
    pub fn set_state(&self, s: u8) {
        self.state.store(s, Ordering::Relaxed);
    }

    /// The device name (empty until opened).
    pub fn device_name(&self) -> String {
        self.device.lock().map(|d| d.clone()).unwrap_or_default()
    }

    /// The state as the public [`DeviceState`].
    pub fn status(&self) -> DeviceState {
        state_of(self.state(), || self.device_name())
    }
}

fn state_of(s: u8, name: impl FnOnce() -> String) -> DeviceState {
    match s {
        DEVICE_OPENING => DeviceState::Opening,
        DEVICE_LIVE => DeviceState::Live(name()),
        _ => DeviceState::Failed,
    }
}

/// The open voice output stream. Dropping it closes the stream (like [`MicSession`]).
pub struct OutputSession {
    /// The device asked for (`None` = system default).
    pub requested: Option<String>,
    /// The device's shared state.
    pub shared: Arc<OutputShared>,
    /// Mixed stereo frames go here.
    pub frames: SyncSender<StereoFrame>,
    /// Dropping this closes the device.
    pub stop: Option<mpsc::Sender<()>>,
}

impl OutputSession {
    /// A session plus what its consumer thread needs (the frame receiver + the stop receiver).
    pub fn new(requested: Option<String>) -> (Self, Receiver<StereoFrame>, mpsc::Receiver<()>) {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::sync_channel::<StereoFrame>(OUTPUT_QUEUE);
        (Self { requested, shared: Arc::new(OutputShared::default()), frames: tx, stop: Some(stop_tx) }, rx, stop_rx)
    }
}

/// Everything that touches audio hardware (the seam).
pub trait AudioIo: Send + Sync + 'static {
    /// Start scanning the device names (asynchronously: fill the job when done).
    fn start_scan(&self) -> ScanJob;
    /// Open the microphone (`None` = system default; a name that does not exist = the system
    /// default too): publish peaks and 16 kHz mono frames. Asynchronous: `DEVICE_OPENING` until it
    /// runs.
    fn open_mic(&self, device: Option<String>) -> MicSession;
    /// Open the voice output stream (`None` / unknown name = the system default). Asynchronous.
    fn open_output(&self, device: Option<String>) -> OutputSession;
    /// What the "microphone" is, for status lines ("microphone" / "WAV file ...").
    fn mic_kind(&self) -> String {
        "microphone".into()
    }
}

/// No devices at all (headless tools, the stack test): scans are empty, the mic and the output
/// fail at once. Voice then simply stays off.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullIo;

impl AudioIo for NullIo {
    fn start_scan(&self) -> ScanJob {
        let job = ScanJob::default();
        job.deliver(DeviceLists::default());
        job
    }

    fn open_mic(&self, device: Option<String>) -> MicSession {
        let (s, _tx, _stop) = MicSession::new(device);
        s.shared.set_state(DEVICE_FAILED);
        s
    }

    fn open_output(&self, device: Option<String>) -> OutputSession {
        let (s, _rx, _stop) = OutputSession::new(device);
        s.shared.set_state(DEVICE_FAILED);
        s
    }
}

/// The output callback's state (pure; tested): pulls queued 16 kHz stereo frames, resamples
/// them to the device rate, writes silence when the queue is empty. No allocation, no locks.
pub struct OutputPump {
    rx: Receiver<StereoFrame>,
    current: StereoFrame,
    pos: usize,
    resampler: PullResampler,
    shared: Arc<OutputShared>,
}

impl OutputPump {
    /// A pump reading `rx`, resampling `voice_rate` -> `device_rate`.
    pub fn new(rx: Receiver<StereoFrame>, voice_rate: u32, device_rate: u32, shared: Arc<OutputShared>) -> Self {
        Self { rx, current: [0.0; FRAME * 2], pos: FRAME, resampler: PullResampler::new(voice_rate, device_rate), shared }
    }

    fn fetch(&mut self) -> [f32; 2] {
        if self.pos >= FRAME {
            match self.rx.try_recv() {
                Ok(f) => {
                    self.current = f;
                    self.pos = 0;
                    self.shared.consumed.fetch_add(1, Ordering::Relaxed);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return [0.0; 2],
            }
        }
        let i = self.pos * 2;
        self.pos += 1;
        [self.current.get(i).copied().unwrap_or(0.0), self.current.get(i + 1).copied().unwrap_or(0.0)]
    }

    /// Fill one callback's worth of interleaved f32 output with `channels` channels.
    pub fn fill(&mut self, out: &mut [f32], channels: usize) {
        let ch = channels.max(1);
        for frame in out.chunks_mut(ch) {
            let mut r = self.resampler;
            let [l, rr] = r.next(|| self.fetch());
            self.resampler = r;
            match frame {
                [m] => *m = (l + rr) * 0.5,
                [a, b, rest @ ..] => {
                    *a = l;
                    *b = rr;
                    for x in rest {
                        *x = (l + rr) * 0.5;
                    }
                }
                [] => {}
            }
        }
    }
}
