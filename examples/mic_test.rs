//! A settings-screen style mic test: lists the devices, opens the default microphone with the
//! loopback on (you hear yourself through filters, codec, jitter buffer and mixer — nothing is
//! sent), prints the live level and device status twice a second, and quits after 15 seconds.
//! An outgoing filter (a crude "robot" ring modulator) shows the filter hook.
//!
//! Pass a WAV file to use it as the microphone instead (looped):
//!
//! `cargo run --example mic_test`
//! `cargo run --example mic_test -- path/to/voice.wav`

use bevy::prelude::*;
use bevy_voice_chat::prelude::*;
use std::sync::Arc;

fn main() {
    let mut app = App::new();
    app.add_plugins((DefaultPlugins, VoiceChatPlugin::default()));
    if let Some(path) = std::env::args().nth(1) {
        match WavFileIo::load(std::path::Path::new(&path), Arc::new(CpalIo)) {
            Ok(wav) => {
                app.insert_resource(VoiceIo(Arc::new(wav)));
            }
            Err(e) => eprintln!("{e} - using the microphone"),
        }
    }
    app.world_mut().resource_mut::<VoiceFilters>().add(FilterStage::Outgoing, 0, Robot { phase: 0.0 });
    app.add_systems(Update, test_mic.before(VoiceChatSystems::Devices)).add_systems(Update, report.after(VoiceChatSystems::Playback)).run();
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

fn report(
    time: Res<Time<Real>>,
    devices: Res<VoiceDevices>,
    state: Res<VoiceChatState>,
    stats: Res<VoiceStats>,
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
    println!("{:?} ({}) | output {:?} | level {:<40} | looped back {} frames", state.mic, state.mic_kind, state.output, bar, stats.loopback);
    if now > 15.0 {
        exit.write(AppExit::Success);
    }
}
