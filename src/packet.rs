//! **Packet checks** for whoever relays or receives voice (a host / server, every listener): the
//! frame must be a well-formed frame of a known wire codec, and each speaker gets a packet budget.
//! Transport-agnostic: the crate never sends anything itself (see [`crate::OutgoingVoice`] /
//! [`crate::IncomingVoice`]). Everything here is pure and feature-independent: a relay built
//! without the `opus` feature still forwards Opus.

use crate::codec::{ImaAdpcm, MAX_VOICE_BYTES, OPUS_ID};

/// Why a packet was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// No known speaker behind the sender (not authorised yet, left, unknown).
    UnknownSender,
    /// Not a known wire codec (or, for [`validate_frame`], not the expected one).
    WrongCodec,
    /// Not a valid size for its codec (or over [`MAX_VOICE_BYTES`]).
    BadSize,
    /// Well-sized but not a frame this crate accepts: an Opus TOC that is not SILK 20 ms mono,
    /// an ADPCM step index past the table.
    Malformed,
    /// Over the per-speaker packet budget.
    RateLimited,
}

/// Is this the start of an Opus packet this crate accepts: SILK-only, 20 ms, mono, one frame
/// (code 0, or code 3 with a frame count of 1)? Narrowband, mediumband and wideband are all fine
/// (TOC configs 1, 5, 9). A lone TOC byte (a discontinuous-transmission frame) is accepted.
pub fn opus_toc_ok(frame: &[u8]) -> bool {
    let Some(&toc) = frame.first() else { return false };
    let (config, stereo, code) = (toc >> 3, toc & 0x04 != 0, toc & 0x03);
    matches!(config, 1 | 5 | 9)
        && !stereo
        && match code {
            0 => true,
            3 => frame.get(1).is_some_and(|c| c & 0x3F == 1),
            _ => false,
        }
}

/// THE relay / receiver check, for every wire codec: a known codec id and a well-formed frame.
///
/// - IMA-ADPCM ([`ImaAdpcm::ID`]): exactly [`ImaAdpcm::FRAME_BYTES`] bytes, step index (byte 2)
///   at most 88.
/// - Opus ([`OPUS_ID`]): `1..=MAX_VOICE_BYTES` bytes and [`opus_toc_ok`].
/// - anything else (including the local-only `Pcm16`): [`Reject::WrongCodec`].
pub fn validate_packet(codec: u8, frame: &[u8]) -> Result<(), Reject> {
    match codec {
        ImaAdpcm::ID => {
            if frame.len() != ImaAdpcm::FRAME_BYTES {
                return Err(Reject::BadSize);
            }
            match frame.get(2) {
                Some(&index) if index <= ImaAdpcm::MAX_STEP_INDEX => Ok(()),
                _ => Err(Reject::Malformed),
            }
        }
        OPUS_ID => {
            if frame.is_empty() || frame.len() > MAX_VOICE_BYTES {
                return Err(Reject::BadSize);
            }
            if opus_toc_ok(frame) {
                Ok(())
            } else {
                Err(Reject::Malformed)
            }
        }
        _ => Err(Reject::WrongCodec),
    }
}

/// The exact-size check for a fixed-size codec: is this one frame of `want_codec`, exactly
/// `want_len` bytes (and at most [`MAX_VOICE_BYTES`])? Prefer [`validate_packet`], which knows
/// every wire codec (including variable-size Opus).
pub fn validate_frame(codec: u8, len: usize, want_codec: u8, want_len: usize) -> Result<(), Reject> {
    if codec != want_codec {
        return Err(Reject::WrongCodec);
    }
    if len != want_len || len > MAX_VOICE_BYTES {
        return Err(Reject::BadSize);
    }
    Ok(())
}

/// A per-speaker packet budget: `rate` per second, bursts up to `burst` (a pre-roll or a slow
/// game frame sends a few at once).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TokenBucket {
    tokens: f32,
    rate: f32,
    burst: f32,
}

impl TokenBucket {
    /// A full bucket.
    pub fn new(rate: f32, burst: f32) -> Self {
        let rate = if rate.is_finite() { rate.max(1.0) } else { 50.0 };
        let burst = if burst.is_finite() { burst.max(1.0) } else { 10.0 };
        Self { tokens: burst, rate, burst }
    }

    /// Refill for `dt` seconds.
    pub fn refill(&mut self, dt: f32) {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        self.tokens = (self.tokens + self.rate * dt).min(self.burst);
    }

    /// Spend one packet; false = over budget.
    pub fn take(&mut self) -> bool {
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}
