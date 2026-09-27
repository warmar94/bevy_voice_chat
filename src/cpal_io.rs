//! [`CpalIo`]: the real [`AudioIo`] on `cpal` 0.17 (the version Bevy 0.19's rodio already uses —
//! one cpal in the tree). Every cpal call runs on a worker thread: device enumeration can take a
//! moment (WASAPI), and a cpal `Stream` lives and dies on the thread that built it. Nothing here
//! panics: every failure is a logged warning + an empty list / `DEVICE_FAILED` (observable through
//! [`crate::VoiceChatState`]), and each worker body is wrapped in `catch_unwind` in case a driver
//! misbehaves.
//!
//! The audio callbacks do fixed work only: convert, downmix, resample, publish a peak (atomic),
//! `try_send` a fixed-size frame into a bounded channel (full = dropped + counted). No locks, no
//! allocation, no panics.

use crate::codec::{MonoFrame, VOICE_RATE};
use crate::dsp::FrameAssembler;
use crate::io::{AudioIo, DeviceLists, MicSession, MicShared, OutputPump, OutputSession, OutputShared, ScanJob, DEVICE_FAILED, DEVICE_LIVE};
use crate::mixer::StereoFrame;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use tracing::warn;

/// The real hardware.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpalIo;

/// A device's human-readable name (what a game stores in its settings).
fn device_name(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|desc| desc.name().trim().to_string()).filter(|n| !n.is_empty())
}

/// Every input and output device name, deduplicated in order. Errors = empty lists.
fn scan() -> DeviceLists {
    let host = cpal::default_host();
    let names = |devs: Option<Vec<cpal::Device>>| {
        let mut out: Vec<String> = Vec::new();
        for n in devs.unwrap_or_default().iter().filter_map(device_name) {
            if !out.contains(&n) {
                out.push(n);
            }
        }
        out
    };
    let inputs = host.input_devices().map(|d| d.collect()).inspect_err(|e| warn!("voice chat: cannot list microphones ({e})")).ok();
    let outputs = host.output_devices().map(|d| d.collect()).inspect_err(|e| warn!("voice chat: cannot list output devices ({e})")).ok();
    DeviceLists { inputs: names(inputs), outputs: names(outputs) }
}

/// `wanted` by name (else the system default; `None` + a warning when there is none).
fn pick(host: &cpal::Host, wanted: Option<&str>, input: bool) -> Option<cpal::Device> {
    let what = if input { "microphone" } else { "output device" };
    let named = wanted.and_then(|w| {
        let list = if input { host.input_devices().ok().map(|d| d.collect::<Vec<_>>()) } else { host.output_devices().ok().map(|d| d.collect::<Vec<_>>()) };
        let found = list.unwrap_or_default().into_iter().find(|d| device_name(d).as_deref() == Some(w));
        if found.is_none() {
            warn!("voice chat: {what} {w:?} not found - using the system default");
        }
        found
    });
    let d = named.or_else(|| if input { host.default_input_device() } else { host.default_output_device() });
    if d.is_none() {
        warn!("voice chat: no {what} found");
    }
    d
}

/// The capture thread: open the mic, publish peaks + frames until `stop` is dropped.
fn capture(wanted: Option<String>, shared: Arc<MicShared>, frames: SyncSender<MonoFrame>, stop: mpsc::Receiver<()>) {
    let host = cpal::default_host();
    let Some(device) = pick(&host, wanted.as_deref(), true) else {
        shared.set_state(DEVICE_FAILED);
        return;
    };
    let stream = match open_input(&device, shared.clone(), frames) {
        Ok(s) => s,
        Err(e) => {
            warn!("voice chat: cannot open the microphone ({e})");
            shared.set_state(DEVICE_FAILED);
            return;
        }
    };
    if let Err(e) = stream.play() {
        warn!("voice chat: cannot start the microphone ({e})");
        shared.set_state(DEVICE_FAILED);
        return;
    }
    if let Ok(mut d) = shared.device.lock() {
        *d = device_name(&device).unwrap_or_default();
    }
    shared.set_state(DEVICE_LIVE);
    // Blocks until the session (holding the sender) is dropped.
    let _ = stop.recv();
    drop(stream);
}

fn open_input(device: &cpal::Device, shared: Arc<MicShared>, frames: SyncSender<MonoFrame>) -> Result<cpal::Stream, String> {
    let supported = device.default_input_config().map_err(|e| e.to_string())?;
    let config: cpal::StreamConfig = supported.config();
    shared.rate.store(config.sample_rate, Ordering::Relaxed);
    shared.channels.store(u32::from(config.channels), Ordering::Relaxed);
    let r = match supported.sample_format() {
        SampleFormat::F32 => build_input::<f32>(device, &config, shared, frames),
        SampleFormat::I16 => build_input::<i16>(device, &config, shared, frames),
        SampleFormat::U16 => build_input::<u16>(device, &config, shared, frames),
        SampleFormat::I32 => build_input::<i32>(device, &config, shared, frames),
        SampleFormat::I8 => build_input::<i8>(device, &config, shared, frames),
        SampleFormat::U8 => build_input::<u8>(device, &config, shared, frames),
        other => return Err(format!("unsupported sample format {other}")),
    };
    r.map_err(|e| e.to_string())
}

fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<MicShared>,
    frames: SyncSender<MonoFrame>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let on_error = shared.clone();
    let mut assembler = FrameAssembler::new(config.sample_rate, config.channels, VOICE_RATE);
    device.build_input_stream(
        config,
        move |data: &[T], info: &cpal::InputCallbackInfo| {
            let ts = info.timestamp();
            if let Some(d) = ts.callback.duration_since(&ts.capture) {
                shared.latency_us.store(d.as_micros().min(u128::from(u32::MAX)) as u32, Ordering::Relaxed);
            }
            let mut loudest = 0.0_f32;
            assembler.push_with(
                data,
                |s| {
                    let v = <f32 as FromSample<T>>::from_sample_(s);
                    if v.is_finite() {
                        loudest = loudest.max(v.abs());
                    }
                    v
                },
                |frame| match frames.try_send(*frame) {
                    Ok(()) | Err(TrySendError::Disconnected(_)) => {}
                    Err(TrySendError::Full(_)) => {
                        shared.overflow.fetch_add(1, Ordering::Relaxed);
                    }
                },
            );
            shared.publish_peak(loudest);
        },
        // Unplugged (or any stream error): the state turns Failed; voice stops sending.
        move |_e| on_error.set_state(DEVICE_FAILED),
        None,
    )
}

/// The output thread: open the device, play queued frames until `stop` is dropped.
fn playback(wanted: Option<String>, shared: Arc<OutputShared>, frames: Receiver<StereoFrame>, stop: mpsc::Receiver<()>) {
    let host = cpal::default_host();
    let Some(device) = pick(&host, wanted.as_deref(), false) else {
        shared.set_state(DEVICE_FAILED);
        return;
    };
    let stream = match open_output_stream(&device, shared.clone(), frames) {
        Ok(s) => s,
        Err(e) => {
            warn!("voice chat: cannot open the output device ({e})");
            shared.set_state(DEVICE_FAILED);
            return;
        }
    };
    if let Err(e) = stream.play() {
        warn!("voice chat: cannot start the output device ({e})");
        shared.set_state(DEVICE_FAILED);
        return;
    }
    if let Ok(mut d) = shared.device.lock() {
        *d = device_name(&device).unwrap_or_default();
    }
    shared.set_state(DEVICE_LIVE);
    let _ = stop.recv();
    drop(stream);
}

fn open_output_stream(device: &cpal::Device, shared: Arc<OutputShared>, frames: Receiver<StereoFrame>) -> Result<cpal::Stream, String> {
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let config: cpal::StreamConfig = supported.config();
    shared.rate.store(config.sample_rate, Ordering::Relaxed);
    let pump = OutputPump::new(frames, VOICE_RATE, config.sample_rate, shared.clone());
    let r = match supported.sample_format() {
        SampleFormat::F32 => build_output::<f32>(device, &config, pump, shared),
        SampleFormat::I16 => build_output::<i16>(device, &config, pump, shared),
        SampleFormat::U16 => build_output::<u16>(device, &config, pump, shared),
        SampleFormat::I32 => build_output::<i32>(device, &config, pump, shared),
        SampleFormat::I8 => build_output::<i8>(device, &config, pump, shared),
        SampleFormat::U8 => build_output::<u8>(device, &config, pump, shared),
        other => return Err(format!("unsupported sample format {other}")),
    };
    r.map_err(|e| e.to_string())
}

/// Output callback scratch: the pump writes f32, converted per sample format. Sized once.
const OUT_SCRATCH: usize = 16_384;

fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut pump: OutputPump,
    shared: Arc<OutputShared>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels.max(1));
    let on_error = shared.clone();
    let mut scratch = vec![0.0_f32; OUT_SCRATCH - OUT_SCRATCH % channels];
    device.build_output_stream(
        config,
        move |data: &mut [T], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            if let Some(d) = ts.playback.duration_since(&ts.callback) {
                shared.latency_us.store(d.as_micros().min(u128::from(u32::MAX)) as u32, Ordering::Relaxed);
            }
            for chunk in data.chunks_mut(scratch.len().max(channels)) {
                let n = chunk.len().min(scratch.len());
                let (buf, _) = scratch.split_at_mut(n);
                pump.fill(buf, channels);
                for (o, s) in chunk.iter_mut().zip(buf.iter()) {
                    *o = T::from_sample(*s);
                }
            }
        },
        move |_e| on_error.set_state(DEVICE_FAILED),
        None,
    )
}

impl AudioIo for CpalIo {
    fn start_scan(&self) -> ScanJob {
        let job = ScanJob::default();
        let out = job.clone();
        let spawned = std::thread::Builder::new().name("voice-chat-scan".into()).spawn(move || {
            let lists = std::panic::catch_unwind(scan).unwrap_or_else(|_| {
                warn!("voice chat: the device scan failed - no devices listed");
                DeviceLists::default()
            });
            out.deliver(lists);
        });
        if let Err(e) = spawned {
            warn!("voice chat: cannot start the device scan ({e})");
            job.deliver(DeviceLists::default());
        }
        job
    }

    fn open_mic(&self, device: Option<String>) -> MicSession {
        let (session, tx, stop) = MicSession::new(device.clone());
        let thread_shared = session.shared.clone();
        let spawned = std::thread::Builder::new().name("voice-chat-mic".into()).spawn(move || {
            let fail = thread_shared.clone();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| capture(device, thread_shared, tx, stop))).is_err() {
                warn!("voice chat: the microphone thread failed");
                fail.set_state(DEVICE_FAILED);
            }
        });
        if let Err(e) = spawned {
            warn!("voice chat: cannot start the microphone thread ({e})");
            session.shared.set_state(DEVICE_FAILED);
        }
        session
    }

    fn open_output(&self, device: Option<String>) -> OutputSession {
        let (session, rx, stop) = OutputSession::new(device.clone());
        let thread_shared = session.shared.clone();
        let spawned = std::thread::Builder::new().name("voice-chat-out".into()).spawn(move || {
            let fail = thread_shared.clone();
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| playback(device, thread_shared, rx, stop))).is_err() {
                warn!("voice chat: the output thread failed");
                fail.set_state(DEVICE_FAILED);
            }
        });
        if let Err(e) = spawned {
            warn!("voice chat: cannot start the output thread ({e})");
            session.shared.set_state(DEVICE_FAILED);
        }
        session
    }
}
