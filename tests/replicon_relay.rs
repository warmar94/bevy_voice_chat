//! The `replicon` transport end to end, in-process: one listen server + two clients connected with
//! replicon's test helper (no sockets, no audio device — every app uses `NullIo`).

#![cfg(feature = "replicon")]

use bevy::ecs::schedule::{LogLevel, ScheduleBuildSettings};
use bevy::prelude::*;
use bevy::state::app::StatesPlugin;
use bevy_replicon::prelude::*;
use bevy_replicon::test_app::{ServerTestAppExt, TestClientEntity};
use bevy_voice_chat::codec::ImaAdpcm;
use bevy_voice_chat::prelude::*;
use std::sync::Arc;

/// Every IncomingVoice this peer produced.
#[derive(Resource, Default)]
struct Heard(Vec<IncomingVoice>);

fn collect(mut incoming: MessageReader<IncomingVoice>, mut heard: ResMut<Heard>) {
    heard.0.extend(incoming.read().cloned());
}

fn peer() -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, StatesPlugin, RepliconPlugins.set(ServerPlugin::new(PostUpdate)), VoiceChatPlugin::default()))
        .add_plugins(VoiceRepliconPlugin::default())
        .insert_resource(VoiceIo(Arc::new(NullIo)))
        .init_resource::<Heard>()
        .add_systems(Update, collect.after(VoiceTransportSystems).before(VoiceChatSystems::Playback));
    app.edit_schedule(Update, |s| {
        s.set_build_settings(ScheduleBuildSettings { ambiguity_detection: LogLevel::Error, ..default() });
    });
    app.finish();
    app
}

fn frame(seq: u32, len: usize) -> OutgoingVoice {
    OutgoingVoice { seq, ts: 0, codec: ImaAdpcm::ID, frame: vec![0; len] }
}

fn speakers(app: &App) -> Vec<SpeakerId> {
    app.world().resource::<Heard>().0.iter().map(|m| m.speaker).collect()
}

/// One round: the clients update, their messages reach the server, it updates twice (the host's
/// own voice loops back locally one frame later), and its messages reach the clients.
fn round(server: &mut App, clients: &mut [&mut App]) {
    for c in clients.iter_mut() {
        c.update();
        server.exchange_with_client(c);
    }
    server.update();
    server.update();
    for c in clients.iter_mut() {
        server.exchange_with_client(c);
        c.update();
    }
}

#[test]
fn clients_voices_are_relayed_to_everyone_else_and_the_hosts_voice_reaches_every_client() {
    let (mut server, mut c1, mut c2) = (peer(), peer(), peer());
    server.connect_client(&mut c1);
    server.connect_client(&mut c2);
    let e1 = **c1.world().resource::<TestClientEntity>();
    let e2 = **c2.world().resource::<TestClientEntity>();
    server.world_mut().entity_mut(e1).insert(VoiceSenderId(SpeakerId(1)));
    server.world_mut().entity_mut(e2).insert(VoiceSenderId(SpeakerId(2)));
    server.insert_resource(HostVoiceSpeaker(Some(SpeakerId(0))));

    let bytes = ImaAdpcm::default().frame_bytes();
    c1.world_mut().write_message(frame(0, bytes));
    round(&mut server, &mut [&mut c1, &mut c2]);
    assert_eq!(speakers(&c2), vec![SpeakerId(1)], "client 2 hears client 1");
    assert_eq!(speakers(&server), vec![SpeakerId(1)], "the host hears client 1 through the same path");
    assert!(speakers(&c1).is_empty(), "never your own voice back");
    assert_eq!(server.world().resource::<VoiceRelayStats>().relayed, 1);

    server.world_mut().write_message(frame(0, bytes));
    round(&mut server, &mut [&mut c1, &mut c2]);
    assert_eq!(speakers(&c1), vec![SpeakerId(0)], "the host's own voice reaches client 1");
    assert_eq!(speakers(&c2), vec![SpeakerId(1), SpeakerId(0)], "and client 2");
    assert_eq!(speakers(&server), vec![SpeakerId(1)], "the host never hears itself");
}

#[test]
fn the_relay_drops_unknown_senders_bad_frames_floods_and_whatever_the_hook_refuses() {
    let (mut server, mut c1, mut c2) = (peer(), peer(), peer());
    server.connect_client(&mut c1);
    server.connect_client(&mut c2);
    let e1 = **c1.world().resource::<TestClientEntity>();
    server.world_mut().entity_mut(e1).insert(VoiceSenderId(SpeakerId(1)));
    let bytes = ImaAdpcm::default().frame_bytes();

    // Client 2 has no speaker id: dropped (fail closed). Client 1 sends a wrong size.
    c2.world_mut().write_message(frame(0, bytes));
    c1.world_mut().write_message(frame(0, 10));
    round(&mut server, &mut [&mut c1, &mut c2]);
    let stats = *server.world().resource::<VoiceRelayStats>();
    assert_eq!((stats.unknown_sender, stats.invalid, stats.relayed), (1, 1, 0));

    // A flood: 40 frames at once, the budget lets a burst of 15 through.
    for seq in 0..40 {
        c1.world_mut().write_message(frame(seq, bytes));
    }
    round(&mut server, &mut [&mut c1, &mut c2]);
    let stats = *server.world().resource::<VoiceRelayStats>();
    assert_eq!((stats.relayed, stats.rate_limited), (15, 25));
    assert_eq!(speakers(&c2).len(), 15);

    // The game's hook: speaker 1 is muted.
    server.insert_resource(VoiceRelayHook::new(|speaker, _| speaker != SpeakerId(1)));
    c1.world_mut().write_message(frame(100, bytes));
    round(&mut server, &mut [&mut c1, &mut c2]);
    assert_eq!(server.world().resource::<VoiceRelayStats>().refused, 1);
    assert_eq!(speakers(&c2).len(), 15, "nothing new reached client 2");
}
