//! **Who talks when**: voice activity with a hangover, or push-to-talk with a short release tail.
//! Pure: one call per 20 ms frame.

/// This frame's input to the gate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GateInput {
    /// `level` = the frame's peak after the mic gain (`0..=1`, the same scale as
    /// [`crate::VoiceChatState::mic_level`]), `threshold` = [`crate::VoiceInput::threshold`].
    OpenMic {
        /// This frame's level.
        level: f32,
        /// The line to cross.
        threshold: f32,
    },
    /// The game's talk input is held ([`crate::VoiceInput::talk_held`]).
    PushToTalk {
        /// The talk input is held.
        held: bool,
    },
}

/// Voice activity: a frame at or past the threshold opens the gate; it stays open `hangover`
/// frames after the last loud one (word ends and short pauses are not clipped). Push-to-talk:
/// open while held + `ptt_release` frames after release.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TalkGate {
    /// Voice activity: frames kept open after the level drops.
    pub hangover: u32,
    /// Push-to-talk: frames kept open after release.
    pub ptt_release: u32,
    left: u32,
    open: bool,
}

impl TalkGate {
    /// A closed gate with these tails (in frames).
    pub fn new(hangover: u32, ptt_release: u32) -> Self {
        Self { hangover, ptt_release, left: 0, open: false }
    }

    /// Is the gate open for this frame?
    pub fn step(&mut self, input: GateInput) -> bool {
        let (active, tail) = match input {
            GateInput::OpenMic { level, threshold } => {
                let level = if level.is_finite() { level } else { 0.0 };
                let threshold = if threshold.is_finite() { threshold.max(0.0) } else { 1.0 };
                // Same rule as the meter turning green: past the line and not silent.
                (level > 0.0 && level >= threshold, self.hangover)
            }
            GateInput::PushToTalk { held } => (held, self.ptt_release),
        };
        if active {
            self.left = tail;
            self.open = true;
        } else if self.left > 0 {
            self.left -= 1;
            self.open = true;
        } else {
            self.open = false;
        }
        self.open
    }

    /// Was the gate open for the last frame?
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Close at once (the microphone reopened).
    pub fn reset(&mut self) {
        self.left = 0;
        self.open = false;
    }
}

/// Milliseconds -> whole frames (rounded up; at least `min`).
pub fn ms_to_frames(ms: f32, min: u32) -> u32 {
    let f = if ms.is_finite() { (ms.max(0.0) / crate::codec::FRAME_MS as f32).ceil() } else { 0.0 };
    (f as u32).max(min)
}
