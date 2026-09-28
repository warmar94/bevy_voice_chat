//! **The per-speaker jitter buffer**. Packets arrive unordered, late, twice
//! or never; playback pulls one frame every 20 ms (the output device's clock, through the mixer).
//!
//! - Buffers `target` frames (or waits `target` pulls) before it starts a talk spurt.
//! - Plays in sequence order; a packet older than the playout point is LATE and dropped; a
//!   duplicate is dropped.
//! - A missing frame while later ones are waiting = LOSS: concealed (by default by repeating the
//!   last frame, halved each time) for up to `conceal_max` frames; then it skips ahead.
//! - A missing frame with NOTHING waiting = an underflow (the sender's game frame was slow) or
//!   the end of the spurt: the playout point WAITS (a frame arriving a little late still plays),
//!   one faded tail frame, silence, and after `conceal_max` empty pulls the spurt ends (the next
//!   one re-buffers — the buffer adapts to a jittery sender).
//! - More than `2 x target + 1` frames waiting: one is skipped (delay won back after a burst);
//!   never more than `max` frames: the oldest go.
//!
//! [`JitterCore`] is that state machine over any payload (the mixer stores encoded packets of a
//! stateful codec in it and decodes them at playout); [`JitterBuffer`] is the core over decoded PCM
//! frames plus the repeat-and-fade concealment.

use crate::codec::{MonoFrame, FRAME};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Jitter buffer sizes, in frames.
pub struct JitterConfig {
    /// Frames buffered before a spurt starts.
    pub target: usize,
    /// Frames kept at most.
    pub max: usize,
    /// Pulls concealed / waited before giving up.
    pub conceal_max: usize,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self { target: 3, max: 12, conceal_max: 3 }
    }
}

/// What one pull produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pull {
    /// A real frame; `ts` = the sender's clock stamp (ms, wrapping).
    Played {
        /// The sender's clock stamp.
        ts: u32,
    },
    /// Loss concealment or the faded tail.
    Concealed,
    /// Nothing (buffering, or idle).
    Silent,
}

/// What an insert did (for the stats).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    /// Kept for playout.
    Stored,
    /// Behind the playout point: dropped.
    Late,
    /// Already buffered: dropped.
    Duplicate,
}

/// Counters (cumulative).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitterStats {
    /// Frames inserted.
    pub received: u64,
    /// Real frames played.
    pub played: u64,
    /// Dropped: behind the playout point.
    pub late: u64,
    /// Dropped: already buffered.
    pub duplicate: u64,
    /// Frames missing while later ones were waiting.
    pub lost: u64,
    /// Frames concealed (loss or the end-of-spurt tail).
    pub concealed: u64,
    /// Dropped because the buffer was over `max`.
    pub overflow: u64,
    /// Stored packets that failed to decode at playout (concealed instead).
    pub undecodable: u64,
}

/// Pulls per drift-control window (1 s).
const WINDOW: usize = 50;

/// A sender that restarts (reconnect) begins at sequence 0 again: anything this far behind the
/// playout point is a restart, not a late packet.
const RESTART_GAP: u32 = 250;

/// What one [`JitterCore::next_frame`] asks the consumer to produce.
#[derive(Debug, PartialEq)]
pub enum Next<'a, P> {
    /// A real frame (it was [`Pull::Played`]).
    Play {
        /// The stored payload.
        payload: P,
        /// The sender's clock stamp.
        ts: u32,
    },
    /// The first empty pull after a frame (the speaker stopped or is late): fade the last frame out.
    Tail,
    /// A LOST frame while later ones are waiting: conceal it (`run` = 1, 2, .. in a row).
    Conceal {
        /// Frames concealed in a row, this one included.
        run: usize,
        /// The following payload when it is already buffered (for forward error correction).
        next: Option<&'a P>,
    },
    /// Nothing (buffering, idle, waiting).
    Silent,
}

/// The jitter state machine over any payload: ordering, late / duplicate drop, underflow wait,
/// spurt end, drift trim, sender restart. [`JitterBuffer`] = this over PCM frames.
#[derive(Clone, Debug)]
pub struct JitterCore<P> {
    cfg: JitterConfig,
    frames: BTreeMap<u32, (P, u32)>,
    playing: bool,
    next: u32,
    waited: usize,
    missing_run: usize,
    has_last: bool,
    last_played: Option<u32>,
    /// Drift control: pulls in the current window and the smallest depth seen in it.
    window_pulls: usize,
    window_min: usize,
    generation: u32,
    /// Counters.
    pub stats: JitterStats,
}

impl<P> Default for JitterCore<P> {
    fn default() -> Self {
        Self {
            cfg: JitterConfig::default(),
            frames: BTreeMap::new(),
            playing: false,
            next: 0,
            waited: 0,
            missing_run: 0,
            has_last: false,
            last_played: None,
            window_pulls: 0,
            window_min: 0,
            generation: 0,
            stats: JitterStats::default(),
        }
    }
}

impl<P> JitterCore<P> {
    /// An empty buffer (sizes clamped to sane minimums).
    pub fn new(cfg: JitterConfig) -> Self {
        let cfg = JitterConfig { target: cfg.target.max(1), max: cfg.max.max(cfg.target.max(1) + 1), conceal_max: cfg.conceal_max };
        Self { cfg, ..Default::default() }
    }

    /// Frames waiting.
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// Is a talk spurt playing?
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Nothing buffered and not in a spurt.
    pub fn is_idle(&self) -> bool {
        !self.playing && self.frames.is_empty()
    }

    /// Bumped by a sender restart (its sequence jumped far back): a stateful decoder must reset.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// A frame played in the current spurt (so a tail / concealment has something to work from).
    pub(crate) fn has_last(&self) -> bool {
        self.has_last
    }

    fn restart(&mut self) {
        self.frames.clear();
        self.playing = false;
        self.waited = 0;
        self.missing_run = 0;
        self.has_last = false;
        self.last_played = None;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Store a payload.
    pub fn insert(&mut self, seq: u32, payload: P, ts: u32) -> Insert {
        self.stats.received += 1;
        let floor = if self.playing { Some(self.next) } else { self.last_played.map(|p| p.saturating_add(1)) };
        if let Some(floor) = floor {
            if seq.saturating_add(RESTART_GAP) < floor {
                self.restart();
            } else if seq < floor {
                self.stats.late += 1;
                return Insert::Late;
            }
        }
        if self.frames.contains_key(&seq) {
            self.stats.duplicate += 1;
            return Insert::Duplicate;
        }
        self.frames.insert(seq, (payload, ts));
        while self.frames.len() > self.cfg.max {
            if let Some((&oldest, _)) = self.frames.iter().next() {
                self.frames.remove(&oldest);
                self.stats.overflow += 1;
                if self.playing && self.next <= oldest {
                    self.next = oldest.saturating_add(1);
                }
            }
        }
        Insert::Stored
    }

    /// What the next 20 ms of this speaker is.
    pub fn next_frame(&mut self) -> Next<'_, P> {
        if !self.playing {
            let Some(&first) = self.frames.keys().next() else {
                self.waited = 0;
                return Next::Silent;
            };
            self.waited += 1;
            if self.frames.len() < self.cfg.target && self.waited < self.cfg.target {
                return Next::Silent;
            }
            self.playing = true;
            self.next = first;
            self.missing_run = 0;
        }
        // Drift control: when the buffer never dropped under `target - 1` frames for a whole
        // second, one frame is surplus delay (a burst after a stall, clock drift): skip it. Too
        // much queued at once (> 2 x target + 1): skip at once.
        self.window_min = if self.window_pulls == 0 { self.frames.len() } else { self.window_min.min(self.frames.len()) };
        self.window_pulls += 1;
        let surplus = self.window_pulls >= WINDOW && self.window_min > self.cfg.target.saturating_sub(1).max(1);
        if self.window_pulls >= WINDOW {
            self.window_pulls = 0;
        }
        if surplus || self.frames.len() > 2 * self.cfg.target + 1 {
            if let Some(&first) = self.frames.keys().next() {
                self.frames.remove(&first);
                self.next = self.next.max(first.saturating_add(1));
                self.stats.overflow += 1;
            }
        }
        let seq = self.next;
        if let Some((payload, ts)) = self.frames.remove(&seq) {
            self.next = seq.saturating_add(1);
            self.has_last = true;
            self.last_played = Some(seq);
            self.missing_run = 0;
            self.stats.played += 1;
            return Next::Play { payload, ts };
        }
        self.missing_run += 1;
        if self.frames.is_empty() {
            // UNDERFLOW: nothing has arrived yet. The playout point waits (the frame may still
            // come — a burst from a slow game frame); after `conceal_max` empty pulls the spurt
            // ends (the speaker stopped) and the next one re-buffers.
            if self.missing_run > self.cfg.conceal_max {
                self.playing = false;
                self.waited = 0;
                self.has_last = false;
                return Next::Silent;
            }
            if self.missing_run == 1 && self.has_last {
                // One faded tail frame: no click at the end of a word.
                self.stats.concealed += 1;
                return Next::Tail;
            }
            return Next::Silent;
        }
        // LOSS: this frame is missing but later ones are waiting — conceal it and move on.
        self.next = seq.saturating_add(1);
        self.stats.lost += 1;
        if self.missing_run <= self.cfg.conceal_max {
            if self.has_last {
                self.stats.concealed += 1;
                let next = self.frames.get(&seq.saturating_add(1)).map(|(p, _)| p);
                return Next::Conceal { run: self.missing_run, next };
            }
        } else if let Some(&first) = self.frames.keys().next() {
            // A long hole: jump to what is waiting.
            self.next = first;
            self.missing_run = 0;
        }
        Next::Silent
    }
}

/// The faded tail: `last` ramped from full level down to silence across the frame.
pub(crate) fn fade_tail(last: &MonoFrame, out: &mut MonoFrame) {
    for (i, (o, s)) in out.iter_mut().zip(last.iter()).enumerate() {
        *o = s * (1.0 - i as f32 / FRAME as f32);
    }
}

/// Loss concealment by repetition: `last` at `0.5 ^ run`.
pub(crate) fn repeat_halved(last: &MonoFrame, run: usize, out: &mut MonoFrame) {
    let g = 0.5_f32.powi(run as i32);
    for (o, s) in out.iter_mut().zip(last.iter()) {
        *o = s * g;
    }
}

/// One speaker's jitter buffer of decoded frames: [`JitterCore`] + repeat-and-fade concealment.
#[derive(Clone, Debug, Default)]
pub struct JitterBuffer {
    core: JitterCore<Box<MonoFrame>>,
    last: Option<Box<MonoFrame>>,
    /// Counters.
    pub stats: JitterStats,
}

impl JitterBuffer {
    /// An empty buffer (sizes clamped to sane minimums).
    pub fn new(cfg: JitterConfig) -> Self {
        Self { core: JitterCore::new(cfg), last: None, stats: JitterStats::default() }
    }

    /// Frames waiting.
    pub fn depth(&self) -> usize {
        self.core.depth()
    }

    /// Is a talk spurt playing?
    pub fn is_playing(&self) -> bool {
        self.core.is_playing()
    }

    /// Nothing buffered and not in a spurt.
    pub fn is_idle(&self) -> bool {
        self.core.is_idle()
    }

    /// Store a decoded frame.
    pub fn insert(&mut self, seq: u32, frame: &MonoFrame, ts: u32) -> Insert {
        let generation = self.core.generation();
        let r = self.core.insert(seq, Box::new(*frame), ts);
        if self.core.generation() != generation {
            self.last = None;
        }
        self.stats = self.core.stats;
        r
    }

    /// The next 20 ms for this speaker, written into `out` (silence for `Silent`).
    pub fn pull(&mut self, out: &mut MonoFrame) -> Pull {
        let pulled = match self.core.next_frame() {
            Next::Play { payload, ts } => {
                out.copy_from_slice(payload.as_ref());
                self.last = Some(payload);
                Pull::Played { ts }
            }
            Next::Tail => match &self.last {
                Some(last) => {
                    fade_tail(last, out);
                    Pull::Concealed
                }
                None => {
                    out.fill(0.0);
                    Pull::Silent
                }
            },
            Next::Conceal { run, .. } => match &self.last {
                Some(last) => {
                    repeat_halved(last, run, out);
                    Pull::Concealed
                }
                None => {
                    out.fill(0.0);
                    Pull::Silent
                }
            },
            Next::Silent => {
                out.fill(0.0);
                Pull::Silent
            }
        };
        if !self.core.has_last() {
            self.last = None;
        }
        self.stats = self.core.stats;
        pulled
    }
}
