//! The worker must listen again after an idle spell. Before the fix only the first model load opened the mic tap:
//! after Auto (or Accent) was switched off and on, detection never heard the mic again and Re-detect showed
//! "Not started yet" for the rest of the session (25 Nov).
//!
//! Uses the real models: `GENDER_MODELS_DIR` (default app/assets/app_models) and `ORT_DLL` (default
//! app/assets/onnxruntime.dll). Without them the test prints SKIPPED and passes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use dsp_core::{Stage, FRAME_SAMPLES};
use gender::pipeline::{start, GenderConfig, GenderHandle, GenderStage, S_LISTENING};

fn setup() -> Option<(GenderStage, GenderHandle)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets");
    let dir = std::env::var_os("GENDER_MODELS_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("app_models"));
    let dll = std::env::var_os("ORT_DLL").map(PathBuf::from).unwrap_or_else(|| root.join("onnxruntime.dll"));
    if !dir.join("ecapa.onnx").is_file() || !dll.is_file() {
        eprintln!("SKIPPED: models or onnxruntime.dll not found");
        return None;
    }
    Some(start(GenderConfig { models_dir: Some(dir), onnxruntime_dll: dll, known: None, enabled: false }))
}

/// A voiced, speech-like 48 kHz signal (130 Hz harmonics, syllable-rate loudness changes).
fn speech_frame(n: &mut usize) -> [f32; FRAME_SAMPLES] {
    let mut f = [0.0f32; FRAME_SAMPLES];
    for v in f.iter_mut() {
        let t = *n as f32 / 48_000.0;
        let env = 0.6 + 0.4 * (2.0 * std::f32::consts::PI * 4.0 * t).sin();
        *v = env * (1..8).map(|h| (2.0 * std::f32::consts::PI * 130.0 * h as f32 * t).sin() / h as f32).sum::<f32>() * 0.2;
        *n += 1;
    }
    f
}

/// Feeds speech (faster than real time, but slowly enough for the 4 s ring) until `done` or the timeout.
fn feed_until(stage: &mut GenderStage, n: &mut usize, secs: u64, done: impl Fn() -> bool) -> bool {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        if done() {
            return true;
        }
        for _ in 0..20 {
            stage.process(&mut speech_frame(n));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

#[test]
fn auto_hears_the_mic_again_after_being_switched_off_and_on() {
    let Some((mut stage, h)) = setup() else { return };
    let mut n = 0usize;

    h.set_enabled(true);
    assert!(feed_until(&mut stage, &mut n, 60, || h.stats().0 > 2_000), "first listen heard {} ms", h.stats().0);
    h.set_enabled(false);
    std::thread::sleep(Duration::from_millis(600));
    assert_ne!(h.state(), S_LISTENING, "idle while Auto is off");

    for round in 1..=2 {
        let before = h.stats().0;
        h.set_enabled(true);
        assert!(
            feed_until(&mut stage, &mut n, 60, || h.stats().0 > before + 2_000),
            "round {round}: heard nothing after switching back on (state {})",
            h.state()
        );
        h.set_enabled(false);
        std::thread::sleep(Duration::from_millis(600));
    }
}

#[test]
fn re_detect_starts_from_nothing_and_listens() {
    let Some((mut stage, h)) = setup() else { return };
    let mut n = 0usize;
    h.set_enabled(true);
    assert!(feed_until(&mut stage, &mut n, 60, || h.stats().0 > 2_000));
    h.reset(None);
    assert_eq!((h.gender(), h.confirmed()), (None, false), "Re-detect forgets the answer at once");
    std::thread::sleep(Duration::from_millis(300));
    assert!(feed_until(&mut stage, &mut n, 60, || h.stats().0 > 2_000), "listening again after Re-detect");
}
