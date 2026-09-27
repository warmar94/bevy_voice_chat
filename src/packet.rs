//! **Packet checks** for whoever relays voice (a host / server): the frame must be exactly one
//! encoded frame of the right codec, and each speaker gets a packet budget. Transport-agnostic:
//! the crate never sends anything itself (see [`crate::OutgoingVoice`] / [`crate::IncomingVoice`]).

use crate::codec::MAX_VOICE_BYTES;

/// Why a packet was dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// No known speaker behind the sender (not authorised yet, left, unknown).
    UnknownSender,
    /// Another codec than this build's.
    WrongCodec,
    /// Not exactly one encoded frame (or over [`MAX_VOICE_BYTES`]).
    BadSize,
    /// Over the per-speaker packet budget.
    RateLimited,
}

/// Is this one frame of our codec? (pure; use it before relaying and before decoding).
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
