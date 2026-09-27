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
        codec.encode(&sine_frame(300.0, 0.5, seq as usize * FRAME), &mut bytes);
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
