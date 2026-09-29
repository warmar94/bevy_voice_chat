# bevy_voice_chat

[![crates.io](https://img.shields.io/crates/v/bevy_voice_chat.svg)](https://crates.io/crates/bevy_voice_chat)
[![docs.rs](https://img.shields.io/docsrs/bevy_voice_chat)](https://docs.rs/bevy_voice_chat)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![CI](https://github.com/warmar94/bevy_voice_chat/actions/workflows/ci.yml/badge.svg)](https://github.com/warmar94/bevy_voice_chat/actions/workflows/ci.yml)
[![Bevy 0.19.0](https://img.shields.io/badge/Bevy-0.19.0-informational)](https://bevyengine.org)
[![cpal 0.17.3](https://img.shields.io/badge/cpal-0.17.3-informational)](https://crates.io/crates/cpal)
[![bevy_replicon 0.44.2 (optional)](https://img.shields.io/badge/bevy__replicon-0.44.2%20(optional)-informational)](https://crates.io/crates/bevy_replicon)
[![opus-rs 0.1.34 (optional)](https://img.shields.io/badge/opus--rs-0.1.34%20(optional)-informational)](https://crates.io/crates/opus-rs)

Game-agnostic, transport-agnostic **voice chat for [Bevy](https://bevyengine.org)**: microphone
capture, voice activity or push-to-talk, filter hooks (voice changers, radio effects), pure-Rust
codecs (IMA-ADPCM built in, Opus optional), per-speaker jitter buffers and a mixer that plays
every voice by distance (with left/right pan) or globally, on the output device the player
picked.

The crate **never touches the network**: encoded 20 ms frames come out as a message, you send them
with whatever networking you already use, and you feed received frames back in as a message. A
ready-made [`bevy_replicon`](https://crates.io/crates/bevy_replicon) transport is included behind
the `replicon` feature.

## Contents

- [Highlights](#highlights)
- [Quick start](#quick-start)
- [How to use it](#how-to-use-it)
  - [1. Add the plugin and tune it](#1-add-the-plugin-and-tune-it)
  - [2. Turn voice on](#2-turn-voice-on)
  - [3. Voice activity or push-to-talk](#3-voice-activity-or-push-to-talk)
  - [4. Send and receive frames (any networking)](#4-send-and-receive-frames-any-networking)
  - [5. Positional voice: speakers and the listener](#5-positional-voice-speakers-and-the-listener)
  - [6. Who is talking](#6-who-is-talking)
  - [7. A settings screen: devices, meter, gains, mic test](#7-a-settings-screen-devices-meter-gains-mic-test)
  - [8. Filters](#8-filters)
  - [9. Audio devices: real, WAV file, none, your own](#9-audio-devices-real-wav-file-none-your-own)
  - [10. Stats and diagnostics](#10-stats-and-diagnostics)
  - [11. The replicon transport (feature `replicon`)](#11-the-replicon-transport-feature-replicon)
  - [12. System order](#12-system-order)
  - [13. Choosing a codec (ADPCM or Opus)](#13-choosing-a-codec-adpcm-or-opus)
- [How it works](#how-it-works)
- [API reference](#api-reference)
- [Cargo features](#cargo-features)
- [Compatibility](#compatibility)
- [Examples](#examples)
- [Limitations and FAQ](#limitations-and-faq)
- [License](#license)
- [Contributing](#contributing)

## Highlights

- **One plugin, ECS-shaped API**: resources you write (`VoiceInput`, `VoiceChatConfig`,
  `VoiceFilters`), resources you read (`VoiceDevices`, `VoiceChatState`, `VoiceActivity`,
  `VoiceStats`), two messages (`OutgoingVoice`, `IncomingVoice`), two components (`VoiceSpeaker`,
  `VoiceListener`) and a public `SystemSet` to order your systems against.
- **Any networking**: frames are plain messages. Relays get packet checks and a rate limiter.
- **Voice activity** (threshold + hangover + pre-roll so word starts and ends are not clipped) or
  **push-to-talk** (the crate knows no keys; you set a "talk held" flag from your own input).
- **Filter hooks** on both sides: your own voice before it is encoded (everyone hears it, including
  your mic test) and each remote voice before it is mixed (only you hear it).
- **Proximity or global hearing**: distance falloff with a configurable range, full-volume radius,
  rolloff and pan, per-speaker range overrides, or everyone at full volume.
- **Its own output stream on a chosen device** (Bevy's audio cannot pick a device), device lists,
  a live mic level for a meter, a loopback mic test, a WAV file as a stand-in microphone.
- **Pure Rust, no native library to ship**: IMA-ADPCM (~66 kbps per talking speaker, the
  default, zero extra dependencies, near-zero CPU) or, with the `opus` feature, Opus (24 kbps by
  default: about 2.5x less bandwidth for bigger lobbies and weak uploads). Receivers play both, so
  players on different codecs hear each other, and the codec can be changed at runtime.
- **Fail closed, never panic**: no microphone, an unplugged headset or a driver error becomes an
  observable state; audio threads only touch atomics and bounded channels.
- **Tested without hardware**: every device goes through a trait, the tests use fakes.

## Quick start

Add the crate:

```toml
[dependencies]
bevy = "0.19.0"
bevy_voice_chat = "0.2"
# or, with the optional low-bandwidth Opus codec:
# bevy_voice_chat = { version = "0.2", features = ["opus"] }
```

An open-mic voice chat whose "network" is an echo: every frame you send comes straight back as
another speaker, so you hear yourself through the whole pipeline. Use headphones.

```rust,no_run
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;

fn main() {
    App::new()
        .add_plugins((DefaultPlugins, VoiceChatPlugin::default()))
        .add_systems(Update, join_voice.before(VoiceChatSystems::Devices))
        .add_systems(Update, echo_network.after(VoiceChatSystems::Capture).before(VoiceChatSystems::Playback))
        .run();
}

/// Turn voice on (a real game does this while the player is in a multiplayer session).
fn join_voice(mut input: ResMut<VoiceInput>) {
    input.enabled = true;
    input.mode = TalkMode::VoiceActivity;
    input.threshold = 0.05;
    // No positions yet: every voice at full volume, centred.
    input.positional = false;
}

/// Your networking goes here: send each `OutgoingVoice`, and turn what arrives into `IncomingVoice`
/// with the sender's id. This one echoes your own frames back as speaker 1.
fn echo_network(mut outgoing: MessageReader<OutgoingVoice>, mut incoming: MessageWriter<IncomingVoice>) {
    for f in outgoing.read() {
        incoming.write(IncomingVoice { speaker: SpeakerId(1), seq: f.seq, ts: f.ts, codec: f.codec, frame: f.frame.clone() });
    }
}
```

The same program is `cargo run --example quick_start`.

## How to use it

Everything below uses `use bevy::prelude::*; use bevy_voice_chat::prelude::*;`.

### 1. Add the plugin and tune it

`VoiceChatPlugin { config }` inserts every resource and system. `config` becomes the
`VoiceChatConfig` resource, which you may replace or edit at any time (jitter sizes apply to
speakers that start talking afterwards; everything else applies at once).

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;

fn main() {
    let config = VoiceChatConfig { hearing: Hearing::Proximity, range: 30.0, full_volume_distance: 3.0, ..Default::default() };
    assert!(config.problems().is_empty(), "{:?}", config.problems());
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, VoiceChatPlugin { config }));
}
```

`VoiceChatConfig` is `serde::Deserialize` with defaults for missing fields, so the numbers can live
in your own data files (RON, JSON, TOML, ...):

```rust
use bevy_voice_chat::prelude::*;

let config: VoiceChatConfig = ron::from_str("(hearing: Global, jitter_target_ms: 80.0)").expect("valid");
assert_eq!(config.hearing, Hearing::Global);

// The codec and its settings (a partial `opus` block keeps the other defaults).
let opus: VoiceChatConfig = ron::from_str("(codec: Opus, opus: (bitrate_bps: 32000))").expect("valid");
assert_eq!((opus.codec, opus.opus.bitrate_bps, opus.opus.complexity), (VoiceCodecChoice::Opus, 32_000, 5));
```

| field | default | meaning |
|---|---|---|
| `hearing` | `Proximity` | `Proximity` (distance + pan) or `Global` (everyone at full volume) |
| `range` | `20.0` | Proximity: silent at and beyond this distance (world units) |
| `full_volume_distance` | `2.0` | Proximity: full volume up to this distance |
| `rolloff` | `1.5` | falloff exponent between the two (1 = linear) |
| `pan_strength` | `0.6` | 0 = mono, 1 = a voice hard to one side is silent in the other ear |
| `jitter_target_ms` | `60.0` | buffered before a talk spurt starts playing |
| `jitter_max_ms` | `240.0` | most audio kept per speaker |
| `conceal_max_ms` | `60.0` | a lost / late frame is concealed or waited for this long |
| `vad_hangover_ms` | `300.0` | voice activity: keep sending this long after the level drops |
| `vad_preroll_ms` | `40.0` | voice activity: frames from just before the threshold crossing go too |
| `ptt_release_ms` | `120.0` | push-to-talk: keep sending this long after release |
| `output_queue_ms` | `40.0` | mixed audio queued ahead of the output device |
| `mic_backlog_frames` | `5` | captured frames processed per game frame; older ones (a stall) are dropped |
| `speaker_timeout_secs` | `5.0` | a silent speaker's buffer is forgotten after this |
| `heard_hold_ms` | `250.0` | `VoiceActivity`: a speaker counts as heard this long after its last frame |
| `codec` | `ImaAdpcm` | what this player sends with; `Opus` needs the `opus` feature ([section 13](#13-choosing-a-codec-adpcm-or-opus)) |
| `opus.bitrate_bps` | `24000` | Opus bitrate, `6000..=64000` (16000 = low bandwidth, 32000 = quality) |
| `opus.complexity` | `5` | Opus encoder effort `0..=10` (CPU per frame) |
| `opus.vbr` | `false` | `false` = constant bitrate, `true` = variable, capped at the bitrate |

`VoiceChatConfig::problems()` lists every invalid value (empty = fine), including invalid Opus
settings and `codec: Opus` in a build without the `opus` feature.

### 2. Turn voice on

`VoiceInput` is what your game wants right now. Write it every frame (or on change), ordered
`.before(VoiceChatSystems::Devices)`. Devices open and close from it.

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;

#[derive(Resource)]
struct Lobby { connected: bool, in_match: bool }

fn drive_voice(lobby: Res<Lobby>, mut input: ResMut<VoiceInput>) {
    input.enabled = lobby.connected; // mic + output open, frames go out
    input.positional = lobby.in_match; // distance applies in the match; menus hear everyone fully
    input.volume = 0.8; // voice volume 0..=1 (fold your master volume into it)
}
```

| `VoiceInput` field | default | meaning |
|---|---|---|
| `enabled` | `false` | in a session: microphone and output open, frames go out as `OutgoingVoice` |
| `meter` | `false` | keep the microphone open for a level meter only; nothing is sent |
| `loopback` | `false` | mic test: your voice plays back to you through the whole pipeline; nothing is sent |
| `mode` | `VoiceActivity` | `TalkMode::VoiceActivity` or `TalkMode::PushToTalk` |
| `threshold` | `0.1` | voice activity threshold, `0..=1` of full scale after `mic_gain` |
| `talk_held` | `false` | push-to-talk: your talk input is held |
| `mic_gain` | `1.0` | microphone gain (clamped to `0..=4`) |
| `volume` | `1.0` | voice output volume `0..=1` |
| `input_device` | `None` | microphone by label, one of `VoiceDevices::inputs` (`None` or a missing label = system default) |
| `output_device` | `None` | output device by label, one of `VoiceDevices::outputs` (`None` or a missing label = system default) |
| `positional` | `true` | distance applies right now (see [section 5](#5-positional-voice-speakers-and-the-listener)) |

> With the default `Hearing::Proximity` and `positional: true`, a remote voice is only heard when
> both a `VoiceListener` and that speaker's `VoiceSpeaker` exist (a voice that cannot be placed is
> silent). Set `positional = false` or `hearing = Global` until your entities are in place.

### 3. Voice activity or push-to-talk

Voice activity sends while the mic level (after `mic_gain`) is at or over `threshold`, plus a
hangover and a short pre-roll. Push-to-talk sends while `talk_held` is true, plus a release tail.
The crate knows no keys, so any input system works:

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;

fn push_to_talk(keys: Res<ButtonInput<KeyCode>>, mut input: ResMut<VoiceInput>) {
    input.mode = TalkMode::PushToTalk;
    input.talk_held = keys.pressed(KeyCode::KeyV);
}
```

`VoiceChatState::transmitting` (and `VoiceActivity::transmitting`) says whether your voice goes
out right now, for a "you are talking" indicator.

### 4. Send and receive frames (any networking)

Each 20 ms frame of your voice is one `OutgoingVoice { seq, ts, codec, frame }` message, written
in `VoiceChatSystems::Capture`. Send it **unreliably** (a late voice frame is worthless) and
stamp it with the sender's id on the way. On arrival, write an `IncomingVoice` with that id before
`VoiceChatSystems::Playback`. Never feed a player's own frames back to them.

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;

/// Stand-in for your networking layer.
#[derive(Resource, Default)]
struct MyNet {
    inbox: Vec<(u64, OutgoingVoice)>,
}

impl MyNet {
    fn send_unreliable(&mut self, _frame: &OutgoingVoice) { /* serialize + send */ }
}

fn send_voice(mut outgoing: MessageReader<OutgoingVoice>, mut net: ResMut<MyNet>) {
    for frame in outgoing.read() {
        net.send_unreliable(frame);
    }
}

fn receive_voice(mut net: ResMut<MyNet>, mut incoming: MessageWriter<IncomingVoice>) {
    for (sender, f) in net.inbox.drain(..) {
        incoming.write(IncomingVoice { speaker: SpeakerId(sender), seq: f.seq, ts: f.ts, codec: f.codec, frame: f.frame });
    }
}

fn main() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, VoiceChatPlugin::default()))
        .init_resource::<MyNet>()
        .add_systems(Update, (send_voice, receive_voice).chain().after(VoiceChatSystems::Capture).before(VoiceChatSystems::Playback));
}
```

Both messages are `Serialize + Deserialize`. With the default codec (IMA-ADPCM) one frame is 164
bytes of payload and a whole `OutgoingVoice` is at most ~177 bytes in a compact format such as
postcard; with Opus at its default 24 kbps a frame is 60 bytes (~73 bytes per message).

**Relaying through a host or server?** Check each packet before passing it on: the sender must be
known, the frame must be a well-formed frame of a known codec (`validate_packet` knows IMA-ADPCM
and Opus, and needs no decoder, so a relay built without the `opus` feature still forwards Opus),
and each speaker gets a packet budget (a talking player sends 50 per second):

```rust
use bevy_voice_chat::packet::{validate_packet, TokenBucket};

/// `bucket` is this speaker's; call `bucket.refill(delta_seconds)` once per frame.
fn accept(bucket: &mut TokenBucket, codec: u8, frame: &[u8]) -> bool {
    validate_packet(codec, frame).is_ok() && bucket.take()
}

let mut bucket = TokenBucket::new(75.0, 15.0); // 75 packets/s, bursts of 15
assert!(!accept(&mut bucket, 1, &[0; 10]), "not one IMA-ADPCM frame");
```

### 5. Positional voice: speakers and the listener

Put a `VoiceSpeaker` on every entity a remote voice comes from (its `GlobalTransform` is the
position) and one `VoiceListener` on the entity this player hears from. `right_from` takes the
left/right orientation from another entity, usually the camera.

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;

#[derive(Component)]
struct Player { id: u64, local: bool }

#[derive(Component)]
struct MainCamera;

fn mark_voices(mut commands: Commands, players: Query<(Entity, &Player), Added<Player>>, camera: Query<Entity, With<MainCamera>>) {
    for (entity, player) in &players {
        if player.local {
            commands.entity(entity).insert(VoiceListener { right_from: camera.iter().next() });
        } else {
            // `range: Some(40.0)` would make this one speaker carry further (a megaphone).
            commands.entity(entity).insert(VoiceSpeaker { id: SpeakerId(player.id), range: None });
        }
    }
}
```

The gain is decided in one place, `spatial::voice_gains`:

- `Hearing::Global`, or `VoiceInput::positional == false`: every voice at full volume, centred.
- `Hearing::Proximity`: 1 up to `full_volume_distance`, 0 at and beyond `range` (or the speaker's
  own `range`), `((range - d) / (range - full)) ^ rolloff` between; the ear facing away from the
  speaker is lowered by up to `pan_strength` (the near ear stays at 1).
- A voice without a listener or without its speaker entity is silent (fail closed).

Changing `hearing` at runtime is instant:

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;

fn party_call(mut config: ResMut<VoiceChatConfig>) {
    config.hearing = Hearing::Global;
}
```

### 6. Who is talking

`VoiceActivity` (read after `VoiceChatSystems::Playback`) lists the remote speakers heard right now
(a real frame of theirs played within `heard_hold_ms` and their gain is not zero) and whether this
player is transmitting. Nothing extra is networked: it is derived from the frames played.

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;

/// Your own marker for a "speaking" icon over each player.
#[derive(Component)]
struct SpeakingIcon { shown: bool }

fn speaking_icons(activity: Res<VoiceActivity>, mut icons: Query<(&VoiceSpeaker, &mut SpeakingIcon)>) {
    if !activity.is_changed() {
        return;
    }
    for (speaker, mut icon) in &mut icons {
        icon.shown = activity.heard.contains(&speaker.id);
    }
}
```

### 7. A settings screen: devices, meter, gains, mic test

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;

/// When the audio settings open: refresh the device lists (a headset plugged in since shows up).
fn on_settings_opened(mut rescan: MessageWriter<RescanDevices>) {
    rescan.write(RescanDevices);
}

/// While the audio settings are visible.
fn audio_settings(devices: Res<VoiceDevices>, state: Res<VoiceChatState>, mut input: ResMut<VoiceInput>) {
    input.meter = true; // keep the microphone open for the level meter
    if devices.scanned {
        let _mics: &[String] = &devices.inputs; // choices for a dropdown (+ devices.outputs)
    }
    input.input_device = None; // or Some(label): a label that disappeared falls back to the default
    input.mic_gain = 1.5;
    input.threshold = 0.1;
    // The meter: `mic_level` is on the same 0..=1 scale as `threshold` (after `mic_gain`).
    let _would_send = state.mic_level >= input.threshold;
    // A "test mic" toggle: hear yourself through filters, codec and jitter buffer.
    input.loopback = true;
    match &state.mic {
        DeviceState::Closed | DeviceState::Opening => {}
        DeviceState::Live(name) => { let _ = name; }
        DeviceState::Failed => { /* "No microphone found" */ }
    }
    // Your voice cannot be sent (e.g. `codec: Opus` in a build without the `opus` feature).
    if let Some(reason) = &state.codec_error {
        let _ = reason; // show it: "Voice chat: {reason}"
    }
}
```

In push-to-talk mode a menu usually cannot hold the talk key; set `talk_held = true` while the mic
test runs so the player hears everything. `VoiceChatState` also reports the output device
(`output`), the microphone's native format (`mic_format`), the output rate (`output_rate`) and what
the microphone is (`mic_kind`, e.g. a WAV file), plus the codec you send with (`send_codec`) and,
when your voice cannot be sent, why (`codec_error`).

### 8. Filters

Two chains of processors, each a `VoiceFilter` with an `order` (lower runs first):

- `FilterStage::Outgoing`: this player's own voice after the mic gain, before the encoder. Runs on
  every captured frame while the microphone is open (stateful filters see a continuous signal), so
  the mic test sounds exactly like what the others hear. Use it for voice changers.
- `FilterStage::Incoming`: each remote voice before it is mixed; `FilterContext::speaker` says
  whose. Only this player hears the result. Use it for per-speaker effects (a radio crackle).

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;

/// A ring modulator: the classic robot voice.
struct Robot { hz: f32, phase: f32 }

impl VoiceFilter for Robot {
    fn name(&self) -> &str {
        "robot"
    }

    fn process(&mut self, frame: &mut [f32], ctx: &FilterContext) {
        let step = std::f32::consts::TAU * self.hz / ctx.sample_rate as f32;
        for s in frame.iter_mut() {
            *s *= self.phase.sin();
            self.phase = (self.phase + step) % std::f32::consts::TAU;
        }
    }

    fn reset(&mut self) {
        self.phase = 0.0;
    }
}

fn setup_filters(mut filters: ResMut<VoiceFilters>) {
    filters.add(FilterStage::Outgoing, 100, Robot { hz: 50.0, phase: 0.0 });
    // A closure works too.
    filters.add_fn(FilterStage::Incoming, 0, "quieter", |frame, ctx| {
        if ctx.speaker == Some(SpeakerId(7)) {
            frame.iter_mut().for_each(|s| *s *= 0.5);
        }
    });
}

/// Tune your filter from game state every frame.
fn tune_robot(time: Res<Time>, mut filters: ResMut<VoiceFilters>) {
    if let Some(robot) = filters.get_mut::<Robot>() {
        robot.hz = 40.0 + 20.0 * time.elapsed_secs().sin();
    }
}
```

`remove::<F>()` drops every filter of a type, `names(stage)` lists a chain, `is_empty(stage)` says
it is a pass-through. After every filter the frame is cleaned (NaN becomes 0, clamped to `-1..=1`),
so a misbehaving filter cannot break the encoder or anyone's ears. `FilterContext` also carries
`transmitting` (the frame goes out this time) and `loopback` (the mic test).

### 9. Audio devices: real, WAV file, none, your own

All hardware goes through the `AudioIo` trait inside the `VoiceIo` resource:

- `CpalIo` (default): real devices through cpal, each on its own thread.
  **Device labels:** `VoiceDevices` lists one label per device, and `VoiceInput::input_device` /
  `output_device` take one of them (store the label the player picked).
  - **Windows:** the full friendly name from the Sound settings ("Microphone (USB PnP Audio
    Device)"), so two devices Windows both calls "Microphone" are two choices. Every device is
    listed; if two still share a label, the later ones get " #2", " #3" (never a label another
    device really has). "#N" follows the order Windows lists devices in, which can change after a
    replug or a reboot, so with two identical devices "X #2" may later open the other one.
  - **macOS and other platforms:** the name the OS reports; two devices with the same name are
    numbered the same way.
  - **Linux / BSD:** the name ALSA reports. ALSA lists one sound card once per mode under the same
    name, so identical names are merged into one choice (as in 0.2.0).
  - **Settings saved by 0.1 / 0.2.0** stored the plain name ("Microphone"); it still opens the first
    device with that name. A label that is no longer present (unplugged, renamed) falls back to the
    system default with a warning.
- `WavFileIo`: a WAV file (PCM 8/16/24/32-bit or float, any rate / channels) played in a loop as
  the microphone; output and device lists still go to the inner seam. Handy for testing two game
  instances on one machine.
- `NullIo`: no devices at all (dedicated servers, CI). The microphone and output report `Failed`.

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;
use std::sync::Arc;

fn use_a_wav_as_the_microphone(app: &mut App, path: &std::path::Path) {
    match WavFileIo::load(path, Arc::new(CpalIo)) {
        Ok(wav) => {
            app.insert_resource(VoiceIo(Arc::new(wav)));
        }
        Err(e) => warn!("{e}"),
    }
}

fn headless_server(app: &mut App) {
    app.insert_resource(VoiceIo(Arc::new(NullIo)));
}
```

Your own `AudioIo` (a fake for tests, another audio backend) creates sessions with
`MicSession::new` / `OutputSession::new`, reports state through the shared atomics, and pushes
16 kHz mono frames (or pulls stereo frames) through the bounded channels. Dropping a session
disconnects its stop channel, which is how the producer thread knows to close the device.

```rust
use bevy_voice_chat::codec::FRAME;
use bevy_voice_chat::io::{AudioIo, DeviceLists, MicSession, OutputSession, ScanJob, DEVICE_LIVE};

/// A microphone that sends silence and an output that discards everything.
struct SilentIo;

impl AudioIo for SilentIo {
    fn start_scan(&self) -> ScanJob {
        let job = ScanJob::default();
        job.deliver(DeviceLists { inputs: vec!["Silence".into()], outputs: vec!["Void".into()] });
        job
    }

    fn open_mic(&self, device: Option<String>) -> MicSession {
        let (session, frames, stop) = MicSession::new(device);
        session.shared.set_state(DEVICE_LIVE);
        std::thread::spawn(move || {
            // Until the session is dropped: one frame every 20 ms.
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(std::time::Duration::from_millis(20)) {
                let _ = frames.try_send([0.0; FRAME]);
            }
        });
        session
    }

    fn open_output(&self, device: Option<String>) -> OutputSession {
        let (session, frames, stop) = OutputSession::new(device);
        session.shared.set_state(DEVICE_LIVE);
        let shared = session.shared.clone();
        std::thread::spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stop.recv_timeout(std::time::Duration::from_millis(20)) {
                while frames.try_recv().is_ok() {
                    shared.consumed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
        session
    }
}
```

### 10. Stats and diagnostics

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;
use bevy_voice_chat::{Sessions, VoiceRuntime};

fn voice_debug_line(stats: Res<VoiceStats>, runtime: Res<VoiceRuntime>, sessions: Res<Sessions>) {
    let (jitter, depth) = runtime.mixer().totals();
    info!(
        "voice: out {} pkt / {} B, in {} pkt, rejected {}, unsupported {}, encode errors {}, played {}, lost {}, late {}, buffered {} frames, output queue {} frames, underruns {}, mic latency {:?} ms, sending with {:?}",
        stats.packets_out,
        stats.bytes_out,
        stats.packets_in,
        stats.rejected,
        stats.unsupported,
        stats.encode_errors,
        jitter.played,
        jitter.lost,
        jitter.late,
        depth,
        runtime.output_queued(&sessions),
        stats.underruns,
        sessions.mic_latency_ms(),
        // None = sending is off; `runtime.codec_error()` says why.
        runtime.send_codec().map(|c| (c.name(), c.bitrate())),
    );
}
```

`VoiceStats` counts packets and bytes out, packets in, rejected packets, packets of a codec this
build cannot decode (`unsupported`), frames the encoder failed on (`encode_errors`), mic-test
frames, mixed frames, output underruns and stale microphone frames. Per speaker,
`runtime.mixer().channel(key)` gives the jitter buffer (`stats`, `depth()`), the codec it last
played (`decoder_codec()`) and `latency_ms` (sender clock to mixed; meaningful when both run on
one machine).

### 11. The replicon transport (feature `replicon`)

```toml
[dependencies]
bevy_voice_chat = { version = "0.2", features = ["replicon"] }
```

`VoiceRepliconPlugin` sends your frames as a client message (`VoiceUp`) on replicon's unreliable
channel. The server checks each one (a known sender, a well-formed frame of any known codec with
`validate_packet`, your `VoiceRelayHook`, a per-speaker budget) and relays it as a server message
(`VoiceDown`) to everyone except the speaker, the listen-server host included, where it becomes
`IncomingVoice`. The host's own voice takes the same path. The relay never decodes, so a host
built without the `opus` feature still relays Opus between players who have it.

You say who is who: put `VoiceSenderId` on each client's connection entity on the server (the
entity with replicon's `ConnectedClient`) and set `HostVoiceSpeaker` for a listen server's own
voice. Use the same ids for your `VoiceSpeaker` components. A sender without an id is dropped.

```rust
use bevy::prelude::*;
use bevy_replicon::prelude::*;
use bevy_voice_chat::prelude::*;

fn main() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, bevy::state::app::StatesPlugin, RepliconPlugins, VoiceChatPlugin::default()))
        // After RepliconPlugins, on every peer, in the same order (the protocol must match).
        .add_plugins(VoiceRepliconPlugin { max_packets_per_sec: 75.0, burst: 15.0 })
        .insert_resource(HostVoiceSpeaker(Some(SpeakerId(0))))
        // Your own rule on top: here, speaker 13 is muted for everyone.
        .insert_resource(VoiceRelayHook::new(|speaker, _packet| speaker != SpeakerId(13)))
        .add_systems(Update, tag_new_clients);
}

/// Server: give each new client the speaker id your players use (here a simple counter).
fn tag_new_clients(mut commands: Commands, new: Query<Entity, Added<ConnectedClient>>, mut next: Local<u64>) {
    for client in &new {
        *next += 1;
        commands.entity(client).insert(VoiceSenderId(SpeakerId(*next)));
    }
}
```

`VoiceRelayStats` counts relayed packets and each drop reason. Messages are registered on every
peer regardless of role, so the protocol hash always matches.

### 12. System order

The crate's systems run in `Update`, chained:

```text
VoiceChatSystems::Devices  ->  VoiceChatSystems::Capture  ->  VoiceChatSystems::Playback
```

| your system | order it |
|---|---|
| writes `VoiceInput` | `.before(VoiceChatSystems::Devices)` |
| reads `OutgoingVoice` (send) | `.after(VoiceChatSystems::Capture)` |
| writes `IncomingVoice` (receive) | `.before(VoiceChatSystems::Playback)` |
| reads `VoiceActivity`, `VoiceChatState`, `VoiceStats` | `.after(VoiceChatSystems::Playback)` |

The replicon transport runs in `VoiceTransportSystems`, between `Capture` and `Playback`.

### 13. Choosing a codec (ADPCM or Opus)

IMA-ADPCM is the default and always built in. Opus is the optional **low-bandwidth** codec: turn
on the `opus` cargo feature, then choose it in the config.

```toml
[dependencies]
bevy_voice_chat = { version = "0.2", features = ["opus"] }
```

```rust
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;

fn main() {
    // `OpusSettings::LOW_BANDWIDTH` (16 kbps) and `OpusSettings::QUALITY` (32 kbps) are presets.
    let config = VoiceChatConfig { codec: VoiceCodecChoice::Opus, opus: OpusSettings { bitrate_bps: 24_000, ..Default::default() }, ..Default::default() };
    // Without the `opus` feature this lists "codec: Opus needs the `opus` cargo feature".
    for problem in config.problems() {
        warn!("voice config: {problem}");
    }
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, VoiceChatPlugin { config }));
}

/// A settings screen can switch at runtime: the next frame you send uses the new codec.
fn use_low_bandwidth_voice(mut config: ResMut<VoiceChatConfig>) {
    config.codec = VoiceCodecChoice::Opus;
    config.opus = OpusSettings::LOW_BANDWIDTH;
}
```

| | IMA-ADPCM (default) | Opus 16 kbps | **Opus 24 kbps (default)** | Opus 32 kbps |
|---|---|---|---|---|
| bytes per 20 ms frame | 164 | 40 | 60 | 80 |
| payload per talking player | 65.6 kbps | 16 kbps | 24 kbps | 32 kbps |
| roughly on the wire (+ UDP and transport headers, estimated) | ~86 kbps | ~36 kbps | ~44 kbps | ~52 kbps |
| speech quality, objective proxy (lower = better) | 1.33 dB | 2.27 dB (measured at complexity 9) | 1.62 dB | 1.08 dB (complexity 9) |
| encode per frame (desktop CPU, release, complexity 5) | ~4 us | ~200 us | ~200 us | ~200 us |
| decode per frame, per speaker | ~1 us | ~7-10 us | ~7-10 us | ~7-10 us |
| codec memory per remote speaker | none (stateless) | ~173 KB | ~173 KB | ~173 KB |
| a lost packet | the last frame repeated at half level | Opus packet loss concealment | same | same |
| extra dependencies | none | `opus-rs` (pure Rust, no dependencies of its own) | same | same |

The quality row is a rough objective measure (log-spectral distance against the original, on a
real speech recording), not a listening test: at the default 24 kbps Opus is close to IMA-ADPCM
(slightly behind on this measure) for about 2.5x less bandwidth, and 32 kbps scored better than
IMA-ADPCM (measured at complexity 9; expect a little less at the default 5) while still using
about half the bandwidth. 16 kbps trades more quality for the smallest packets. Judge by ear
with the `mic_test` example ([Examples](#examples)).

How codecs behave together:

- **Receivers decode every codec compiled into them**, by the codec id in each packet, so a player
  on Opus and a player on IMA-ADPCM hear each other. A build **without** the `opus` feature drops
  Opus packets and counts them in `VoiceStats::unsupported` (with one warning in the log).
- **No automatic fallback.** If the chosen codec cannot run (`codec: Opus` without the feature,
  invalid `opus` settings, or an encoder that keeps failing), your voice is **not sent** and
  `VoiceChatState::codec_error` (also `VoiceRuntime::codec_error()`) says why; receiving keeps
  working. Check `VoiceChatConfig::problems()` up front. The crate never switches to another codec
  on its own.
- **Runtime change:** edit `VoiceChatConfig::codec` or `opus` and the next frame sent uses it (the
  encoder is rebuilt once, ~0.25 ms). Receivers switch per speaker as the packets change.
- **Relays** (`validate_packet`, the replicon transport) forward Opus even without the feature.
- IMA-ADPCM frames are decoded when they arrive; Opus frames are buffered encoded and decoded in
  order when they play (Opus decoding carries state from frame to frame).

## How it works

**Speaker side.** The capture thread converts the device's samples to f32, downmixes to mono,
low-passes and resamples to **16 kHz**, cuts **20 ms frames** (320 samples), publishes the peak for
the meter (an atomic) and pushes each frame into a bounded channel (25 frames; full = dropped and
counted). Each game frame takes at most `mic_backlog_frames` of them (a loading stall is stale
speech), applies `mic_gain`, runs the gate (voice activity or push-to-talk), the outgoing filters,
and encodes. Frames go out as `OutgoingVoice`, or in mic-test mode straight into this player's own
jitter buffer.

**Listener side.** `IncomingVoice` is validated (`validate_packet`) and put in that speaker's
**jitter buffer**: IMA-ADPCM frames are decoded on arrival (each one stands alone); Opus frames
are stored encoded and decoded in sequence order when they play, by that speaker's own decoder.
The buffer waits for `jitter_target_ms` before a talk spurt, plays in sequence order, drops late
and duplicate frames, conceals a lost frame (IMA-ADPCM: the previous one at half level; Opus: its
own packet loss concealment), waits on an underflow (a frame arriving a little late still plays),
fades the end of a spurt, trims one frame when the depth stayed above target for a second, and
never holds more than `jitter_max_ms`. A speaker who switches codec gets a fresh decoder.

**Mixing** runs on the output device's clock: the app keeps `output_queue_ms` of mixed 16 kHz stereo
queued ahead of the device; each mixed frame pulls every speaker, runs the incoming filters,
applies its spatial gain (ramped across the frame, no zipper noise), sums, applies `volume` and
soft-clips (transparent under 0.8). The output callback resamples to the device rate and plays
silence when the queue is empty.

**Latency.** One capture frame (20 ms) + the network + the jitter target (60 ms) + the output queue
(40 ms) + the device (~10-30 ms): around 100-150 ms mouth to ear on a LAN.

## API reference

The full documentation is generated with `cargo doc --open --all-features`. Every public item:

### Crate root (`bevy_voice_chat`)

| item | kind | what / example |
|---|---|---|
| `VoiceChatPlugin { config }` | plugin | `app.add_plugins(VoiceChatPlugin::default())` |
| `VoiceChatSystems::{Devices, Capture, Playback}` | system sets | `my_system.before(VoiceChatSystems::Devices)` |
| `VoiceInput` | resource (you write) | `input.enabled = true;` ([fields](#2-turn-voice-on)) |
| `TalkMode::{VoiceActivity, PushToTalk}` | enum | `input.mode = TalkMode::PushToTalk;` |
| `VoiceChatConfig` | resource (you write) | `config.range = 30.0;` ([fields](#1-add-the-plugin-and-tune-it)); `config.codec = VoiceCodecChoice::Opus;` |
| `VoiceChatConfig::problems()` | fn | `assert!(cfg.problems().is_empty())` |
| `VoiceChatConfig::jitter()` / `spatial()` / `output_queue_frames()` | fn | the derived `JitterConfig`, `SpatialParams`, queue depth in frames |
| `RescanDevices` | message (you write) | `rescan.write(RescanDevices);` |
| `VoiceDevices { scanned, inputs, outputs }` | resource (read) | device labels: `for label in &devices.inputs { .. }` |
| `DeviceState::{Closed, Opening, Live(name), Failed}` | enum | `state.mic == DeviceState::Failed` |
| `VoiceChatState { mic, output, mic_level, transmitting, mic_format, output_rate, mic_kind, send_codec, codec_error }` | resource (read) | `meter.set(state.mic_level)`; `if let Some(why) = &state.codec_error { .. }` |
| `VoiceActivity { heard, transmitting }` | resource (read) | `activity.heard.contains(&SpeakerId(2))` |
| `VoiceStats { packets_out, bytes_out, packets_in, rejected, unsupported, encode_errors, loopback, mixed, underruns, mic_dropped }` | resource (read) | `stats.packets_out` |
| `SpeakerId(u64)` | id | `SpeakerId(player_id)` |
| `OutgoingVoice { seq, ts, codec, frame }` | message (you read) | `for f in outgoing.read() { net.send(f) }` |
| `IncomingVoice { speaker, seq, ts, codec, frame }` | message (you write) | `incoming.write(IncomingVoice { speaker, .. })` |
| `VoiceSpeaker { id, range }` | component | `VoiceSpeaker { id: SpeakerId(2), range: None }` |
| `VoiceListener { right_from }` | component | `VoiceListener { right_from: Some(camera) }` |
| `VoiceIo(Arc<dyn AudioIo>)` | resource | `VoiceIo(Arc::new(NullIo))` |
| `Sessions` | resource (read) | `sessions.mic()`, `output()`, `mic_latency_ms()`, `output_latency_ms()`, `output_failed()` |
| `VoiceRuntime` | resource (read) | `runtime.send_codec()` (`None` = sending is off), `codec_error()`, `mixer()`, `output_queued(&sessions)`; `VoiceRuntime::new(&config)` |
| `wall_ms()` | fn | the wall clock in ms (wrapping `u32`), what `OutgoingVoice::ts` carries |
| `prelude` | module | `use bevy_voice_chat::prelude::*;` |

### `codec`

| item | what / example |
|---|---|
| `VOICE_RATE` (16 000), `FRAME` (320), `FRAME_MS` (20) | the wire format: 20 ms of 16 kHz mono per frame |
| `MAX_VOICE_BYTES` (700) | no accepted packet is larger |
| `OPUS_ID` (3) | the wire id of Opus packets (defined in every build) |
| `MonoFrame` | `[f32; FRAME]` |
| `VoiceCodecChoice::{ImaAdpcm, Opus}` | what this player sends with (`config.codec`); in the prelude |
| `OpusSettings { bitrate_bps, complexity, vbr }` | `config.opus`; `problems()`, `frame_bytes()`, presets `LOW_BANDWIDTH` / `QUALITY`, limits `MIN_BITRATE` / `MAX_BITRATE` / `MAX_COMPLEXITY`; in the prelude |
| `VoiceCodec` trait | `id()`, `name()`, `max_frame_bytes()`, `fixed_frame_bytes()`, `bitrate()`, `encode(&pcm, &mut out) -> Result`, `decode(&bytes, &mut pcm) -> Result` (takes `&mut self`), `is_stateful()`, `conceal(next, &mut pcm) -> bool`, `reset()` |
| `ImaAdpcm` (`ID` = 1, `FRAME_BYTES` = 164, `MAX_STEP_INDEX` = 88) | the default: 65.6 kbps; `ImaAdpcm::default().encode(&frame, &mut bytes)?` |
| `Opus` (`ID` = 3; feature `opus`) | `Opus::encoder(settings)?`, `Opus::decoder()?`, `settings()`; SILK wideband via `opus-rs` |
| `Pcm16` (`ID` = 2, `FRAME_BYTES` = 640) | a local reference codec (256 kbps), never accepted on the wire |
| `CodecError::{WrongLength { got, want }, BadHeader, NotCompiled, BadSettings(_), Backend(_), Panicked}` | a malformed packet or a codec that cannot run (`Display`) |
| `new_encoder(choice, &opus)` | `Result<Box<dyn VoiceCodec>, CodecError>`: never falls back to another codec |
| `new_decoder(id)` / `can_decode(id)` | a decoder for a wire id this build decodes / whether it does |
| `default_codec()` | `Box<dyn VoiceCodec>`: the default sending codec (IMA-ADPCM) |
| `to_i16(x)` / `from_i16(v)` | sample conversions (NaN -> 0) |

### `filter`

| item | what / example |
|---|---|
| `VoiceFilters` | resource: `add(stage, order, filter)`, `add_fn(stage, order, name, closure)`, `get_mut::<F>()`, `remove::<F>()`, `names(stage)`, `is_empty(stage)`, `run(&mut frame, &ctx)`, `reset()` |
| `VoiceFilter` trait | `name()`, `process(&mut frame, &ctx)`, `reset()` (default no-op) |
| `FilterStage::{Outgoing, Incoming}` | where a filter runs |
| `FilterContext { sample_rate, stage, speaker, transmitting, loopback }` | what a filter knows |
| `FnFilter<F>` | a closure as a filter (what `add_fn` creates) |
| `GainFilter(f32)` | multiply by a fixed gain: `filters.add(FilterStage::Incoming, 0, GainFilter(0.5))` |

### `spatial`

| item | what / example |
|---|---|
| `Hearing::{Proximity, Global}` | `config.hearing = Hearing::Global` |
| `voice_gains(&params, positional, listener, speaker, range)` | THE (left, right) gain of one voice |
| `spatial_gain(distance, full, range, rolloff)` | `spatial_gain(5.0, 2.0, 20.0, 1.5)` -> `0..=1` |
| `pan_gains(listener, right, speaker, strength)` | `(left, right)` pan multipliers |
| `SpatialParams { hearing, full, range, rolloff, pan_strength }` | from `config.spatial()` |
| `Listener { pos, right }` | the listener's position and right direction |

### `io` (the device seam)

| item | what / example |
|---|---|
| `AudioIo` trait | `start_scan()`, `open_mic(device)`, `open_output(device)`, `mic_kind()` |
| `NullIo` | no devices: `VoiceIo(Arc::new(NullIo))` |
| `DeviceLists { inputs, outputs }` | one scan's result |
| `ScanJob` | a scan in flight: `job.deliver(lists)` (worker), `job.take()` (app) |
| `MicSession { requested, shared, stop, frames }` | an open microphone: `MicSession::new(device)`, `drain(&mut out)`; dropping it closes the device |
| `MicShared` | `publish_peak`, `take_peak`, `set_state`, `state`, `device_name`, `status`; atomics `rate`, `channels`, `overflow`, `latency_us` |
| `OutputSession { requested, shared, frames, stop }` | an open output stream: `OutputSession::new(device)` |
| `OutputShared` | `consumed`, `rate`, `latency_us`; `state`, `set_state`, `device_name`, `status` |
| `DEVICE_OPENING`, `DEVICE_LIVE`, `DEVICE_FAILED` | device states: `shared.set_state(DEVICE_LIVE)` |
| `MIC_QUEUE` (25), `OUTPUT_QUEUE` (16) | bounded channel sizes in frames |
| `OutputPump` | the output callback's resampler: `OutputPump::new(rx, VOICE_RATE, device_rate, shared).fill(&mut out, channels)` |

### `cpal_io`, `wav`

| item | what / example |
|---|---|
| `cpal_io::CpalIo` | the real hardware (default `VoiceIo`) |
| `wav::WavFileIo { inner, name, samples }` | `WavFileIo::load(path, Arc::new(CpalIo))?` |
| `wav::parse_wav(&bytes)` | `-> Result<Wav, String>` (mono, channels averaged) |
| `wav::Wav { rate, channels, mono }` | a decoded file |
| `wav::to_voice_rate(&mono, rate)` | resample to 16 kHz |

### `packet` (for relays)

| item | what / example |
|---|---|
| `validate_packet(codec, &frame)` | THE check for every wire codec: `validate_packet(1, &[0; 164]) == Ok(())` |
| `opus_toc_ok(&frame)` | an Opus packet this crate accepts (SILK, 20 ms, mono, one frame) |
| `validate_frame(codec, len, want_codec, want_len)` | an exact-size check for a fixed-size codec: `validate_frame(1, 164, 1, 164) == Ok(())` |
| `Reject::{UnknownSender, WrongCodec, BadSize, Malformed, RateLimited}` | why a packet was dropped |
| `TokenBucket` | `TokenBucket::new(rate, burst)`, `refill(dt)`, `take() -> bool` |

### `jitter`, `mixer`, `gate`, `dsp` (building blocks, usable on their own)

| item | what / example |
|---|---|
| `jitter::JitterBuffer` | `new(cfg)`, `insert(seq, &frame, ts) -> Insert`, `pull(&mut out) -> Pull`, `depth()`, `is_playing()`, `is_idle()`, `stats` |
| `jitter::JitterCore<P>` | the same state machine over any payload: `insert(seq, payload, ts)`, `next_frame() -> Next`, `generation()` (bumped by a sender restart), `depth()`, `is_playing()`, `is_idle()`, `stats` |
| `jitter::Next::{Play { payload, ts }, Tail, Conceal { run, next }, Silent}` | what one `JitterCore::next_frame()` asks for |
| `jitter::JitterConfig { target, max, conceal_max }` | sizes in frames (`config.jitter()`) |
| `jitter::Pull::{Played { ts }, Concealed, Silent}`, `jitter::Insert::{Stored, Late, Duplicate}`, `jitter::JitterStats` (incl. `undecodable`) | results and counters |
| `mixer::Mixer` | `new`, `set_config`, `insert` (a decoded frame), `insert_packet` (an encoded packet, decoded at playout), `set_target`, `keys`, `channel`, `any_active`, `mix`, `heard`, `forget_idle`, `remove`, `clear`, `totals` |
| `mixer::Payload::{Pcm(frame), Encoded { codec, bytes }}`, `mixer::PacketJitter` | a buffered frame; `JitterCore<Payload>` |
| `mixer::SpeakerKey::{Remote(SpeakerId), Loopback}` | a mixer channel (`Loopback` = the mic test) |
| `mixer::SpeakerChannel { jitter, latency_ms, .. }` | one speaker's channel; `decoder_codec()` |
| `mixer::StereoFrame` | `[f32; FRAME * 2]`, interleaved L R |
| `gate::TalkGate` | `TalkGate::new(hangover_frames, release_frames).step(GateInput::PushToTalk { held: true })` |
| `gate::GateInput::{OpenMic { level, threshold }, PushToTalk { held }}` | one frame's gate input |
| `gate::ms_to_frames(ms, min)` | `ms_to_frames(300.0, 0) == 15` |
| `dsp::Biquad` | `Biquad::low_pass(7200.0, 48000.0).process(x)`, `Biquad::IDENTITY` |
| `dsp::Resampler` | push resampler: `Resampler::new(48000, 16000).push(x, \|y\| out.push(y))` |
| `dsp::PullResampler` | pull resampler for stereo output |
| `dsp::FrameAssembler` | interleaved device samples -> 16 kHz mono frames: `push_interleaved`, `push_with` |
| `dsp::peak`, `dsp::apply_gain`, `dsp::soft_clip` | helpers (NaN safe) |

### `replicon` (feature `replicon`)

| item | what / example |
|---|---|
| `VoiceRepliconPlugin { max_packets_per_sec, burst }` | `app.add_plugins(VoiceRepliconPlugin::default())` (75 / 15) |
| `VoiceUp { seq, ts, codec, frame }` | client message, unreliable |
| `VoiceDown { speaker, seq, ts, codec, frame }` | server message, unreliable, independent |
| `VoiceSenderId(SpeakerId)` | component on a client's connection entity (server) |
| `HostVoiceSpeaker(Option<SpeakerId>)` | resource: a listen server's own speaker id |
| `VoiceRelayHook` | resource: `VoiceRelayHook::new(\|speaker, packet\| true)` |
| `VoiceRelayStats { relayed, unknown_sender, invalid, rate_limited, refused }` | resource (server); `invalid` = unknown codec, bad size or malformed |
| `VoiceTransportSystems` | system set between `Capture` and `Playback` |
| `check_up(speaker, &up)` | the relay's first check (a known sender + `validate_packet`), for a custom relay |

## Cargo features

| feature | default | what it adds |
|---|---|---|
| `opus` | no | the Opus codec ([section 13](#13-choosing-a-codec-adpcm-or-opus)) via `opus-rs` 0.1.34: pure Rust, no build script, no dependencies of its own, BSD-3-Clause (see [License](#license)) |
| `replicon` | no | `VoiceRepliconPlugin` over `bevy_replicon` 0.44.2 (+ `bevy_state` for its run conditions) |

Without features the crate depends on Bevy's `bevy_app`, `bevy_ecs`, `bevy_math`, `bevy_time`,
`bevy_transform` (all without default features), `cpal`, `serde` and `tracing`. Without `opus`
the crate still understands Opus on the wire (validation, relaying, config), it just cannot
encode or play it.

## Compatibility

| bevy_voice_chat | Bevy | cpal | bevy_replicon (optional) | opus-rs (optional) | Rust |
|---|---|---|---|---|---|
| 0.2 | 0.19.0 | 0.17.3 | 0.44.2 | 0.1.34 | 1.95+ |
| 0.1 | 0.19.0 | 0.17.3 | 0.44.2 | - | 1.95+ |

A default (IMA-ADPCM) 0.2 player and a 0.1 player hear each other: the IMA-ADPCM wire format did
not change.

cpal 0.17.3 is the version Bevy 0.19's own audio (rodio) uses, so a game has one cpal. Platforms:
whatever cpal supports (Windows WASAPI, macOS CoreAudio, Linux ALSA; Linux builds need the ALSA
development package, e.g. `libasound2-dev`). The web is not supported yet.

## Examples

| example | what it shows | needs |
|---|---|---|
| `cargo run --example quick_start` | open mic + an echo "network": hear yourself through the whole pipeline | mic + headphones |
| `cargo run --example mic_test` | a settings-style mic test: device lists, live level, loopback, a robot filter (`-- --clean` without it); `-- path/to/file.wav` uses a WAV as the mic; with `--features opus`: `-- --opus [--kbps 16\|24\|32]` hears yourself through Opus, `-- --cycle` switches codec at runtime every 5 s | mic (or a WAV) + output |
| `cargo run --example device_check` | the raw device seam without Bevy: lists devices, records 0.5 s, plays a tone | a sound card |

## Limitations and FAQ

**Which codec should I use?** Start with the default, **IMA-ADPCM**: 164 bytes per 20 ms frame,
**~66 kbps per talking player** (roughly 86 kbps with UDP and transport headers, and only while
talking), no extra dependency, almost no CPU, and every frame decodes on its own (a lost packet
never corrupts the next). Choose **Opus** (the `opus` feature + `codec: Opus`) when bandwidth
matters: bigger lobbies, a player hosting on a weak upload, mobile connections. At its default
24 kbps it needs about 2.5x less bandwidth with similar speech quality; at 32 kbps it scored
better than IMA-ADPCM on an objective measure and still uses about half the bandwidth (the table
in [section 13](#13-choosing-a-codec-adpcm-or-opus) has the numbers). Opus hides a lost packet
with real loss concealment (it continues the voice) instead of repeating the last frame; on the
same objective measure IMA-ADPCM's simple repeat actually degraded a little less under 5-20 %
packet loss, so judge that by ear too.

**What does Opus cost?** About 200 us of CPU per 20 ms frame to encode at complexity 5 on a
desktop CPU (complexity 8-10 roughly doubles it, 0-2 roughly halves it), 7-10 us per frame per
speaker to decode, and ~173 KB of decoder state per remote speaker. With one encoder and eight
talking speakers that is about 1.3 % of one core, on the game thread. The Opus library
(`opus-rs`) is pure Rust but young and uses `unsafe` internally; this crate only hands it packets
that pass a header check, and every call runs inside `catch_unwind`, so a panic inside it becomes
an error (and a dropped decoder), not a crash. That cannot help a game built with
`panic = "abort"`, and it cannot catch undefined behaviour.

**Forward error correction, packet-loss tuning, DTX?** Not offered yet: the Opus library version
this crate uses cannot decode forward error correction, and its variable-bitrate and FEC modes
do not behave as documented, so the crate sticks to constant bitrate (or a strict per-frame cap
with `vbr: true`). DTX (sending nothing in silence) is covered by the voice activity gate and
push-to-talk already. These settings may come in a later version.

**Bandwidth for a host?** A listen-server host relays every talking voice to every other player
and sends its own voice to everyone. With everybody talking at once (the worst case) that is
`(players - 1)^2` streams of upload:

| players | IMA-ADPCM (~86 kbps each) | Opus 24 kbps (~44 kbps each) | Opus 16 kbps (~36 kbps each) |
|---|---|---|---|
| 2 | ~86 kbps | ~44 kbps | ~36 kbps |
| 4 | ~0.8 Mbps | ~0.4 Mbps | ~0.3 Mbps |
| 6 | ~2.2 Mbps | ~1.1 Mbps | ~0.9 Mbps |
| 8 | ~4.2 Mbps | ~2.2 Mbps | ~1.8 Mbps |

Usually only one or two players talk at a time: one talker in an 8-player lobby costs the host
about 7 x 86 = ~600 kbps with IMA-ADPCM, ~300 kbps with Opus 24 kbps. Voice activity and
push-to-talk keep it at zero while nobody talks. Each player downloads one stream per talking
player.

**I hear nothing.** Check, in order: `VoiceInput::enabled` is true on both sides;
`VoiceChatState::output` is `Live`; frames arrive (`VoiceStats::packets_in` grows, `rejected` does
not); with `Hearing::Proximity` and `positional: true` there is a `VoiceListener` and a
`VoiceSpeaker` with the sender's id within `range` (or set `positional = false`); `volume` is not 0.

**Does it use Bevy's audio?** No. Bevy's audio cannot choose an output device nor stream at low
latency, so voice plays on its own cpal stream. Music and effects keep playing through Bevy.

**Echo cancellation, noise suppression, automatic gain?** Not built in. The outgoing filter hook
is the place for them (a noise gate or suppressor runs there before encoding).

**Encryption?** Frames are whatever your transport makes of them; use an encrypted transport if
you need it.

**Is anything networked or saved by the crate?** No. It only produces and consumes messages; the
"who is heard" state is derived locally from the frames played. Captured audio is never written
anywhere by the crate.

**Other limitations:** mono voices only; Opus is used in its 16 kHz (wideband) voice mode only
and accepts only single-frame 20 ms mono packets; linear-interpolation resampling (fine for
speech); occlusion ("muffled through walls") is up to an incoming filter; the web (wasm) is not
supported.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

The optional `opus` feature compiles in [opus-rs](https://crates.io/crates/opus-rs), which is
licensed under the BSD-3-Clause licence (Copyright Xiph.Org Foundation and contributors, and
restsend.com), compatible with both licences above. A binary built with the `opus` feature must
include that notice (its `COPYING` file).

## Contributing

Issues and pull requests are welcome. Before opening a pull request, please run:

```text
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets -- -D warnings
cargo test --all-features
cargo test --features opus
cargo test --no-default-features
cargo doc --no-deps --all-features
cargo build --examples --all-features
```

Tests must not need audio hardware: go through `AudioIo` with a fake (see `src/app_tests.rs`).
Keep the README examples compiling (they are checked by `cargo test --all-features`) and add a
line to `CHANGELOG.md`.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without
any additional terms or conditions.
