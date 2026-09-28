//! A settings-screen style mic test: lists the devices, opens the default microphone with the
//! loopback on (you hear yourself through filters, codec, jitter buffer and mixer — nothing is
//! sent), prints the live level, device status and codec twice a second, and quits after 15
//! seconds. An outgoing filter (a crude "robot" ring modulator) shows the filter hook; `--clean`
//! leaves it out so you can judge the codec itself.
//!
//! `cargo run --example mic_test`
//! `cargo run --example mic_test -- path/to/voice.wav` (a WAV file as the microphone, looped)
//! `cargo run --example mic_test --features opus -- --opus` (hear yourself through Opus, 24 kbps)
//! `cargo run --example mic_test --features opus -- --opus --kbps 16` (or 32; any 6..=64)
//! `cargo run --example mic_test --features opus -- --cycle` (switch codec at runtime every 5 s:
//! IMA-ADPCM, Opus 16, 24 and 32 kbps, then IMA-ADPCM again)
//!
//! Without the `opus` feature, `--opus` shows the error state instead (no fallback).

use bevy::prelude::*;
use bevy_voice_chat::prelude::*;
use std::sync::Arc;

/// Seconds per step of `--cycle`.
const CYCLE_SECS: f64 = 5.0;

/// What `--cycle` steps through.
const CYCLE: [(VoiceCodecChoice, u32); 5] = [
    (VoiceCodecChoice::ImaAdpcm, 24_000),
    (VoiceCodecChoice::Opus, 16_000),
    (VoiceCodecChoice::Opus, 24_000),
    (VoiceCodecChoice::Opus, 32_000),
    (VoiceCodecChoice::ImaAdpcm, 24_000),
];

#[derive(Resource, Clone, Copy)]
struct Options {
    cycle: bool,
    run_secs: f64,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config = VoiceChatConfig::default();
    let (mut wav, mut clean, mut cycle) = (None, false, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--opus" => config.codec = VoiceCodecChoice::Opus,
            "--clean" => clean = true,
            "--cycle" => cycle = true,
            "--kbps" => {
                i += 1;
                match args.get(i).and_then(|v| v.parse::<u32>().ok()) {
                    Some(k) => config.opus.bitrate_bps = k.saturating_mul(1000),
                    None => eprintln!("--kbps needs a number (e.g. 16, 24, 32)"),
                }
            }
            path => wav = Some(path.to_string()),
        }
        i += 1;
    }
    if !config.problems().is_empty() {
        eprintln!("config problems: {:?}", config.problems());
    }
    let run_secs = if cycle { CYCLE_SECS * CYCLE.len() as f64 } else { 15.0 };

    let mut app = App::new();
    app.add_plugins((DefaultPlugins, VoiceChatPlugin { config }));
    if let Some(path) = wav {
        match WavFileIo::load(std::path::Path::new(&path), Arc::new(CpalIo)) {
            Ok(wav) => {
                app.insert_resource(VoiceIo(Arc::new(wav)));
            }
            Err(e) => eprintln!("{e} - using the microphone"),
        }
    }
    if !clean {
        app.world_mut().resource_mut::<VoiceFilters>().add(FilterStage::Outgoing, 0, Robot { phase: 0.0 });
    }
    app.insert_resource(Options { cycle, run_secs })
        .add_systems(Update, (test_mic, cycle_codec).before(VoiceChatSystems::Devices))
        .add_systems(Update, report.after(VoiceChatSystems::Playback))
        .run();
}

/// A ring modulator at 50 Hz: the classic robot voice.
struct Robot {
    phase: f32,
}

impl VoiceFilter for Robot {
    fn name(&self) -> &str {
        "robot"
    }

    fn process(&mut self, frame: &mut [f32], ctx: &FilterContext) {
        let step = std::f32::consts::TAU * 50.0 / ctx.sample_rate as f32;
        for s in frame.iter_mut() {
            *s *= self.phase.sin();
            self.phase = (self.phase + step) % std::f32::consts::TAU;
        }
    }
}

fn test_mic(mut input: ResMut<VoiceInput>) {
    input.meter = true;
    input.loopback = true;
    input.mode = TalkMode::VoiceActivity;
    input.threshold = 0.05;
}

/// `--cycle`: a runtime codec change every few seconds (applies to the next frame).
fn cycle_codec(time: Res<Time<Real>>, options: Res<Options>, mut config: ResMut<VoiceChatConfig>) {
    if !options.cycle {
        return;
    }
    let step = ((time.elapsed_secs_f64() / CYCLE_SECS) as usize).min(CYCLE.len() - 1);
    let (codec, bitrate_bps) = CYCLE[step];
    if config.codec != codec || config.opus.bitrate_bps != bitrate_bps {
        config.codec = codec;
        config.opus.bitrate_bps = bitrate_bps;
        println!("--- now: {codec:?} ({} kbps if Opus)", bitrate_bps / 1000);
    }
}

#[allow(clippy::too_many_arguments)]
fn report(
    time: Res<Time<Real>>,
    options: Res<Options>,
    devices: Res<VoiceDevices>,
    state: Res<VoiceChatState>,
    stats: Res<VoiceStats>,
    runtime: Res<bevy_voice_chat::VoiceRuntime>,
    mut next: Local<f64>,
    mut exit: MessageWriter<AppExit>,
) {
    let now = time.elapsed_secs_f64();
    if now < *next {
        return;
    }
    if *next == 0.0 {
        println!("inputs: {:?}\noutputs: {:?}", devices.inputs, devices.outputs);
    }
    *next = now + 0.5;
    let bar = "#".repeat((state.mic_level * 40.0) as usize);
    let codec = match (runtime.send_codec(), &state.codec_error) {
        (Some(c), _) => format!("{} {:.1} kbps", c.name(), f64::from(c.bitrate()) / 1000.0),
        (None, Some(e)) => format!("NOT SENDING: {e}"),
        (None, None) => "no encoder".to_string(),
    };
    println!(
        "{:?} ({}) | output {:?} | level {:<40} | {codec} | looped back {} frames, {} encode errors",
        state.mic, state.mic_kind, state.output, bar, stats.loopback, stats.encode_errors
    );
    if now > options.run_secs {
        exit.write(AppExit::Success);
    }
}
