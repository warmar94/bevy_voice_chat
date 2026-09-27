//! Small, allocation-free DSP used on the audio threads and in the pipeline:
//! a low-pass biquad, a streaming PUSH resampler (capture: device rate -> 16 kHz), a PULL
//! resampler (playback: 16 kHz stereo -> device rate), the capture frame assembler, gain, peak
//! and a soft clipper. Plain `f32` buffers, so everything is unit-tested without a device.

use crate::codec::{MonoFrame, FRAME};

/// A second-order low-pass (RBJ cookbook, Butterworth Q). Used before decimating so 48 kHz input
/// does not alias into the 16 kHz voice band.
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Biquad {
    /// Pass-through (no filtering).
    pub const IDENTITY: Biquad = Biquad { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0, x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 };

    /// Low-pass at `cutoff` Hz for `rate` Hz; identity when the cutoff is at/above Nyquist or the
    /// input is bad.
    pub fn low_pass(cutoff: f32, rate: f32) -> Self {
        if !(cutoff.is_finite() && rate.is_finite() && cutoff > 0.0 && rate > 0.0 && cutoff < rate * 0.49) {
            return Self::IDENTITY;
        }
        let w0 = std::f32::consts::TAU * cutoff / rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * std::f32::consts::FRAC_1_SQRT_2);
        let a0 = 1.0 + alpha;
        let b1 = (1.0 - cos) / a0;
        Biquad { b0: b1 * 0.5, b1, b2: b1 * 0.5, a1: -2.0 * cos / a0, a2: (1.0 - alpha) / a0, x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }

    /// Filter one sample (NaN in = 0).
    pub fn process(&mut self, x: f32) -> f32 {
        let x = if x.is_finite() { x } else { 0.0 };
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        let y = if y.is_finite() { y } else { 0.0 };
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// A streaming PUSH resampler (linear interpolation, low-pass first when decimating): feed input
/// samples one at a time, it emits output samples through the callback. No allocation.
#[derive(Clone, Copy, Debug)]
pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Position of the next output between `prev` (0) and the incoming sample (1).
    t: f64,
    prev: f32,
    lp: Biquad,
}

impl Resampler {
    /// A resampler from `in_rate` to `out_rate` Hz.
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let (i, o) = (f64::from(in_rate.max(1)), f64::from(out_rate.max(1)));
        let lp = if in_rate > out_rate { Biquad::low_pass(out_rate as f32 * 0.45, in_rate as f32) } else { Biquad::IDENTITY };
        Self { step: i / o, t: 0.0, prev: 0.0, lp }
    }

    /// Push one input sample; `emit` gets every output sample it completes.
    pub fn push(&mut self, x: f32, mut emit: impl FnMut(f32)) {
        let x = self.lp.process(x);
        while self.t <= 1.0 {
            let t = self.t as f32;
            emit(self.prev + (x - self.prev) * t);
            self.t += self.step;
        }
        self.t -= 1.0;
        self.prev = x;
    }
}

/// A PULL resampler for a stereo stream: asks `fetch` for input frames as it needs them.
#[derive(Clone, Copy, Debug)]
pub struct PullResampler {
    step: f64,
    t: f64,
    prev: [f32; 2],
    next: [f32; 2],
}

impl PullResampler {
    /// A resampler from `in_rate` to `out_rate` Hz.
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { step: f64::from(in_rate.max(1)) / f64::from(out_rate.max(1)), t: 1.0, prev: [0.0; 2], next: [0.0; 2] }
    }

    /// The next output frame (L, R).
    pub fn next(&mut self, mut fetch: impl FnMut() -> [f32; 2]) -> [f32; 2] {
        while self.t >= 1.0 {
            self.prev = self.next;
            self.next = fetch();
            self.t -= 1.0;
        }
        let t = self.t as f32;
        self.t += self.step;
        [self.prev[0] + (self.next[0] - self.prev[0]) * t, self.prev[1] + (self.next[1] - self.prev[1]) * t]
    }
}

/// Capture side: interleaved device samples (any channel count, any rate) -> mono 16 kHz
/// [`FRAME`]s. Lives inside the cpal input callback: fixed buffers, no allocation.
#[derive(Clone, Debug)]
pub struct FrameAssembler {
    channels: usize,
    resampler: Resampler,
    frame: MonoFrame,
    filled: usize,
}

impl FrameAssembler {
    /// An assembler for a device running at `device_rate` Hz with `channels` channels.
    pub fn new(device_rate: u32, channels: u16, voice_rate: u32) -> Self {
        Self { channels: usize::from(channels.max(1)), resampler: Resampler::new(device_rate, voice_rate), frame: [0.0; FRAME], filled: 0 }
    }

    /// Feed interleaved samples (already f32); every completed frame goes to `emit`.
    pub fn push_interleaved(&mut self, data: &[f32], emit: impl FnMut(&MonoFrame)) {
        self.push_with(data, |s| s, emit);
    }

    /// Feed interleaved samples of any type through `to_f32` (the cpal callback converts here).
    pub fn push_with<T: Copy>(&mut self, data: &[T], mut to_f32: impl FnMut(T) -> f32, mut emit: impl FnMut(&MonoFrame)) {
        let ch = self.channels;
        for chunk in data.chunks(ch) {
            let sum: f32 = chunk.iter().map(|s| to_f32(*s)).filter(|s| s.is_finite()).sum();
            let mono = sum / ch as f32;
            let (frame, filled) = (&mut self.frame, &mut self.filled);
            self.resampler.push(mono, |y| {
                if let Some(slot) = frame.get_mut(*filled) {
                    *slot = y;
                }
                *filled += 1;
                if *filled >= FRAME {
                    emit(frame);
                    *filled = 0;
                }
            });
        }
    }
}

/// The loudest absolute sample (0 for an empty / non-finite buffer).
pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().filter(|s| s.is_finite()).fold(0.0_f32, |m, s| m.max(s.abs())).min(1.0)
}

/// Multiply by `gain` and clamp to `-1..=1` (NaN -> 0).
pub fn apply_gain(samples: &mut [f32], gain: f32) {
    let g = if gain.is_finite() { gain.max(0.0) } else { 1.0 };
    for s in samples {
        let v = *s * g;
        *s = if v.is_finite() { v.clamp(-1.0, 1.0) } else { 0.0 };
    }
}

/// Transparent below 0.8, then a smooth knee that never exceeds 1.0 (the mix of several loud
/// voices must not wrap or crackle).
pub fn soft_clip(x: f32) -> f32 {
    if !x.is_finite() {
        return 0.0;
    }
    let a = x.abs();
    if a <= 0.8 {
        x
    } else {
        (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh()).copysign(x)
    }
}
