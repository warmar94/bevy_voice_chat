//! **A ready-made transport over `bevy_replicon`** (feature `replicon`).
//!
//! Voice is transient intent, so it travels as MESSAGES on replicon's UNRELIABLE channel — never as
//! replicated components:
//!
//! - [`VoiceUp`] (client message): this peer's [`OutgoingVoice`] as it leaves. On a listen server
//!   the host's own voice takes the same path (replicon hands it back locally as
//!   `FromClient { client_id: ClientId::Server, .. }`).
//! - The server checks every packet — a known sender ([`VoiceSenderId`] on the client's entity,
//!   [`HostVoiceSpeaker`] for its own voice), a well-formed frame of a known wire codec
//!   ([`validate_packet`]: IMA-ADPCM or Opus, whatever this build decodes itself), the game's own
//!   [`VoiceRelayHook`] (mute lists, teams, "spectators can't talk"), then a per-speaker packet
//!   budget — and relays [`VoiceDown`] to everyone EXCEPT the speaker (itself included).
//! - Every peer turns [`VoiceDown`] into [`IncomingVoice`] for the pipeline.
//!
//! The game decides what a speaker id is: it inserts [`VoiceSenderId`] on the server-side client
//! entity (the one replicon puts `ConnectedClient` on) and uses the SAME ids for its
//! [`VoiceSpeaker`](crate::VoiceSpeaker) components. A sender without an id is dropped (fail closed).
//!
//! ```no_run
//! use bevy::prelude::*;
//! use bevy_replicon::prelude::*;
//! use bevy_voice_chat::prelude::*;
//!
//! App::new()
//!     .add_plugins((DefaultPlugins, RepliconPlugins, VoiceChatPlugin::default()))
//!     // after RepliconPlugins, on EVERY peer, in the same plugin order (the protocol must match)
//!     .add_plugins(VoiceRepliconPlugin::default())
//!     .run();
//!
//! // Server: tag each client's connection entity with the speaker id your players use.
//! fn tag_new_clients(mut commands: Commands, new: Query<Entity, Added<ConnectedClient>>) {
//!     for (n, client) in new.iter().enumerate() {
//!         commands.entity(client).insert(VoiceSenderId(SpeakerId(100 + n as u64)));
//!     }
//! }
//! ```

use crate::packet::{validate_packet, Reject, TokenBucket};
use crate::{IncomingVoice, OutgoingVoice, SpeakerId, VoiceChatSystems};
use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_replicon::prelude::*;
use bevy_state::condition::in_state;
use bevy_time::{Real, Time};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

/// Speaker -> server: one encoded frame (client message, unreliable).
#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceUp {
    /// Per-sender frame counter ([`OutgoingVoice::seq`]).
    pub seq: u32,
    /// The sender's wall clock in ms ([`OutgoingVoice::ts`]).
    pub ts: u32,
    /// The codec id ([`OutgoingVoice::codec`]).
    pub codec: u8,
    /// The encoded frame ([`OutgoingVoice::frame`]).
    pub frame: Vec<u8>,
}

/// Server -> listeners: a checked frame with its speaker (server message, unreliable, independent).
#[derive(Message, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceDown {
    /// Whose voice.
    pub speaker: SpeakerId,
    /// From [`VoiceUp::seq`].
    pub seq: u32,
    /// From [`VoiceUp::ts`].
    pub ts: u32,
    /// From [`VoiceUp::codec`].
    pub codec: u8,
    /// From [`VoiceUp::frame`].
    pub frame: Vec<u8>,
}

/// Server side: the speaker id of the client whose connection entity carries it. The game inserts
/// it (typically when the client joins); without it, that client's voice is dropped.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VoiceSenderId(pub SpeakerId);

/// Server side: the speaker id of the host's own voice on a listen server (`None` = a dedicated
/// server, or the host does not talk: its frames are dropped).
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostVoiceSpeaker(pub Option<SpeakerId>);

/// The game's extra relay check: `(speaker, packet) -> relay it?` (mute lists, teams, "spectators
/// can't talk"). Runs after the sender / frame checks and before the packet budget (a refused
/// packet costs nothing); `None` = relay every valid packet. It runs on the server's main thread
/// for every packet (50 per second per talking player): keep it cheap.
#[derive(Resource, Clone, Default)]
pub struct VoiceRelayHook(pub Option<Arc<dyn Fn(SpeakerId, &VoiceUp) -> bool + Send + Sync>>);

impl VoiceRelayHook {
    /// A hook from a closure.
    pub fn new(f: impl Fn(SpeakerId, &VoiceUp) -> bool + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(f)))
    }
}

/// Server side: relay counters (cumulative).
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceRelayStats {
    /// Packets relayed.
    pub relayed: u64,
    /// Dropped: no speaker id for the sender.
    pub unknown_sender: u64,
    /// Dropped: unknown codec, wrong size or malformed.
    pub invalid: u64,
    /// Dropped: over the per-speaker packet budget.
    pub rate_limited: u64,
    /// Dropped by the game's [`VoiceRelayHook`].
    pub refused: u64,
}

/// The transport's systems in `Update`, between [`VoiceChatSystems::Capture`] and
/// [`VoiceChatSystems::Playback`] (send -> relay -> receive).
#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VoiceTransportSystems;

/// Voice over replicon. Add it AFTER `RepliconPlugins` and in the same order on every peer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoiceRepliconPlugin {
    /// Per-speaker packet budget on the server (a talking player sends 50 per second).
    pub max_packets_per_sec: f32,
    /// Packets a speaker may send in one burst (a pre-roll or a slow game frame).
    pub burst: f32,
}

impl Default for VoiceRepliconPlugin {
    fn default() -> Self {
        Self { max_packets_per_sec: 75.0, burst: 15.0 }
    }
}

/// The relay's per-speaker budgets (server side).
#[derive(Resource, Default)]
struct RelayBudgets {
    buckets: HashMap<SpeakerId, TokenBucket>,
}

impl Plugin for VoiceRepliconPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<RepliconSharedPlugin>() {
            tracing::warn!("voice chat: VoiceRepliconPlugin needs RepliconPlugins added first - voice transport disabled");
            return;
        }
        let limits = *self;
        app.insert_resource(RelayLimits(limits))
            .init_resource::<HostVoiceSpeaker>()
            .init_resource::<VoiceRelayHook>()
            .init_resource::<VoiceRelayStats>()
            .init_resource::<RelayBudgets>()
            .add_client_message::<VoiceUp>(Channel::Unreliable)
            .add_server_message::<VoiceDown>(Channel::Unreliable)
            // A voice frame references no entity: send it at once, not with the replication tick.
            .make_message_independent::<VoiceDown>()
            .configure_sets(Update, VoiceTransportSystems.after(VoiceChatSystems::Capture).before(VoiceChatSystems::Playback))
            .add_systems(
                Update,
                (
                    send_voice.run_if(in_state(ClientState::Connected).or_else(in_state(ServerState::Running))),
                    relay_voice.run_if(in_state(ServerState::Running)),
                    receive_voice,
                )
                    .chain()
                    .in_set(VoiceTransportSystems),
            );
    }
}

#[derive(Resource, Clone, Copy)]
struct RelayLimits(VoiceRepliconPlugin);

/// The server's first check of one packet (pure; also usable by a custom relay): a known speaker
/// and a well-formed frame of any known wire codec ([`validate_packet`] - the relay needs no
/// decoder, so a relay built without the `opus` feature still forwards Opus). The relay then asks
/// the [`VoiceRelayHook`] and spends the speaker's [`TokenBucket`].
pub fn check_up(speaker: Option<SpeakerId>, up: &VoiceUp) -> Result<SpeakerId, Reject> {
    let speaker = speaker.ok_or(Reject::UnknownSender)?;
    validate_packet(up.codec, &up.frame)?;
    Ok(speaker)
}

/// This peer's encoded frames go to the server.
fn send_voice(mut outgoing: MessageReader<OutgoingVoice>, mut up: MessageWriter<VoiceUp>) {
    for m in outgoing.read() {
        up.write(VoiceUp { seq: m.seq, ts: m.ts, codec: m.codec, frame: m.frame.clone() });
    }
}

/// Server: check every packet and relay it to everyone but its speaker.
fn relay_voice(
    time: Res<Time<Real>>,
    limits: Res<RelayLimits>,
    host: Res<HostVoiceSpeaker>,
    hook: Res<VoiceRelayHook>,
    senders: Query<&VoiceSenderId>,
    mut incoming: MessageReader<FromClient<VoiceUp>>,
    mut budgets: ResMut<RelayBudgets>,
    mut stats: ResMut<VoiceRelayStats>,
    mut outgoing: MessageWriter<ToClients<VoiceDown>>,
) {
    let dt = time.delta_secs();
    for b in budgets.buckets.values_mut() {
        b.refill(dt);
    }
    let RelayLimits(VoiceRepliconPlugin { max_packets_per_sec, burst }) = *limits;
    for m in incoming.read() {
        let speaker = match m.client_id {
            ClientId::Server => host.0,
            ClientId::Client(e) => senders.get(e).ok().map(|s| s.0),
        };
        let speaker = match check_up(speaker, &m.message) {
            Ok(s) => s,
            Err(Reject::UnknownSender) => {
                stats.unknown_sender += 1;
                continue;
            }
            Err(_) => {
                stats.invalid += 1;
                continue;
            }
        };
        // The game's verdict before the budget: a muted speaker spends nothing.
        if hook.0.as_ref().is_some_and(|f| !f(speaker, &m.message)) {
            stats.refused += 1;
            continue;
        }
        if !budgets.buckets.entry(speaker).or_insert_with(|| TokenBucket::new(max_packets_per_sec, burst)).take() {
            stats.rate_limited += 1;
            continue;
        }
        let up = &m.message;
        outgoing.write(ToClients {
            targets: SendTargets::AllExcept(m.client_id),
            message: VoiceDown { speaker, seq: up.seq, ts: up.ts, codec: up.codec, frame: up.frame.clone() },
        });
        stats.relayed += 1;
    }
    // Budgets of speakers who left go.
    let present: BTreeSet<SpeakerId> = senders.iter().map(|s| s.0).chain(host.0).collect();
    budgets.buckets.retain(|id, _| present.contains(id));
}

/// Every relayed frame -> the pipeline.
fn receive_voice(mut down: MessageReader<VoiceDown>, mut incoming: MessageWriter<IncomingVoice>) {
    for m in down.read() {
        incoming.write(IncomingVoice { speaker: m.speaker, seq: m.seq, ts: m.ts, codec: m.codec, frame: m.frame.clone() });
    }
}
