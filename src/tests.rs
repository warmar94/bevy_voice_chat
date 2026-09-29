//! bevy_voice_chat tests: pure DSP / codec / gate / jitter / spatial / packet checks, then strict
//! headless apps with a FAKE [`AudioIo`] (no device is ever opened).

use super::*;
use crate::codec::{can_decode, default_codec, from_i16, new_decoder, new_encoder, CodecError, ImaAdpcm, Pcm16, FRAME_MS, OPUS_ID};
use crate::dsp::{FrameAssembler, Resampler};
use crate::filter::{GainFilter, VoiceFilter};
use crate::io::{MicShared, OutputPump, OutputShared};
use crate::jitter::{Insert, JitterBuffer, JitterCore, Next, Pull};
use crate::packet::{opus_toc_ok, validate_frame, validate_packet, Reject, TokenBucket};
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
    let mut dec = ImaAdpcm::default();
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
        enc.encode(&pcm, &mut bytes).expect("ADPCM always encodes");
        assert_eq!(bytes.len(), ImaAdpcm::FRAME_BYTES);
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
    let mut dec = ImaAdpcm::default();
    let mut packets = Vec::new();
    for n in 0..3 {
        let mut b = Vec::new();
        enc.encode(&sine_frame(500.0, 0.5, n * FRAME), &mut b).expect("encodes");
        packets.push(b);
    }
    let (mut a, mut b) = ([0.0; FRAME], [0.0; FRAME]);
    dec.decode(&packets[2], &mut a).expect("frame 3 alone");
    // A fresh decoder that also saw frame 1 (frame 2 "lost") gives the same frame 3.
    let mut other = ImaAdpcm::default();
    other.decode(&packets[0], &mut b).expect("frame 1");
    other.decode(&packets[2], &mut b).expect("frame 3");
    assert_eq!(a, b);
}

#[test]
fn codecs_reject_foreign_packets_and_pcm16_is_near_exact() {
    let mut dec = ImaAdpcm::default();
    let mut out = [0.0; FRAME];
    assert_eq!(dec.decode(&[0; 10], &mut out), Err(CodecError::WrongLength { got: 10, want: 164 }));
    let mut bad = vec![0u8; 164];
    bad[2] = 200;
    assert_eq!(dec.decode(&bad, &mut out), Err(CodecError::BadHeader), "a step index past the table");
    let mut pcm = Pcm16;
    let mut bytes = Vec::new();
    let f = sine_frame(440.0, 0.7, 0);
    pcm.encode(&f, &mut bytes).expect("encodes");
    assert_eq!(bytes.len(), Pcm16::FRAME_BYTES);
    pcm.decode(&bytes, &mut out).expect("decodes");
    assert!(f.iter().zip(&out).all(|(a, b)| (a - b).abs() < 1e-4));
    // NaN / out of range never panic.
    let mut wild = [f32::NAN; FRAME];
    wild[1] = 9.0;
    let mut enc = ImaAdpcm::default();
    enc.encode(&wild, &mut bytes).expect("encodes");
    assert_eq!(bytes.len(), 164);
    assert_eq!(from_i16(i16::MIN), -1.0);
}

/// The bandwidth budget: codec payload per talking speaker, measured.
#[test]
fn bandwidth_per_speaker_is_about_66_kbps_of_payload() {
    let adpcm = ImaAdpcm::default();
    assert_eq!(ImaAdpcm::FRAME_BYTES, 164);
    assert_eq!((adpcm.max_frame_bytes(), adpcm.fixed_frame_bytes()), (164, Some(164)));
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
    for pair in f.as_chunks_mut::<2>().0 {
        *pair = [0.5, -0.25];
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
fn validate_frame_still_checks_an_exact_size() {
    assert_eq!(validate_frame(1, 164, 1, 164), Ok(()));
    assert_eq!(validate_frame(9, 164, 1, 164), Err(Reject::WrongCodec));
    assert_eq!(validate_frame(1, 5000, 1, 164), Err(Reject::BadSize));
    assert_eq!(validate_frame(1, 800, 1, 800), Err(Reject::BadSize), "over MAX_VOICE_BYTES whatever the codec says");
}

#[test]
fn packets_are_validated_before_relay_and_rate_limited() {
    assert_eq!(validate_packet(ImaAdpcm::ID, &[0; 164]), Ok(()));
    assert_eq!(validate_packet(9, &[0; 164]), Err(Reject::WrongCodec));
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

// ------------------------------------------------------------------ codecs (0.2)

/// A deterministic LCG for fuzz inputs (tests never use a runtime RNG).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    fn below(&mut self, n: usize) -> usize {
        self.next() as usize % n.max(1)
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// A speech-like frame: harmonics of a slowly gliding pitch, shaped by three formant bumps.
fn speech_frame(n: usize) -> MonoFrame {
    let mut f = [0.0; FRAME];
    let formants = [(700.0_f32, 130.0_f32), (1200.0, 150.0), (2600.0, 250.0)];
    for (i, s) in f.iter_mut().enumerate() {
        let t = (n * FRAME + i) as f32 / VOICE_RATE as f32;
        let f0 = 125.0 + 15.0 * (std::f32::consts::TAU * 0.7 * t).sin();
        let mut v = 0.0;
        for h in 1..40 {
            let hz = f0 * h as f32;
            let amp: f32 = formants.iter().map(|(c, w)| (-((hz - c) / w).powi(2)).exp()).sum::<f32>() + 0.02;
            v += amp * (std::f32::consts::TAU * hz * t).sin();
        }
        *s = 0.12 * v;
    }
    f
}

#[test]
fn adpcm_decode_never_panics_on_garbage() {
    let mut rng = Lcg(7);
    let mut dec = ImaAdpcm::default();
    let mut out = [0.0; FRAME];
    let mut ok = 0;
    for n in 0..2000 {
        // Every other packet is exactly frame-sized, so the decoder body runs on garbage too.
        let len = if n % 2 == 0 { ImaAdpcm::FRAME_BYTES } else { rng.below(700) };
        let bytes = rng.bytes(len);
        if dec.decode(&bytes, &mut out).is_ok() {
            ok += 1;
            assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        }
    }
    assert!(ok > 0, "some well-sized garbage decodes (as noise)");
}

#[test]
fn adpcm_conceal_is_not_provided_by_the_codec() {
    let mut adpcm = ImaAdpcm::default();
    let mut out = [0.5; FRAME];
    assert!(!adpcm.is_stateful());
    assert!(!adpcm.conceal(None, &mut out), "the jitter buffer's repeat-and-fade applies");
    assert_eq!(out, [0.5; FRAME], "untouched");
}

#[test]
fn opus_settings_problems() {
    assert!(OpusSettings::default().problems().is_empty());
    assert_eq!(OpusSettings::default().frame_bytes(), 60);
    assert_eq!((OpusSettings::LOW_BANDWIDTH.frame_bytes(), OpusSettings::QUALITY.frame_bytes()), (40, 80));
    for bitrate_bps in [0, 5_999, 64_001] {
        assert_eq!(OpusSettings { bitrate_bps, ..default() }.problems(), vec!["opus.bitrate_bps must be 6000..=64000"], "{bitrate_bps}");
    }
    assert_eq!(OpusSettings { complexity: 11, ..default() }.problems(), vec!["opus.complexity must be 0..=10"]);
    assert!(OpusSettings { bitrate_bps: 6_000, complexity: 0, vbr: true }.problems().is_empty());
    assert!(OpusSettings { bitrate_bps: 64_000, complexity: 10, vbr: false }.problems().is_empty());
}

#[test]
fn registry_builds_what_the_build_can_do() {
    assert_eq!(new_decoder(ImaAdpcm::ID).map(|d| d.id()), Some(ImaAdpcm::ID));
    #[cfg(feature = "opus")]
    assert_eq!(new_decoder(OPUS_ID).map(|d| (d.id(), d.is_stateful())), Some((OPUS_ID, true)));
    #[cfg(not(feature = "opus"))]
    assert!(new_decoder(OPUS_ID).is_none());
    assert!(new_decoder(Pcm16::ID).is_none() && new_decoder(7).is_none(), "Pcm16 is not a wire codec");
    assert!(can_decode(ImaAdpcm::ID));
    assert_eq!(can_decode(OPUS_ID), cfg!(feature = "opus"));
    assert!(!can_decode(Pcm16::ID) && !can_decode(7) && !can_decode(0));
    let adpcm = new_encoder(VoiceCodecChoice::ImaAdpcm, &OpusSettings { bitrate_bps: 0, ..default() }).expect("ADPCM ignores the Opus settings");
    assert_eq!((adpcm.id(), adpcm.bitrate()), (ImaAdpcm::ID, 65_600));
    assert_eq!(default_codec().id(), ImaAdpcm::ID);
}

#[cfg(not(feature = "opus"))]
#[test]
fn opus_encoder_without_the_feature_is_not_compiled() {
    assert_eq!(new_encoder(VoiceCodecChoice::Opus, &OpusSettings::default()).err(), Some(CodecError::NotCompiled));
    assert!(CodecError::NotCompiled.to_string().contains("`opus` cargo feature"));
}

#[cfg(feature = "opus")]
mod opus {
    use super::*;
    use crate::codec::Opus;
    use crate::mixer::SpeakerChannel;

    fn sine_mix(n: usize) -> MonoFrame {
        let a = sine_frame(300.0, 0.4, n * FRAME);
        let b = sine_frame(1100.0, 0.2, n * FRAME);
        let mut pcm = [0.0; FRAME];
        for i in 0..FRAME {
            pcm[i] = a[i] + b[i];
        }
        pcm
    }

    fn encoder(bitrate_bps: u32, vbr: bool) -> Opus {
        Opus::encoder(OpusSettings { bitrate_bps, complexity: 5, vbr }).expect("valid settings")
    }

    fn encode_all(enc: &mut Opus, frames: impl Iterator<Item = MonoFrame>) -> Vec<Vec<u8>> {
        frames
            .map(|f| {
                let mut b = Vec::new();
                enc.encode(&f, &mut b).expect("encodes");
                b
            })
            .collect()
    }

    #[test]
    fn opus_cbr_frames_are_bitrate_over_400_bytes() {
        for (bitrate, bytes) in [(12_000, 30), (24_000, 60), (32_000, 80)] {
            let mut enc = encoder(bitrate, false);
            assert_eq!(enc.max_frame_bytes(), bytes);
            for p in encode_all(&mut enc, (0..50).map(speech_frame)) {
                assert_eq!(p.len(), bytes, "{bitrate} bps");
                assert!(opus_toc_ok(&p), "TOC {:#04x}", p[0]);
            }
        }
    }

    #[test]
    fn opus_vbr_never_exceeds_its_cap_and_averages_below_it() {
        let mut enc = encoder(24_000, true);
        let packets = encode_all(&mut enc, (0..50).map(speech_frame));
        assert!(packets.iter().all(|p| (1..=60).contains(&p.len()) && opus_toc_ok(p)));
        let avg = packets.iter().map(Vec::len).sum::<usize>() as f32 / packets.len() as f32;
        assert!(avg < 60.0, "capped VBR averages under the cap: {avg}");
    }

    #[test]
    fn opus_round_trips_speech_like_audio() {
        let mut enc = encoder(24_000, false);
        let mut dec = Opus::decoder().expect("decoder");
        let (mut input, mut output) = (Vec::new(), Vec::new());
        for n in 0..100 {
            let pcm = sine_mix(n);
            let mut bytes = Vec::new();
            enc.encode(&pcm, &mut bytes).expect("encodes");
            let mut out = [0.0; FRAME];
            dec.decode(&bytes, &mut out).expect("decodes");
            input.extend_from_slice(&pcm);
            output.extend_from_slice(&out);
        }
        // Opus delays the signal (about 90 samples): find the best alignment, skip the warm-up.
        let skip = 10 * FRAME;
        let len = input.len() - skip - 400;
        let (mut best, mut best_lag) = (0.0_f32, 0);
        for lag in 0..=400 {
            let (a, b) = (&input[skip..skip + len], &output[skip + lag..skip + lag + len]);
            let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
            let norm = (a.iter().map(|x| x * x).sum::<f32>() * b.iter().map(|y| y * y).sum::<f32>()).sqrt().max(1e-9);
            if dot / norm > best {
                (best, best_lag) = (dot / norm, lag);
            }
        }
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
        let ratio = rms(&output[skip + best_lag..skip + best_lag + len]) / rms(&input[skip..skip + len]);
        assert!(best > 0.8, "normalised cross-correlation {best:.3} at lag {best_lag}");
        assert!((0.5..=2.0).contains(&ratio), "level kept: {ratio:.2}");
    }

    #[test]
    fn opus_encodes_nan_and_out_of_range_samples_without_panicking() {
        let mut enc = encoder(24_000, false);
        let mut dec = Opus::decoder().expect("decoder");
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 9.0, -9.0] {
            let mut bytes = Vec::new();
            enc.encode(&[v; FRAME], &mut bytes).expect("encodes anyway");
            assert!(opus_toc_ok(&bytes));
            let mut out = [0.0; FRAME];
            dec.decode(&bytes, &mut out).expect("decodes");
            assert!(out.iter().all(|s| s.is_finite()), "{v}");
        }
    }

    #[test]
    fn opus_decode_never_panics_on_garbage() {
        let mut rng = Lcg(42);
        let mut enc = encoder(24_000, false);
        let real = encode_all(&mut enc, (0..20).map(speech_frame));
        let mut dec = Opus::decoder().expect("decoder");
        let mut out = [0.0; FRAME];
        let (mut ok, mut err) = (0, 0);
        for n in 0..2000 {
            let packet = match n % 5 {
                0 => {
                    let len = rng.below(700);
                    rng.bytes(len)
                }
                1 => {
                    let mut p = vec![if rng.next().is_multiple_of(2) { 0x48 } else { 0x4B }, 0x01];
                    let len = rng.below(120);
                    p.extend(rng.bytes(len));
                    p
                }
                2 => {
                    let mut p = real[rng.below(real.len())].clone();
                    for _ in 0..1 + rng.below(8) {
                        let i = 1 + rng.below(p.len() - 1);
                        p[i] ^= 1 << rng.below(8);
                    }
                    p
                }
                3 => {
                    let mut p = real[rng.below(real.len())].clone();
                    p.truncate(1 + rng.below(p.len()));
                    p
                }
                _ => {
                    let mut p = real[rng.below(real.len())].clone();
                    let extra = rng.below(200);
                    p.extend(rng.bytes(extra));
                    p
                }
            };
            match dec.decode(&packet, &mut out) {
                Ok(()) => ok += 1,
                Err(_) => err += 1,
            }
            assert!(out.iter().all(|s| s.is_finite()), "packet {n}");
            if n % 7 == 0 {
                dec.conceal(None, &mut out);
                assert!(out.iter().all(|s| s.is_finite()));
            }
        }
        assert!(ok > 0 && err > 0, "both outcomes seen: {ok} ok, {err} err");
    }

    #[test]
    fn opus_rejects_foreign_tocs_it_would_mis_decode() {
        let mut dec = Opus::decoder().expect("decoder");
        let mut out = [0.0; FRAME];
        let mut tail = vec![0x01];
        tail.extend([0x55; 40]);
        // CELT, stereo, 40 ms, code 1, code 2, code 3 with two frames, code 3 without its count.
        for p in [[&[0x80][..], &tail].concat(), [&[0x4C][..], &tail].concat(), [&[0x58][..], &tail].concat()] {
            assert_eq!(dec.decode(&p, &mut out), Err(CodecError::BadHeader), "TOC {:#04x}", p[0]);
        }
        for toc in [0x49, 0x4A] {
            assert_eq!(dec.decode(&[toc, 0x01, 0x55, 0x55], &mut out), Err(CodecError::BadHeader), "code {}", toc & 3);
        }
        assert_eq!(dec.decode(&[0x4B, 0x02, 0x55], &mut out), Err(CodecError::BadHeader), "two frames");
        assert_eq!(dec.decode(&[0x4B], &mut out), Err(CodecError::BadHeader), "no count byte");
        assert_eq!(dec.decode(&[], &mut out), Err(CodecError::BadHeader));
    }

    #[test]
    fn opus_plc_fills_a_missing_frame_with_finite_nonsilent_audio() {
        let mut enc = encoder(24_000, false);
        let mut dec = Opus::decoder().expect("decoder");
        let mut out = [0.0; FRAME];
        let mut last_was_code3 = false;
        for (n, p) in encode_all(&mut enc, (0..40).map(speech_frame)).iter().enumerate() {
            dec.decode(p, &mut out).expect("decodes");
            last_was_code3 = p[0] & 3 == 3;
            if n >= 10 && last_was_code3 {
                break;
            }
        }
        assert!(last_was_code3, "a CBR-padded (code 3) packet came last: PLC must mask it to code 0");
        let mut plc = [0.0; FRAME];
        assert!(dec.conceal(None, &mut plc), "PLC after a code-3 packet");
        assert!(plc.iter().all(|s| s.is_finite()));
        let energy: f32 = plc.iter().map(|s| s * s).sum();
        assert!(energy > 0.0, "the concealment continues the voice");
    }

    #[test]
    fn opus_plc_before_any_packet_is_safe() {
        let mut dec = Opus::decoder().expect("decoder");
        let mut out = [0.0; FRAME];
        let _ = dec.conceal(None, &mut out);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn opus_survives_a_long_plc_run_and_resumes() {
        let mut enc = encoder(24_000, false);
        let mut dec = Opus::decoder().expect("decoder");
        let packets = encode_all(&mut enc, (0..60).map(speech_frame));
        let mut out = [0.0; FRAME];
        for p in &packets[..10] {
            dec.decode(p, &mut out).expect("decodes");
        }
        for _ in 0..200 {
            dec.conceal(None, &mut out);
            assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.5));
        }
        for p in &packets[10..] {
            dec.decode(p, &mut out).expect("resumes");
            assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.5));
        }
    }

    #[test]
    fn opus_reset_starts_a_clean_stream() {
        let mut dec = Opus::decoder().expect("decoder");
        let mut out = [0.0; FRAME];
        for p in encode_all(&mut encoder(24_000, false), (0..20).map(speech_frame)) {
            dec.decode(&p, &mut out).expect("decodes");
        }
        dec.reset();
        for p in encode_all(&mut encoder(16_000, false), (100..105).map(speech_frame)) {
            dec.decode(&p, &mut out).expect("a new stream decodes");
            assert!(out.iter().all(|s| s.is_finite()));
        }
        let mut enc = encoder(24_000, false);
        enc.reset();
        assert_eq!(encode_all(&mut enc, (0..3).map(speech_frame)).iter().map(Vec::len).collect::<Vec<_>>(), vec![60; 3], "a reset encoder keeps its settings");
    }

    #[test]
    fn opus_encoder_with_bad_settings_is_an_error_not_a_panic() {
        assert!(matches!(Opus::encoder(OpusSettings { bitrate_bps: 0, ..default() }), Err(CodecError::BadSettings(_))));
        assert!(matches!(Opus::encoder(OpusSettings { complexity: 200, ..default() }), Err(CodecError::BadSettings(_))));
        assert!(matches!(new_encoder(VoiceCodecChoice::Opus, &OpusSettings { bitrate_bps: 100_000, ..default() }), Err(CodecError::BadSettings(_))));
        let ok = new_encoder(VoiceCodecChoice::Opus, &OpusSettings::QUALITY).expect("valid");
        assert_eq!((ok.id(), ok.bitrate(), ok.max_frame_bytes(), ok.fixed_frame_bytes()), (OPUS_ID, 32_000, 80, None));
    }

    #[test]
    fn opus_state_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Opus>();
        assert_send_sync::<Box<dyn VoiceCodec>>();
        let boxed: Box<dyn VoiceCodec> = Box::new(Opus::decoder().expect("decoder"));
        assert!(boxed.is_stateful());
        assert!(format!("{:?}", Opus::decoder().expect("decoder")).contains("Opus"));
    }

    // -------------------------------------------------------------- mixer with Opus

    fn one_channel() -> Mixer {
        Mixer::new(JitterConfig { target: 3, max: 12, conceal_max: 3 })
    }

    fn mix_one(m: &mut Mixer) -> StereoFrame {
        let mut out = [0.0; FRAME * 2];
        m.mix(&mut out, 1.0, 0.0, 0, |_, _| {});
        out
    }

    const A: SpeakerKey = SpeakerKey::Remote(SpeakerId(1));

    #[test]
    fn opus_packets_arriving_out_of_order_decode_in_sequence_order() {
        let packets = encode_all(&mut encoder(24_000, false), (0..30).map(speech_frame));
        let (mut a, mut b) = (one_channel(), one_channel());
        let (mut out_a, mut out_b) = (Vec::new(), Vec::new());
        for pair in 0..15u32 {
            let (s0, s1) = (pair * 2, pair * 2 + 1);
            a.insert_packet(A, s0, OPUS_ID, &packets[s0 as usize], 0, 0.0);
            a.insert_packet(A, s1, OPUS_ID, &packets[s1 as usize], 0, 0.0);
            // B receives every pair swapped.
            b.insert_packet(A, s1, OPUS_ID, &packets[s1 as usize], 0, 0.0);
            b.insert_packet(A, s0, OPUS_ID, &packets[s0 as usize], 0, 0.0);
            a.set_target(A, 1.0, 1.0);
            b.set_target(A, 1.0, 1.0);
            for _ in 0..2 {
                out_a.extend(mix_one(&mut a));
                out_b.extend(mix_one(&mut b));
            }
        }
        assert!(out_a.iter().any(|s| s.abs() > 0.01), "voice played");
        assert_eq!(out_a, out_b, "the decoder saw both streams in sequence order");
        assert_eq!(a.totals().0.undecodable, 0);
    }

    #[test]
    fn a_lost_opus_frame_is_concealed_by_plc() {
        let packets = encode_all(&mut encoder(24_000, false), (0..20).map(speech_frame));
        let mut m = one_channel();
        let mut next = 0u32;
        let mut feed = |m: &mut Mixer| {
            if next != 10 && (next as usize) < packets.len() {
                m.insert_packet(A, next, OPUS_ID, &packets[next as usize], next, 0.0);
            }
            next += 1;
        };
        for _ in 0..3 {
            feed(&mut m);
        }
        let mut results = Vec::new();
        for _ in 0..16 {
            feed(&mut m);
            let mut frame = [0.0; FRAME];
            let pulled = m.channel_mut(A).expect("channel").pull(&mut frame);
            results.push((pulled, frame));
        }
        let (pulled, frame) = results.iter().find(|(p, _)| *p == Pull::Concealed).expect("frame 10 concealed");
        assert_eq!(*pulled, Pull::Concealed);
        assert!(frame.iter().all(|s| s.is_finite()) && frame.iter().any(|s| s.abs() > 1e-4), "Opus PLC fills the hole");
        let stats = m.channel(A).expect("channel").jitter.stats;
        assert_eq!((stats.lost, stats.undecodable), (1, 0));
        assert!(results.iter().filter(|(p, _)| matches!(p, Pull::Played { .. })).count() >= 12);
    }

    #[test]
    fn an_undecodable_opus_packet_is_concealed_and_counted() {
        let packets = encode_all(&mut encoder(24_000, false), (0..12).map(speech_frame));
        let mut m = one_channel();
        m.set_target(A, 1.0, 1.0);
        for (seq, p) in packets.iter().enumerate() {
            // Seq 6 is a code-3 TOC without its count byte: past `insert_packet`, refused by the codec.
            let bytes: &[u8] = if seq == 6 { &[0x4B] } else { p };
            m.insert_packet(A, seq as u32, OPUS_ID, bytes, 0, 0.0);
            m.set_target(A, 1.0, 1.0);
            assert!(mix_one(&mut m).iter().all(|s| s.is_finite()));
        }
        for _ in 0..6 {
            assert!(mix_one(&mut m).iter().all(|s| s.is_finite()));
        }
        assert_eq!(m.totals().0.undecodable, 1);
    }

    #[test]
    fn a_speaker_switching_codec_mid_stream_resets_cleanly() {
        let mut opus = encoder(24_000, false);
        let mut adpcm = ImaAdpcm::default();
        let mut m = Mixer::new(JitterConfig { target: 1, max: 12, conceal_max: 3 });
        let mut seen = Vec::new();
        for seq in 0..18u32 {
            let pcm = speech_frame(seq as usize);
            if (6..12).contains(&seq) {
                let mut b = Vec::new();
                opus.encode(&pcm, &mut b).expect("encodes");
                m.insert_packet(A, seq, OPUS_ID, &b, 0, 0.0);
            } else {
                let mut b = Vec::new();
                adpcm.encode(&pcm, &mut b).expect("encodes");
                let mut decoded = [0.0; FRAME];
                adpcm.decode(&b, &mut decoded).expect("decodes");
                m.insert(A, seq, &decoded, 0, 0.0);
            }
            let ch = m.channel_mut(A).expect("channel");
            let mut frame = [0.0; FRAME];
            let pulled = ch.pull(&mut frame);
            assert!(frame.iter().all(|s| s.is_finite()), "seq {seq}");
            if matches!(pulled, Pull::Played { .. }) {
                seen.push(ch.decoder_codec());
            }
        }
        assert!(seen.contains(&None) && seen.contains(&Some(OPUS_ID)), "played from both codecs: {seen:?}");
        assert_eq!(seen.last(), Some(&None), "back on ADPCM: the Opus decoder was dropped");
        assert_eq!(m.totals().0.undecodable, 0);
    }

    #[test]
    fn the_opus_tail_fades_out() {
        let packets = encode_all(&mut encoder(24_000, false), (0..8).map(speech_frame));
        let mut m = Mixer::new(JitterConfig { target: 1, max: 12, conceal_max: 3 });
        for (seq, p) in packets.iter().enumerate() {
            m.insert_packet(A, seq as u32, OPUS_ID, p, 0, 0.0);
            let mut f = [0.0; FRAME];
            m.channel_mut(A).expect("channel").pull(&mut f);
        }
        let mut tail = [0.0; FRAME];
        assert_eq!(m.channel_mut(A).expect("channel").pull(&mut tail), Pull::Concealed);
        let energy = |s: &[f32]| s.iter().map(|x| x * x).sum::<f32>();
        assert!(energy(&tail[..80]) > 0.0, "the tail starts from the voice");
        assert!(energy(&tail[240..]) < energy(&tail[..80]), "and fades out toward the frame end");
        assert!(tail[FRAME - 1].abs() < 0.01);
        let mut after = [1.0; FRAME];
        assert_eq!(m.channel_mut(A).expect("channel").pull(&mut after), Pull::Silent);
    }

    #[test]
    fn forgetting_a_speaker_drops_its_decoder() {
        let packets = encode_all(&mut encoder(24_000, false), (0..4).map(speech_frame));
        let mut m = Mixer::new(JitterConfig { target: 1, max: 12, conceal_max: 1 });
        for (seq, p) in packets.iter().enumerate() {
            m.insert_packet(A, seq as u32, OPUS_ID, p, 0, 0.0);
        }
        for _ in 0..10 {
            mix_one(&mut m);
        }
        assert_eq!(m.channel(A).and_then(SpeakerChannel::decoder_codec), Some(OPUS_ID));
        let played = m.totals().0.played;
        m.forget_idle(100.0, 5.0);
        assert!(m.channel(A).is_none(), "the channel and its decoder are gone");
        assert_eq!(m.totals().0.played, played, "its stats are retired, not lost");
    }

    // -------------------------------------------------------------- codec switches in the mixer

    /// One frame of `pcm` as IMA-ADPCM, decoded (what `receive_voice` stores).
    fn adpcm_pcm(enc: &mut ImaAdpcm, pcm: &MonoFrame) -> MonoFrame {
        let mut b = Vec::new();
        enc.encode(pcm, &mut b).expect("encodes");
        let mut out = [0.0; FRAME];
        ImaAdpcm::default().decode(&b, &mut out).expect("decodes");
        out
    }

    /// A lost frame is concealed the way ITS stream's codec does it: IMA-ADPCM by repeating the
    /// last frame at half level, Opus by its own packet loss concealment (never the repeat).
    #[test]
    fn concealment_follows_the_codec_of_the_stream() {
        let mut adpcm = ImaAdpcm::default();
        let mut opus = encoder(24_000, false);
        let mut m = Mixer::new(JitterConfig { target: 1, max: 12, conceal_max: 3 });
        // IMA-ADPCM 0..6 with 3 lost (4 already waiting when 3 is due).
        let mut pulls = Vec::new();
        for seq in 0..6u32 {
            let pcm = adpcm_pcm(&mut adpcm, &speech_frame(seq as usize));
            if seq != 3 {
                m.insert(A, seq, &pcm, 0, 0.0);
            }
            if seq == 3 {
                continue; // nothing pulled while 3 "is in flight"; 4 arrives next
            }
            let mut f = [0.0; FRAME];
            let p = m.channel_mut(A).expect("channel").pull(&mut f);
            pulls.push((p, f));
            if seq == 4 {
                // 3 was due: concealed from frame 2 at half level.
                let (p3, f3) = *pulls.last().expect("pulled");
                assert_eq!(p3, Pull::Concealed);
                let two = pulls[2].1;
                assert!(f3.iter().zip(two.iter()).all(|(c, l)| (c - l * 0.5).abs() < 1e-6), "IMA-ADPCM: the repeat at half level");
            }
        }
        // Opus 6..14 with 10 lost.
        let packets: Vec<Vec<u8>> = (6..14).map(|n| encode_all(&mut opus, std::iter::once(speech_frame(n))).remove(0)).collect();
        let mut prev = [0.0; FRAME];
        let mut concealed = None;
        for (i, seq) in (6..14u32).enumerate() {
            if seq != 10 {
                m.insert_packet(A, seq, OPUS_ID, &packets[i], 0, 0.0);
            }
            if seq == 10 {
                continue;
            }
            let mut f = [0.0; FRAME];
            let p = m.channel_mut(A).expect("channel").pull(&mut f);
            if p == Pull::Concealed && concealed.is_none() && seq > 10 {
                concealed = Some((f, prev));
            }
            if matches!(p, Pull::Played { .. }) {
                prev = f;
            }
        }
        let (plc, before) = concealed.expect("frame 10 concealed");
        assert!(plc.iter().all(|s| s.is_finite()) && plc.iter().any(|s| s.abs() > 1e-4), "Opus PLC continues the voice");
        assert!(plc.iter().zip(before.iter()).any(|(c, l)| (c - l * 0.5).abs() > 1e-3), "not the IMA-ADPCM repeat");
        let stats = m.channel(A).expect("channel").jitter.stats;
        assert_eq!((stats.lost, stats.undecodable), (2, 0));
    }

    /// Two speakers on different codecs into ONE mixer: each keeps its own level (IMA-ADPCM hard
    /// left, Opus hard right), and each channel's stats and decoder are its own.
    #[test]
    fn two_codecs_into_one_mixer_keep_their_levels() {
        let (a, b) = (SpeakerKey::Remote(SpeakerId(3)), SpeakerKey::Remote(SpeakerId(4)));
        let mut adpcm = ImaAdpcm::default();
        let mut opus = encoder(24_000, false);
        let mut m = one_channel();
        let (mut left, mut right, mut input) = (Vec::new(), Vec::new(), Vec::new());
        for seq in 0..40u32 {
            let pcm = sine_frame(300.0, 0.3, seq as usize * FRAME);
            input.extend_from_slice(&pcm);
            m.insert(a, seq, &adpcm_pcm(&mut adpcm, &pcm), 0, 0.0);
            m.insert_packet(b, seq, OPUS_ID, &encode_all(&mut opus, std::iter::once(pcm)).remove(0), 0, 0.0);
            m.set_target(a, 1.0, 0.0);
            m.set_target(b, 0.0, 1.0);
            let out = mix_one(&mut m);
            if seq >= 10 {
                left.extend(out.iter().step_by(2));
                right.extend(out.iter().skip(1).step_by(2));
            }
        }
        let rms = |v: &[f32]| (v.iter().map(|x| x * x).sum::<f32>() / v.len() as f32).sqrt();
        let want = rms(&input);
        for (name, got) in [("IMA-ADPCM (left)", rms(&left)), ("Opus (right)", rms(&right))] {
            assert!((0.7..=1.3).contains(&(got / want)), "{name}: rms {got:.3} vs input {want:.3}");
        }
        let (ca, cb) = (m.channel(a).expect("a"), m.channel(b).expect("b"));
        assert_eq!((ca.decoder_codec(), cb.decoder_codec()), (None, Some(OPUS_ID)), "each channel decodes its own codec");
        assert!(ca.jitter.stats.played >= 35 && cb.jitter.stats.played >= 35);
        assert_eq!(m.totals().0.undecodable, 0);
    }

    /// Loss, reordering and a duplicate right around a codec switch (IMA-ADPCM 0..10, Opus 10..20,
    /// back to IMA-ADPCM 20..30): 9 and 10 swapped, 11 twice, 12 lost, 20 and 21 swapped.
    #[test]
    fn loss_reordering_and_duplicates_around_a_codec_switch() {
        let mut adpcm = ImaAdpcm::default();
        let mut opus = encoder(24_000, false);
        let mut m = one_channel();
        m.set_config(JitterConfig { target: 3, max: 12, conceal_max: 3 });
        let order: Vec<u32> = [(0..9).collect::<Vec<u32>>(), vec![10, 9, 11, 11], (13..20).collect(), vec![21, 20], (22..30).collect()].concat();
        let is_opus = |seq: u32| (10..20).contains(&seq);
        let mut codecs_played = Vec::new();
        for (n, &seq) in order.iter().enumerate() {
            let pcm = speech_frame(seq as usize);
            if is_opus(seq) {
                let p = encode_all(&mut opus, std::iter::once(pcm)).remove(0);
                m.insert_packet(A, seq, OPUS_ID, &p, 0, 0.0);
            } else {
                m.insert(A, seq, &adpcm_pcm(&mut adpcm, &pcm), 0, 0.0);
            }
            // One pull per packet received, like a steady 50 packets per second.
            let mut f = [0.0; FRAME];
            let ch = m.channel_mut(A).expect("channel");
            let p = ch.pull(&mut f);
            assert!(f.iter().all(|s| s.is_finite() && s.abs() <= 1.5), "packet {n} (seq {seq})");
            if matches!(p, Pull::Played { .. }) {
                codecs_played.push(ch.decoder_codec());
            }
        }
        for _ in 0..6 {
            let mut f = [0.0; FRAME];
            m.channel_mut(A).expect("channel").pull(&mut f);
            assert!(f.iter().all(|s| s.is_finite()));
        }
        let stats = m.channel(A).expect("channel").jitter.stats;
        assert_eq!((stats.duplicate, stats.lost, stats.undecodable), (1, 1, 0), "{stats:?}");
        assert!(codecs_played.contains(&Some(OPUS_ID)) && codecs_played.first() == Some(&None) && codecs_played.last() == Some(&None), "{codecs_played:?}");
    }
}

// ------------------------------------------------------------------ packets (0.2)

#[test]
fn validate_packet_accepts_each_known_codec() {
    assert_eq!(validate_packet(ImaAdpcm::ID, &[0; 164]), Ok(()));
    let mut adpcm = vec![0; 164];
    adpcm[2] = 88;
    assert_eq!(validate_packet(ImaAdpcm::ID, &adpcm), Ok(()));
    let with = |head: &[u8], len: usize| -> Vec<u8> {
        let mut v = head.to_vec();
        v.resize(len, 0x5A);
        v
    };
    for p in [vec![0x48], with(&[0x48], 60), with(&[0x4B, 0x01], 60), with(&[0x4B, 0x41], 60), with(&[0x08], 30), with(&[0x28], 40), with(&[0x48], 700)] {
        assert_eq!(validate_packet(OPUS_ID, &p), Ok(()), "{:02x?}", &p[..p.len().min(2)]);
        assert!(opus_toc_ok(&p));
    }
}

#[test]
fn validate_packet_rejects_wrong_codec_size_and_malformed() {
    for id in [0, Pcm16::ID, 7] {
        assert_eq!(validate_packet(id, &[0x48; 60]), Err(Reject::WrongCodec), "id {id}");
    }
    assert_eq!(validate_packet(Pcm16::ID, &[0; 640]), Err(Reject::WrongCodec), "Pcm16 is never on the wire");
    assert_eq!(validate_packet(ImaAdpcm::ID, &[0; 163]), Err(Reject::BadSize));
    assert_eq!(validate_packet(ImaAdpcm::ID, &[0; 165]), Err(Reject::BadSize));
    assert_eq!(validate_packet(OPUS_ID, &[]), Err(Reject::BadSize));
    assert_eq!(validate_packet(OPUS_ID, &[0x48; 701]), Err(Reject::BadSize));
    let mut adpcm = vec![0; 164];
    adpcm[2] = 89;
    assert_eq!(validate_packet(ImaAdpcm::ID, &adpcm), Err(Reject::Malformed), "a step index past the table");
    for p in [&[0x80, 1, 2][..], &[0x4C, 1, 2], &[0x58, 1, 2], &[0x49, 1, 2], &[0x4B, 0x02, 2], &[0x4B]] {
        assert_eq!(validate_packet(OPUS_ID, p), Err(Reject::Malformed), "{:02x?}", p);
    }
}

// ------------------------------------------------------------------ jitter core (0.2)

#[test]
fn jitter_core_reports_play_tail_conceal_and_silent_in_the_old_order() {
    // The same script as `the_jitter_buffer_conceals_a_lost_frame_then_catches_up`.
    let mut j: JitterCore<u32> = JitterCore::new(JitterConfig { target: 1, max: 10, conceal_max: 2 });
    j.insert(0, 8, 0);
    j.insert(2, 4, 0);
    assert_eq!(j.next_frame(), Next::Play { payload: 8, ts: 0 });
    assert_eq!(j.next_frame(), Next::Conceal { run: 1, next: Some(&4) }, "a loss, with the next packet visible");
    assert_eq!(j.next_frame(), Next::Play { payload: 4, ts: 0 });
    assert_eq!(j.next_frame(), Next::Tail);
    assert_eq!(j.next_frame(), Next::Silent);
    assert!(j.is_playing());
    assert_eq!(j.next_frame(), Next::Silent);
    assert!(!j.is_playing(), "gave up after conceal_max empty pulls");
    assert_eq!((j.stats.lost, j.stats.concealed, j.stats.played), (1, 2, 2));
    // Buffering before a spurt, then a long hole that skips ahead.
    let mut k: JitterCore<u32> = JitterCore::new(JitterConfig { target: 2, max: 10, conceal_max: 1 });
    k.insert(0, 1, 0);
    assert_eq!(k.next_frame(), Next::Silent, "buffering");
    assert_eq!(k.next_frame(), Next::Play { payload: 1, ts: 0 }, "waited target pulls");
    k.insert(5, 6, 0);
    assert_eq!(k.next_frame(), Next::Conceal { run: 1, next: None });
    assert_eq!(k.next_frame(), Next::Silent, "past conceal_max: jump");
    assert_eq!(k.next_frame(), Next::Play { payload: 6, ts: 0 });
}

#[test]
fn a_sender_restart_bumps_the_generation() {
    let mut j: JitterCore<u32> = JitterCore::new(JitterConfig { target: 1, max: 4, conceal_max: 1 });
    assert_eq!(j.generation(), 0);
    j.insert(1000, 1, 0);
    let _ = j.next_frame();
    assert_eq!(j.insert(1001, 2, 0), Insert::Stored);
    assert_eq!(j.generation(), 0, "an ordinary packet is not a restart");
    assert_eq!(j.insert(0, 3, 0), Insert::Stored);
    assert_eq!(j.generation(), 1, "seq far behind the playout point = a restarted sender");
}

#[test]
fn adpcm_through_the_mixer_is_sample_identical_to_the_old_jitter_path() {
    enum Ev {
        In(u32),
        Pull,
    }
    use Ev::{In, Pull as P};
    // Reorder, a duplicate, a late frame, a loss (4), an underflow wait (8 arrives late) and a
    // spurt end + restart of a new spurt.
    let script = [
        In(1),
        P,
        In(0),
        In(2),
        In(2),
        P,
        P,
        P,
        In(1),
        In(3),
        In(5),
        In(6),
        P,
        P,
        P,
        P,
        In(7),
        P,
        P,
        P,
        In(8),
        In(9),
        P,
        P,
        P,
        P,
        P,
        P,
        P,
        In(20),
        In(21),
        In(22),
        P,
        P,
        P,
        P,
        P,
    ];
    let mut enc = ImaAdpcm::default();
    let mut dec = ImaAdpcm::default();
    let frames: Vec<MonoFrame> = (0..30)
        .map(|n| {
            let mut b = Vec::new();
            enc.encode(&speech_frame(n), &mut b).expect("encodes");
            let mut f = [0.0; FRAME];
            dec.decode(&b, &mut f).expect("decodes");
            f
        })
        .collect();
    let cfg = JitterConfig { target: 2, max: 8, conceal_max: 2 };
    let key = SpeakerKey::Remote(SpeakerId(9));
    let mut mixer = Mixer::new(cfg);
    let mut old = JitterBuffer::new(cfg);
    let mut pulls = 0;
    for ev in &script {
        match ev {
            In(seq) => {
                mixer.insert(key, *seq, &frames[*seq as usize], *seq, 0.0);
                mixer.set_target(key, 1.0, 1.0);
                old.insert(*seq, &frames[*seq as usize], *seq);
            }
            P => {
                let mut got = [0.0; FRAME * 2];
                mixer.mix(&mut got, 1.0, 0.0, 0, |_, _| {});
                let mut mono = [0.0; FRAME];
                old.pull(&mut mono);
                // The mixer's gain ramps from 0 on a channel's first pull, then stays at 1.
                let g0 = if pulls == 0 { 0.0 } else { 1.0 };
                pulls += 1;
                for (i, s) in mono.iter().enumerate() {
                    let g = g0 + (1.0 - g0) * ((i + 1) as f32 / FRAME as f32);
                    let want = dsp::soft_clip(s * g);
                    assert_eq!((got[i * 2], got[i * 2 + 1]), (want, want), "pull {pulls}, sample {i}");
                }
            }
        }
    }
    let (totals, _) = mixer.totals();
    assert_eq!(totals, old.stats, "identical counters");
    assert!(totals.lost >= 1 && totals.late >= 1 && totals.duplicate >= 1 && totals.concealed >= 2, "{totals:?}");
}

// ------------------------------------------------------------------ config (0.2)

#[test]
fn an_old_config_file_without_codec_fields_still_loads() {
    let cfg: VoiceChatConfig = ron::from_str("(hearing: Global, jitter_target_ms: 80.0)").expect("parses");
    assert_eq!((cfg.codec, cfg.opus), (VoiceCodecChoice::ImaAdpcm, OpusSettings::default()));
    assert!(cfg.problems().is_empty());
    let full = "(hearing: Proximity, range: 20.0, full_volume_distance: 2.0, rolloff: 1.5, pan_strength: 0.6, jitter_target_ms: 60.0, \
                jitter_max_ms: 240.0, conceal_max_ms: 60.0, vad_hangover_ms: 300.0, vad_preroll_ms: 40.0, ptt_release_ms: 120.0, \
                output_queue_ms: 40.0, mic_backlog_frames: 5, speaker_timeout_secs: 5.0, heard_hold_ms: 250.0)";
    let old: VoiceChatConfig = ron::from_str(full).expect("every 0.1 field");
    assert_eq!(old, VoiceChatConfig::default());
}

#[test]
fn codec_and_opus_fields_deserialise() {
    let opus: VoiceChatConfig = ron::from_str("(codec: Opus)").expect("parses");
    assert_eq!(opus.codec, VoiceCodecChoice::Opus);
    let rate: VoiceChatConfig = ron::from_str("(opus: (bitrate_bps: 32000))").expect("parses");
    assert_eq!(rate.opus, OpusSettings { bitrate_bps: 32_000, complexity: 5, vbr: false }, "a partial block keeps the other defaults");
    let vbr: VoiceChatConfig = ron::from_str("(codec: Opus, opus: (vbr: true, complexity: 2))").expect("parses");
    assert_eq!(vbr.opus, OpusSettings { bitrate_bps: 24_000, complexity: 2, vbr: true });
    assert!(ron::from_str::<VoiceChatConfig>("(codec: Flac)").is_err());
    assert!(ron::from_str::<VoiceChatConfig>("(opus: (fec: true))").is_err(), "no FEC knob (deny_unknown_fields)");
    let round: OpusSettings = ron::from_str(&ron::to_string(&OpusSettings::QUALITY).expect("serialises")).expect("parses");
    assert_eq!(round, OpusSettings::QUALITY);
}

#[test]
fn problems_lists_bad_opus_settings_and_a_missing_feature() {
    let bad_rate = VoiceChatConfig { opus: OpusSettings { bitrate_bps: 1_000, ..default() }, ..default() };
    assert_eq!(bad_rate.problems(), vec!["opus.bitrate_bps must be 6000..=64000, opus.complexity 0..=10"], "listed even while sending ADPCM");
    let bad_cx = VoiceChatConfig { codec: VoiceCodecChoice::Opus, opus: OpusSettings { complexity: 11, ..default() }, ..default() };
    assert!(bad_cx.problems().contains(&"opus.bitrate_bps must be 6000..=64000, opus.complexity 0..=10"));
    let opus = VoiceChatConfig { codec: VoiceCodecChoice::Opus, ..default() };
    let missing = opus.problems().contains(&"codec: Opus needs the `opus` cargo feature");
    #[cfg(feature = "opus")]
    assert!(!missing && opus.problems().is_empty());
    #[cfg(not(feature = "opus"))]
    assert!(missing);
}

// ---- device labels (cpal_io: what the settings list shows and a game stores) ----

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn listed(raw: &[&str], number: bool) -> Vec<Option<String>> {
    crate::cpal_io::device_labels(&strings(raw), number)
}

#[test]
fn a_windows_device_is_labelled_with_its_friendly_name() {
    use crate::cpal_io::raw_label;
    // WASAPI: short name + the full friendly name as an extended line.
    assert_eq!(
        raw_label("Microphone", &strings(&["Microphone (USB PnP Audio Device)"]), Some("USB PnP Audio Device"), true),
        "Microphone (USB PnP Audio Device)"
    );
    // No friendly line: "name (interface)" from the driver field.
    assert_eq!(raw_label("Speakers", &[], Some("Realtek USB2.0 Audio"), true), "Speakers (Realtek USB2.0 Audio)");
    // The name already IS the friendly name (DeviceDesc missing): not doubled.
    assert_eq!(raw_label("Speakers (Realtek USB2.0 Audio)", &[], Some("Realtek USB2.0 Audio"), true), "Speakers (Realtek USB2.0 Audio)");
    // Nothing more to go on: the name, trimmed.
    assert_eq!(raw_label("  Headphones ", &strings(&["Headphones"]), None, true), "Headphones");
    // Only "<name> (...)" counts as a friendly name: a longer name sharing the prefix does not.
    assert_eq!(raw_label("Mic", &strings(&["Microphone (X)"]), None, true), "Mic");
}

#[test]
fn a_linux_card_keeps_its_plain_name() {
    use crate::cpal_io::raw_label;
    // ALSA: extended = every DESC line (the first repeats the name), driver = the PCM id.
    let ext = strings(&["HDA Intel PCH, ALC892 Analog", "Front output / input"]);
    assert_eq!(raw_label("HDA Intel PCH, ALC892 Analog", &ext, Some("front:CARD=PCH,DEV=0"), false), "HDA Intel PCH, ALC892 Analog");
}

#[test]
fn every_windows_device_is_listed_and_a_repeated_label_is_numbered() {
    let raw = ["Microphone (USB PnP Audio Device)", "Headset Microphone (Oculus)", "Microphone (USB PnP Audio Device)", "Microphone (USB PnP Audio Device)"];
    let got: Vec<String> = listed(&raw, true).into_iter().flatten().collect();
    assert_eq!(
        got,
        strings(&[
            "Microphone (USB PnP Audio Device)",
            "Headset Microphone (Oculus)",
            "Microphone (USB PnP Audio Device) #2",
            "Microphone (USB PnP Audio Device) #3"
        ]),
        "no device is dropped, each label is unique"
    );
    assert!(listed(&[], true).is_empty());
}

#[test]
fn numbering_never_takes_a_label_a_real_device_has() {
    // A real device called "Mic #2" keeps its label, wherever it is in the list.
    let got: Vec<String> = listed(&["Mic", "Mic", "Mic #2"], true).into_iter().flatten().collect();
    assert_eq!(got, strings(&["Mic", "Mic #3", "Mic #2"]));
    let got: Vec<String> = listed(&["Mic #2", "Mic", "Mic"], true).into_iter().flatten().collect();
    assert_eq!(got, strings(&["Mic #2", "Mic", "Mic #3"]));
}

#[test]
fn a_linux_card_listed_once_per_pcm_mode_is_one_choice() {
    // ALSA lists one card as sysdefault / front / hw / plughw / dsnoop ... all with one name.
    let card = "HDA Intel PCH, ALC892 Analog";
    let got = listed(&[card, card, card, "USB Audio Device", card], false);
    assert_eq!(got, vec![Some(card.to_string()), None, None, Some("USB Audio Device".to_string()), None], "merged like 0.2.0, never \"#2\"..\"#8\"");
}

#[test]
fn two_microphones_with_the_same_short_name_become_two_choices() {
    use crate::cpal_io::raw_label;
    // The owner's PC: a USB mic and Steam's streaming mic, both called "Microphone" by Windows.
    let raw = vec![
        raw_label("Microphone", &strings(&["Microphone (USB PnP Audio Device)"]), None, true),
        raw_label("Headset Microphone", &strings(&["Headset Microphone (Oculus Virtual Audio Device)"]), None, true),
        raw_label("Microphone", &strings(&["Microphone (Steam Streaming Microphone)"]), None, true),
    ];
    let labels: Vec<String> = crate::cpal_io::device_labels(&raw, true).into_iter().flatten().collect();
    assert_eq!(labels.len(), 3, "0.2.0 listed only 2 of these (the second \"Microphone\" was dropped)");
    assert_ne!(labels[0], labels[2]);
}

#[test]
fn a_setting_picks_its_label_and_a_plain_name_from_0_2_0_still_works() {
    use crate::cpal_io::match_setting;
    let labels = strings(&["Microphone (USB PnP Audio Device)", "Headset Microphone (Oculus)", "Microphone (Steam Streaming Microphone)"]);
    let names = strings(&["Microphone", "Headset Microphone", "Microphone"]);
    assert_eq!(match_setting(&labels, &names, "Microphone (Steam Streaming Microphone)"), Some(2), "the exact label");
    assert_eq!(match_setting(&labels, &names, "Microphone"), Some(0), "an old setting (plain name) = the first such device, as in 0.2.0");
    assert_eq!(match_setting(&labels, &names, " Headset Microphone (Oculus) "), Some(1), "trimmed");
    assert_eq!(match_setting(&labels, &names, "Unplugged Mic"), None, "missing = the caller falls back to the system default");
    let labels = strings(&["Mic (X)", "Mic (X) #2"]);
    let names = strings(&["Mic", "Mic"]);
    assert_eq!(match_setting(&labels, &names, "Mic (X) #2"), Some(1), "a numbered duplicate is its own choice");
}

#[test]
fn a_setting_names_its_label_or_the_plain_name_it_came_from() {
    use crate::io::setting_names_label;
    assert!(setting_names_label("Microphone (USB PnP Audio Device)", "Microphone (USB PnP Audio Device)"));
    assert!(setting_names_label("Microphone", "Microphone (USB PnP Audio Device)"), "a pre-0.2.1 plain name");
    assert!(setting_names_label("Microphone", "Microphone #2"));
    assert!(setting_names_label(" Microphone ", "Microphone (X)"), "trimmed");
    assert!(!setting_names_label("Mic", "Microphone (X)"), "a prefix of the WORD is not the name");
    assert!(!setting_names_label("", "Microphone"), "an empty setting names nothing");
    assert!(!setting_names_label("Speakers", "Microphone (X)"));
}
