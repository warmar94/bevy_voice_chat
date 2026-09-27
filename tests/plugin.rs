//! The public surface from a game's point of view: the plugin in a strict headless app (ambiguity
//! detection = Error), the game's systems ordered around the public sets, a machine without audio
//! devices (`NullIo`) — no test ever opens a real device.

use bevy::ecs::schedule::{LogLevel, ScheduleBuildSettings, ScheduleLabel};
use bevy::prelude::*;
use bevy_voice_chat::prelude::*;
use bevy_voice_chat::{Sessions, VoiceRuntime};
use std::sync::Arc;

#[derive(Resource, Default)]
struct Seen {
    outgoing: usize,
}

fn drive(mut input: ResMut<VoiceInput>) {
    input.enabled = true;
    input.mode = TalkMode::PushToTalk;
    input.talk_held = true;
}

fn send(mut out: MessageReader<OutgoingVoice>, mut seen: ResMut<Seen>) {
    seen.outgoing += out.read().count();
}

fn game() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, VoiceChatPlugin::default()))
        .insert_resource(VoiceIo(Arc::new(NullIo)))
        .init_resource::<Seen>()
        .add_systems(Update, drive.before(VoiceChatSystems::Devices))
        .add_systems(Update, send.after(VoiceChatSystems::Capture).before(VoiceChatSystems::Playback));
    for label in [PreUpdate.intern(), Update.intern(), PostUpdate.intern()] {
        app.edit_schedule(label, |s| {
            s.set_build_settings(ScheduleBuildSettings { ambiguity_detection: LogLevel::Error, ..default() });
        });
    }
    app
}

#[test]
fn no_audio_devices_is_an_observable_state_never_a_panic() {
    let mut app = game();
    for _ in 0..3 {
        app.update();
    }
    let devices = app.world().resource::<VoiceDevices>();
    assert!(devices.scanned && devices.inputs.is_empty() && devices.outputs.is_empty());
    let state = app.world().resource::<VoiceChatState>();
    assert_eq!((&state.mic, &state.output, state.transmitting), (&DeviceState::Failed, &DeviceState::Failed, false));
    assert_eq!(app.world().resource::<Seen>().outgoing, 0);
    let sessions = app.world().resource::<Sessions>();
    assert!(sessions.mic().is_some() && sessions.output_failed());
    assert_eq!((sessions.mic_latency_ms(), sessions.output_latency_ms()), (None, None));
    let rt = app.world().resource::<VoiceRuntime>();
    assert_eq!(rt.codec().bitrate(), 65_600, "IMA-ADPCM by default");
    assert!(rt.mixer().keys().is_empty());
    assert_eq!(rt.output_queued(sessions), 0);
}

#[test]
fn nothing_asked_opens_nothing() {
    let mut quiet = App::new();
    quiet.add_plugins((MinimalPlugins, VoiceChatPlugin::default())).insert_resource(VoiceIo(Arc::new(NullIo)));
    quiet.update();
    let sessions = quiet.world().resource::<Sessions>();
    assert!(sessions.mic().is_none() && sessions.output().is_none(), "nothing asked = nothing open");
    assert_eq!(quiet.world().resource::<VoiceChatState>().mic, DeviceState::Closed);
}
