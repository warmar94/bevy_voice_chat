//! The voice codec. Everything on the wire is one [`FRAME`] of mono audio at
//! [`VOICE_RATE`] (20 ms at 16 kHz). The codec sits behind [`VoiceCodec`] so Opus (or anything
//! else) can replace it later without touching capture, transport or playback.
//!
//! **IMA-ADPCM 4:1, pure Rust, no native library** (nothing to bundle next to a game's
//! executable). Opus needs libopus + a C toolchain (audiopus / opus bindings) and no mature
//! pure-Rust Opus encoder exists yet. ADPCM at 16 kHz = 4 bits/sample = 64 kbps + a 4-byte header per
//! frame (164 B / 20 ms = 65.6 kbps codec payload). Every frame carries its own predictor + step
//! index, so a lost packet never corrupts the next one. [`Pcm16`] (256 kbps) is the reference /
//! fallback codec.

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

/// One mono frame.
pub type MonoFrame = [f32; FRAME];

/// A decode failure (a malformed or foreign packet — dropped, counted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// Not one frame's worth of bytes.
    WrongLength {
        /// Bytes received.
        got: usize,
        /// Bytes one frame has.
        want: usize,
    },
    /// The header's step index is out of range.
    BadHeader,
}

/// A voice codec: one [`FRAME`] in, a few bytes out, and back.
pub trait VoiceCodec: Send + Sync + 'static {
    /// On the wire in every packet: a receiver drops a packet whose id is not its codec's.
    fn id(&self) -> u8;
    /// A readable name (logs, status lines).
    fn name(&self) -> &'static str;
    /// Exact encoded size of one frame (the host checks it before relaying).
    fn frame_bytes(&self) -> usize;
    /// Encode one frame into `out` (cleared first). May keep state between frames.
    fn encode(&mut self, pcm: &MonoFrame, out: &mut Vec<u8>);
    /// Decode one frame. Stateless per frame, so one decoder serves every speaker.
    fn decode(&self, bytes: &[u8], out: &mut MonoFrame) -> Result<(), CodecError>;
    /// Codec payload bits per second (for bandwidth estimates).
    fn bitrate(&self) -> u32 {
        (self.frame_bytes() as u32 * 8 * 1000) / FRAME_MS
    }
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

    fn frame_bytes(&self) -> usize {
        ADPCM_HEADER + FRAME / 2
    }

    fn encode(&mut self, pcm: &MonoFrame, out: &mut Vec<u8>) {
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
    }

    fn decode(&self, bytes: &[u8], out: &mut MonoFrame) -> Result<(), CodecError> {
        let want = self.frame_bytes();
        if bytes.len() != want {
            return Err(CodecError::WrongLength { got: bytes.len(), want });
        }
        let (Some(&p0), Some(&p1), Some(&idx)) = (bytes.first(), bytes.get(1), bytes.get(2)) else { return Err(CodecError::BadHeader) };
        if idx > 88 {
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

/// Plain 16-bit PCM (little endian): the reference / fallback codec.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pcm16;

impl Pcm16 {
    /// The id on the wire.
    pub const ID: u8 = 2;
}

impl VoiceCodec for Pcm16 {
    fn id(&self) -> u8 {
        Self::ID
    }

    fn name(&self) -> &'static str {
        "PCM 16-bit"
    }

    fn frame_bytes(&self) -> usize {
        FRAME * 2
    }

    fn encode(&mut self, pcm: &MonoFrame, out: &mut Vec<u8>) {
        out.clear();
        for &x in pcm {
            out.extend_from_slice(&to_i16(x).to_le_bytes());
        }
    }

    fn decode(&self, bytes: &[u8], out: &mut MonoFrame) -> Result<(), CodecError> {
        let want = self.frame_bytes();
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

/// The codec this build speaks (one place to swap in Opus later).
pub fn default_codec() -> Box<dyn VoiceCodec> {
    Box::new(ImaAdpcm::default())
}
