//! **The mixer**: one [`JitterBuffer`] per speaker, pulled 20 ms at a time on the output device's
//! clock, through the incoming filter hook, each scaled by its spatial (left, right) gain (ramped
//! across the frame, no zipper noise), summed, times the voice volume, soft-clipped into ONE
//! stereo frame.

use crate::codec::{MonoFrame, FRAME};
use crate::dsp::soft_clip;
use crate::jitter::{Insert, JitterBuffer, JitterConfig, JitterStats, Pull};
use crate::SpeakerId;
use std::collections::BTreeMap;

/// One stereo frame, interleaved L R.
pub type StereoFrame = [f32; FRAME * 2];

/// Whose audio a channel carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SpeakerKey {
    /// A remote speaker.
    Remote(SpeakerId),
    /// The mic test: this player's own voice, through the same filter + codec + buffer.
    Loopback,
}

#[derive(Clone, Debug)]
/// One speaker's channel: its buffer, gains and timing.
pub struct SpeakerChannel {
    /// The speaker's jitter buffer.
    pub jitter: JitterBuffer,
    gain: (f32, f32),
    target: (f32, f32),
    /// Seconds (real time) when a real frame of this speaker last played.
    last_played: Option<f64>,
    last_packet: f64,
    /// Sender clock -> mixed, ms (meaningful when both run on one PC).
    pub latency_ms: Option<u32>,
}

impl SpeakerChannel {
    fn new(cfg: JitterConfig, now: f64) -> Self {
        Self { jitter: JitterBuffer::new(cfg), gain: (0.0, 0.0), target: (1.0, 1.0), last_played: None, last_packet: now, latency_ms: None }
    }
}

#[derive(Clone, Debug, Default)]
/// Every speaker's channel, mixed into one stereo stream.
pub struct Mixer {
    cfg: JitterConfig,
    channels: BTreeMap<SpeakerKey, SpeakerChannel>,
    /// Stats of speakers already forgotten (the totals stay cumulative).
    retired: JitterStats,
}

impl Mixer {
    /// An empty mixer.
    pub fn new(cfg: JitterConfig) -> Self {
        Self { cfg, channels: BTreeMap::new(), retired: JitterStats::default() }
    }

    /// New jitter sizes (for channels created from now on).
    pub fn set_config(&mut self, cfg: JitterConfig) {
        self.cfg = cfg;
    }

    /// A decoded frame from `key` (seq, sender clock stamp); `now` = real seconds.
    pub fn insert(&mut self, key: SpeakerKey, seq: u32, frame: &MonoFrame, ts: u32, now: f64) -> Insert {
        let cfg = self.cfg;
        let ch = self.channels.entry(key).or_insert_with(|| SpeakerChannel::new(cfg, now));
        ch.last_packet = now;
        ch.jitter.insert(seq, frame, ts)
    }

    /// The (left, right) gain this speaker should have (spatial x pan).
    pub fn set_target(&mut self, key: SpeakerKey, left: f32, right: f32) {
        if let Some(ch) = self.channels.get_mut(&key) {
            let g = |v: f32| if v.is_finite() { v.clamp(0.0, 2.0) } else { 0.0 };
            ch.target = (g(left), g(right));
        }
    }

    /// Every speaker with a channel.
    pub fn keys(&self) -> Vec<SpeakerKey> {
        self.channels.keys().copied().collect()
    }

    /// One speaker's channel.
    pub fn channel(&self, key: SpeakerKey) -> Option<&SpeakerChannel> {
        self.channels.get(&key)
    }

    /// Anything buffered or playing (else the mixer produces nothing and the device idles).
    pub fn any_active(&self) -> bool {
        self.channels.values().any(|c| !c.jitter.is_idle())
    }

    /// Mix one stereo frame into `out` (overwritten). `volume` = the game's voice volume.
    /// `filter(key, frame)` runs on every speaker's pulled frame before mixing (the incoming
    /// filter hook). Returns whether any speaker produced sound.
    pub fn mix(&mut self, out: &mut StereoFrame, volume: f32, now: f64, now_ms: u32, mut filter: impl FnMut(SpeakerKey, &mut MonoFrame)) -> bool {
        out.fill(0.0);
        let vol = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 0.0 };
        let mut frame: MonoFrame = [0.0; FRAME];
        let mut any = false;
        for (key, ch) in self.channels.iter_mut() {
            let pulled = ch.jitter.pull(&mut frame);
            let (g0, g1) = (ch.gain, ch.target);
            ch.gain = g1;
            match pulled {
                Pull::Silent => continue,
                Pull::Played { ts } => {
                    ch.last_played = Some(now);
                    let lat = now_ms.wrapping_sub(ts);
                    ch.latency_ms = (lat < 10_000).then_some(lat);
                }
                Pull::Concealed => {}
            }
            filter(*key, &mut frame);
            any = true;
            for (i, (s, pair)) in frame.iter().zip(out.chunks_exact_mut(2)).enumerate() {
                let t = (i + 1) as f32 / FRAME as f32;
                let l = g0.0 + (g1.0 - g0.0) * t;
                let r = g0.1 + (g1.1 - g0.1) * t;
                if let [a, b] = pair {
                    *a += s * l;
                    *b += s * r;
                }
            }
        }
        for s in out.iter_mut() {
            *s = soft_clip(*s * vol);
        }
        any
    }

    /// Is `key` being heard: a real frame played within `hold` seconds and its gain is not zero?
    pub fn heard(&self, key: SpeakerKey, now: f64, hold: f64) -> bool {
        self.channels.get(&key).is_some_and(|c| c.last_played.is_some_and(|t| now - t <= hold) && (c.target.0 > 1e-3 || c.target.1 > 1e-3))
    }

    /// Forget speakers silent for `timeout` seconds (a leaver's buffer does not live forever).
    pub fn forget_idle(&mut self, now: f64, timeout: f64) {
        let gone: Vec<SpeakerKey> = self.channels.iter().filter(|(_, c)| c.jitter.is_idle() && now - c.last_packet > timeout).map(|(k, _)| *k).collect();
        for k in gone {
            self.remove(k);
        }
    }

    /// Drop one speaker's channel.
    pub fn remove(&mut self, key: SpeakerKey) {
        if let Some(c) = self.channels.remove(&key) {
            add_stats(&mut self.retired, &c.jitter.stats);
        }
    }

    /// Drop every channel.
    pub fn clear(&mut self) {
        for k in self.keys() {
            self.remove(k);
        }
    }

    /// Summed jitter stats over every speaker (+ total depth).
    pub fn totals(&self) -> (JitterStats, usize) {
        let mut t = self.retired;
        let mut depth = 0;
        for c in self.channels.values() {
            add_stats(&mut t, &c.jitter.stats);
            depth += c.jitter.depth();
        }
        (t, depth)
    }
}

fn add_stats(t: &mut JitterStats, s: &JitterStats) {
    t.received += s.received;
    t.played += s.played;
    t.late += s.late;
    t.duplicate += s.duplicate;
    t.lost += s.lost;
    t.concealed += s.concealed;
    t.overflow += s.overflow;
}
