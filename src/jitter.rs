//! **The per-speaker jitter buffer**. Packets arrive unordered, late, twice
//! or never; playback pulls one frame every 20 ms (the output device's clock, through the mixer).
//!
//! - Buffers `target` frames (or waits `target` pulls) before it starts a talk spurt.
//! - Plays in sequence order; a packet older than the playout point is LATE and dropped; a
//!   duplicate is dropped.
//! - A missing frame while later ones are waiting = LOSS: concealed by repeating the last frame,
//!   halved each time, for up to `conceal_max` frames; then it skips ahead.
//! - A missing frame with NOTHING waiting = an underflow (the sender's game frame was slow) or
//!   the end of the spurt: the playout point WAITS (a frame arriving a little late still plays),
//!   one faded tail frame, silence, and after `conceal_max` empty pulls the spurt ends (the next
//!   one re-buffers — the buffer adapts to a jittery sender).
//! - More than `2 x target + 1` frames waiting: one is skipped (delay won back after a burst);
//!   never more than `max` frames: the oldest go.

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
}

/// Pulls per drift-control window (1 s).
const WINDOW: usize = 50;

/// A sender that restarts (reconnect) begins at sequence 0 again: anything this far behind the
/// playout point is a restart, not a late packet.
const RESTART_GAP: u32 = 250;

#[derive(Clone, Debug, Default)]
/// One speaker's jitter buffer.
pub struct JitterBuffer {
    cfg: JitterConfig,
    frames: BTreeMap<u32, (Box<MonoFrame>, u32)>,
    playing: bool,
    next: u32,
    waited: usize,
    missing_run: usize,
    last: Option<Box<MonoFrame>>,
    last_played: Option<u32>,
    /// Drift control: pulls in the current window and the smallest depth seen in it.
    window_pulls: usize,
    window_min: usize,
    /// Counters.
    pub stats: JitterStats,
}

impl JitterBuffer {
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

    fn restart(&mut self) {
        self.frames.clear();
        self.playing = false;
        self.waited = 0;
        self.missing_run = 0;
        self.last = None;
        self.last_played = None;
    }

    /// Store a decoded frame.
    pub fn insert(&mut self, seq: u32, frame: &MonoFrame, ts: u32) -> Insert {
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
        self.frames.insert(seq, (Box::new(*frame), ts));
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

    /// The next 20 ms for this speaker, written into `out` (silence for `Silent`).
    pub fn pull(&mut self, out: &mut MonoFrame) -> Pull {
        if !self.playing {
            let Some(&first) = self.frames.keys().next() else {
                self.waited = 0;
                out.fill(0.0);
                return Pull::Silent;
            };
            self.waited += 1;
            if self.frames.len() < self.cfg.target && self.waited < self.cfg.target {
                out.fill(0.0);
                return Pull::Silent;
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
        if let Some((frame, ts)) = self.frames.remove(&seq) {
            self.next = seq.saturating_add(1);
            out.copy_from_slice(frame.as_ref());
            self.last = Some(frame);
            self.last_played = Some(seq);
            self.missing_run = 0;
            self.stats.played += 1;
            return Pull::Played { ts };
        }
        self.missing_run += 1;
        if self.frames.is_empty() {
            // UNDERFLOW: nothing has arrived yet. The playout point waits (the frame may still
            // come — a burst from a slow game frame); after `conceal_max` empty pulls the spurt
            // ends (the speaker stopped) and the next one re-buffers.
            if self.missing_run > self.cfg.conceal_max {
                self.playing = false;
                self.waited = 0;
                self.last = None;
                out.fill(0.0);
                return Pull::Silent;
            }
            if self.missing_run == 1 {
                if let Some(last) = &self.last {
                    // One faded tail frame: no click at the end of a word.
                    for (i, (o, s)) in out.iter_mut().zip(last.iter()).enumerate() {
                        *o = s * (1.0 - i as f32 / FRAME as f32);
                    }
                    self.stats.concealed += 1;
                    return Pull::Concealed;
                }
            }
            out.fill(0.0);
            return Pull::Silent;
        }
        // LOSS: this frame is missing but later ones are waiting — conceal it and move on.
        self.next = seq.saturating_add(1);
        self.stats.lost += 1;
        if self.missing_run <= self.cfg.conceal_max {
            if let Some(last) = &self.last {
                let g = 0.5_f32.powi(self.missing_run as i32);
                for (o, s) in out.iter_mut().zip(last.iter()) {
                    *o = s * g;
                }
                self.stats.concealed += 1;
                return Pull::Concealed;
            }
        } else if let Some(&first) = self.frames.keys().next() {
            // A long hole: jump to what is waiting.
            self.next = first;
            self.missing_run = 0;
        }
        out.fill(0.0);
        Pull::Silent
    }
}
