//! A WAV file played as the MICROPHONE, looped — so a second local game instance "talks" without
//! a second microphone (testing, demos). A tiny WAV reader (PCM 8/16/24/32-bit, float 32-bit,
//! WAVE_FORMAT_EXTENSIBLE) — no extra crate.

use crate::codec::{MonoFrame, FRAME, FRAME_MS, VOICE_RATE};
use crate::dsp::{peak, Resampler};
use crate::io::{AudioIo, MicSession, OutputSession, ScanJob, DEVICE_LIVE};
use std::sync::mpsc::{RecvTimeoutError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A decoded WAV: mono samples at its own rate.
#[derive(Clone, Debug, PartialEq)]
pub struct Wav {
    /// Sample rate (Hz).
    pub rate: u32,
    /// Channels in the file.
    pub channels: u16,
    /// The samples, channels averaged.
    pub mono: Vec<f32>,
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, *b.get(i + 3)?]))
}

/// Parse a WAV file's bytes into mono f32 (channels averaged). Errors are one readable sentence.
pub fn parse_wav(bytes: &[u8]) -> Result<Wav, String> {
    if bytes.get(0..4) != Some(b"RIFF") || bytes.get(8..12) != Some(b"WAVE") {
        return Err("not a RIFF/WAVE file".into());
    }
    let mut i = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut data: Option<&[u8]> = None;
    while i + 8 <= bytes.len() {
        let id = bytes.get(i..i + 4).unwrap_or_default();
        let size = u32_at(bytes, i + 4).ok_or("truncated chunk")? as usize;
        let body_start = i + 8;
        let body_end = body_start.saturating_add(size).min(bytes.len());
        let body = bytes.get(body_start..body_end).unwrap_or_default();
        match id {
            b"fmt " => {
                let mut format = u16_at(body, 0).ok_or("short fmt chunk")?;
                let channels = u16_at(body, 2).ok_or("short fmt chunk")?;
                let rate = u32_at(body, 4).ok_or("short fmt chunk")?;
                let bits = u16_at(body, 14).ok_or("short fmt chunk")?;
                if format == 0xFFFE {
                    // WAVE_FORMAT_EXTENSIBLE: the sub-format GUID starts with the real tag.
                    format = u16_at(body, 24).ok_or("short extensible fmt chunk")?;
                }
                fmt = Some((format, channels, rate, bits));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks are word aligned.
        i = body_start.saturating_add(size).saturating_add(size % 2);
    }
    let (format, channels, rate, bits) = fmt.ok_or("no fmt chunk")?;
    let data = data.ok_or("no data chunk")?;
    if channels == 0 || rate == 0 {
        return Err("zero channels or sample rate".into());
    }
    let width = usize::from(bits / 8);
    let sample = |s: &[u8]| -> Option<f32> {
        Some(match (format, bits) {
            (1, 8) => (f32::from(*s.first()?) - 128.0) / 128.0,
            (1, 16) => f32::from(i16::from_le_bytes([*s.first()?, *s.get(1)?])) / 32768.0,
            (1, 24) => (i32::from_le_bytes([0, *s.first()?, *s.get(1)?, *s.get(2)?]) >> 8) as f32 / 8_388_608.0,
            (1, 32) => i32::from_le_bytes([*s.first()?, *s.get(1)?, *s.get(2)?, *s.get(3)?]) as f32 / 2_147_483_648.0,
            (3, 32) => f32::from_le_bytes([*s.first()?, *s.get(1)?, *s.get(2)?, *s.get(3)?]),
            _ => return None,
        })
    };
    if sample(&[0, 0, 0, 0]).is_none() {
        return Err(format!("unsupported WAV format (tag {format}, {bits} bits)"));
    }
    let frame_bytes = width * usize::from(channels);
    let mut mono = Vec::with_capacity(data.len() / frame_bytes.max(1));
    for f in data.chunks_exact(frame_bytes.max(1)) {
        let sum: f32 = f.chunks_exact(width.max(1)).filter_map(sample).filter(|v| v.is_finite()).sum();
        mono.push((sum / f32::from(channels)).clamp(-1.0, 1.0));
    }
    if mono.is_empty() {
        return Err("no samples".into());
    }
    Ok(Wav { rate, channels, mono })
}

/// Resample mono samples to the voice rate.
pub fn to_voice_rate(mono: &[f32], rate: u32) -> Vec<f32> {
    let mut r = Resampler::new(rate, VOICE_RATE);
    let mut out = Vec::with_capacity(mono.len() * VOICE_RATE as usize / rate.max(1) as usize + FRAME);
    for &x in mono {
        r.push(x, |y| out.push(y));
    }
    out
}

/// The WAV file as the microphone (scans + output go to `inner`, the real hardware).
pub struct WavFileIo {
    /// Scans and the output go to this seam.
    pub inner: Arc<dyn AudioIo>,
    /// The file name (status lines).
    pub name: String,
    /// Mono at [`VOICE_RATE`], at least one frame long.
    pub samples: Arc<Vec<f32>>,
}

impl WavFileIo {
    /// Read + decode + resample the file (at startup, once). Errors are one sentence.
    pub fn load(path: &std::path::Path, inner: Arc<dyn AudioIo>) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let wav = parse_wav(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut samples = to_voice_rate(&wav.mono, wav.rate);
        if samples.len() < FRAME {
            samples.resize(FRAME, 0.0);
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "voice.wav".into());
        Ok(Self { inner, name, samples: Arc::new(samples) })
    }
}

impl AudioIo for WavFileIo {
    fn start_scan(&self) -> ScanJob {
        self.inner.start_scan()
    }

    fn open_output(&self, device: Option<String>) -> OutputSession {
        self.inner.open_output(device)
    }

    fn mic_kind(&self) -> String {
        format!("WAV file {:?} (looped)", self.name)
    }

    /// A feeder thread pushes one frame every 20 ms (deadline-based, no drift), looping the file.
    fn open_mic(&self, device: Option<String>) -> MicSession {
        let (session, tx, stop) = MicSession::new(device);
        let shared = session.shared.clone();
        if let Ok(mut d) = shared.device.lock() {
            *d = format!("WAV {}", self.name);
        }
        shared.rate.store(VOICE_RATE, std::sync::atomic::Ordering::Relaxed);
        shared.channels.store(1, std::sync::atomic::Ordering::Relaxed);
        shared.set_state(DEVICE_LIVE);
        let samples = self.samples.clone();
        let spawned = std::thread::Builder::new().name("voice-chat-wav".into()).spawn(move || {
            let period = Duration::from_millis(u64::from(FRAME_MS));
            let mut next = Instant::now() + period;
            let mut pos = 0usize;
            let len = samples.len().max(1);
            loop {
                let wait = next.saturating_duration_since(Instant::now());
                match stop.recv_timeout(wait) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
                next += period;
                let mut frame: MonoFrame = [0.0; FRAME];
                for slot in frame.iter_mut() {
                    *slot = samples.get(pos % len).copied().unwrap_or(0.0);
                    pos = (pos + 1) % len;
                }
                shared.publish_peak(peak(&frame));
                match tx.try_send(frame) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        shared.overflow.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
        });
        if spawned.is_err() {
            session.shared.set_state(crate::io::DEVICE_FAILED);
        }
        session
    }
}
