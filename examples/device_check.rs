//! A raw hardware check without Bevy (needs a sound card): lists this machine's audio devices,
//! records the default microphone for half a second (peak + frame count) and plays half a second
//! of a quiet 440 Hz tone on the default output through the voice output pump.
//!
//! `cargo run --example device_check`

use bevy_voice_chat::codec::{FRAME, VOICE_RATE};
use bevy_voice_chat::cpal_io::CpalIo;
use bevy_voice_chat::io::AudioIo;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn main() {
    let io = CpalIo;
    let job = io.start_scan();
    let lists = loop {
        if let Some(l) = job.take() {
            break l;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    println!("inputs:  {:?}\noutputs: {:?}", lists.inputs, lists.outputs);

    let mic = io.open_mic(None);
    let (mut peak, mut frames) = (0.0_f32, Vec::new());
    for _ in 0..25 {
        std::thread::sleep(Duration::from_millis(20));
        peak = peak.max(mic.shared.take_peak());
        mic.drain(&mut frames);
    }
    println!("microphone {:?}: peak {peak:.3} over 0.5 s, {} frames of 20 ms", mic.shared.status(), frames.len());
    drop(mic);

    let out = io.open_output(None);
    for n in 0..25 {
        let mut st = [0.0; FRAME * 2];
        for i in 0..FRAME {
            let s = 0.05 * (std::f32::consts::TAU * 440.0 * (n * FRAME + i) as f32 / VOICE_RATE as f32).sin();
            st[i * 2] = s;
            st[i * 2 + 1] = s;
        }
        let _ = out.frames.try_send(st);
        std::thread::sleep(Duration::from_millis(20));
    }
    println!(
        "output {:?} at {} Hz: {} frames played, device latency {} us",
        out.shared.status(),
        out.shared.rate.load(Ordering::Relaxed),
        out.shared.consumed.load(Ordering::Relaxed),
        out.shared.latency_us.load(Ordering::Relaxed)
    );
}
