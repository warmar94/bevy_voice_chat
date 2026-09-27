//! The README's quick start: open-mic voice chat whose "network" is an echo — every frame you send
//! comes straight back as another speaker, so you hear yourself about 100 ms late through the whole
//! pipeline (capture, voice activity, codec, jitter buffer, mixer, output). Needs a microphone and
//! speakers or headphones (headphones avoid feedback). Stop it with Ctrl+C.
//!
//! `cargo run --example quick_start`

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
