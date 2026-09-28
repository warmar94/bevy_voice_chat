# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0, a breaking
change or a Bevy / key dependency bump raises the minor version).

## [0.2.0] - 2026-09-28

Opus as an optional second codec, for Bevy 0.19.0, cpal 0.17.3 and opus-rs 0.1.34.

### Added

- Feature `opus` (off by default): Opus (SILK wideband, 20 ms, mono) through the pure-Rust
  `opus-rs` 0.1.34 as the low-bandwidth codec: 60 B per frame at the default 24 kbps (constant
  bitrate, complexity 5; presets 16 and 32 kbps) instead of 164 B, with Opus packet loss
  concealment. Every call into the library runs inside `catch_unwind`, and incoming packets are
  header-checked before it sees them.
- `VoiceChatConfig::{codec, opus}` (`VoiceCodecChoice::{ImaAdpcm, Opus}`, `OpusSettings { bitrate_bps,
  complexity, vbr }` with `problems()`, `frame_bytes()`, `LOW_BANDWIDTH` / `QUALITY` presets). An
  old config file without these fields still loads (IMA-ADPCM).
- Runtime codec change: editing `codec` / `opus` rebuilds the encoder for the next frame sent.
- Mixed-codec receive: every codec compiled in is decoded, by the packet's codec id; a build
  without `opus` drops Opus packets and counts them (`VoiceStats::unsupported`, one warning).
- No automatic fallback: `VoiceChatConfig::problems()` reports invalid Opus settings and
  `codec: Opus` without the feature; an encoder that cannot start (or keeps failing) turns
  sending off with `VoiceChatState::codec_error` / `VoiceRuntime::codec_error()`.
- `VoiceChatState::{send_codec, codec_error}`, `VoiceStats::{unsupported, encode_errors}`,
  `JitterStats::undecodable`.
- `codec::{OPUS_ID, Opus (feature), new_encoder, new_decoder, can_decode}`, `CodecError::{NotCompiled,
  BadSettings, Backend, Panicked}` (+ `Display`), `ImaAdpcm::{FRAME_BYTES, MAX_STEP_INDEX}`,
  `Pcm16::FRAME_BYTES`.
- `packet::{validate_packet, opus_toc_ok}`, `Reject::Malformed`.
- `jitter::{JitterCore, Next}` (the jitter state machine over any payload, with a restart
  `generation()`), `mixer::{Payload, PacketJitter}`, `Mixer::insert_packet`,
  `SpeakerChannel::decoder_codec()`: stateful codecs are decoded at playout, in sequence order.
- `mic_test` example: `--opus`, `--kbps N`, `--cycle` (runtime codec switching), `--clean`.

### Changed (breaking)

- The `VoiceCodec` trait: `encode` returns `Result`, `decode` takes `&mut self`,
  `max_frame_bytes()` / `fixed_frame_bytes()` replace `frame_bytes()`, `bitrate()` is required,
  new `is_stateful()` / `conceal()` / `reset()` (with defaults).
- `VoiceRuntime::codec()` is now `send_codec() -> Option<&dyn VoiceCodec>` (`None` = sending is
  off).
- `replicon::check_up(speaker, &up)` (no `want_codec` / `want_len`); relays accept every known
  wire codec (`validate_packet`), not only their own.
- `SpeakerChannel::jitter` is a `PacketJitter`; `Mixer` and `SpeakerChannel` are no longer
  `Clone` (`Debug` is kept).
- New fields on `VoiceChatConfig`, `VoiceChatState`, `VoiceStats`, `JitterStats` (struct literals
  need them or `..Default::default()`); new `Reject` variant.
- `Pcm16` is documented as a local reference codec; it is never accepted on the wire.

### Unchanged

- IMA-ADPCM stays the default codec and its wire format is identical, so a default 0.2 peer and
  a 0.1 peer hear each other. Its receive path (decode on arrival, repeat-and-fade concealment)
  is sample-identical to 0.1.

### Internal

- CI also runs `cargo test --features opus`, `cargo test --no-default-features` and clippy with
  `--features opus`. `Cargo.lock` keeps every Bevy crate at 0.19.0.

## [0.1.0] - 2026-09-27

First release, for Bevy 0.19.0 and cpal 0.17.3.

### Added

- `VoiceChatPlugin`: microphone capture on its own thread (any device rate / channel count ->
  16 kHz mono, 20 ms frames), voice activity (threshold + hangover + pre-roll) or push-to-talk
  (a game-driven "talk held" flag + release tail), outgoing and incoming filter chains, the
  IMA-ADPCM codec (~66 kbps per talking speaker, pure Rust), per-speaker jitter buffers (reorder,
  loss concealment, late / duplicate drop, underflow wait, drift trim), a stereo mixer on its own
  output stream on a chosen device, `Hearing::Proximity` (distance falloff + pan) or
  `Hearing::Global`, the mic-test loopback, device scans, a live mic level, stats.
- Transport-agnostic hooks: `OutgoingVoice` / `IncomingVoice` messages, `packet::validate_frame`
  and `packet::TokenBucket` for relays.
- Game-facing resources, messages and components: `VoiceInput`, `VoiceChatConfig` (serde),
  `VoiceDevices`, `VoiceChatState`, `VoiceActivity`, `VoiceStats`, `VoiceFilters`, `VoiceIo`,
  `RescanDevices`, `VoiceSpeaker`, `VoiceListener`, the `VoiceChatSystems` sets; read-only
  `Sessions` / `VoiceRuntime` accessors for diagnostics.
- Device seam `AudioIo` with `CpalIo` (real hardware), `WavFileIo` (a WAV file as the
  microphone) and `NullIo` (no devices).
- Optional feature `replicon`: `VoiceRepliconPlugin` for bevy_replicon 0.44.2 (a client message
  on an unreliable channel, a host relay with packet checks, a per-speaker budget and a
  `VoiceRelayHook`).
- Examples `quick_start`, `mic_test`, `device_check`.
