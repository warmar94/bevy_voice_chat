//! **bevy_voice_chat** — game-agnostic voice chat for Bevy.
//!
//! The crate captures a microphone, decides when the player talks (voice activity with a
//! threshold + hangover, or push-to-talk driven by the game), runs the game's filters, encodes
//! 20 ms frames, and on the other side buffers each speaker's frames against network jitter and
//! mixes them — by distance or globally — into its own output stream on a chosen device.
//!
//! **It never touches the network.** Encoded frames come out as [`OutgoingVoice`] messages; the
//! game sends them however it likes and feeds received frames back as [`IncomingVoice`] with the
//! speaker's id. It knows nothing about the game either: the game drives [`VoiceInput`] (on/off,
//! devices, gains, threshold, talk mode, the "talking" key, the mic test), sets
//! [`VoiceChatConfig`], and marks entities with [`VoiceSpeaker`] / [`VoiceListener`].
//!
//! What the game reads back: [`VoiceDevices`] (device lists), [`VoiceChatState`] (mic / output
//! status + the live mic level + "am I transmitting"), [`VoiceActivity`] (who is heard right now),
//! [`VoiceStats`]. Hooks: [`VoiceFilters`] (outgoing + incoming processor chains), the
//! [`io::AudioIo`] device seam ([`VoiceIo`]), the [`codec::VoiceCodec`] trait.
//!
//! Codecs: IMA-ADPCM (~66 kbps, the default, always built) or, with the `opus` cargo feature,
//! Opus (the low-bandwidth option, 24 kbps by default), chosen by [`VoiceChatConfig::codec`].
//! Receivers play every codec compiled into them, so players on different codecs hear each other.
//!
//! With the `replicon` feature, [`replicon::VoiceRepliconPlugin`] is a ready-made transport over
//! `bevy_replicon` (a client message on an unreliable channel, relayed by the host).
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_voice_chat::prelude::*;
//!
//! fn main() {
//!     App::new()
//!         .add_plugins((DefaultPlugins, VoiceChatPlugin::default()))
//!         .add_systems(Update, talk_when_space_is_held.before(VoiceChatSystems::Devices))
//!         .run();
//! }
//!
//! fn talk_when_space_is_held(keys: Res<ButtonInput<KeyCode>>, mut input: ResMut<VoiceInput>) {
//!     input.enabled = true; // in a session
//!     input.mode = TalkMode::PushToTalk;
//!     input.talk_held = keys.pressed(KeyCode::Space);
//! }
//! ```
//!
//! Fail closed: no microphone, no output device or a stream error never panic — the state
//! resource says so and voice simply stops. Audio threads only touch atomics and bounded
//! channels.

#![warn(missing_docs)]
// Bevy system signatures are long by nature.
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_time::{Real, Time};
use bevy_transform::components::GlobalTransform;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::mpsc::TrySendError;
use std::sync::Arc;

pub mod codec;
pub mod cpal_io;
pub mod dsp;
pub mod filter;
pub mod gate;
pub mod io;
pub mod jitter;
pub mod mixer;
pub mod packet;
#[cfg(feature = "replicon")]
pub mod replicon;
pub mod spatial;
pub mod wav;

use codec::{CodecError, ImaAdpcm, MonoFrame, OpusSettings, VoiceCodec, VoiceCodecChoice, FRAME, VOICE_RATE};
use filter::{FilterContext, FilterStage, VoiceFilters};
use gate::{ms_to_frames, GateInput, TalkGate};
use io::{AudioIo, MicSession, OutputSession, ScanJob, DEVICE_FAILED, DEVICE_LIVE};
use jitter::{Insert, JitterConfig};
use mixer::{Mixer, SpeakerKey, StereoFrame};
use spatial::{Hearing, SpatialParams};

/// Everything a game usually needs: `use bevy_voice_chat::prelude::*;`.
pub mod prelude {
    pub use crate::codec::{OpusSettings, VoiceCodec, VoiceCodecChoice, FRAME, FRAME_MS, VOICE_RATE};
    pub use crate::cpal_io::CpalIo;
    pub use crate::filter::{FilterContext, FilterStage, GainFilter, VoiceFilter, VoiceFilters};
    pub use crate::io::{AudioIo, NullIo};
    #[cfg(feature = "replicon")]
    pub use crate::replicon::{
        HostVoiceSpeaker, VoiceDown, VoiceRelayHook, VoiceRelayStats, VoiceRepliconPlugin, VoiceSenderId, VoiceTransportSystems, VoiceUp,
    };
    pub use crate::spatial::Hearing;
    pub use crate::wav::WavFileIo;
    pub use crate::{
        DeviceState, IncomingVoice, OutgoingVoice, RescanDevices, SpeakerId, TalkMode, VoiceActivity, VoiceChatConfig, VoiceChatPlugin, VoiceChatState,
        VoiceChatSystems, VoiceDevices, VoiceInput, VoiceIo, VoiceListener, VoiceSpeaker, VoiceStats,
    };
}

// ---------------------------------------------------------------- public types

/// A speaker's identity — whatever id the game gives its players.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SpeakerId(pub u64);

/// How the player's voice goes out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TalkMode {
    /// Voice activity: frames at or past [`VoiceInput::threshold`] (+ a hangover) go out.
    #[default]
    VoiceActivity,
    /// Only while [`VoiceInput::talk_held`] (+ a short release tail).
    PushToTalk,
}

/// **What the game wants right now** (resource; the game writes it every frame or on change,
/// ordered `.before(VoiceChatSystems::Devices)`).
#[derive(Resource, Clone, Debug, PartialEq)]
pub struct VoiceInput {
    /// Voice chat is on (the player is in a session): the microphone and the output are open and
    /// frames go out as [`OutgoingVoice`].
    pub enabled: bool,
    /// Keep the microphone open for the level meter only (a settings screen), nothing is sent.
    pub meter: bool,
    /// The mic test: this player's voice plays back to them through the whole pipeline
    /// (filters, codec, jitter buffer, output) and nothing is sent meanwhile.
    pub loopback: bool,
    /// Voice activity or push-to-talk.
    pub mode: TalkMode,
    /// Voice activity threshold, `0..=1` of full scale after [`VoiceInput::mic_gain`].
    pub threshold: f32,
    /// Push-to-talk: the game's talk input is held (the crate knows no keys).
    pub talk_held: bool,
    /// Microphone gain (1 = as captured).
    pub mic_gain: f32,
    /// Voice output volume `0..=1` (the game applies its master volume into it).
    pub volume: f32,
    /// Input device by name (`None` / unknown = the system default).
    pub input_device: Option<String>,
    /// Output device by name (`None` / unknown = the system default).
    pub output_device: Option<String>,
    /// Distance applies right now (e.g. in a level; off in menus = everyone at full volume).
    pub positional: bool,
}

impl Default for VoiceInput {
    fn default() -> Self {
        Self {
            enabled: false,
            meter: false,
            loopback: false,
            mode: TalkMode::VoiceActivity,
            threshold: 0.1,
            talk_held: false,
            mic_gain: 1.0,
            volume: 1.0,
            input_device: None,
            output_device: None,
            positional: true,
        }
    }
}

/// **Tuning** (resource; the plugin inserts [`VoiceChatPlugin::config`], the game may replace it).
/// Deserialisable, so a game can keep the numbers in its own data files.
#[derive(Resource, Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VoiceChatConfig {
    /// `Proximity` (by distance + pan) or `Global` (everyone at full volume, no pan).
    pub hearing: Hearing,
    /// Proximity: silent at and beyond this distance (world units).
    pub range: f32,
    /// Proximity: full volume up to this distance.
    pub full_volume_distance: f32,
    /// Proximity: falloff exponent between the two (1 = linear).
    pub rolloff: f32,
    /// Proximity: 0 = mono, 1 = a voice hard to one side is silent in the other ear.
    pub pan_strength: f32,
    /// Jitter buffer: frames buffered before a talk spurt starts (ms).
    pub jitter_target_ms: f32,
    /// Jitter buffer: at most this much is kept (ms).
    pub jitter_max_ms: f32,
    /// Jitter buffer: loss concealed / an underflow waited for this long before giving up (ms).
    pub conceal_max_ms: f32,
    /// Voice activity: keep sending this long after the level drops under the threshold (ms).
    pub vad_hangover_ms: f32,
    /// Voice activity: frames from just before the level crossed the threshold go too (ms).
    pub vad_preroll_ms: f32,
    /// Push-to-talk: keep sending this long after release (ms).
    pub ptt_release_ms: f32,
    /// Mixed audio queued ahead of the output device (ms; covers a slow game frame).
    pub output_queue_ms: f32,
    /// At most this many captured frames are processed per game frame; older ones are stale.
    pub mic_backlog_frames: u32,
    /// A speaker silent for this long is forgotten (seconds).
    pub speaker_timeout_secs: f32,
    /// [`VoiceActivity`]: a speaker counts as heard this long after its last frame played (ms).
    pub heard_hold_ms: f32,
    /// The codec this player SENDS with: `ImaAdpcm` (the default) or `Opus` (needs the `opus`
    /// cargo feature). Receivers decode every codec compiled in. Changing it at runtime applies
    /// to the next frame sent. There is no automatic fallback: see [`VoiceChatConfig::problems`]
    /// and [`VoiceChatState::codec_error`].
    pub codec: VoiceCodecChoice,
    /// Opus encoder settings (used when `codec` is `Opus`).
    pub opus: OpusSettings,
}

impl Default for VoiceChatConfig {
    fn default() -> Self {
        Self {
            hearing: Hearing::Proximity,
            range: 20.0,
            full_volume_distance: 2.0,
            rolloff: 1.5,
            pan_strength: 0.6,
            jitter_target_ms: 60.0,
            jitter_max_ms: 240.0,
            conceal_max_ms: 60.0,
            vad_hangover_ms: 300.0,
            vad_preroll_ms: 40.0,
            ptt_release_ms: 120.0,
            output_queue_ms: 40.0,
            mic_backlog_frames: 5,
            speaker_timeout_secs: 5.0,
            heard_hold_ms: 250.0,
            codec: VoiceCodecChoice::ImaAdpcm,
            opus: OpusSettings::default(),
        }
    }
}

impl VoiceChatConfig {
    /// Everything wrong with the values (empty = fine).
    pub fn problems(&self) -> Vec<&'static str> {
        let pos = |v: f32| v.is_finite() && v > 0.0;
        let non_neg = |v: f32| v.is_finite() && v >= 0.0;
        let mut out = Vec::new();
        if !pos(self.range) || !non_neg(self.full_volume_distance) || self.full_volume_distance >= self.range || !pos(self.rolloff) {
            out.push("range > full_volume_distance >= 0, rolloff > 0");
        }
        if !(non_neg(self.pan_strength) && self.pan_strength <= 1.0) {
            out.push("pan_strength must be 0..=1");
        }
        if !pos(self.jitter_target_ms) || !pos(self.jitter_max_ms) || self.jitter_max_ms <= self.jitter_target_ms || !non_neg(self.conceal_max_ms) {
            out.push("jitter_max_ms > jitter_target_ms > 0, conceal_max_ms >= 0");
        }
        if ![self.vad_hangover_ms, self.vad_preroll_ms, self.ptt_release_ms].iter().all(|v| non_neg(*v)) || !pos(self.output_queue_ms) {
            out.push("vad / ptt times >= 0, output_queue_ms > 0");
        }
        if self.mic_backlog_frames == 0 || !pos(self.speaker_timeout_secs) || !pos(self.heard_hold_ms) {
            out.push("mic_backlog_frames, speaker_timeout_secs, heard_hold_ms must be > 0");
        }
        // Checked whatever `codec` is: a config's validity does not depend on the current choice.
        if !self.opus.problems().is_empty() {
            out.push("opus.bitrate_bps must be 6000..=64000, opus.complexity 0..=10");
        }
        if self.codec == VoiceCodecChoice::Opus && !cfg!(feature = "opus") {
            out.push("codec: Opus needs the `opus` cargo feature");
        }
        out
    }

    /// The jitter buffer in frames.
    pub fn jitter(&self) -> JitterConfig {
        JitterConfig {
            target: ms_to_frames(self.jitter_target_ms, 1) as usize,
            max: ms_to_frames(self.jitter_max_ms, 2) as usize,
            conceal_max: ms_to_frames(self.conceal_max_ms, 0) as usize,
        }
    }

    /// The spatial numbers for [`spatial::voice_gains`].
    pub fn spatial(&self) -> SpatialParams {
        SpatialParams { hearing: self.hearing, full: self.full_volume_distance, range: self.range, rolloff: self.rolloff, pan_strength: self.pan_strength }
    }

    /// Frames queued ahead of the output device.
    pub fn output_queue_frames(&self) -> u64 {
        u64::from(ms_to_frames(self.output_queue_ms, 1))
    }
}

/// Ask for a fresh device scan (e.g. when a settings screen opens: a device plugged in since
/// shows up). One scan also runs at startup.
#[derive(Message, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RescanDevices;

/// **The device lists** (resource; written by the crate). `scanned = false` until the first scan.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct VoiceDevices {
    /// A scan has landed.
    pub scanned: bool,
    /// Input device names (what [`VoiceInput::input_device`] takes).
    pub inputs: Vec<String>,
    /// Output device names (what [`VoiceInput::output_device`] takes).
    pub outputs: Vec<String>,
}

/// A device's state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DeviceState {
    /// Not needed right now.
    #[default]
    Closed,
    /// Being opened (a frame or two).
    Opening,
    /// Running on this device.
    Live(String),
    /// Missing, denied, unplugged or a stream error: voice is off that way.
    Failed,
}

/// **What the pipeline is doing** (resource; written by the crate) — what a settings screen or a
/// HUD shows.
#[derive(Resource, Clone, Debug, Default, PartialEq)]
pub struct VoiceChatState {
    /// The microphone.
    pub mic: DeviceState,
    /// The output stream.
    pub output: DeviceState,
    /// The live mic level: this frame's loudest sample x [`VoiceInput::mic_gain`], `0..=1` (the
    /// same scale as [`VoiceInput::threshold`]).
    pub mic_level: f32,
    /// This player's voice goes out right now.
    pub transmitting: bool,
    /// The microphone's native format (rate, channels) once open.
    pub mic_format: Option<(u32, u16)>,
    /// The output device's rate once open.
    pub output_rate: Option<u32>,
    /// What the microphone is ([`io::AudioIo::mic_kind`]).
    pub mic_kind: String,
    /// The codec this player sends with ([`VoiceChatConfig::codec`]).
    pub send_codec: VoiceCodecChoice,
    /// `Some(reason)`: this player's voice CANNOT be sent (the chosen codec is not compiled in,
    /// its settings are invalid, or its encoder failed). Receiving keeps working. There is no
    /// automatic fallback: fix the config and sending resumes on the next frame.
    pub codec_error: Option<String>,
}

/// **Who is heard right now** (resource; derived from played frames — nothing is networked):
/// indicators read it.
#[derive(Resource, Clone, Debug, Default, PartialEq, Eq)]
pub struct VoiceActivity {
    /// Remote speakers heard within [`VoiceChatConfig::heard_hold_ms`] (and not out of range).
    pub heard: BTreeSet<SpeakerId>,
    /// This player is transmitting.
    pub transmitting: bool,
}

/// Counters (resource; cumulative per process).
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
pub struct VoiceStats {
    /// [`OutgoingVoice`] frames produced.
    pub packets_out: u64,
    /// Their encoded bytes.
    pub bytes_out: u64,
    /// [`IncomingVoice`] frames received.
    pub packets_in: u64,
    /// Incoming frames dropped (unknown codec, wrong size, malformed, undecodable).
    pub rejected: u64,
    /// Incoming frames of a codec this build cannot decode (Opus without the `opus` feature):
    /// dropped.
    pub unsupported: u64,
    /// Frames this player's encoder failed on (dropped).
    pub encode_errors: u64,
    /// Mic-test frames looped back.
    pub loopback: u64,
    /// Mixed stereo frames sent to the output device.
    pub mixed: u64,
    /// The output queue ran dry while a voice was playing.
    pub underruns: u64,
    /// Stale captured frames dropped after a game stall.
    pub mic_dropped: u64,
}

/// One encoded frame of this player's voice for the game to send (message; written in
/// [`VoiceChatSystems::Capture`]). Stamp it with the sender's id on the way.
#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingVoice {
    /// Per-sender frame counter (receivers order by it).
    pub seq: u32,
    /// The sender's wall clock in ms (wrapping): a latency measure when both share a clock.
    pub ts: u32,
    /// [`codec::VoiceCodec::id`].
    pub codec: u8,
    /// The encoded frame.
    pub frame: Vec<u8>,
}

/// A received frame the game feeds back with the speaker's id (message; read in
/// [`VoiceChatSystems::Playback`]). Never feed the local player's own frames.
#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingVoice {
    /// Whose voice.
    pub speaker: SpeakerId,
    /// From [`OutgoingVoice::seq`].
    pub seq: u32,
    /// From [`OutgoingVoice::ts`].
    pub ts: u32,
    /// From [`OutgoingVoice::codec`].
    pub codec: u8,
    /// From [`OutgoingVoice::frame`].
    pub frame: Vec<u8>,
}

/// Marks an entity whose position a remote voice comes from (its `GlobalTransform`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct VoiceSpeaker {
    /// The speaker id its frames carry.
    pub id: SpeakerId,
    /// This speaker's own range (e.g. a megaphone), else [`VoiceChatConfig::range`].
    pub range: Option<f32>,
}

/// Marks the ONE entity this player hears from (its `GlobalTransform`).
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct VoiceListener {
    /// Take the left/right orientation from this entity instead (e.g. the camera).
    pub right_from: Option<Entity>,
}

/// The device seam (resource): [`cpal_io::CpalIo`] by default; a [`wav::WavFileIo`], a
/// [`io::NullIo`] or a test fake can replace it.
#[derive(Resource, Clone)]
pub struct VoiceIo(pub Arc<dyn AudioIo>);

impl Default for VoiceIo {
    fn default() -> Self {
        Self(Arc::new(cpal_io::CpalIo))
    }
}

/// The crate's systems, chained in this order in `Update`. Order the game's systems against
/// them: write [`VoiceInput`] before `Devices`; send [`OutgoingVoice`] after `Capture`; write
/// [`IncomingVoice`] before `Playback`; read [`VoiceActivity`] / [`VoiceChatState`] after
/// `Playback`.
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VoiceChatSystems {
    /// Device scans, opening / closing the microphone and the output stream.
    Devices,
    /// Capture -> gate -> outgoing filters -> encode -> [`OutgoingVoice`] (or the loopback).
    Capture,
    /// [`IncomingVoice`] -> decode -> jitter buffers -> incoming filters -> mix -> output.
    Playback,
}

// ---------------------------------------------------------------- plugin

/// The plugin. `config` becomes the [`VoiceChatConfig`] resource (the game may replace it later).
#[derive(Clone, Debug, Default)]
pub struct VoiceChatPlugin {
    /// Initial tuning.
    pub config: VoiceChatConfig,
}

impl Plugin for VoiceChatPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.config.clone())
            .insert_resource(VoiceRuntime::new(&self.config))
            .init_resource::<VoiceIo>()
            .init_resource::<VoiceInput>()
            .init_resource::<VoiceDevices>()
            .init_resource::<VoiceChatState>()
            .init_resource::<VoiceActivity>()
            .init_resource::<VoiceStats>()
            .init_resource::<VoiceFilters>()
            .init_resource::<Sessions>()
            .add_message::<OutgoingVoice>()
            .add_message::<IncomingVoice>()
            .add_message::<RescanDevices>()
            .configure_sets(Update, (VoiceChatSystems::Devices, VoiceChatSystems::Capture, VoiceChatSystems::Playback).chain())
            .add_systems(Update, (scan_devices, run_devices).chain().in_set(VoiceChatSystems::Devices))
            .add_systems(Update, capture_voice.in_set(VoiceChatSystems::Capture))
            .add_systems(Update, (receive_voice, mix_voice, publish_activity).chain().in_set(VoiceChatSystems::Playback));
    }
}

// ---------------------------------------------------------------- internals

/// The open devices + the scan in flight (resource; written by the crate, read-only outside).
#[derive(Resource, Default)]
pub struct Sessions {
    mic: Option<MicSession>,
    output: Option<OutputSession>,
    scan: Option<ScanJob>,
    scan_started: bool,
}

impl Sessions {
    /// The open microphone, if any (its shared state has the device's format, peak and latency).
    pub fn mic(&self) -> Option<&MicSession> {
        self.mic.as_ref()
    }

    /// The open voice output stream, if any.
    pub fn output(&self) -> Option<&OutputSession> {
        self.output.as_ref()
    }

    /// The microphone driver's reported capture latency in ms (`None` = closed or unknown).
    pub fn mic_latency_ms(&self) -> Option<u32> {
        self.mic.as_ref().map(|s| s.shared.latency_us.load(Ordering::Relaxed) / 1000).filter(|v| *v > 0)
    }

    /// The output driver's reported playback latency in ms (`None` = closed or unknown).
    pub fn output_latency_ms(&self) -> Option<u32> {
        self.output.as_ref().map(|o| o.shared.latency_us.load(Ordering::Relaxed) / 1000).filter(|v| *v > 0)
    }

    /// Is the output stream open but failed? (Helper for status lines.)
    pub fn output_failed(&self) -> bool {
        self.output.as_ref().is_some_and(|o| o.shared.state() == DEVICE_FAILED)
    }
}

/// Consecutive encode failures (0.5 s of frames) after which sending turns off.
const ENCODE_FAIL_LIMIT: u32 = 25;

/// The pipeline's working state (resource; written by the crate, read-only outside).
#[derive(Resource)]
pub struct VoiceRuntime {
    /// This player's encoder (`None` = sending is off, see `codec_error`).
    encoder: Option<Box<dyn VoiceCodec>>,
    /// The (codec, settings) the encoder was built from.
    encoder_spec: (VoiceCodecChoice, OpusSettings),
    codec_error: Option<String>,
    encode_fail_run: u32,
    warned_encode: bool,
    /// The shared stateless decoder for frames decoded on arrival.
    adpcm: ImaAdpcm,
    warned_unsupported: bool,
    gate: TalkGate,
    preroll: VecDeque<MonoFrame>,
    seq: u32,
    loop_seq: u32,
    encoded: Vec<u8>,
    captured: Vec<MonoFrame>,
    mixer: Mixer,
    produced: u64,
    was_playing: bool,
}

impl VoiceRuntime {
    /// A fresh runtime; the encoder is built from `cfg.codec` / `cfg.opus` (on failure sending
    /// is off and [`VoiceRuntime::codec_error`] says why).
    pub fn new(cfg: &VoiceChatConfig) -> Self {
        let mut rt = Self {
            encoder: None,
            encoder_spec: (cfg.codec, cfg.opus),
            codec_error: None,
            encode_fail_run: 0,
            warned_encode: false,
            adpcm: ImaAdpcm::default(),
            warned_unsupported: false,
            gate: TalkGate::default(),
            preroll: VecDeque::new(),
            seq: 0,
            loop_seq: 0,
            encoded: Vec::new(),
            captured: Vec::new(),
            mixer: Mixer::new(cfg.jitter()),
            produced: 0,
            was_playing: false,
        };
        rt.build_encoder();
        rt
    }

    /// The encoder this player sends with (`None` = sending is off; [`Self::codec_error`] says
    /// why). A relay should not check packets against it: use [`packet::validate_packet`].
    pub fn send_codec(&self) -> Option<&dyn VoiceCodec> {
        self.encoder.as_deref()
    }

    /// Why sending is off (`None` = it is not).
    pub fn codec_error(&self) -> Option<&str> {
        self.codec_error.as_deref()
    }

    /// Rebuild the encoder when the config asks for another (codec, settings). Never falls back
    /// to another codec: a failure turns sending off with a reason.
    fn apply_codec(&mut self, cfg: &VoiceChatConfig) {
        let spec = (cfg.codec, cfg.opus);
        if spec == self.encoder_spec && (self.encoder.is_some() || self.codec_error.is_some()) {
            return;
        }
        self.encoder_spec = spec;
        self.build_encoder();
    }

    fn build_encoder(&mut self) {
        let (choice, opus) = self.encoder_spec;
        self.encode_fail_run = 0;
        self.warned_encode = false;
        match codec::new_encoder(choice, &opus) {
            Ok(e) => {
                self.encoder = Some(e);
                self.codec_error = None;
            }
            Err(e) => {
                let reason = e.to_string();
                tracing::warn!("voice chat: cannot start the {choice:?} encoder: {reason} - voice sending is off");
                self.encoder = None;
                self.codec_error = Some(reason);
            }
        }
    }

    /// Test hook: replace the encoder (keeps the spec, so the config does not rebuild it).
    #[cfg(test)]
    pub(crate) fn set_encoder_for_test(&mut self, encoder: Box<dyn VoiceCodec>) {
        self.encoder = Some(encoder);
        self.codec_error = None;
        self.encode_fail_run = 0;
    }

    /// Every speaker's jitter buffer, gain and timing (read-only: stats, latency, "who is buffered").
    pub fn mixer(&self) -> &Mixer {
        &self.mixer
    }

    /// Mixed frames queued ahead of the output device right now.
    pub fn output_queued(&self, sessions: &Sessions) -> u64 {
        sessions.output.as_ref().map_or(0, |o| self.produced.saturating_sub(o.shared.consumed.load(Ordering::Relaxed)))
    }
}

/// The wall clock in ms (wrapping u32).
pub fn wall_ms() -> u32 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u32).unwrap_or(0)
}

fn sane(v: f32, lo: f32, hi: f32, fallback: f32) -> f32 {
    if v.is_finite() {
        v.clamp(lo, hi)
    } else {
        fallback
    }
}

/// Scan at startup and on [`RescanDevices`]; land the lists in [`VoiceDevices`].
fn scan_devices(io: Res<VoiceIo>, mut rescan: MessageReader<RescanDevices>, mut sessions: ResMut<Sessions>, mut devices: ResMut<VoiceDevices>) {
    let asked = rescan.read().count() > 0;
    if (!sessions.scan_started || asked) && sessions.scan.is_none() {
        sessions.scan_started = true;
        sessions.scan = Some(io.0.start_scan());
    }
    let Some(lists) = sessions.scan.as_ref().and_then(ScanJob::take) else { return };
    sessions.scan = None;
    let want = VoiceDevices { scanned: true, inputs: lists.inputs, outputs: lists.outputs };
    if *devices != want {
        *devices = want;
    }
}

/// The device to open for a wanted name: known name -> it; unknown / unscanned -> as asked (the
/// seam falls back to the default for an unknown name); `None` = the system default.
fn device_to_open(wanted: Option<&str>, list: &[String], scanned: bool) -> Option<String> {
    match wanted {
        Some(n) if !scanned || list.iter().any(|d| d == n) => Some(n.to_string()),
        _ => None,
    }
}

/// Open / close / reopen the microphone and the output as [`VoiceInput`] asks; write the device
/// part of [`VoiceChatState`] and the mic level.
fn run_devices(
    io: Res<VoiceIo>,
    input: Res<VoiceInput>,
    devices: Res<VoiceDevices>,
    mut sessions: ResMut<Sessions>,
    mut rt: ResMut<VoiceRuntime>,
    mut filters: ResMut<VoiceFilters>,
    mut state: ResMut<VoiceChatState>,
) {
    let want_mic = (input.enabled || input.meter || input.loopback).then(|| device_to_open(input.input_device.as_deref(), &devices.inputs, devices.scanned));
    if sessions.mic.as_ref().is_some_and(|s| want_mic.as_ref() != Some(&s.requested)) {
        // Dropping the session stops its thread.
        sessions.mic = None;
    }
    if let (Some(device), None) = (&want_mic, &sessions.mic) {
        sessions.mic = Some(io.0.open_mic(device.clone()));
        rt.gate.reset();
        rt.preroll.clear();
        filters.reset();
    }
    let want_out = (input.enabled || input.loopback).then(|| device_to_open(input.output_device.as_deref(), &devices.outputs, devices.scanned));
    if sessions.output.as_ref().is_some_and(|o| want_out.as_ref() != Some(&o.requested)) {
        sessions.output = None;
    }
    if let (Some(device), None) = (&want_out, &sessions.output) {
        sessions.output = Some(io.0.open_output(device.clone()));
        rt.produced = 0;
        rt.was_playing = false;
    }
    if want_out.is_none() {
        // Nothing to play to: every buffer goes (the next session starts clean).
        rt.mixer.clear();
    }
    let gain = sane(input.mic_gain, 0.0, 4.0, 1.0);
    let (mic, level, format) = match &sessions.mic {
        None => (DeviceState::Closed, 0.0, None),
        Some(s) => {
            let st = s.shared.status();
            let raw = s.shared.take_peak();
            let level = if matches!(st, DeviceState::Live(_)) { (raw * gain).clamp(0.0, 1.0) } else { 0.0 };
            let rate = s.shared.rate.load(Ordering::Relaxed);
            let ch = s.shared.channels.load(Ordering::Relaxed) as u16;
            (st, level, (rate > 0).then_some((rate, ch)))
        }
    };
    let (output, rate) = match &sessions.output {
        None => (DeviceState::Closed, None),
        Some(o) => {
            let r = o.shared.rate.load(Ordering::Relaxed);
            (o.shared.status(), (r > 0).then_some(r))
        }
    };
    // `transmitting`, `send_codec` and `codec_error` belong to `capture_voice`: carried over.
    let want = VoiceChatState {
        mic,
        output,
        mic_level: level,
        transmitting: state.transmitting,
        mic_format: format,
        output_rate: rate,
        mic_kind: io.0.mic_kind(),
        send_codec: state.send_codec,
        codec_error: state.codec_error.clone(),
    };
    if *state != want {
        *state = want;
    }
}

/// Speaker side: captured frames -> gain -> gate -> outgoing filters -> codec -> out.
fn capture_voice(
    time: Res<Time<Real>>,
    cfg: Res<VoiceChatConfig>,
    input: Res<VoiceInput>,
    sessions: Res<Sessions>,
    mut rt: ResMut<VoiceRuntime>,
    mut filters: ResMut<VoiceFilters>,
    mut out: MessageWriter<OutgoingVoice>,
    mut stats: ResMut<VoiceStats>,
    mut state: ResMut<VoiceChatState>,
) {
    let rt = &mut *rt;
    // A codec change applies to the next frame sent (before this frame's backlog is encoded).
    if cfg.is_changed() {
        rt.apply_codec(&cfg);
    }
    publish_codec_state(rt, &mut state);
    rt.gate.hangover = ms_to_frames(cfg.vad_hangover_ms, 0);
    rt.gate.ptt_release = ms_to_frames(cfg.ptt_release_ms, 0);
    let preroll_len = ms_to_frames(cfg.vad_preroll_ms, 0) as usize;
    let live = sessions.mic.as_ref().filter(|s| s.shared.state() == DEVICE_LIVE);
    let Some(mic) = live else {
        if let Some(s) = &sessions.mic {
            // Opening or failed: whatever is queued is stale.
            s.drain(&mut rt.captured);
            rt.captured.clear();
        }
        rt.gate.reset();
        rt.preroll.clear();
        if state.transmitting {
            state.transmitting = false;
        }
        return;
    };
    mic.drain(&mut rt.captured);
    let backlog = rt.captured.len().saturating_sub(cfg.mic_backlog_frames.max(1) as usize);
    if backlog > 0 {
        rt.captured.drain(..backlog);
        stats.mic_dropped += backlog as u64;
    }
    let gain = sane(input.mic_gain, 0.0, 4.0, 1.0);
    let threshold = sane(input.threshold, 0.0, 1.0, 1.0);
    let now = time.elapsed_secs_f64();
    let loopback = input.loopback;
    let mut frames = std::mem::take(&mut rt.captured);
    for mut frame in frames.drain(..) {
        let raw = dsp::peak(&frame);
        dsp::apply_gain(&mut frame, gain);
        let gate_in = match input.mode {
            TalkMode::VoiceActivity => GateInput::OpenMic { level: (raw * gain).clamp(0.0, 1.0), threshold },
            TalkMode::PushToTalk => GateInput::PushToTalk { held: input.talk_held },
        };
        let open = rt.gate.step(gate_in);
        let sending = open && (loopback || input.enabled);
        let ctx = FilterContext { sample_rate: VOICE_RATE, stage: FilterStage::Outgoing, speaker: None, transmitting: sending && !loopback, loopback };
        filters.run(&mut frame, &ctx);
        if !sending {
            rt.preroll.push_back(frame);
            while rt.preroll.len() > preroll_len {
                rt.preroll.pop_front();
            }
            continue;
        }
        let mut batch: Vec<MonoFrame> = rt.preroll.drain(..).collect();
        batch.push(frame);
        for f in &batch {
            // No encoder = sending is off (`codec_error` says why): the gate and meter still run.
            let Some(enc) = rt.encoder.as_mut() else { continue };
            let id = enc.id();
            if let Err(e) = enc.encode(f, &mut rt.encoded) {
                stats.encode_errors += 1;
                rt.encode_fail_run += 1;
                if !rt.warned_encode {
                    rt.warned_encode = true;
                    tracing::warn!("voice chat: the {} encoder failed on a frame: {e} - frame dropped", enc.name());
                }
                if rt.encode_fail_run >= ENCODE_FAIL_LIMIT {
                    tracing::warn!("voice chat: the encoder keeps failing ({e}) - voice sending is off until the codec config changes");
                    rt.encoder = None;
                    rt.codec_error = Some(format!("encoder failing: {e}"));
                }
                continue;
            }
            rt.encode_fail_run = 0;
            if loopback {
                // The mic test takes the receivers' path: decode on arrival or at playout.
                let seq = rt.loop_seq;
                rt.loop_seq = rt.loop_seq.wrapping_add(1);
                if accept_frame(&mut rt.mixer, &mut rt.adpcm, SpeakerKey::Loopback, seq, id, &rt.encoded, wall_ms(), now).is_ok() {
                    stats.loopback += 1;
                }
            } else {
                out.write(OutgoingVoice { seq: rt.seq, ts: wall_ms(), codec: id, frame: rt.encoded.clone() });
                rt.seq = rt.seq.wrapping_add(1);
                stats.packets_out += 1;
                stats.bytes_out += rt.encoded.len() as u64;
            }
        }
    }
    rt.captured = frames;
    publish_codec_state(rt, &mut state);
    let transmitting = rt.gate.is_open() && input.enabled && !loopback && rt.encoder.is_some();
    if state.transmitting != transmitting {
        state.transmitting = transmitting;
    }
}

/// Mirror the runtime's codec choice / error into [`VoiceChatState`] (on change only).
fn publish_codec_state(rt: &VoiceRuntime, state: &mut ResMut<VoiceChatState>) {
    let send = rt.encoder_spec.0;
    if state.send_codec != send || state.codec_error.as_deref() != rt.codec_error.as_deref() {
        let s = &mut **state;
        s.send_codec = send;
        s.codec_error = rt.codec_error.clone();
    }
}

/// One validated frame into `key`'s channel: a self-contained frame (IMA-ADPCM) is decoded now;
/// a frame of a stateful codec (Opus) is stored encoded and decoded in order at playout.
fn accept_frame(mixer: &mut Mixer, adpcm: &mut ImaAdpcm, key: SpeakerKey, seq: u32, codec: u8, bytes: &[u8], ts: u32, now: f64) -> Result<Insert, CodecError> {
    if codec == ImaAdpcm::ID {
        let mut frame: MonoFrame = [0.0; FRAME];
        adpcm.decode(bytes, &mut frame)?;
        Ok(mixer.insert(key, seq, &frame, ts, now))
    } else {
        Ok(mixer.insert_packet(key, seq, codec, bytes, ts, now))
    }
}

/// Listener side: every [`IncomingVoice`] of a codec this build decodes goes into its speaker's
/// jitter buffer (IMA-ADPCM decoded now, Opus stored for decoding at playout).
fn receive_voice(
    time: Res<Time<Real>>,
    sessions: Res<Sessions>,
    mut incoming: MessageReader<IncomingVoice>,
    mut rt: ResMut<VoiceRuntime>,
    mut stats: ResMut<VoiceStats>,
) {
    let rt = &mut *rt;
    let now = time.elapsed_secs_f64();
    for m in incoming.read() {
        stats.packets_in += 1;
        // Nothing to play to: drop instead of buffering.
        if sessions.output.is_none() {
            continue;
        }
        if packet::validate_packet(m.codec, &m.frame).is_err() {
            stats.rejected += 1;
            continue;
        }
        if !codec::can_decode(m.codec) {
            stats.unsupported += 1;
            if !rt.warned_unsupported {
                rt.warned_unsupported = true;
                tracing::warn!("voice chat: received Opus voice but this build has no `opus` feature - enable it to hear these players");
            }
            continue;
        }
        if accept_frame(&mut rt.mixer, &mut rt.adpcm, SpeakerKey::Remote(m.speaker), m.seq, m.codec, &m.frame, m.ts, now).is_err() {
            stats.rejected += 1;
        }
    }
}

/// Mix on the output device's clock: keep [`VoiceChatConfig::output_queue_ms`] queued, every
/// speaker at its gain ([`spatial::voice_gains`], ONE place) x [`VoiceInput::volume`].
fn mix_voice(
    time: Res<Time<Real>>,
    cfg: Res<VoiceChatConfig>,
    input: Res<VoiceInput>,
    sessions: Res<Sessions>,
    listeners: Query<(&GlobalTransform, &VoiceListener)>,
    transforms: Query<&GlobalTransform>,
    speakers: Query<(&GlobalTransform, &VoiceSpeaker)>,
    mut rt: ResMut<VoiceRuntime>,
    mut filters: ResMut<VoiceFilters>,
    mut stats: ResMut<VoiceStats>,
) {
    let rt = &mut *rt;
    if cfg.is_changed() {
        rt.mixer.set_config(cfg.jitter());
    }
    let now = time.elapsed_secs_f64();
    rt.mixer.forget_idle(now, f64::from(cfg.speaker_timeout_secs));
    let Some(out) = sessions.output.as_ref().filter(|o| o.shared.state() == DEVICE_LIVE) else {
        rt.was_playing = false;
        return;
    };
    let me = listeners.iter().next().map(|(t, l)| {
        let right = l.right_from.and_then(|e| transforms.get(e).ok()).map_or_else(|| t.right(), |c| c.right());
        spatial::Listener { pos: t.translation(), right: *right }
    });
    let params = cfg.spatial();
    for key in rt.mixer.keys() {
        let (l, r) = match key {
            SpeakerKey::Loopback => (1.0, 1.0),
            SpeakerKey::Remote(id) => {
                let found = speakers.iter().find(|(_, s)| s.id == id);
                let pos = found.map(|(t, _)| t.translation());
                let range = found.and_then(|(_, s)| s.range);
                spatial::voice_gains(&params, input.positional, me, pos, range)
            }
        };
        rt.mixer.set_target(key, l, r);
    }
    if !rt.mixer.any_active() {
        rt.was_playing = false;
        return;
    }
    let consumed = out.shared.consumed.load(Ordering::Relaxed);
    let queued = rt.produced.saturating_sub(consumed);
    if queued == 0 && rt.was_playing {
        stats.underruns += 1;
    }
    let now_ms = wall_ms();
    let volume = sane(input.volume, 0.0, 1.0, 0.0);
    let mut frame: StereoFrame = [0.0; FRAME * 2];
    let filters = &mut *filters;
    for _ in 0..cfg.output_queue_frames().saturating_sub(queued) {
        rt.mixer.mix(&mut frame, volume, now, now_ms, |key, f| {
            if let SpeakerKey::Remote(id) = key {
                let ctx = FilterContext { sample_rate: VOICE_RATE, stage: FilterStage::Incoming, speaker: Some(id), transmitting: false, loopback: false };
                filters.run(f, &ctx);
            }
        });
        match out.frames.try_send(frame) {
            Ok(()) => {
                rt.produced += 1;
                stats.mixed += 1;
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => break,
        }
    }
    rt.was_playing = true;
}

/// [`VoiceActivity`]: who was heard within the hold time (on change only).
fn publish_activity(time: Res<Time<Real>>, cfg: Res<VoiceChatConfig>, rt: Res<VoiceRuntime>, state: Res<VoiceChatState>, mut activity: ResMut<VoiceActivity>) {
    let now = time.elapsed_secs_f64();
    let hold = f64::from(cfg.heard_hold_ms) / 1000.0;
    let heard: BTreeSet<SpeakerId> = rt
        .mixer
        .keys()
        .into_iter()
        .filter_map(|k| match k {
            SpeakerKey::Remote(id) if rt.mixer.heard(k, now, hold) => Some(id),
            _ => None,
        })
        .collect();
    let want = VoiceActivity { heard, transmitting: state.transmitting };
    if *activity != want {
        *activity = want;
    }
}

#[cfg(test)]
mod app_tests;
#[cfg(test)]
mod tests;

/// Every Rust example in the README compiles (checked by `cargo test --all-features`).
#[cfg(all(doctest, feature = "replicon"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
