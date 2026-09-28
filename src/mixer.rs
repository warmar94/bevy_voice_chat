//! **The mixer**: one jitter buffer per speaker, pulled 20 ms at a time on the output device's
//! clock, through the incoming filter hook, each scaled by its spatial (left, right) gain (ramped
//! across the frame, no zipper noise), summed, times the voice volume, soft-clipped into ONE
//! stereo frame.
//!
//! A buffered frame is either PCM (decoded on arrival: IMA-ADPCM, whose frames are
//! self-contained) or an ENCODED packet of a stateful codec (Opus), decoded here at playout, in
//! sequence order, by that speaker's own decoder, with the codec's own concealment for a lost
//! frame. A speaker may switch codec mid-stream: the payload says which, and the channel switches
//! to a fresh decoder.

use crate::codec::{self, MonoFrame, VoiceCodec, FRAME};
use crate::dsp::soft_clip;
use crate::jitter::{fade_tail, repeat_halved, Insert, JitterConfig, JitterCore, JitterStats, Next, Pull};
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

/// A buffered frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Payload {
    /// Decoded on arrival (IMA-ADPCM).
    Pcm(Box<MonoFrame>),
    /// An encoded packet of a stateful codec, decoded at playout.
    Encoded {
        /// The wire codec id.
        codec: u8,
        /// The packet.
        bytes: Vec<u8>,
    },
}

/// A speaker's jitter buffer: [`JitterCore`] over [`Payload`]s.
pub type PacketJitter = JitterCore<Payload>;

/// One speaker's playout decoder: which codec it last played, its (stateful) decoder, the last
/// played frame for repeat-and-fade.
#[derive(Default)]
struct ChannelDecoder {
    /// Codec of the last played ENCODED payload (`None` = PCM).
    codec: Option<u8>,
    dec: Option<Box<dyn VoiceCodec>>,
    last: Option<Box<MonoFrame>>,
    /// The jitter generation `dec` belongs to.
    generation: u32,
}

/// One step of a channel, owned (so the jitter buffer is free again).
enum Step {
    Play(Payload, u32),
    Tail,
    Conceal(usize, Option<Vec<u8>>),
    Silent,
}

/// One speaker's channel: its buffer, decoder, gains and timing.
pub struct SpeakerChannel {
    /// The speaker's jitter buffer (`stats`, `depth()`, ..).
    pub jitter: PacketJitter,
    decoder: ChannelDecoder,
    gain: (f32, f32),
    target: (f32, f32),
    /// Seconds (real time) when a real frame of this speaker last played.
    last_played: Option<f64>,
    last_packet: f64,
    /// Sender clock -> mixed, ms (meaningful when both run on one PC).
    pub latency_ms: Option<u32>,
}

impl std::fmt::Debug for SpeakerChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpeakerChannel")
            .field("jitter", &self.jitter)
            .field("decoder_codec", &self.decoder.codec)
            .field("gain", &self.gain)
            .field("target", &self.target)
            .field("last_played", &self.last_played)
            .field("latency_ms", &self.latency_ms)
            .finish()
    }
}

impl SpeakerChannel {
    fn new(cfg: JitterConfig, now: f64) -> Self {
        Self {
            jitter: PacketJitter::new(cfg),
            decoder: ChannelDecoder::default(),
            gain: (0.0, 0.0),
            target: (1.0, 1.0),
            last_played: None,
            last_packet: now,
            latency_ms: None,
        }
    }

    /// The codec of the last played encoded payload (`None` = PCM / nothing yet).
    pub fn decoder_codec(&self) -> Option<u8> {
        self.decoder.codec
    }

    /// Produce this channel's next 20 ms into `frame`.
    pub(crate) fn pull(&mut self, frame: &mut MonoFrame) -> Pull {
        let generation = self.jitter.generation();
        if generation != self.decoder.generation {
            // The sender restarted: a stateful decoder must not carry the old stream over.
            self.decoder.generation = generation;
            if let Some(d) = self.decoder.dec.as_mut() {
                d.reset();
            }
            self.decoder.last = None;
        }
        let step = match self.jitter.next_frame() {
            Next::Play { payload, ts } => Step::Play(payload, ts),
            Next::Tail => Step::Tail,
            Next::Conceal { run, next } => Step::Conceal(
                run,
                next.and_then(|p| match p {
                    Payload::Encoded { bytes, .. } => Some(bytes.clone()),
                    Payload::Pcm(_) => None,
                }),
            ),
            Next::Silent => Step::Silent,
        };
        let d = &mut self.decoder;
        let pulled = match step {
            Step::Play(Payload::Pcm(f), ts) => {
                frame.copy_from_slice(f.as_ref());
                if d.codec.is_some() {
                    // Switched to a decoded-on-arrival codec: the stateful decoder goes.
                    d.codec = None;
                    d.dec = None;
                }
                d.last = Some(f);
                Pull::Played { ts }
            }
            Step::Play(Payload::Encoded { codec: c, bytes }, ts) => {
                if d.codec != Some(c) || d.dec.is_none() {
                    // A clean switch: a fresh decoder, no state carried over.
                    d.codec = Some(c);
                    d.dec = codec::new_decoder(c);
                    d.last = None;
                }
                match d.dec.as_mut().map(|dec| dec.decode(&bytes, frame)) {
                    Some(Ok(())) => {
                        d.last = Some(Box::new(*frame));
                        Pull::Played { ts }
                    }
                    _ => {
                        self.jitter.stats.undecodable += 1;
                        conceal(d, 1, None, frame)
                    }
                }
            }
            Step::Conceal(run, next) => conceal(d, run, next.as_deref(), frame),
            Step::Tail => {
                let own = d.codec.is_some() && d.dec.as_mut().is_some_and(|dec| dec.conceal(None, frame));
                if own {
                    // The speaker stopped: fade the codec's concealment out instead of letting it ring.
                    let plc = *frame;
                    fade_tail(&plc, frame);
                    Pull::Concealed
                } else if let Some(last) = &d.last {
                    fade_tail(last, frame);
                    Pull::Concealed
                } else {
                    Pull::Silent
                }
            }
            Step::Silent => Pull::Silent,
        };
        if !self.jitter.has_last() {
            self.decoder.last = None;
        }
        pulled
    }
}

/// Conceal one missing frame: the codec's own concealment when it has one (Opus), else the last
/// frame at `0.5 ^ run` (IMA-ADPCM), else nothing.
fn conceal(d: &mut ChannelDecoder, run: usize, next: Option<&[u8]>, frame: &mut MonoFrame) -> Pull {
    if d.dec.as_mut().is_some_and(|dec| dec.conceal(next, frame)) {
        return Pull::Concealed;
    }
    match &d.last {
        Some(last) => {
            repeat_halved(last, run, frame);
            Pull::Concealed
        }
        None => Pull::Silent,
    }
}

/// Every speaker's channel, mixed into one stereo stream.
#[derive(Debug, Default)]
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

    fn entry(&mut self, key: SpeakerKey, now: f64) -> &mut SpeakerChannel {
        let cfg = self.cfg;
        let ch = self.channels.entry(key).or_insert_with(|| SpeakerChannel::new(cfg, now));
        ch.last_packet = now;
        ch
    }

    /// A decoded frame from `key` (seq, sender clock stamp); `now` = real seconds.
    pub fn insert(&mut self, key: SpeakerKey, seq: u32, frame: &MonoFrame, ts: u32, now: f64) -> Insert {
        self.entry(key, now).jitter.insert(seq, Payload::Pcm(Box::new(*frame)), ts)
    }

    /// An ENCODED packet of codec `codec` from `key`, decoded at playout in sequence order (for a
    /// stateful codec such as Opus). A packet that fails to decode then is concealed and counted
    /// in [`JitterStats::undecodable`].
    pub fn insert_packet(&mut self, key: SpeakerKey, seq: u32, codec: u8, bytes: &[u8], ts: u32, now: f64) -> Insert {
        self.entry(key, now).jitter.insert(seq, Payload::Encoded { codec, bytes: bytes.to_vec() }, ts)
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

    /// One speaker's channel, mutable (crate tests pull a channel directly).
    #[cfg(all(test, feature = "opus"))]
    pub(crate) fn channel_mut(&mut self, key: SpeakerKey) -> Option<&mut SpeakerChannel> {
        self.channels.get_mut(&key)
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
            let pulled = ch.pull(&mut frame);
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

    /// Forget speakers silent for `timeout` seconds (a leaver's buffer and decoder do not live
    /// forever).
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
    t.undecodable += s.undecodable;
}
