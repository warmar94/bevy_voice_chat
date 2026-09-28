//! bevy_voice_chat app tests: strict headless apps (MinimalPlugins, ambiguity detection = Error)
//! with a FAKE [`AudioIo`] — no device is ever opened.

use super::*;
use crate::codec::ImaAdpcm;
use crate::filter::{GainFilter, VoiceFilter};
use crate::io::{DeviceLists, MicShared, OutputShared};
use bevy::ecs::message::Messages;
use bevy::ecs::schedule::{LogLevel, ScheduleBuildSettings};
use bevy::prelude::*;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Mutex;

/// (requested device, shared, frame sender, the stop receiver — alive while the session is).
type FakeMic = (Option<String>, Arc<MicShared>, SyncSender<MonoFrame>, Receiver<()>);
type FakeOut = (Option<String>, Arc<OutputShared>, Receiver<StereoFrame>, Receiver<()>);

/// A fake sound card: fixed device lists; every opened mic / output is recorded (the test pushes
/// mic frames and plays the output by draining it).
#[derive(Default)]
struct FakeIo {
    lists: DeviceLists,
    mics: Mutex<Vec<FakeMic>>,
    outs: Mutex<Vec<FakeOut>>,
    scans: AtomicUsize,
    /// Opening the mic fails (no microphone).
    fail: bool,
}

impl AudioIo for FakeIo {
    fn start_scan(&self) -> ScanJob {
        self.scans.fetch_add(1, Ordering::SeqCst);
        let job = ScanJob::default();
        job.deliver(self.lists.clone());
        job
    }

    fn open_mic(&self, device: Option<String>) -> MicSession {
        let (s, tx, stop) = MicSession::new(device.clone());
        if self.fail {
            s.shared.set_state(DEVICE_FAILED);
        } else {
            *s.shared.device.lock().expect("lock") = device.clone().unwrap_or_else(|| "Default Mic".into());
            s.shared.rate.store(48_000, Ordering::Relaxed);
            s.shared.channels.store(1, Ordering::Relaxed);
            s.shared.set_state(DEVICE_LIVE);
        }
        self.mics.lock().expect("lock").push((device, s.shared.clone(), tx, stop));
        s
    }

    fn open_output(&self, device: Option<String>) -> OutputSession {
        let (s, rx, stop) = OutputSession::new(device.clone());
        *s.shared.device.lock().expect("lock") = device.clone().unwrap_or_else(|| "Default Speakers".into());
        s.shared.rate.store(48_000, Ordering::Relaxed);
        s.shared.set_state(DEVICE_LIVE);
        self.outs.lock().expect("lock").push((device, s.shared.clone(), rx, stop));
        s
    }
}

fn alive(stop: &Receiver<()>) -> bool {
    !matches!(stop.try_recv(), Err(mpsc::TryRecvError::Disconnected))
}

impl FakeIo {
    fn mics_open(&self) -> usize {
        self.mics.lock().expect("lock").iter().filter(|m| alive(&m.3)).count()
    }

    fn outs_open(&self) -> usize {
        self.outs.lock().expect("lock").iter().filter(|o| alive(&o.3)).count()
    }

    fn last_mic(&self) -> (Option<String>, Arc<MicShared>) {
        let g = self.mics.lock().expect("lock");
        let (d, s, _, _) = g.last().expect("a mic was opened");
        (d.clone(), s.clone())
    }

    fn last_out_device(&self) -> Option<String> {
        self.outs.lock().expect("lock").last().expect("an output was opened").0.clone()
    }

    /// Push frames into the last opened mic (like the capture thread).
    fn speak(&self, frames: &[MonoFrame]) {
        let g = self.mics.lock().expect("lock");
        let (_, shared, tx, _) = g.last().expect("a mic was opened");
        for f in frames {
            shared.publish_peak(dsp::peak(f));
            tx.try_send(*f).expect("mic queue has room");
        }
    }

    /// Play the last output like a device: take every queued frame; (count, peak).
    fn play(&self) -> (usize, f32) {
        let g = self.outs.lock().expect("lock");
        let Some((_, shared, rx, _)) = g.last() else { return (0, 0.0) };
        let (mut n, mut p) = (0, 0.0_f32);
        while let Ok(f) = rx.try_recv() {
            n += 1;
            p = p.max(dsp::peak(&f));
            shared.consumed.fetch_add(1, Ordering::Relaxed);
        }
        (n, p)
    }
}

/// A strict headless app: MinimalPlugins + the plugin + the fake.
fn app_with(fake: Arc<FakeIo>) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, VoiceChatPlugin::default()));
    let strict = |s: &mut Schedule| {
        s.set_build_settings(ScheduleBuildSettings { ambiguity_detection: LogLevel::Error, ..default() });
    };
    app.edit_schedule(PreUpdate, strict).edit_schedule(Update, strict).edit_schedule(PostUpdate, strict);
    app.insert_resource(VoiceIo(fake));
    app
}

fn lists() -> DeviceLists {
    DeviceLists { inputs: vec!["USB Mic".into(), "Webcam Mic".into()], outputs: vec!["Speakers".into(), "Headset".into()] }
}

fn input(app: &mut App) -> Mut<'_, VoiceInput> {
    app.world_mut().resource_mut::<VoiceInput>()
}

fn sine_frame(freq: f32, amp: f32, start: usize) -> MonoFrame {
    let mut f = [0.0; FRAME];
    for (i, s) in f.iter_mut().enumerate() {
        *s = amp * (std::f32::consts::TAU * freq * (start + i) as f32 / VOICE_RATE as f32).sin();
    }
    f
}

/// Push `frames` like real time: at most the backlog (5) per game frame.
fn speak_all(app: &mut App, fake: &FakeIo, frames: &[MonoFrame]) {
    for chunk in frames.chunks(5) {
        fake.speak(chunk);
        app.update();
    }
}

fn talk() -> Vec<MonoFrame> {
    (0..4).map(|n| sine_frame(300.0, 0.5, n * FRAME)).collect()
}

fn silence(n: usize) -> Vec<MonoFrame> {
    vec![[0.0; FRAME]; n]
}

fn feed(app: &mut App, speaker: u64, seqs: std::ops::Range<u32>) {
    let mut codec = ImaAdpcm::default();
    for seq in seqs {
        let mut bytes = Vec::new();
        codec.encode(&sine_frame(300.0, 0.5, seq as usize * FRAME), &mut bytes).expect("ADPCM always encodes");
        app.world_mut().write_message(IncomingVoice { speaker: SpeakerId(speaker), seq, ts: 0, codec: ImaAdpcm::ID, frame: bytes });
    }
}

/// Two updates with the device playing in between; the loudest sample heard.
fn listen(app: &mut App, fake: &FakeIo) -> f32 {
    app.update();
    let (_, a) = fake.play();
    app.update();
    let (_, b) = fake.play();
    a.max(b)
}

struct Recorder {
    log: Arc<Mutex<Vec<(f32, FilterContext)>>>,
}

impl VoiceFilter for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }
    fn process(&mut self, frame: &mut [f32], ctx: &FilterContext) {
        self.log.lock().expect("lock").push((dsp::peak(frame), *ctx));
    }
}

#[test]
fn the_plugin_builds_strict_and_scans_once_then_on_request() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = app_with(fake.clone());
    app.update();
    app.update();
    let d = app.world().resource::<VoiceDevices>().clone();
    assert!(d.scanned);
    assert_eq!((d.inputs, d.outputs), (lists().inputs, lists().outputs));
    assert_eq!(fake.scans.load(Ordering::SeqCst), 1, "one scan at startup");
    app.world_mut().write_message(RescanDevices);
    app.update();
    assert_eq!(fake.scans.load(Ordering::SeqCst), 2, "and one per request");
    assert_eq!(fake.mics_open() + fake.outs_open(), 0, "nothing opened while nothing is asked");
    assert_eq!(*app.world().resource::<VoiceChatState>(), VoiceChatState { mic_kind: "microphone".into(), ..default() });
}

#[test]
fn the_mic_and_output_open_only_when_asked_and_report_state_and_level() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = app_with(fake.clone());
    app.update();
    input(&mut app).meter = true;
    app.update();
    assert_eq!((fake.mics_open(), fake.outs_open()), (1, 0), "the meter needs no output");
    let (dev, shared) = fake.last_mic();
    assert_eq!(dev, None, "nothing chosen = the system default");
    shared.publish_peak(0.3);
    input(&mut app).mic_gain = 2.0;
    app.update();
    let st = app.world().resource::<VoiceChatState>().clone();
    assert_eq!(st.mic, DeviceState::Live("Default Mic".into()));
    assert!((st.mic_level - 0.6).abs() < 1e-6, "peak x gain: {}", st.mic_level);
    assert_eq!(st.mic_format, Some((48_000, 1)));
    app.update();
    assert_eq!(app.world().resource::<VoiceChatState>().mic_level, 0.0, "no new samples = silence");
    input(&mut app).meter = false;
    app.update();
    assert_eq!(fake.mics_open(), 0, "closed the same frame");
    input(&mut app).enabled = true;
    app.update();
    assert_eq!((fake.mics_open(), fake.outs_open()), (1, 1), "a session opens both");
    assert_eq!(app.world().resource::<VoiceChatState>().output, DeviceState::Live("Default Speakers".into()));
    input(&mut app).enabled = false;
    app.update();
    assert_eq!((fake.mics_open(), fake.outs_open()), (0, 0));
}

#[test]
fn device_choices_reopen_and_an_unknown_device_uses_the_default() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = app_with(fake.clone());
    app.update();
    {
        let mut i = input(&mut app);
        i.enabled = true;
        i.input_device = Some("Webcam Mic".into());
        i.output_device = Some("Headset".into());
    }
    app.update();
    assert_eq!(fake.last_mic().0.as_deref(), Some("Webcam Mic"));
    assert_eq!(fake.last_out_device().as_deref(), Some("Headset"));
    {
        let mut i = input(&mut app);
        i.input_device = Some("Unplugged Headset".into());
        i.output_device = None;
    }
    app.update();
    assert_eq!(fake.last_mic().0, None, "a device that is gone opens the system default");
    assert_eq!(fake.last_out_device(), None);
    assert_eq!((fake.mics_open(), fake.outs_open()), (1, 1), "the old sessions were closed");
}

#[test]
fn no_microphone_is_a_state_never_a_panic() {
    let fake = Arc::new(FakeIo { fail: true, ..default() });
    let mut app = app_with(fake.clone());
    input(&mut app).enabled = true;
    app.update();
    fake.last_mic().1.publish_peak(0.9);
    app.update();
    let st = app.world().resource::<VoiceChatState>().clone();
    assert_eq!((st.mic, st.mic_level, st.transmitting), (DeviceState::Failed, 0.0, false));
}

#[test]
fn voice_activity_sends_speech_with_preroll_and_hangover_and_drops_a_stale_backlog() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = app_with(fake.clone());
    input(&mut app).enabled = true;
    app.update();
    speak_all(&mut app, &fake, &silence(10));
    assert_eq!(app.world().resource::<VoiceStats>().packets_out, 0, "silence stays home");
    fake.speak(&talk());
    app.update();
    // 4 loud frames + 2 pre-roll frames (40 ms of what came just before).
    assert_eq!(app.world().resource::<VoiceStats>().packets_out, 6);
    assert!(app.world().resource::<VoiceChatState>().transmitting);
    assert!(app.world().resource::<VoiceActivity>().transmitting);
    assert_eq!(app.world().resource::<Messages<OutgoingVoice>>().len(), 6, "one message per frame");
    speak_all(&mut app, &fake, &silence(20));
    // The hangover (300 ms = 15 frames) keeps sending the quiet end of the word, then stops.
    assert_eq!(app.world().resource::<VoiceStats>().packets_out, 6 + 15);
    assert!(!app.world().resource::<VoiceChatState>().transmitting);
    assert_eq!(app.world().resource::<VoiceStats>().bytes_out, 21 * 164);
    let before = app.world().resource::<VoiceStats>().mic_dropped;
    fake.speak(&[talk(), talk(), talk(), talk(), talk(), talk()].concat()[..24]);
    app.update();
    assert_eq!(app.world().resource::<VoiceStats>().mic_dropped - before, 19, "a stall: only the newest 5 frames");
}

#[test]
fn push_to_talk_follows_the_games_talk_input_not_the_level() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = app_with(fake.clone());
    {
        let mut i = input(&mut app);
        i.enabled = true;
        i.mode = TalkMode::PushToTalk;
    }
    app.update();
    fake.speak(&talk());
    app.update();
    assert_eq!(app.world().resource::<VoiceStats>().packets_out, 0, "loud but not held");
    input(&mut app).talk_held = true;
    fake.speak(&silence(3));
    app.update();
    assert_eq!(app.world().resource::<VoiceStats>().packets_out, 2 + 3, "held: quiet frames go too (after the pre-roll)");
    input(&mut app).talk_held = false;
    speak_all(&mut app, &fake, &[talk(), talk()].concat());
    assert_eq!(app.world().resource::<VoiceStats>().packets_out, 5 + 6, "the 120 ms release tail only");
}

#[test]
fn the_mic_test_loops_back_through_filters_codec_and_buffer_and_sends_nothing() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = app_with(fake.clone());
    let log = Arc::new(Mutex::new(Vec::new()));
    app.world_mut().resource_mut::<VoiceFilters>().add(FilterStage::Outgoing, 0, Recorder { log: log.clone() });
    {
        let mut i = input(&mut app);
        i.loopback = true;
        i.mic_gain = 2.0;
        i.volume = 0.8;
    }
    app.update();
    assert_eq!((fake.mics_open(), fake.outs_open()), (1, 1), "the loopback opens the output too");
    for _ in 0..3 {
        fake.speak(&talk());
        app.update();
    }
    let (frames, peak) = fake.play();
    assert!(frames >= 2, "mixed frames reached the output: {frames}");
    assert!(peak > 0.3, "your own voice comes back (x gain 2, x volume 0.8): {peak}");
    let stats = *app.world().resource::<VoiceStats>();
    assert_eq!(stats.packets_out, 0, "the mic test sends nothing");
    assert!(stats.loopback >= 12);
    let seen = log.lock().expect("lock").clone();
    assert!(seen.iter().any(|(p, ctx)| *p > 0.95 && ctx.loopback && !ctx.transmitting), "the filter runs AFTER the gain");
    // A filter that silences everything silences the loopback: it runs BEFORE the encoder.
    app.world_mut().resource_mut::<VoiceFilters>().add(FilterStage::Outgoing, 1, GainFilter(0.0));
    for _ in 0..12 {
        fake.speak(&talk());
        app.update();
        fake.play();
    }
    fake.speak(&talk());
    app.update();
    assert!(fake.play().1 < 1e-3, "filtered to silence before encoding");
    input(&mut app).loopback = false;
    app.update();
    assert_eq!(fake.outs_open(), 0);
}

#[test]
fn incoming_voice_plays_at_the_games_volume_through_incoming_filters_and_marks_the_speaker_heard() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = app_with(fake.clone());
    {
        let mut i = input(&mut app);
        i.enabled = true;
        i.positional = false;
        i.volume = 0.8;
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    app.world_mut().resource_mut::<VoiceFilters>().add_fn(FilterStage::Incoming, 0, "spy", move |_, ctx| {
        log.lock().expect("lock").push(ctx.speaker);
    });
    app.update();
    feed(&mut app, 3, 0..6);
    app.world_mut().write_message(IncomingVoice { speaker: SpeakerId(4), seq: 0, ts: 0, codec: 7, frame: vec![1, 2, 3] });
    let peak = listen(&mut app, &fake);
    assert!(peak > 0.25, "not positional: full volume x 0.8 ({peak})");
    assert!(app.world().resource::<VoiceActivity>().heard.contains(&SpeakerId(3)));
    assert!(!app.world().resource::<VoiceActivity>().heard.contains(&SpeakerId(4)));
    assert_eq!(app.world().resource::<VoiceStats>().rejected, 1, "a foreign packet is dropped, never decoded");
    assert!(seen.lock().expect("lock").iter().all(|s| *s == Some(SpeakerId(3))), "the incoming filter knows whose voice");
    input(&mut app).volume = 0.0;
    feed(&mut app, 3, 6..12);
    listen(&mut app, &fake);
    assert!(listen(&mut app, &fake) < 1e-6, "volume 0 = silence");
}

#[test]
fn proximity_hearing_uses_speaker_and_listener_positions_and_global_ignores_them() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = app_with(fake.clone());
    input(&mut app).enabled = true;
    app.world_mut().spawn((VoiceListener::default(), GlobalTransform::from_translation(Vec3::ZERO)));
    app.world_mut().spawn((VoiceSpeaker { id: SpeakerId(5), range: None }, GlobalTransform::from_translation(Vec3::new(0.0, 0.0, -50.0))));
    app.update();
    feed(&mut app, 5, 0..6);
    assert!(listen(&mut app, &fake) < 1e-6, "50 m away, range 20 m: silent");
    assert!(!app.world().resource::<VoiceActivity>().heard.contains(&SpeakerId(5)), "out of range = not heard");
    // Global hearing: everyone at full volume, whatever the distance.
    app.world_mut().resource_mut::<VoiceChatConfig>().hearing = Hearing::Global;
    feed(&mut app, 5, 6..12);
    let peak = listen(&mut app, &fake).max(listen(&mut app, &fake));
    assert!(peak > 0.3, "Global: heard at 50 m ({peak})");
    assert!(app.world().resource::<VoiceActivity>().heard.contains(&SpeakerId(5)));
}

// ------------------------------------------------------------------ codecs in the app (0.2)

/// Every `OutgoingVoice` the plugin wrote.
#[derive(Resource, Default)]
struct Sent(Vec<OutgoingVoice>);

fn collect_sent(mut outgoing: MessageReader<OutgoingVoice>, mut sent: ResMut<Sent>) {
    sent.0.extend(outgoing.read().cloned());
}

/// `app_with` + a collector of sent frames + push-to-talk held, positions off.
fn talking_app(fake: Arc<FakeIo>, cfg: VoiceChatConfig) -> App {
    let mut app = app_with(fake);
    app.insert_resource(cfg).init_resource::<Sent>().add_systems(Update, collect_sent.after(VoiceChatSystems::Capture).before(VoiceChatSystems::Playback));
    {
        let mut i = input(&mut app);
        i.enabled = true;
        i.mode = TalkMode::PushToTalk;
        i.talk_held = true;
        i.positional = false;
    }
    app.update();
    app
}

fn sent(app: &App) -> Vec<(u32, u8, usize)> {
    app.world().resource::<Sent>().0.iter().map(|m| (m.seq, m.codec, m.frame.len())).collect()
}

/// A test encoder that always fails.
struct Failing;

impl VoiceCodec for Failing {
    fn id(&self) -> u8 {
        ImaAdpcm::ID
    }
    fn name(&self) -> &'static str {
        "failing"
    }
    fn max_frame_bytes(&self) -> usize {
        1
    }
    fn bitrate(&self) -> u32 {
        0
    }
    fn encode(&mut self, _pcm: &MonoFrame, out: &mut Vec<u8>) -> Result<(), CodecError> {
        out.clear();
        Err(CodecError::Backend("always fails"))
    }
    fn decode(&mut self, _bytes: &[u8], _out: &mut MonoFrame) -> Result<(), CodecError> {
        Err(CodecError::BadHeader)
    }
}

#[test]
fn an_encoder_that_cannot_start_turns_sending_off_with_an_error_state() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let bad = VoiceChatConfig { codec: VoiceCodecChoice::Opus, opus: OpusSettings { bitrate_bps: 0, ..default() }, ..default() };
    assert!(!bad.problems().is_empty(), "problems() says so up front");
    let mut app = talking_app(fake.clone(), bad);
    speak_all(&mut app, &fake, &[talk(), talk()].concat());
    let state = app.world().resource::<VoiceChatState>().clone();
    assert_eq!(state.send_codec, VoiceCodecChoice::Opus);
    assert!(state.codec_error.is_some(), "an observable error state");
    assert!(!state.transmitting, "held, but nothing can be sent");
    assert_eq!(app.world().resource::<VoiceStats>().packets_out, 0);
    assert!(sent(&app).is_empty(), "no ADPCM frame was sent on the player's behalf: no fallback");
    let rt = app.world().resource::<VoiceRuntime>();
    assert!(rt.send_codec().is_none() && rt.codec_error().is_some());
    // Receiving still works.
    feed(&mut app, 3, 0..6);
    assert!(listen(&mut app, &fake) > 0.25);
    assert!(app.world().resource::<VoiceActivity>().heard.contains(&SpeakerId(3)));
    // Fixing the config applies on the next frame.
    {
        let mut cfg = app.world_mut().resource_mut::<VoiceChatConfig>();
        if cfg!(feature = "opus") {
            cfg.opus.bitrate_bps = 24_000;
        } else {
            cfg.codec = VoiceCodecChoice::ImaAdpcm;
        }
    }
    fake.speak(&talk());
    app.update();
    let state = app.world().resource::<VoiceChatState>().clone();
    assert_eq!(state.codec_error, None);
    assert!(state.transmitting);
    assert!(app.world().resource::<VoiceStats>().packets_out > 0);
}

#[cfg(not(feature = "opus"))]
#[test]
fn choosing_opus_without_the_feature_is_an_error_state_not_a_fallback() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = talking_app(fake.clone(), VoiceChatConfig { codec: VoiceCodecChoice::Opus, ..default() });
    speak_all(&mut app, &fake, &talk());
    let state = app.world().resource::<VoiceChatState>().clone();
    assert!(state.codec_error.as_deref().is_some_and(|e| e.contains("needs the `opus` cargo feature")), "{:?}", state.codec_error);
    assert_eq!(app.world().resource::<VoiceStats>().packets_out, 0);
    assert!(app.world().resource::<VoiceRuntime>().send_codec().is_none());
}

#[cfg(not(feature = "opus"))]
#[test]
fn opus_packets_without_the_feature_are_dropped_and_counted() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = app_with(fake.clone());
    {
        let mut i = input(&mut app);
        i.enabled = true;
        i.positional = false;
    }
    app.update();
    feed(&mut app, 3, 0..6);
    for seq in 0..6 {
        let mut frame = vec![0x48];
        frame.resize(60, 0x5A);
        app.world_mut().write_message(IncomingVoice { speaker: SpeakerId(4), seq, ts: 0, codec: crate::codec::OPUS_ID, frame });
    }
    assert!(listen(&mut app, &fake) > 0.25);
    let heard = app.world().resource::<VoiceActivity>().heard.clone();
    assert!(heard.contains(&SpeakerId(3)) && !heard.contains(&SpeakerId(4)));
    let stats = *app.world().resource::<VoiceStats>();
    assert_eq!((stats.unsupported, stats.rejected), (6, 0));
    assert!(app.world().resource::<VoiceRuntime>().warned_unsupported, "warned (once)");
}

#[test]
fn encode_errors_are_counted_and_a_run_of_them_stops_sending() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = talking_app(fake.clone(), VoiceChatConfig::default());
    app.world_mut().resource_mut::<VoiceRuntime>().set_encoder_for_test(Box::new(Failing));
    speak_all(&mut app, &fake, &[talk(), talk(), talk()].concat()[..10]);
    let stats = *app.world().resource::<VoiceStats>();
    assert_eq!((stats.encode_errors, stats.packets_out), (10, 0), "each failed frame is counted and dropped");
    assert!(app.world().resource::<VoiceChatState>().codec_error.is_none(), "not yet: a short run is survivable");
    speak_all(&mut app, &fake, &[talk(), talk(), talk(), talk(), talk(), talk(), talk(), talk()].concat());
    assert_eq!(app.world().resource::<VoiceStats>().encode_errors, 25, "sending stopped after 25 in a row");
    let state = app.world().resource::<VoiceChatState>().clone();
    assert!(state.codec_error.as_deref().is_some_and(|e| e.starts_with("encoder failing")), "{:?}", state.codec_error);
    assert!(!state.transmitting);
    assert!(app.world().resource::<VoiceRuntime>().send_codec().is_none());
}

/// Without the `opus` feature: switching to Opus at runtime stops sending (no fallback, no panic,
/// problems() says why), receiving keeps working, and switching back resumes IMA-ADPCM at once.
#[cfg(not(feature = "opus"))]
#[test]
fn switching_to_opus_at_runtime_without_the_feature_stops_sending_until_switched_back() {
    let fake = Arc::new(FakeIo { lists: lists(), ..default() });
    let mut app = talking_app(fake.clone(), VoiceChatConfig::default());
    speak_all(&mut app, &fake, &talk());
    let before = app.world().resource::<VoiceStats>().packets_out;
    assert!(before > 0);
    app.world_mut().resource_mut::<VoiceChatConfig>().codec = VoiceCodecChoice::Opus;
    assert!(app.world().resource::<VoiceChatConfig>().problems().contains(&"codec: Opus needs the `opus` cargo feature"));
    speak_all(&mut app, &fake, &[talk(), talk()].concat());
    assert_eq!(app.world().resource::<VoiceStats>().packets_out, before, "nothing sent, and no IMA-ADPCM either");
    let state = app.world().resource::<VoiceChatState>().clone();
    assert!(state.codec_error.is_some() && !state.transmitting && state.send_codec == VoiceCodecChoice::Opus);
    feed(&mut app, 3, 0..6);
    assert!(listen(&mut app, &fake) > 0.25, "receiving keeps working");
    app.world_mut().resource_mut::<VoiceChatConfig>().codec = VoiceCodecChoice::ImaAdpcm;
    speak_all(&mut app, &fake, &talk());
    let sent_after: Vec<(u32, u8, usize)> = sent(&app);
    assert!(app.world().resource::<VoiceStats>().packets_out > before);
    assert!(sent_after.iter().all(|&(_, c, n)| (c, n) == (ImaAdpcm::ID, 164)));
    assert_eq!(app.world().resource::<VoiceChatState>().codec_error, None);
}

#[cfg(feature = "opus")]
mod opus {
    use super::*;
    use crate::codec::{Opus, OPUS_ID};

    #[test]
    fn adpcm_and_opus_speakers_are_heard_together() {
        let fake = Arc::new(FakeIo { lists: lists(), ..default() });
        let mut app = app_with(fake.clone());
        {
            let mut i = input(&mut app);
            i.enabled = true;
            i.positional = false;
        }
        app.update();
        feed(&mut app, 3, 0..6);
        let mut enc = Opus::encoder(OpusSettings::default()).expect("encoder");
        for seq in 0..6u32 {
            let mut frame = Vec::new();
            enc.encode(&sine_frame(300.0, 0.5, seq as usize * FRAME), &mut frame).expect("encodes");
            app.world_mut().write_message(IncomingVoice { speaker: SpeakerId(4), seq, ts: 0, codec: OPUS_ID, frame });
        }
        let peak = listen(&mut app, &fake);
        assert!(peak > 0.25, "{peak}");
        let heard = app.world().resource::<VoiceActivity>().heard.clone();
        assert_eq!(heard.into_iter().collect::<Vec<_>>(), vec![SpeakerId(3), SpeakerId(4)]);
        let stats = *app.world().resource::<VoiceStats>();
        assert_eq!((stats.rejected, stats.unsupported), (0, 0));
    }

    #[test]
    fn the_mic_test_loops_back_through_opus() {
        let fake = Arc::new(FakeIo { lists: lists(), ..default() });
        let mut app = app_with(fake.clone());
        app.insert_resource(VoiceChatConfig { codec: VoiceCodecChoice::Opus, ..default() });
        {
            let mut i = input(&mut app);
            i.loopback = true;
            i.mic_gain = 2.0;
            i.volume = 0.8;
        }
        app.update();
        for _ in 0..3 {
            fake.speak(&talk());
            app.update();
        }
        let (frames, peak) = fake.play();
        assert!(frames >= 2, "mixed frames reached the output: {frames}");
        assert!(peak > 0.3, "your own voice comes back through Opus: {peak}");
        let stats = *app.world().resource::<VoiceStats>();
        assert_eq!(stats.packets_out, 0, "the mic test sends nothing");
        assert!(stats.loopback >= 12);
        assert_eq!(app.world().resource::<VoiceRuntime>().send_codec().map(|c| c.id()), Some(OPUS_ID));
        app.world_mut().resource_mut::<VoiceFilters>().add(FilterStage::Outgoing, 1, GainFilter(0.0));
        for _ in 0..12 {
            fake.speak(&talk());
            app.update();
            fake.play();
        }
        fake.speak(&talk());
        app.update();
        assert!(fake.play().1 < 1e-3, "filtered to silence before encoding");
    }

    #[test]
    fn changing_the_codec_at_runtime_applies_on_the_next_frame() {
        let fake = Arc::new(FakeIo { lists: lists(), ..default() });
        let mut app = talking_app(fake.clone(), VoiceChatConfig::default());
        speak_all(&mut app, &fake, &talk());
        let first = sent(&app);
        assert!(!first.is_empty() && first.iter().all(|&(_, c, n)| (c, n) == (ImaAdpcm::ID, 164)), "{first:?}");
        let set = |app: &mut App, codec: VoiceCodecChoice, bitrate_bps: u32| {
            let mut cfg = app.world_mut().resource_mut::<VoiceChatConfig>();
            cfg.codec = codec;
            cfg.opus.bitrate_bps = bitrate_bps;
        };
        let since = |app: &App, from: usize| sent(app)[from..].to_vec();
        set(&mut app, VoiceCodecChoice::Opus, 24_000);
        let mark = sent(&app).len();
        speak_all(&mut app, &fake, &talk());
        let opus = since(&app, mark);
        assert!(!opus.is_empty() && opus.iter().all(|&(_, c, n)| (c, n) == (OPUS_ID, 60)), "{opus:?}");
        assert_eq!(app.world().resource::<VoiceChatState>().send_codec, VoiceCodecChoice::Opus);
        // A bitrate-only change rebuilds the encoder.
        set(&mut app, VoiceCodecChoice::Opus, 32_000);
        let mark = sent(&app).len();
        speak_all(&mut app, &fake, &talk());
        assert!(since(&app, mark).iter().all(|&(_, c, n)| (c, n) == (OPUS_ID, 80)));
        // And back.
        set(&mut app, VoiceCodecChoice::ImaAdpcm, 32_000);
        let mark = sent(&app).len();
        speak_all(&mut app, &fake, &talk());
        assert!(since(&app, mark).iter().all(|&(_, c, n)| (c, n) == (ImaAdpcm::ID, 164)));
        assert_eq!(app.world().resource::<VoiceChatState>().send_codec, VoiceCodecChoice::ImaAdpcm);
        let seqs: Vec<u32> = sent(&app).iter().map(|s| s.0).collect();
        assert_eq!(seqs, (0..seqs.len() as u32).collect::<Vec<_>>(), "seq continues across codec switches");
    }

    fn set_codec(app: &mut App, codec: VoiceCodecChoice) {
        app.world_mut().resource_mut::<VoiceChatConfig>().codec = codec;
    }

    fn id_of(codec: VoiceCodecChoice) -> u8 {
        match codec {
            VoiceCodecChoice::ImaAdpcm => ImaAdpcm::ID,
            VoiceCodecChoice::Opus => OPUS_ID,
        }
    }

    /// Switched while NOT talking: the next talk spurt (its pre-roll included) uses the new codec.
    #[test]
    fn a_codec_switch_between_talk_spurts_applies_to_the_next_spurt() {
        let fake = Arc::new(FakeIo { lists: lists(), ..default() });
        let mut app = talking_app(fake.clone(), VoiceChatConfig::default());
        for (spurt, codec) in [VoiceCodecChoice::ImaAdpcm, VoiceCodecChoice::Opus, VoiceCodecChoice::ImaAdpcm, VoiceCodecChoice::Opus].into_iter().enumerate() {
            // Between spurts: released, the tail runs out, silence keeps the mic busy.
            input(&mut app).talk_held = false;
            speak_all(&mut app, &fake, &silence(10));
            assert!(!app.world().resource::<VoiceChatState>().transmitting);
            set_codec(&mut app, codec);
            speak_all(&mut app, &fake, &silence(3));
            let mark = sent(&app).len();
            input(&mut app).talk_held = true;
            speak_all(&mut app, &fake, &[talk(), talk()].concat());
            let spurt_frames = sent(&app)[mark..].to_vec();
            assert!(spurt_frames.len() >= 8, "spurt {spurt}: {} frames", spurt_frames.len());
            assert!(spurt_frames.iter().all(|&(_, c, _)| c == id_of(codec)), "spurt {spurt}: every frame (pre-roll too) is {codec:?}: {spurt_frames:?}");
            assert_eq!(app.world().resource::<VoiceChatState>().send_codec, codec);
        }
        let all = app.world().resource::<Sent>().0.clone();
        assert!(all.iter().all(|m| crate::packet::validate_packet(m.codec, &m.frame).is_ok()), "every frame sent is a valid wire frame");
        assert_eq!(app.world().resource::<VoiceStats>().encode_errors, 0);
    }

    /// One remote speaker switches codec mid-stream (IMA-ADPCM, Opus, IMA-ADPCM, Opus, with no gap):
    /// the listener keeps playing that speaker through every switch, no frame is undecodable,
    /// nothing is rejected, every output sample is finite.
    #[test]
    fn a_listener_follows_one_speaker_switching_codec_mid_stream() {
        let fake = Arc::new(FakeIo { lists: lists(), ..default() });
        let mut app = app_with(fake.clone());
        {
            let mut i = input(&mut app);
            i.enabled = true;
            i.positional = false;
        }
        app.update();
        let mut adpcm = ImaAdpcm::default();
        let mut opus = Opus::encoder(OpusSettings::default()).expect("encoder");
        let mut seen_codecs = Vec::new();
        for block in 0..8u32 {
            let use_opus = block % 2 == 1;
            for seq in block * 5..block * 5 + 5 {
                let pcm = sine_frame(300.0, 0.5, seq as usize * FRAME);
                let mut frame = Vec::new();
                let codec = if use_opus {
                    opus.encode(&pcm, &mut frame).expect("encodes");
                    OPUS_ID
                } else {
                    adpcm.encode(&pcm, &mut frame).expect("encodes");
                    ImaAdpcm::ID
                };
                app.world_mut().write_message(IncomingVoice { speaker: SpeakerId(5), seq, ts: 0, codec, frame });
            }
            let peak = listen(&mut app, &fake);
            assert!(peak.is_finite() && peak <= 1.0);
            if block > 0 {
                assert!(peak > 0.2, "block {block}: still heard through the switch ({peak})");
                assert!(app.world().resource::<VoiceActivity>().heard.contains(&SpeakerId(5)));
            }
            let rt = app.world().resource::<VoiceRuntime>();
            seen_codecs.push(rt.mixer().channel(SpeakerKey::Remote(SpeakerId(5))).and_then(|c| c.decoder_codec()));
        }
        let stats = *app.world().resource::<VoiceStats>();
        assert_eq!((stats.rejected, stats.unsupported), (0, 0));
        let (jitter, _) = app.world().resource::<VoiceRuntime>().mixer().totals();
        assert_eq!(jitter.undecodable, 0);
        assert!(seen_codecs.contains(&Some(OPUS_ID)) && seen_codecs.contains(&None), "the channel's decoder followed the stream: {seen_codecs:?}");
        assert_eq!(app.world().resource::<VoiceRuntime>().mixer().keys().len(), 1, "one channel for the speaker, whatever its codec");
    }

    /// Opus bitrate / complexity / vbr changed at runtime: packet sizes change on the next frame
    /// and always stay within bounds.
    #[test]
    fn opus_bitrate_and_complexity_changes_resize_packets_on_the_next_frame() {
        let fake = Arc::new(FakeIo { lists: lists(), ..default() });
        let mut app = talking_app(fake.clone(), VoiceChatConfig { codec: VoiceCodecChoice::Opus, ..default() });
        let steps = [
            (OpusSettings::LOW_BANDWIDTH, 40),
            (OpusSettings::default(), 60),
            (OpusSettings::QUALITY, 80),
            (OpusSettings { complexity: 0, ..OpusSettings::QUALITY }, 80),
            (OpusSettings { complexity: 10, ..OpusSettings::LOW_BANDWIDTH }, 40),
            (OpusSettings { bitrate_bps: 6_000, ..default() }, 15),
            (OpusSettings { bitrate_bps: 64_000, ..default() }, 160),
        ];
        for (settings, bytes) in steps {
            app.world_mut().resource_mut::<VoiceChatConfig>().opus = settings;
            let mark = sent(&app).len();
            speak_all(&mut app, &fake, &talk());
            let frames = sent(&app)[mark..].to_vec();
            assert!(!frames.is_empty());
            assert!(frames.iter().all(|&(_, c, n)| c == OPUS_ID && n == bytes), "{settings:?}: want {bytes} B, got {frames:?}");
            assert_eq!(app.world().resource::<VoiceRuntime>().send_codec().map(|c| c.bitrate()), Some(settings.bitrate_bps));
        }
        // Capped VBR: never over the cap.
        app.world_mut().resource_mut::<VoiceChatConfig>().opus = OpusSettings { vbr: true, ..default() };
        let mark = sent(&app).len();
        speak_all(&mut app, &fake, &[talk(), talk()].concat());
        assert!(sent(&app)[mark..].iter().all(|&(_, c, n)| c == OPUS_ID && (1..=60).contains(&n)));
        assert_eq!(app.world().resource::<VoiceStats>().encode_errors, 0);
        assert_eq!(app.world().resource::<VoiceChatState>().codec_error, None);
    }

    /// The codec flips EVERY frame for 200 frames while talking, and what is sent is fed straight
    /// back in as a remote speaker: every frame carries the codec set for it, every frame is a
    /// valid wire frame, the listener side decodes all of it, nothing grows, nothing panics, and
    /// the strict (ambiguity = Error) schedules stay happy.
    #[test]
    fn switching_codec_every_frame_for_200_frames_stays_sound() {
        let fake = Arc::new(FakeIo { lists: lists(), ..default() });
        let mut app = talking_app(fake.clone(), VoiceChatConfig::default());
        let mut fed = 0;
        let mut max_depth = 0;
        for n in 0..200usize {
            let codec = if n % 2 == 0 { VoiceCodecChoice::Opus } else { VoiceCodecChoice::ImaAdpcm };
            set_codec(&mut app, codec);
            fake.speak(&[sine_frame(300.0, 0.5, n * FRAME)]);
            app.update();
            let new: Vec<OutgoingVoice> = app.world().resource::<Sent>().0[fed..].to_vec();
            fed += new.len();
            assert_eq!(new.len(), 1, "frame {n}: one frame per game frame");
            for m in new {
                assert_eq!(m.codec, id_of(codec), "frame {n} uses the codec set for it");
                assert_eq!(crate::packet::validate_packet(m.codec, &m.frame), Ok(()));
                app.world_mut().write_message(IncomingVoice { speaker: SpeakerId(7), seq: m.seq, ts: m.ts, codec: m.codec, frame: m.frame });
            }
            let (_, peak) = fake.play();
            assert!(peak.is_finite() && peak <= 1.0);
            max_depth = max_depth.max(app.world().resource::<VoiceRuntime>().mixer().totals().1);
        }
        app.update();
        let stats = *app.world().resource::<VoiceStats>();
        assert_eq!((stats.packets_out, stats.encode_errors, stats.rejected, stats.unsupported), (200, 0, 0, 0));
        let rt = app.world().resource::<VoiceRuntime>();
        assert_eq!(rt.mixer().keys(), vec![SpeakerKey::Remote(SpeakerId(7))], "one channel, no leak per switch");
        let (jitter, _) = rt.mixer().totals();
        assert_eq!(jitter.undecodable, 0);
        assert!(jitter.played >= 150, "the looped-back stream played: {jitter:?}");
        assert!(max_depth <= 12, "the buffer never grew past its max: {max_depth}");
        assert!(app.world().resource::<VoiceActivity>().heard.contains(&SpeakerId(7)));
    }

    /// The mic test with each codec, switched while it runs: always your own voice back, nothing sent.
    #[test]
    fn the_mic_test_loopback_works_with_every_codec_and_across_switches() {
        let fake = Arc::new(FakeIo { lists: lists(), ..default() });
        let mut app = app_with(fake.clone());
        {
            let mut i = input(&mut app);
            i.loopback = true;
            i.volume = 0.8;
        }
        app.update();
        for codec in [VoiceCodecChoice::ImaAdpcm, VoiceCodecChoice::Opus, VoiceCodecChoice::ImaAdpcm, VoiceCodecChoice::Opus] {
            set_codec(&mut app, codec);
            let mut peak = 0.0_f32;
            for _ in 0..4 {
                fake.speak(&talk());
                app.update();
                peak = peak.max(fake.play().1);
            }
            assert!(peak > 0.2, "{codec:?}: your own voice comes back ({peak})");
            assert_eq!(app.world().resource::<VoiceRuntime>().send_codec().map(|c| c.id()), Some(id_of(codec)));
        }
        let stats = *app.world().resource::<VoiceStats>();
        assert_eq!((stats.packets_out, stats.encode_errors), (0, 0), "the mic test sends nothing");
        assert_eq!(app.world().resource::<VoiceRuntime>().mixer().totals().0.undecodable, 0);
    }
}
