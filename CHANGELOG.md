# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html) (before 1.0, a breaking
change or a Bevy / key dependency bump raises the minor version).

## [0.1.0] - Unreleased

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
