//! Voice by distance: a gain from the listener-speaker distance and a simple left/right pan, or
//! [`Hearing::Global`] (everyone at full volume). Pure; unit-tested.

use bevy_math::Vec3;

/// 1 up to `full` metres, 0 at and beyond `range`, a smooth monotonic falloff between
/// (`((range - d) / (range - full)) ^ rolloff`). Bad input = 0 (fail closed: a voice you cannot
/// place is not heard).
pub fn spatial_gain(distance: f32, full: f32, range: f32, rolloff: f32) -> f32 {
    if !(distance.is_finite() && full.is_finite() && range.is_finite() && range > 0.0) {
        return 0.0;
    }
    let d = distance.max(0.0);
    let full = full.clamp(0.0, range);
    if d >= range {
        return 0.0;
    }
    if d <= full {
        return 1.0;
    }
    let span = range - full;
    if span <= 0.0 {
        return 0.0;
    }
    let k = if rolloff.is_finite() && rolloff > 0.0 { rolloff } else { 1.0 };
    let g = ((range - d) / span).clamp(0.0, 1.0).powf(k);
    if g.is_finite() {
        g
    } else {
        0.0
    }
}

/// (left, right) multipliers: the ear facing away from the speaker gets quieter, the near ear
/// stays at 1 (no loudness jump in the middle). `right` = the listener's right direction (the
/// camera's), `strength` `0..=1` (0 = mono).
pub fn pan_gains(listener: Vec3, right: Vec3, speaker: Vec3, strength: f32) -> (f32, f32) {
    let to = Vec3::new(speaker.x - listener.x, 0.0, speaker.z - listener.z);
    let r = Vec3::new(right.x, 0.0, right.z);
    let (Some(to), Some(r)) = (to.try_normalize(), r.try_normalize()) else { return (1.0, 1.0) };
    let s = if strength.is_finite() { strength.clamp(0.0, 1.0) } else { 0.0 };
    let p = (to.dot(r) * s).clamp(-1.0, 1.0);
    if !p.is_finite() {
        return (1.0, 1.0);
    }
    (1.0 - p.max(0.0), 1.0 + p.min(0.0))
}

/// How voices are heard ([`crate::VoiceChatConfig::hearing`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub enum Hearing {
    /// By distance (range + falloff) with a left/right pan.
    #[default]
    Proximity,
    /// Everyone hears everyone at full voice volume: no falloff, no pan (a party call).
    Global,
}

/// The numbers [`voice_gains`] needs (from [`crate::VoiceChatConfig::spatial`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpatialParams {
    /// Proximity or Global.
    pub hearing: Hearing,
    /// Full volume up to this distance.
    pub full: f32,
    /// Silent at and beyond this distance.
    pub range: f32,
    /// Falloff exponent.
    pub rolloff: f32,
    /// 0 = mono, 1 = full pan.
    pub pan_strength: f32,
}

/// Where the listener is: position + right direction (the camera's).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Listener {
    /// Where the listener hears from.
    pub pos: Vec3,
    /// The listener's right direction.
    pub right: Vec3,
}

/// **THE one place a voice's (left, right) gain is decided** (the voice volume is applied by the
/// mixer on top). `positional` = distance applies right now ([`crate::VoiceInput::positional`];
/// e.g. off in menus, where every voice is at full volume, centred). `listener` / `speaker` =
/// positions when known; `range` = the speaker's own range ([`crate::VoiceSpeaker::range`]) or
/// `None` for the config's.
pub fn voice_gains(p: &SpatialParams, positional: bool, listener: Option<Listener>, speaker: Option<Vec3>, range: Option<f32>) -> (f32, f32) {
    if p.hearing == Hearing::Global || !positional {
        return (1.0, 1.0);
    }
    // Proximity: a voice that cannot be placed is not heard (fail closed).
    let (Some(l), Some(s)) = (listener, speaker) else { return (0.0, 0.0) };
    let range = range.filter(|r| r.is_finite() && *r > 0.0).unwrap_or(p.range);
    let d = Vec3::new(s.x - l.pos.x, s.y - l.pos.y, s.z - l.pos.z).length();
    let g = spatial_gain(d, p.full.min(range), range, p.rolloff);
    let (pl, pr) = pan_gains(l.pos, l.right, s, p.pan_strength);
    (g * pl, g * pr)
}
