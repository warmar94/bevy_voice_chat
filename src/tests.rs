//! bevy_voice_chat tests: pure DSP / codec / gate / jitter / spatial / packet checks, then strict
//! headless apps with a FAKE [`AudioIo`] (no device is ever opened).

use super::*;
use crate::codec::{from_i16, CodecError, ImaAdpcm, Pcm16, FRAME_MS};
use crate::dsp::{FrameAssembler, Resampler};
use crate::filter::{GainFilter, VoiceFilter};
use crate::io::{MicShared, OutputPump, OutputShared};
use crate::jitter::{Insert, JitterBuffer, Pull};
use crate::packet::{validate_frame, Reject, TokenBucket};
use crate::spatial::{pan_gains, spatial_gain, voice_gains, Listener};
use bevy::prelude::*;
use std::sync::mpsc;
use std::sync::Mutex;

// ------------------------------------------------------------------ helpers

fn sine_frame(freq: f32, amp: f32, start: usize) -> MonoFrame {
    let mut f = [0.0; FRAME];
    for (i, s) in f.iter_mut().enumerate() {
        *s = amp * (std::f32::consts::TAU * freq * (start + i) as f32 / VOICE_RATE as f32).sin();
    }
    f
}

fn snr_db(reference: &[f32], got: &[f32]) -> f32 {
    let sig: f32 = reference.iter().map(|x| x * x).sum();
    let noise: f32 = reference.iter().zip(got).map(|(a, b)| (a - b) * (a - b)).sum();
    10.0 * (sig / noise.max(1e-12)).log10()
}

// ------------------------------------------------------------------ codec

#[test]
fn adpcm_round_trips_speech_like_audio_with_a_good_snr() {
    let mut enc = ImaAdpcm::default();
    let dec = ImaAdpcm::default();
    let mut bytes = Vec::new();
    let mut out = [0.0; FRAME];
    // Warm up (the step size adapts over the first frame), then measure 10 frames of a mix.
    for n in 0..11 {
        let a = sine_frame(300.0, 0.4, n * FRAME);
        let b = sine_frame(1100.0, 0.2, n * FRAME);
        let mut pcm = [0.0; FRAME];
        for i in 0..FRAME {
            pcm[i] = a[i] + b[i];
        }
        enc.encode(&pcm, &mut bytes);
        assert_eq!(bytes.len(), enc.frame_bytes());
        dec.decode(&bytes, &mut out).expect("decodes");
        if n > 0 {
            let snr = snr_db(&pcm, &out);
            assert!(snr > 20.0, "frame {n}: SNR {snr:.1} dB");
        }
    }
}

#[test]
fn every_adpcm_frame_decodes_on_its_own_so_a_lost_packet_never_corrupts_the_next() {
    let mut enc = ImaAdpcm::default();
    let dec = ImaAdpcm::default();
    let mut packets = Vec::new();
    for n in 0..3 {
        let mut b = Vec::new();
        enc.encode(&sine_frame(500.0, 0.5, n * FRAME), &mut b);
        packets.push(b);
    }
    let (mut a, mut b) = ([0.0; FRAME], [0.0; FRAME]);
    dec.decode(&packets[2], &mut a).expect("frame 3 alone");
    // A fresh decoder that also saw frame 1 (frame 2 "lost") gives the same frame 3.
    let other = ImaAdpcm::default();
    other.decode(&packets[0], &mut b).expect("frame 1");
    other.decode(&packets[2], &mut b).expect("frame 3");
    assert_eq!(a, b);
}

#[test]
fn codecs_reject_foreign_packets_and_pcm16_is_near_exact() {
    let dec = ImaAdpcm::default();
    let mut out = [0.0; FRAME];
    assert_eq!(dec.decode(&[0; 10], &mut out), Err(CodecError::WrongLength { got: 10, want: 164 }));
    let mut bad = vec![0u8; 164];
    bad[2] = 200;
    assert_eq!(dec.decode(&bad, &mut out), Err(CodecError::BadHeader), "a step index past the table");
    let mut pcm = Pcm16;
    let mut bytes = Vec::new();
    let f = sine_frame(440.0, 0.7, 0);
    pcm.encode(&f, &mut bytes);
    assert_eq!(bytes.len(), 640);
    pcm.decode(&bytes, &mut out).expect("decodes");
    assert!(f.iter().zip(&out).all(|(a, b)| (a - b).abs() < 1e-4));
    // NaN / out of range never panic.
    let mut wild = [f32::NAN; FRAME];
    wild[1] = 9.0;
    let mut enc = ImaAdpcm::default();
    enc.encode(&wild, &mut bytes);
    assert_eq!(bytes.len(), 164);
    assert_eq!(from_i16(i16::MIN), -1.0);
}

/// The bandwidth budget: codec payload per talking speaker, measured.
#[test]
fn bandwidth_per_speaker_is_about_66_kbps_of_payload() {
    let adpcm = ImaAdpcm::default();
    assert_eq!(adpcm.frame_bytes(), 164);
    assert_eq!(adpcm.bitrate(), 65_600);
    assert_eq!(Pcm16.bitrate(), 256_000);
    // A whole OutgoingVoice as a serde varint format (e.g. postcard) writes it.
    let up = OutgoingVoice { seq: 1_000_000, ts: u32::MAX, codec: 1, frame: vec![0; 164] };
    let size = 5 + 5 + 1 + 2 + up.frame.len();
    assert!(size <= 177, "at most {size} bytes per packet before transport headers");
    let per_sec = 1000 / FRAME_MS;
    assert_eq!(per_sec, 50);
    assert!(size as u32 * per_sec * 8 <= 71_000, "<= 71 kbps per speaker before UDP/netcode headers");
}

// ------------------------------------------------------------------ dsp

#[test]
fn the_capture_assembler_turns_48k_stereo_into_16k_mono_frames() {
    let mut asm = FrameAssembler::new(48_000, 2, VOICE_RATE);
    let mut frames = Vec::new();
    // 100 ms of a 400 Hz tone, left only.
    let data: Vec<f32> = (0..4800).flat_map(|i| [0.8 * (std::f32::consts::TAU * 400.0 * i as f32 / 48_000.0).sin(), 0.0]).collect();
    asm.push_interleaved(&data, |f| frames.push(*f));
    assert_eq!(frames.len(), 5, "100 ms = 5 frames of 20 ms");
    let joined: Vec<f32> = frames.concat();
    // Mono = the average of both channels (0.4 peak); the tone survives the resampler.
    let p = dsp::peak(&joined[160..]);
    assert!((0.33..0.45).contains(&p), "peak {p}");
    let crossings = joined.windows(2).skip(160).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
    assert!((32..=40).contains(&crossings), "400 Hz over ~90 ms: {crossings}");
}

#[test]
fn the_resampler_keeps_the_rate_ratio_and_gain_helpers_never_blow_up() {
    let mut r = Resampler::new(44_100, VOICE_RATE);
    let mut n = 0;
    for _ in 0..44_100 {
        r.push(0.25, |_| n += 1);
    }
    assert!((15_990..=16_010).contains(&n), "{n}");
    let mut s = [0.5, -0.9, f32::NAN, 2.0];
    dsp::apply_gain(&mut s, 2.0);
    assert_eq!(s, [1.0, -1.0, 0.0, 1.0]);
    assert_eq!(dsp::soft_clip(0.5), 0.5);
    assert!(dsp::soft_clip(5.0) <= 1.0 && dsp::soft_clip(-5.0) >= -1.0);
    assert_eq!(dsp::soft_clip(f32::NAN), 0.0);
}

#[test]
fn the_output_pump_resamples_to_the_device_and_counts_consumed_frames() {
    let (tx, rx) = mpsc::sync_channel::<StereoFrame>(4);
    let shared = Arc::new(OutputShared::default());
    let mut pump = OutputPump::new(rx, VOICE_RATE, 48_000, shared.clone());
    let mut f = [0.0; FRAME * 2];
    for pair in f.chunks_exact_mut(2) {
        pair.copy_from_slice(&[0.5, -0.25]);
    }
    tx.send(f).expect("queued");
    let mut out = vec![0.0; 960 * 2];
    pump.fill(&mut out, 2);
    assert_eq!(shared.consumed.load(Ordering::Relaxed), 1);
    assert!((out[1000] - 0.5).abs() < 1e-6 && (out[1001] + 0.25).abs() < 1e-6, "L/R kept");
    // Empty queue = silence, never a panic; a mono device gets the average.
    let mut more = vec![1.0; 600];
    pump.fill(&mut more, 1);
    assert!(more[590].abs() < 1e-6);
}

// ------------------------------------------------------------------ gate

#[test]
fn voice_activity_opens_past_the_threshold_and_holds_for_the_hangover() {
    let mut g = TalkGate::new(3, 1);
    let quiet = GateInput::OpenMic { level: 0.02, threshold: 0.1 };
    let loud = GateInput::OpenMic { level: 0.3, threshold: 0.1 };
    assert!(!g.step(quiet));
    assert!(g.step(loud));
    // Three quiet frames still go out (the end of the word), the fourth does not.
    assert!(g.step(quiet) && g.step(quiet) && g.step(quiet));
    assert!(!g.step(quiet));
    // A loud frame inside the hangover restarts it.
    assert!(g.step(loud));
    assert!(g.step(quiet));
    assert!(g.step(loud));
    assert!(g.step(quiet) && g.step(quiet) && g.step(quiet) && !g.step(quiet));
    // Silence never opens even at threshold 0; NaN is silence.
    assert!(!g.step(GateInput::OpenMic { level: 0.0, threshold: 0.0 }));
    assert!(!g.step(GateInput::OpenMic { level: f32::NAN, threshold: 0.1 }));
    assert_eq!(ms_to_frames(300.0, 0), 15);
    assert_eq!(ms_to_frames(f32::NAN, 2), 2);
}

#[test]
fn push_to_talk_sends_while_held_plus_the_release_tail() {
    let mut g = TalkGate::new(10, 2);
    assert!(!g.step(GateInput::PushToTalk { held: false }));
    assert!(g.step(GateInput::PushToTalk { held: true }));
    assert!(g.step(GateInput::PushToTalk { held: false }));
    assert!(g.step(GateInput::PushToTalk { held: false }));
    assert!(!g.step(GateInput::PushToTalk { held: false }), "tail of 2 frames, not the VAD hangover");
}

// ------------------------------------------------------------------ filters

struct Recorder {
    tag: u8,
    log: Arc<Mutex<Vec<(u8, f32, FilterContext)>>>,
}

impl VoiceFilter for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }
    fn process(&mut self, frame: &mut [f32], ctx: &FilterContext) {
        self.log.lock().expect("lock").push((self.tag, dsp::peak(frame), *ctx));
    }
}

#[test]
fn filters_run_in_order_sanitised_and_an_empty_chain_passes_through() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut chain = VoiceFilters::default();
    let ctx = FilterContext { sample_rate: VOICE_RATE, stage: FilterStage::Outgoing, speaker: None, transmitting: true, loopback: false };
    let mut f = sine_frame(200.0, 0.5, 0);
    let before = f;
    chain.run(&mut f, &ctx);
    assert_eq!(f, before, "pass-through by default");
    assert!(chain.is_empty(FilterStage::Outgoing) && chain.is_empty(FilterStage::Incoming));
    chain.add(FilterStage::Outgoing, 10, Recorder { tag: 3, log: log.clone() });
    chain.add(FilterStage::Outgoing, 5, Recorder { tag: 1, log: log.clone() });
    chain.add(FilterStage::Outgoing, 5, Recorder { tag: 2, log: log.clone() });
    chain.add(FilterStage::Outgoing, 7, GainFilter(f32::NAN));
    chain.add(FilterStage::Incoming, 0, Recorder { tag: 9, log: log.clone() });
    chain.run(&mut f, &ctx);
    let order: Vec<u8> = log.lock().expect("lock").iter().map(|e| e.0).collect();
    assert_eq!(order, vec![1, 2, 3], "lower order first, equal orders in insertion order; only this stage");
    assert!(f.iter().all(|s| *s == 0.0), "a NaN-producing filter is sanitised to silence");
    assert_eq!(chain.names(FilterStage::Outgoing), vec!["recorder", "recorder", "gain", "recorder"]);
    chain.get_mut::<GainFilter>().expect("found by type").0 = 1.0;
    assert_eq!(chain.get_mut::<GainFilter>().map(|g| g.0), Some(1.0));
    // A closure filter + removal by type.
    chain.add_fn(FilterStage::Incoming, 1, "double", |frame, _| frame.iter_mut().for_each(|s| *s *= 2.0));
    let mut g = [0.25; FRAME];
    chain.run(&mut g, &FilterContext { stage: FilterStage::Incoming, speaker: Some(SpeakerId(7)), ..ctx });
    assert_eq!(g[0], 0.5);
    assert_eq!(chain.remove::<GainFilter>(), 1);
    assert_eq!(chain.names(FilterStage::Incoming), vec!["recorder", "double"]);
}

// ------------------------------------------------------------------ jitter buffer

fn tagged(v: f32) -> MonoFrame {
    [v; FRAME]
}

fn pull_value(j: &mut JitterBuffer) -> (Pull, f32) {
    let mut out = [0.0; FRAME];
    let p = j.pull(&mut out);
    (p, out[0])
}

#[test]
fn the_jitter_buffer_reorders_and_waits_for_its_target() {
    let mut j = JitterBuffer::new(JitterConfig { target: 3, max: 10, conceal_max: 2 });
    assert_eq!(j.insert(1, &tagged(0.1), 11), Insert::Stored);
    assert_eq!(pull_value(&mut j).0, Pull::Silent, "buffering");
    assert_eq!(j.insert(0, &tagged(0.0), 10), Insert::Stored);
    assert_eq!(j.insert(2, &tagged(0.2), 12), Insert::Stored);
    assert_eq!(j.insert(2, &tagged(0.2), 12), Insert::Duplicate);
    assert_eq!(pull_value(&mut j), (Pull::Played { ts: 10 }, 0.0));
    assert_eq!(pull_value(&mut j), (Pull::Played { ts: 11 }, 0.1));
    assert_eq!(pull_value(&mut j), (Pull::Played { ts: 12 }, 0.2));
    assert_eq!(j.insert(1, &tagged(0.9), 0), Insert::Late, "behind the playout point");
    assert_eq!(j.stats.late, 1);
    assert_eq!(j.stats.duplicate, 1);
}

#[test]
fn the_jitter_buffer_conceals_a_lost_frame_then_catches_up() {
    let mut j = JitterBuffer::new(JitterConfig { target: 1, max: 10, conceal_max: 2 });
    j.insert(0, &tagged(0.8), 0);
    j.insert(2, &tagged(0.4), 0);
    assert_eq!(pull_value(&mut j), (Pull::Played { ts: 0 }, 0.8));
    let (p, v) = pull_value(&mut j);
    assert_eq!(p, Pull::Concealed);
    assert!((v - 0.4).abs() < 1e-6, "the last frame at half level");
    assert_eq!(pull_value(&mut j).1, 0.4, "then the next real frame");
    assert_eq!(j.stats.lost, 1);
    // The speaker stops: one faded tail, silence, then the spurt ends and re-buffers.
    let (p, _) = pull_value(&mut j);
    assert_eq!(p, Pull::Concealed);
    assert_eq!(pull_value(&mut j).0, Pull::Silent);
    assert!(j.is_playing());
    assert_eq!(pull_value(&mut j).0, Pull::Silent);
    assert!(!j.is_playing(), "gave up after conceal_max empty pulls");
    assert_eq!(j.insert(1, &tagged(0.1), 0), Insert::Late, "already played past it");
    j.insert(7, &tagged(0.7), 0);
    assert_eq!(pull_value(&mut j).1, 0.7, "a new spurt starts at its first frame");
}

#[test]
fn a_long_hole_skips_ahead_and_the_buffer_never_grows_past_max() {
    let mut j = JitterBuffer::new(JitterConfig { target: 1, max: 4, conceal_max: 1 });
    j.insert(0, &tagged(0.1), 0);
    pull_value(&mut j);
    j.insert(5, &tagged(0.5), 0);
    assert_eq!(pull_value(&mut j).0, Pull::Concealed);
    assert_eq!(pull_value(&mut j).0, Pull::Silent, "past conceal_max: jump");
    assert_eq!(pull_value(&mut j).1, 0.5);
    for s in 10..20 {
        j.insert(s, &tagged(s as f32 / 100.0), 0);
    }
    assert_eq!(j.depth(), 4);
    assert_eq!(j.stats.overflow, 6);
    // 16..19 kept (the oldest dropped); 4 > 2 x target + 1, so 16 is also skipped.
    assert_eq!(pull_value(&mut j).1, 0.17, "the oldest were dropped");
    // A restarted sender (seq back at 0) is taken as a new stream, not as late forever.
    let mut k = JitterBuffer::new(JitterConfig { target: 1, max: 4, conceal_max: 1 });
    k.insert(1000, &tagged(0.1), 0);
    pull_value(&mut k);
    assert_eq!(k.insert(0, &tagged(0.2), 0), Insert::Stored);
}

// ------------------------------------------------------------------ spatial

fn params(hearing: Hearing) -> SpatialParams {
    SpatialParams { hearing, full: 2.0, range: 20.0, rolloff: 1.5, pan_strength: 0.6 }
}

#[test]
fn spatial_gain_is_one_near_zero_past_range_and_monotonic() {
    assert_eq!(spatial_gain(0.0, 2.0, 20.0, 1.5), 1.0);
    assert_eq!(spatial_gain(2.0, 2.0, 20.0, 1.5), 1.0);
    assert_eq!(spatial_gain(20.0, 2.0, 20.0, 1.5), 0.0);
    assert_eq!(spatial_gain(500.0, 2.0, 20.0, 1.5), 0.0);
    let mut last = 1.0;
    for i in 0..=200 {
        let g = spatial_gain(i as f32 * 0.1, 2.0, 20.0, 1.5);
        assert!(g <= last + 1e-6 && (0.0..=1.0).contains(&g), "at {} m: {g}", i as f32 * 0.1);
        last = g;
    }
    assert_eq!(spatial_gain(f32::NAN, 2.0, 20.0, 1.0), 0.0);
    assert_eq!(spatial_gain(5.0, 2.0, 0.0, 1.0), 0.0);
}

#[test]
fn pan_quietens_the_far_ear_only() {
    let (l, r) = pan_gains(Vec3::ZERO, Vec3::X, Vec3::new(5.0, 0.0, 0.0), 0.6);
    assert!((l - 0.4).abs() < 1e-5 && r == 1.0, "speaker on the right: {l} {r}");
    let (l, r) = pan_gains(Vec3::ZERO, Vec3::X, Vec3::new(0.0, 0.0, -5.0), 0.6);
    assert!((l - 1.0).abs() < 1e-5 && (r - 1.0).abs() < 1e-5, "straight ahead: centred");
    assert_eq!(pan_gains(Vec3::ZERO, Vec3::X, Vec3::ZERO, 0.6), (1.0, 1.0), "same spot");
}

#[test]
fn hearing_global_is_full_volume_at_any_distance_and_proximity_fails_closed() {
    let me = Some(Listener { pos: Vec3::ZERO, right: Vec3::X });
    let far = Some(Vec3::new(500.0, 0.0, 0.0));
    assert_eq!(voice_gains(&params(Hearing::Global), true, me, far, None), (1.0, 1.0));
    assert_eq!(voice_gains(&params(Hearing::Global), true, None, None, None), (1.0, 1.0));
    assert_eq!(voice_gains(&params(Hearing::Proximity), true, me, far, None), (0.0, 0.0));
    assert_eq!(voice_gains(&params(Hearing::Proximity), true, me, None, None), (0.0, 0.0), "unknown position while positional");
    assert_eq!(voice_gains(&params(Hearing::Proximity), false, None, None, None), (1.0, 1.0), "not positional (e.g. a menu): full volume");
    let near = voice_gains(&params(Hearing::Proximity), true, me, Some(Vec3::new(0.0, 0.0, -1.0)), None);
    assert_eq!(near, (1.0, 1.0));
    // A speaker's own range (e.g. a megaphone) carries further.
    let mid = Some(Vec3::new(0.0, 0.0, -30.0));
    assert_eq!(voice_gains(&params(Hearing::Proximity), true, me, mid, None), (0.0, 0.0));
    assert!(voice_gains(&params(Hearing::Proximity), true, me, mid, Some(40.0)).0 > 0.0);
}

// ------------------------------------------------------------------ config

#[test]
fn the_config_defaults_are_valid_and_both_hearing_modes_deserialise() {
    let cfg = VoiceChatConfig::default();
    assert!(cfg.problems().is_empty(), "{:?}", cfg.problems());
    assert_eq!(cfg.hearing, Hearing::Proximity);
    let j = cfg.jitter();
    assert_eq!((j.target, j.max, j.conceal_max), (3, 12, 3));
    let global: VoiceChatConfig = ron::from_str("(hearing: Global)").expect("parses");
    assert_eq!(global.hearing, Hearing::Global);
    let prox: VoiceChatConfig = ron::from_str("(hearing: Proximity, range: 30.0)").expect("parses");
    assert_eq!((prox.hearing, prox.range), (Hearing::Proximity, 30.0));
    assert!(ron::from_str::<VoiceChatConfig>("(hearing: Loud)").is_err());
    assert!(!VoiceChatConfig { range: -1.0, ..default() }.problems().is_empty());
}

// ------------------------------------------------------------------ packets

#[test]
fn packets_are_validated_before_relay_and_rate_limited() {
    assert_eq!(validate_frame(1, 164, 1, 164), Ok(()));
    assert_eq!(validate_frame(9, 164, 1, 164), Err(Reject::WrongCodec));
    assert_eq!(validate_frame(1, 5000, 1, 164), Err(Reject::BadSize));
    assert_eq!(validate_frame(1, 800, 1, 800), Err(Reject::BadSize), "over MAX_VOICE_BYTES whatever the codec says");
    let mut b = TokenBucket::new(50.0, 5.0);
    assert_eq!((0..8).filter(|_| b.take()).count(), 5, "a burst of 5");
    b.refill(0.1);
    assert_eq!((0..8).filter(|_| b.take()).count(), 5);
    b.refill(f32::NAN);
    assert!(!b.take());
}

// ------------------------------------------------------------------ mixer

#[test]
fn the_mixer_sums_speakers_applies_gain_and_volume_and_tracks_who_is_heard() {
    let mut m = Mixer::new(JitterConfig { target: 1, max: 8, conceal_max: 1 });
    let a = SpeakerKey::Remote(SpeakerId(2));
    let b = SpeakerKey::Remote(SpeakerId(3));
    m.insert(a, 0, &tagged(0.2), 0, 0.0);
    m.insert(b, 0, &tagged(0.3), 0, 0.0);
    m.set_target(a, 1.0, 1.0);
    m.set_target(b, 0.0, 0.0);
    let mut out = [0.0; FRAME * 2];
    // The first frame ramps from 0 (a new channel starts silent: no click).
    assert!(m.mix(&mut out, 0.5, 1.0, 0, |_, _| {}));
    assert!((out[FRAME * 2 - 2] - 0.1).abs() < 1e-4, "0.2 x volume 0.5 at the end of the ramp: {}", out[FRAME * 2 - 2]);
    assert!(m.heard(a, 1.1, 0.25));
    assert!(!m.heard(b, 1.1, 0.25), "out of range = not heard even though it played");
    assert!(!m.heard(a, 2.0, 0.25), "hold expired");
    // Loud voices never exceed full scale.
    for s in 1..40 {
        m.insert(a, s, &tagged(1.0), 0, 1.0);
        m.insert(b, s, &tagged(1.0), 0, 1.0);
    }
    m.set_target(b, 2.0, 2.0);
    m.mix(&mut out, 1.0, 1.0, 0, |_, _| {});
    m.mix(&mut out, 1.0, 1.0, 0, |_, _| {});
    assert!(out.iter().all(|s| s.abs() <= 1.0));
    m.forget_idle(100.0, 5.0);
    assert!(!m.keys().is_empty(), "still buffered = kept");
}

// ------------------------------------------------------------------ WAV

fn wav_bytes(format: u16, bits: u16, channels: u16, rate: u32, samples: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&format.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * u32::from(channels) * u32::from(bits / 8)).to_le_bytes());
    v.extend_from_slice(&(channels * bits / 8).to_le_bytes());
    v.extend_from_slice(&bits.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    v.extend_from_slice(samples);
    v
}

#[test]
fn wav_files_parse_into_mono_and_bad_files_are_errors() {
    let s16: Vec<u8> = [16384i16, -16384, 8192, 8192].iter().flat_map(|s| s.to_le_bytes()).collect();
    let w = wav::parse_wav(&wav_bytes(1, 16, 2, 48_000, &s16)).expect("16-bit stereo");
    assert_eq!((w.rate, w.channels, w.mono.len()), (48_000, 2, 2));
    assert!(w.mono[0].abs() < 1e-6 && (w.mono[1] - 0.25).abs() < 1e-4);
    let f32s: Vec<u8> = [0.5f32, -0.5].iter().flat_map(|s| s.to_le_bytes()).collect();
    assert_eq!(wav::parse_wav(&wav_bytes(3, 32, 1, 16_000, &f32s)).expect("float").mono, vec![0.5, -0.5]);
    let s24 = [0x00, 0x00, 0x40];
    assert!((wav::parse_wav(&wav_bytes(1, 24, 1, 8000, &s24)).expect("24-bit").mono[0] - 0.5).abs() < 1e-4);
    assert!(wav::parse_wav(b"not a wav").is_err());
    assert!(wav::parse_wav(&wav_bytes(2, 4, 1, 8000, &[1, 2])).is_err(), "ADPCM WAV is not supported");
    assert!(wav::to_voice_rate(&vec![0.1; 48_000], 48_000).len().abs_diff(16_000) < 5);
}

#[test]
fn peaks_are_the_loudest_since_the_last_frame_and_always_finite() {
    let m = MicShared::default();
    assert_eq!(m.take_peak(), 0.0);
    m.publish_peak(-0.5);
    m.publish_peak(0.25);
    m.publish_peak(f32::NAN);
    assert_eq!(m.take_peak(), 0.5);
    assert_eq!(m.take_peak(), 0.0);
    m.publish_peak(7.0);
    assert_eq!(m.take_peak(), 1.0);
}

#[test]
fn an_underflow_waits_for_the_late_frame_instead_of_dropping_it() {
    let mut j = JitterBuffer::new(JitterConfig { target: 1, max: 10, conceal_max: 3 });
    j.insert(0, &tagged(0.1), 0);
    assert_eq!(pull_value(&mut j).1, 0.1);
    // Frame 1 is late (a slow game frame on the sender): the tail, then silence while waiting.
    assert_eq!(pull_value(&mut j).0, Pull::Concealed);
    assert_eq!(pull_value(&mut j).0, Pull::Silent);
    assert_eq!(j.insert(1, &tagged(0.2), 0), Insert::Stored, "not late: playout waited for it");
    j.insert(2, &tagged(0.3), 0);
    assert_eq!(pull_value(&mut j).1, 0.2);
    assert_eq!(pull_value(&mut j).1, 0.3);
    assert_eq!((j.stats.lost, j.stats.late), (0, 0));
    // A burst far above the target is trimmed back.
    for s in 3..12 {
        j.insert(s, &tagged(0.5), 0);
    }
    let before = j.depth();
    pull_value(&mut j);
    assert_eq!(j.depth(), before - 2, "played one + skipped one");
    assert_eq!(j.stats.overflow, 1);
}

#[test]
fn surplus_delay_is_trimmed_after_a_calm_second() {
    let mut j = JitterBuffer::new(JitterConfig { target: 3, max: 12, conceal_max: 3 });
    // 6 frames queued (a burst), then one frame in per frame out: the depth would stay at 6.
    let mut seq = 0;
    for _ in 0..6 {
        j.insert(seq, &tagged(0.1), 0);
        seq += 1;
    }
    for _ in 0..200 {
        pull_value(&mut j);
        j.insert(seq, &tagged(0.1), 0);
        seq += 1;
    }
    assert!(j.depth() <= 3, "drifted back towards the target: {}", j.depth());
    assert!(j.stats.overflow >= 3);
    assert_eq!(j.stats.lost + j.stats.late, 0);
}
