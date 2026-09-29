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

/// One enumerated device: the cpal handle, the LABEL a player sees and a game stores
/// ([`device_labels`]), and the plain name 0.1 / 0.2.0 stored (still accepted by [`pick`]).
struct Entry {
    device: cpal::Device,
    label: String,
    name: String,
}

/// Whether cpal's default host here lists each device once (Windows / WASAPI, macOS /
/// CoreAudio, ...): then two entries with one label are two real devices and get numbered, and
/// Windows' friendly "Name (Interface)" is used. On Linux / BSD (ALSA) cpal lists ONE card
/// several times, once per PCM mode (`sysdefault:`, `front:`, `hw:`, `plughw:`, ...), all under
/// the same name; there identical labels are merged into one entry, as 0.2.0 did, instead of
/// becoming meaningless "#2".."#8" choices.
const DISTINCT_ENDPOINTS: bool = !cfg!(any(target_os = "linux", target_os = "freebsd", target_os = "netbsd", target_os = "openbsd", target_os = "dragonfly"));

/// The label of one device before duplicates are handled.
///
/// On a host with distinct endpoints (`distinct`, Windows): the full friendly name
/// ("Microphone (USB PnP Audio Device)"), which WASAPI puts in `extended` when it differs from
/// the short name; failing that, "name (driver)" from the interface name WASAPI reports as the
/// driver; failing that, the name. Elsewhere: the plain name (ALSA's `extended` holds the card's
/// description lines, not a friendly name). Pure (tested).
pub(crate) fn raw_label(name: &str, extended: &[String], driver: Option<&str>, distinct: bool) -> String {
    let name = name.trim();
    if !distinct {
        return name.to_string();
    }
    let prefix = format!("{name} (");
    if let Some(friendly) = extended.iter().map(|l| l.trim()).find(|l| l.starts_with(&prefix) && l.ends_with(')')) {
        return friendly.to_string();
    }
    match driver.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) if !name.ends_with(&format!("({d})")) => format!("{name} ({d})"),
        _ => name.to_string(),
    }
}

/// Final labels in enumeration order; `None` = merged into an earlier entry (not listed).
///
/// `number` (hosts with distinct endpoints): every device is listed, and the 2nd, 3rd... device
/// whose label is taken gets " #2", " #3"..., never a label another device really has, so each
/// label picks exactly one device. Otherwise identical labels are merged (the first one
/// stays). Pure (tested).
pub(crate) fn device_labels(raw: &[String], number: bool) -> Vec<Option<String>> {
    let mut used: Vec<String> = Vec::with_capacity(raw.len());
    let mut out = Vec::with_capacity(raw.len());
    for label in raw {
        if !used.contains(label) {
            used.push(label.clone());
            out.push(Some(label.clone()));
            continue;
        }
        if !number {
            out.push(None);
            continue;
        }
        let mut n = 2;
        let mut candidate = format!("{label} #{n}");
        while used.contains(&candidate) || raw.contains(&candidate) {
            n += 1;
            candidate = format!("{label} #{n}");
        }
        used.push(candidate.clone());
        out.push(Some(candidate));
    }
    out
}

/// Which entry a stored setting means: its exact label, else (a setting saved by 0.1 / 0.2.0,
/// which stored the plain name) the first device with that plain name; `None` = not present.
/// Pure (tested).
pub(crate) fn match_setting(labels: &[String], names: &[String], wanted: &str) -> Option<usize> {
    let wanted = wanted.trim();
    labels.iter().position(|l| l == wanted).or_else(|| names.iter().position(|n| n == wanted))
}

/// Every device of one direction with its label. Devices without a name are skipped, and so is
/// a device whose driver panics while being described (WASAPI `description()` can `expect` on a
/// broken endpoint): it is left out, the others are listed.
fn entries(devs: Vec<cpal::Device>) -> Vec<Entry> {
    let named: Vec<(cpal::Device, String, String)> = devs
        .into_iter()
        .filter_map(|d| {
            let desc = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.description().ok())).ok().flatten()?;
            let name = desc.name().trim().to_string();
            if name.is_empty() {
                return None;
            }
            let raw = raw_label(&name, desc.extended(), desc.driver(), DISTINCT_ENDPOINTS);
            Some((d, name, raw))
        })
        .collect();
    let raws: Vec<String> = named.iter().map(|(_, _, r)| r.clone()).collect();
    named
        .into_iter()
        .zip(device_labels(&raws, DISTINCT_ENDPOINTS))
        .filter_map(|((device, name, _), label)| label.map(|label| Entry { device, label, name }))
        .collect()
}

/// Every device of one direction, labelled. A panic while enumerating (a driver) is an error
/// here, never a dead thread; one device failing to describe itself is skipped in [`entries`].
fn list(host: &cpal::Host, input: bool) -> Result<Vec<Entry>, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let devs: Result<Vec<cpal::Device>, _> = if input { host.input_devices().map(|d| d.collect()) } else { host.output_devices().map(|d| d.collect()) };
        devs.map(entries).map_err(|e| e.to_string())
    }))
    .unwrap_or_else(|_| Err("a device driver failed while listing".into()))
}

/// Every input and output device label, in order. Errors = empty lists.
fn scan() -> DeviceLists {
    let host = cpal::default_host();
    let labels = |input: bool| -> Vec<String> {
        let what = if input { "microphones" } else { "output devices" };
        list(&host, input).inspect_err(|e| warn!("voice chat: cannot list {what} ({e})")).unwrap_or_default().into_iter().map(|e| e.label).collect()
    };
    DeviceLists { inputs: labels(true), outputs: labels(false) }
}

/// `wanted` by label (or a 0.1 / 0.2.0 plain name), else the system default; `None` + a warning
/// when there is none. Returns the device and the label to report.
fn pick(host: &cpal::Host, wanted: Option<&str>, input: bool) -> Option<(cpal::Device, String)> {
    let what = if input { "microphone" } else { "output device" };
    let all = list(host, input).inspect_err(|e| warn!("voice chat: cannot list {what}s ({e})")).unwrap_or_default();
    let labels: Vec<String> = all.iter().map(|e| e.label.clone()).collect();
    let names: Vec<String> = all.iter().map(|e| e.name.clone()).collect();
    if let Some(w) = wanted {
        match match_setting(&labels, &names, w) {
            Some(i) => return all.into_iter().nth(i).map(|e| (e.device, e.label)),
            None => warn!("voice chat: {what} {w:?} not found - using the system default"),
        }
    }
    let Some(d) = (if input { host.default_input_device() } else { host.default_output_device() }) else {
        warn!("voice chat: no {what} found");
        return None;
    };
    // The default's label, found by id in the same list, so the state shows what the menu shows.
    let label = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let id = d.id().ok();
        all.iter()
            .find(|e| id.is_some() && e.device.id().ok() == id)
            .map(|e| e.label.clone())
            .or_else(|| d.description().ok().map(|desc| raw_label(desc.name(), desc.extended(), desc.driver(), DISTINCT_ENDPOINTS)))
    }))
    .ok()
    .flatten()
    .unwrap_or_default();
    Some((d, label))
}

/// The capture thread: open the mic, publish peaks + frames until `stop` is dropped.
fn capture(wanted: Option<String>, shared: Arc<MicShared>, frames: SyncSender<MonoFrame>, stop: mpsc::Receiver<()>) {
    let host = cpal::default_host();
    let Some((device, label)) = pick(&host, wanted.as_deref(), true) else {
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
        *d = label;
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
    let Some((device, label)) = pick(&host, wanted.as_deref(), false) else {
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
        *d = label;
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
