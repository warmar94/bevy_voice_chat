//! The voice codecs. Everything on the wire is one [`FRAME`] of mono audio at [`VOICE_RATE`]
//! (20 ms at 16 kHz), behind the [`VoiceCodec`] trait, with a codec id in every packet.
//!
//! - **[`ImaAdpcm`] (the default, always built):** 4 bits per sample + a 4-byte header = 164 B per
//!   frame (65.6 kbps of payload), pure Rust, no native library, near-zero CPU. Every frame carries
//!   its own predictor and step index, so a lost packet never corrupts the next one: ADPCM frames
//!   are decoded on arrival.
//! - **`Opus` (cargo feature `opus`, off by default):** the SILK wideband mode of the pure-Rust
//!   `opus-rs` crate, the low-bandwidth option (60 B per frame at the default 24 kbps). Its decoder
//!   keeps state between frames, so its packets are buffered encoded and decoded in sequence order
//!   at playout, with Opus packet loss concealment for missing frames.
//! - [`Pcm16`] (256 kbps) is a local reference codec, never accepted on the wire.
//!
//! Which codec a player SENDS with is [`VoiceCodecChoice`] in [`crate::VoiceChatConfig`]; a
//! receiver decodes every codec compiled into it, picked by the packet's codec id
//! ([`new_decoder`], [`can_decode`]). There is no automatic fallback between codecs.

use serde::{Deserialize, Serialize};

/// The voice sample rate on the wire and in the mixer. 16 kHz = "wideband" voice (everything
/// speech needs up to 8 kHz), a third of 48 kHz (the usual device rate — a cheap resample), and
/// it keeps ADPCM at ~66 kbps. Changing it is a wire-format change (every peer must agree).
pub const VOICE_RATE: u32 = 16_000;
/// Samples per frame: 20 ms at [`VOICE_RATE`].
pub const FRAME: usize = 320;
/// Milliseconds per frame.
pub const FRAME_MS: u32 = 20;
/// No accepted voice packet is larger than this (the host drops bigger ones before decoding).
pub const MAX_VOICE_BYTES: usize = 700;
/// The wire id of Opus packets. Defined in every build, so validation and relays know it even
/// without the `opus` feature.
pub const OPUS_ID: u8 = 3;

/// One mono frame.
pub type MonoFrame = [f32; FRAME];

/// A codec failure: a malformed or foreign packet (dropped, counted), or a codec that cannot run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// Not one frame's worth of bytes.
    WrongLength {
        /// Bytes received.
        got: usize,
        /// Bytes one frame has (the most, for a variable-size codec).
        want: usize,
    },
    /// The header is not one this codec accepts (an ADPCM step index out of range, an Opus TOC
    /// byte that is not SILK 20 ms mono).
    BadHeader,
    /// The codec's cargo feature is not compiled into this build (Opus without `opus`).
    NotCompiled,
    /// The codec's settings are invalid (the text says which).
    BadSettings(&'static str),
    /// The codec library reported an error (its own text).
    Backend(&'static str),
    /// The codec library panicked; the panic was caught and that codec state dropped.
    Panicked,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongLength { got, want } => write!(f, "wrong frame length: {got} bytes, want {want}"),
            Self::BadHeader => f.write_str("malformed frame header"),
            Self::NotCompiled => f.write_str("codec Opus needs the `opus` cargo feature"),
            Self::BadSettings(s) => write!(f, "invalid opus settings: {s}"),
            Self::Backend(s) => f.write_str(s),
            Self::Panicked => f.write_str("the codec library panicked"),
        }
    }
}

impl std::error::Error for CodecError {}

/// A voice codec: one [`FRAME`] in, a few bytes out, and back.
pub trait VoiceCodec: Send + Sync + 'static {
    /// On the wire in every packet: receivers pick their decoder by it.
    fn id(&self) -> u8;
    /// A readable name (logs, status lines).
    fn name(&self) -> &'static str;
    /// Largest encoded frame in bytes (every frame, for a fixed-size codec); at most
    /// [`MAX_VOICE_BYTES`].
    fn max_frame_bytes(&self) -> usize;
    /// `Some(n)`: every frame is exactly `n` bytes.
    fn fixed_frame_bytes(&self) -> Option<usize> {
        None
    }
    /// Nominal payload bits per second (for bandwidth estimates).
    fn bitrate(&self) -> u32;
    /// Encode one frame into `out` (cleared first). May keep state between frames.
    fn encode(&mut self, pcm: &MonoFrame, out: &mut Vec<u8>) -> Result<(), CodecError>;
    /// Decode one frame.
    fn decode(&mut self, bytes: &[u8], out: &mut MonoFrame) -> Result<(), CodecError>;
    /// The decoder keeps state between frames: one decoder per speaker, fed in sequence order
    /// (the mixer decodes such packets at playout, not on arrival).
    fn is_stateful(&self) -> bool {
        false
    }
    /// Conceal ONE missing frame into `out`. `next` = the following packet when it is already
    /// buffered (for codecs with forward error correction). `false` = no concealment of its own
    /// (the caller repeats the last frame at a falling level).
    fn conceal(&mut self, next: Option<&[u8]>, out: &mut MonoFrame) -> bool {
        let _ = (next, out);
        false
    }
    /// Forget the stream (a restarted sender, a codec switch).
    fn reset(&mut self) {}
}

/// Which codec THIS player sends with. Receivers decode every codec compiled into them, by the
/// packet's codec id, so players on different codecs hear each other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VoiceCodecChoice {
    /// IMA-ADPCM 4:1: 164 B per frame (~66 kbps), no feature needed. The default.
    #[default]
    ImaAdpcm,
    /// Opus (SILK wideband): the low-bandwidth option; needs the `opus` cargo feature.
    Opus,
}

/// Opus encoder settings (used when [`crate::VoiceChatConfig::codec`] is
/// [`VoiceCodecChoice::Opus`]). Presets: [`OpusSettings::LOW_BANDWIDTH`] (16 kbps, 40 B per
/// frame), the default (24 kbps, 60 B) and [`OpusSettings::QUALITY`] (32 kbps, 80 B).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpusSettings {
    /// Target bits per second, `6_000..=64_000`. Constant bitrate: every frame is
    /// `bitrate_bps / 400` bytes. Variable: that is the per-frame ceiling.
    pub bitrate_bps: u32,
    /// Encoder effort `0..=10` (encode time per frame, roughly: 0 ~ 60 us, 5 ~ 200 us,
    /// 10 ~ 400 us on a desktop CPU).
    pub complexity: u8,
    /// `false` = constant bitrate (the default). `true` = variable, capped at the bitrate: a bit
    /// smaller and cheaper on average, slightly lower quality.
    pub vbr: bool,
}

impl Default for OpusSettings {
    fn default() -> Self {
        Self { bitrate_bps: 24_000, complexity: 5, vbr: false }
    }
}

impl OpusSettings {
    /// The "low bandwidth" preset: 16 kbps, 40 B per frame.
    pub const LOW_BANDWIDTH: Self = Self { bitrate_bps: 16_000, complexity: 5, vbr: false };
    /// The "quality" preset: 32 kbps, 80 B per frame.
    pub const QUALITY: Self = Self { bitrate_bps: 32_000, complexity: 5, vbr: false };
    /// Lowest accepted bitrate.
    pub const MIN_BITRATE: u32 = 6_000;
    /// Highest accepted bitrate.
    pub const MAX_BITRATE: u32 = 64_000;
    /// Highest accepted complexity.
    pub const MAX_COMPLEXITY: u8 = 10;

    /// Everything wrong with these settings (empty = fine).
    pub fn problems(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if !(Self::MIN_BITRATE..=Self::MAX_BITRATE).contains(&self.bitrate_bps) {
            out.push("opus.bitrate_bps must be 6000..=64000");
        }
        if self.complexity > Self::MAX_COMPLEXITY {
            out.push("opus.complexity must be 0..=10");
        }
        out
    }

    /// Bytes per 20 ms frame at the bitrate (the constant-bitrate size, the variable-bitrate
    /// ceiling): `bitrate_bps / 400`.
    pub fn frame_bytes(&self) -> usize {
        (self.bitrate_bps / 400) as usize
    }
}

/// A fresh ENCODER for what the config asks. Opus without the `opus` feature is
/// `Err(CodecError::NotCompiled)`, invalid Opus settings `Err(CodecError::BadSettings(..))`.
/// Never falls back to another codec.
pub fn new_encoder(choice: VoiceCodecChoice, opus: &OpusSettings) -> Result<Box<dyn VoiceCodec>, CodecError> {
    match choice {
        VoiceCodecChoice::ImaAdpcm => Ok(Box::new(ImaAdpcm::default())),
        VoiceCodecChoice::Opus => opus_encoder(opus),
    }
}

#[cfg(feature = "opus")]
fn opus_encoder(settings: &OpusSettings) -> Result<Box<dyn VoiceCodec>, CodecError> {
    Ok(Box::new(Opus::encoder(*settings)?))
}

#[cfg(not(feature = "opus"))]
fn opus_encoder(_settings: &OpusSettings) -> Result<Box<dyn VoiceCodec>, CodecError> {
    Err(CodecError::NotCompiled)
}

/// A fresh DECODER for a wire id this build can decode ([`ImaAdpcm`]; Opus with the `opus`
/// feature), else `None`. [`Pcm16`] is not a wire codec.
pub fn new_decoder(id: u8) -> Option<Box<dyn VoiceCodec>> {
    match id {
        ImaAdpcm::ID => Some(Box::new(ImaAdpcm::default())),
        #[cfg(feature = "opus")]
        OPUS_ID => Opus::decoder().ok().map(|d| Box::new(d) as Box<dyn VoiceCodec>),
        _ => None,
    }
}

/// Can this build decode wire id `id`? ([`ImaAdpcm::ID`] always; [`OPUS_ID`] only with the
/// `opus` feature.)
pub fn can_decode(id: u8) -> bool {
    id == ImaAdpcm::ID || (cfg!(feature = "opus") && id == OPUS_ID)
}

/// f32 `-1..=1` -> i16 (NaN -> 0).
pub fn to_i16(x: f32) -> i16 {
    if x.is_finite() {
        (x.clamp(-1.0, 1.0) * 32767.0).round() as i16
    } else {
        0
    }
}

/// i16 -> f32 `-1..1`.
pub fn from_i16(v: i16) -> f32 {
    f32::from(v) / 32768.0
}

// ---------------------------------------------------------------- IMA-ADPCM

const STEPS: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66, 73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230,
    253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327,
    3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
];
const INDEX_STEP: [i32; 16] = [-1, -1, -1, -1, 2, 4, 6, 8, -1, -1, -1, -1, 2, 4, 6, 8];
/// predictor i16 + step index u8 + reserved u8.
const ADPCM_HEADER: usize = 4;

/// IMA-ADPCM, 4 bits per sample, one self-contained frame per packet.
#[derive(Clone, Debug, Default)]
pub struct ImaAdpcm {
    predictor: i32,
    index: i32,
}

impl ImaAdpcm {
    /// The id on the wire.
    pub const ID: u8 = 1;
    /// Bytes per encoded frame (every frame): a 4-byte header + 4 bits per sample.
    pub const FRAME_BYTES: usize = ADPCM_HEADER + FRAME / 2;
    /// The highest step index a frame header may carry.
    pub const MAX_STEP_INDEX: u8 = 88;
}

/// Decode one nibble, updating (predictor, index).
fn adpcm_step(code: u8, predictor: &mut i32, index: &mut i32) {
    let step = STEPS[(*index).clamp(0, 88) as usize];
    let mut diff = step >> 3;
    if code & 4 != 0 {
        diff += step;
    }
    if code & 2 != 0 {
        diff += step >> 1;
    }
    if code & 1 != 0 {
        diff += step >> 2;
    }
    *predictor = if code & 8 != 0 { *predictor - diff } else { *predictor + diff }.clamp(-32768, 32767);
    *index = (*index + INDEX_STEP[usize::from(code & 15)]).clamp(0, 88);
}

impl VoiceCodec for ImaAdpcm {
    fn id(&self) -> u8 {
        Self::ID
    }

    fn name(&self) -> &'static str {
        "IMA-ADPCM 4:1"
    }

    fn max_frame_bytes(&self) -> usize {
        Self::FRAME_BYTES
    }

    fn fixed_frame_bytes(&self) -> Option<usize> {
        Some(Self::FRAME_BYTES)
    }

    fn bitrate(&self) -> u32 {
        (Self::FRAME_BYTES as u32 * 8 * 1000) / FRAME_MS
    }

    fn encode(&mut self, pcm: &MonoFrame, out: &mut Vec<u8>) -> Result<(), CodecError> {
        out.clear();
        out.extend_from_slice(&(self.predictor as i16).to_le_bytes());
        out.push(self.index as u8);
        out.push(0);
        let mut byte = 0u8;
        for (i, &x) in pcm.iter().enumerate() {
            let sample = i32::from(to_i16(x));
            let step = STEPS[self.index.clamp(0, 88) as usize];
            let mut diff = sample - self.predictor;
            let mut code = 0u8;
            if diff < 0 {
                code = 8;
                diff = -diff;
            }
            if diff >= step {
                code |= 4;
                diff -= step;
            }
            if diff >= step >> 1 {
                code |= 2;
                diff -= step >> 1;
            }
            if diff >= step >> 2 {
                code |= 1;
            }
            // The decoder's own reconstruction keeps encoder and decoder in lock step.
            adpcm_step(code, &mut self.predictor, &mut self.index);
            if i % 2 == 0 {
                byte = code;
            } else {
                out.push(byte | (code << 4));
            }
        }
        Ok(())
    }

    fn decode(&mut self, bytes: &[u8], out: &mut MonoFrame) -> Result<(), CodecError> {
        let want = Self::FRAME_BYTES;
        if bytes.len() != want {
            return Err(CodecError::WrongLength { got: bytes.len(), want });
        }
        let (Some(&p0), Some(&p1), Some(&idx)) = (bytes.first(), bytes.get(1), bytes.get(2)) else { return Err(CodecError::BadHeader) };
        if idx > Self::MAX_STEP_INDEX {
            return Err(CodecError::BadHeader);
        }
        let mut predictor = i32::from(i16::from_le_bytes([p0, p1]));
        let mut index = i32::from(idx);
        for (i, slot) in out.iter_mut().enumerate() {
            let byte = bytes.get(ADPCM_HEADER + i / 2).copied().unwrap_or(0);
            let code = if i % 2 == 0 { byte & 15 } else { byte >> 4 };
            adpcm_step(code, &mut predictor, &mut index);
            *slot = from_i16(predictor as i16);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- PCM16

/// Plain 16-bit PCM (little endian): a local reference codec (tests, comparisons). It is not a
/// wire codec: [`crate::packet::validate_packet`] rejects it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pcm16;

impl Pcm16 {
    /// The id (never accepted on the wire).
    pub const ID: u8 = 2;
    /// Bytes per encoded frame.
    pub const FRAME_BYTES: usize = FRAME * 2;
}

impl VoiceCodec for Pcm16 {
    fn id(&self) -> u8 {
        Self::ID
    }

    fn name(&self) -> &'static str {
        "PCM 16-bit"
    }

    fn max_frame_bytes(&self) -> usize {
        Self::FRAME_BYTES
    }

    fn fixed_frame_bytes(&self) -> Option<usize> {
        Some(Self::FRAME_BYTES)
    }

    fn bitrate(&self) -> u32 {
        (Self::FRAME_BYTES as u32 * 8 * 1000) / FRAME_MS
    }

    fn encode(&mut self, pcm: &MonoFrame, out: &mut Vec<u8>) -> Result<(), CodecError> {
        out.clear();
        for &x in pcm {
            out.extend_from_slice(&to_i16(x).to_le_bytes());
        }
        Ok(())
    }

    fn decode(&mut self, bytes: &[u8], out: &mut MonoFrame) -> Result<(), CodecError> {
        let want = Self::FRAME_BYTES;
        if bytes.len() != want {
            return Err(CodecError::WrongLength { got: bytes.len(), want });
        }
        for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
            if let [a, b] = pair {
                *slot = from_i16(i16::from_le_bytes([*a, *b]));
            }
        }
        Ok(())
    }
}

/// The default sending codec ([`ImaAdpcm`]).
pub fn default_codec() -> Box<dyn VoiceCodec> {
    Box::new(ImaAdpcm::default())
}

// ---------------------------------------------------------------- Opus (feature `opus`)

#[cfg(feature = "opus")]
pub use opus_impl::Opus;

#[cfg(feature = "opus")]
mod opus_impl {
    use super::{CodecError, MonoFrame, OpusSettings, VoiceCodec, FRAME, MAX_VOICE_BYTES, OPUS_ID, VOICE_RATE};
    use crate::packet::opus_toc_ok;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    /// The TOC byte of a SILK wideband 20 ms mono frame (code 0): what a PLC call is given before
    /// any real packet was seen.
    const WIDEBAND_TOC: u8 = 0x48;

    /// **Opus** (feature `opus`): the SILK wideband mode of the pure-Rust `opus-rs` crate, 16 kHz
    /// mono, 20 ms frames. One instance is either a sender's encoder ([`Opus::encoder`]) or one
    /// speaker's decoder ([`Opus::decoder`]); a decoder keeps state between frames, so it must be
    /// fed in sequence order (the mixer does that at playout).
    ///
    /// Every call into the library is wrapped in `catch_unwind`: a panic becomes
    /// [`CodecError::Panicked`] (logged once per instance) and that half of the codec is dropped (a
    /// decoder is recreated by the next [`VoiceCodec::decode`]; an encoder stays gone). Incoming
    /// packets are checked with [`crate::packet::opus_toc_ok`] before the library sees them.
    pub struct Opus {
        enc: Option<opus_rs::OpusEncoder>,
        dec: Option<opus_rs::OpusDecoder>,
        settings: OpusSettings,
        /// Built as an encoder (else as a decoder).
        encoding: bool,
        /// The last good TOC with code 0 (`& 0xFC`): what a PLC call needs.
        last_toc: u8,
        warned: bool,
    }

    impl std::fmt::Debug for Opus {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Opus")
                .field("encoder", &self.enc.is_some())
                .field("decoder", &self.dec.is_some())
                .field("settings", &self.settings)
                .field("encoding", &self.encoding)
                .field("last_toc", &self.last_toc)
                .finish()
        }
    }

    fn make_encoder(s: &OpusSettings) -> Result<opus_rs::OpusEncoder, CodecError> {
        let built = catch_unwind(|| opus_rs::OpusEncoder::new(VOICE_RATE as i32, 1, opus_rs::Application::Voip));
        let mut enc = match built {
            Ok(Ok(e)) => e,
            Ok(Err(e)) => return Err(CodecError::Backend(e)),
            Err(_) => return Err(CodecError::Panicked),
        };
        // Every knob set explicitly: the library defaults are 64 kbps, complexity 9, VBR.
        enc.bitrate_bps = s.bitrate_bps as i32;
        enc.complexity = i32::from(s.complexity);
        enc.use_cbr = !s.vbr;
        // opus-rs 0.1.34: its decoder cannot use in-band FEC, and FEC corrupts VBR streams.
        enc.use_inband_fec = false;
        enc.packet_loss_perc = 0;
        Ok(enc)
    }

    fn make_decoder() -> Result<opus_rs::OpusDecoder, CodecError> {
        match catch_unwind(|| opus_rs::OpusDecoder::new(VOICE_RATE as i32, 1)) {
            Ok(Ok(d)) => Ok(d),
            Ok(Err(e)) => Err(CodecError::Backend(e)),
            Err(_) => Err(CodecError::Panicked),
        }
    }

    /// A library output that is not finite becomes silence (belt and braces).
    fn sanitise(out: &mut MonoFrame) {
        for s in out.iter_mut() {
            if !s.is_finite() {
                *s = 0.0;
            }
        }
    }

    impl Opus {
        /// The id on the wire ([`OPUS_ID`]).
        pub const ID: u8 = OPUS_ID;

        /// A sender's encoder. Invalid settings are `Err(CodecError::BadSettings(..))` (the
        /// runtime twin of [`OpusSettings::problems`]).
        pub fn encoder(settings: OpusSettings) -> Result<Self, CodecError> {
            if let Some(first) = settings.problems().first() {
                return Err(CodecError::BadSettings(first));
            }
            let enc = make_encoder(&settings)?;
            Ok(Self { enc: Some(enc), dec: None, settings, encoding: true, last_toc: WIDEBAND_TOC, warned: false })
        }

        /// One speaker's decoder.
        pub fn decoder() -> Result<Self, CodecError> {
            let dec = make_decoder()?;
            Ok(Self { enc: None, dec: Some(dec), settings: OpusSettings::default(), encoding: false, last_toc: WIDEBAND_TOC, warned: false })
        }

        /// The settings this instance was built with (a decoder's are the defaults, unused).
        pub fn settings(&self) -> OpusSettings {
            self.settings
        }

        fn warn_panic(&mut self, what: &str) {
            if !self.warned {
                self.warned = true;
                tracing::warn!("voice chat: the Opus {what} panicked - the panic was caught and that codec state dropped");
            }
        }
    }

    impl VoiceCodec for Opus {
        fn id(&self) -> u8 {
            OPUS_ID
        }

        fn name(&self) -> &'static str {
            "Opus (SILK wideband)"
        }

        fn max_frame_bytes(&self) -> usize {
            if self.encoding {
                self.settings.frame_bytes()
            } else {
                MAX_VOICE_BYTES
            }
        }

        fn bitrate(&self) -> u32 {
            self.settings.bitrate_bps
        }

        fn encode(&mut self, pcm: &MonoFrame, out: &mut Vec<u8>) -> Result<(), CodecError> {
            out.clear();
            let Some(mut enc) = self.enc.take() else { return Err(CodecError::Backend("the Opus encoder is gone (it panicked earlier)")) };
            // opus-rs 0.1.34 ignores `bitrate_bps` in VBR (SILK-only) mode and takes the rate from
            // the output buffer's size, so the buffer IS the rate control there.
            let cap = if self.settings.vbr { self.settings.frame_bytes() } else { MAX_VOICE_BYTES };
            out.resize(cap, 0);
            let buf = &mut out[..];
            let result = catch_unwind(AssertUnwindSafe(|| enc.encode(pcm, FRAME, buf)));
            let n = match result {
                Err(_) => {
                    out.clear();
                    self.warn_panic("encoder");
                    return Err(CodecError::Panicked);
                }
                Ok(r) => {
                    self.enc = Some(enc);
                    r
                }
            };
            match n {
                Ok(n) if (1..=MAX_VOICE_BYTES).contains(&n) && n <= out.len() => {
                    out.truncate(n);
                    if opus_toc_ok(out) {
                        Ok(())
                    } else {
                        out.clear();
                        Err(CodecError::Backend("the Opus encoder produced a packet this crate does not accept"))
                    }
                }
                Ok(_) => {
                    out.clear();
                    Err(CodecError::Backend("the Opus encoder returned a frame size out of range"))
                }
                Err(e) => {
                    out.clear();
                    Err(CodecError::Backend(e))
                }
            }
        }

        fn decode(&mut self, bytes: &[u8], out: &mut MonoFrame) -> Result<(), CodecError> {
            if bytes.len() > MAX_VOICE_BYTES {
                return Err(CodecError::WrongLength { got: bytes.len(), want: MAX_VOICE_BYTES });
            }
            // Only SILK / 20 ms / mono / one frame reaches the library: the one shape it decodes well.
            if !opus_toc_ok(bytes) {
                return Err(CodecError::BadHeader);
            }
            let mut dec = match self.dec.take() {
                Some(d) => d,
                None => {
                    // Dropped after a panic (or never built): a fresh decoder = a reset.
                    self.last_toc = WIDEBAND_TOC;
                    make_decoder()?
                }
            };
            let buf = &mut out[..];
            let result = catch_unwind(AssertUnwindSafe(|| dec.decode(bytes, FRAME, buf)));
            match result {
                Err(_) => {
                    out.fill(0.0);
                    self.warn_panic("decoder");
                    Err(CodecError::Panicked)
                }
                Ok(r) => {
                    self.dec = Some(dec);
                    match r {
                        Ok(FRAME) => {
                            if let Some(&toc) = bytes.first() {
                                self.last_toc = toc & 0xFC;
                            }
                            sanitise(out);
                            Ok(())
                        }
                        Ok(_) => Err(CodecError::Backend("the Opus decoder returned a frame of the wrong length")),
                        Err(e) => Err(CodecError::Backend(e)),
                    }
                }
            }
        }

        fn is_stateful(&self) -> bool {
            true
        }

        fn conceal(&mut self, next: Option<&[u8]>, out: &mut MonoFrame) -> bool {
            // opus-rs 0.1.34 cannot decode in-band FEC, so `next` is unused.
            let _ = next;
            let Some(mut dec) = self.dec.take() else { return false };
            // PLC = a TOC-only packet, which must be code 0 (a lone code-3 TOC is an error).
            let toc = [self.last_toc & 0xFC];
            let buf = &mut out[..];
            match catch_unwind(AssertUnwindSafe(|| dec.decode(&toc, FRAME, buf))) {
                Err(_) => {
                    out.fill(0.0);
                    self.warn_panic("decoder");
                    false
                }
                Ok(r) => {
                    self.dec = Some(dec);
                    if r == Ok(FRAME) {
                        sanitise(out);
                        true
                    } else {
                        false
                    }
                }
            }
        }

        fn reset(&mut self) {
            // opus-rs has no reset: recreate.
            if self.encoding {
                self.enc = make_encoder(&self.settings).ok();
            }
            if self.dec.is_some() || !self.encoding {
                self.dec = make_decoder().ok();
            }
            self.last_toc = WIDEBAND_TOC;
        }
    }
}
