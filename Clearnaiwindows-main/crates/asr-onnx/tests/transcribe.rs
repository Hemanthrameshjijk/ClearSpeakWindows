//! Golden check against the Python reference (`tools/make_golden.py`): log-mel features and the decoded text of the
//! 5 s accent test clip. The model test needs the model folder (ASR_MODELS_DIR, asr_models next to the exe, or
//! %LOCALAPPDATA%\ClearSpeak\asr_models) and onnxruntime.dll; without them it skips with a message.

use asr_onnx::features::Features;
use std::path::Path;

fn read_f32(p: &Path) -> Vec<f32> {
    std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())).chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

fn golden(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(name)
}

fn clip() -> Vec<f32> {
    read_f32(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../accent-onnx/tests/golden/input16.f32"))
}

#[test]
fn features_match_python() {
    let want = read_f32(&golden("asr_feat.f32"));
    let (got, frames) = Features::new().compute(&clip()).unwrap();
    assert_eq!(got.len(), want.len(), "frames {frames}");
    let max = got.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    println!("max abs feature diff {max:.2e}");
    assert!(max < 2e-3, "features differ from the Python reference by {max}");
}

#[test]
fn text_matches_python() {
    let Some(dir) = asr_onnx::find_models_dir() else {
        eprintln!("SKIP: speech model folder not found");
        return;
    };
    let dll = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/onnxruntime.dll");
    accent_onnx::ort_util::init_ort(&dll).expect("onnxruntime.dll");
    let mut m = asr_onnx::AsrModel::load(&dir, 2).expect("load");
    let want = std::fs::read_to_string(golden("asr_text.txt")).unwrap();
    let got = m.transcribe(&clip()).unwrap();
    println!("got:  {got}\nwant: {want}");
    assert_eq!(got, want);
    // long audio (3 clips, ~16 s) is decoded in windows: every clip must still come out ("insurance" x3)
    let mut long = Vec::new();
    for _ in 0..3 {
        long.extend(clip());
        long.extend(std::iter::repeat(0.0f32).take(4_800));
    }
    let text = m.transcribe(&long).unwrap();
    println!("long: {text}");
    assert!(text.matches("insurance").count() >= 3, "long audio lost words: {text}");
    // silence gives no text
    assert_eq!(m.transcribe(&vec![0.0; 16_000]).unwrap(), "");
}

/// The live pipeline under a busy PC: speech said while the model is held back is transcribed afterwards, nothing
/// lost (the engine used to stop feeding the transcript instead).
#[test]
fn speech_said_while_busy_is_transcribed_later() {
    use dsp_core::{Stage, FRAME_SAMPLES};
    use std::time::{Duration, Instant};
    let Some(dir) = asr_onnx::find_models_dir().or_else(|| {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/asr_models");
        d.join(asr_onnx::model::MODEL_FILE).is_file().then_some(d)
    }) else {
        eprintln!("SKIP: speech model folder not found");
        return;
    };
    let dll = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/onnxruntime.dll");
    let mut cfg = asr_onnx::pipeline::TranscriptConfig::new(dll);
    cfg.models_dir = Some(dir);
    cfg.initial_enabled = true;
    let (mut stage, h) = asr_onnx::pipeline::start(cfg);
    let t0 = Instant::now();
    while h.status().state != asr_onnx::pipeline::TranscriptState::Listening {
        assert!(t0.elapsed() < Duration::from_secs(60), "model did not load: {}", h.status().line());
        std::thread::sleep(Duration::from_millis(50));
    }
    // three sentences at 48 kHz (each 16 kHz sample repeated 3x), 1.5 s of silence after each
    let mut x48: Vec<f32> = Vec::new();
    for _ in 0..3 {
        x48.extend(clip().iter().flat_map(|&s| [s, s, s]));
        x48.extend(std::iter::repeat(0.0).take(72_000));
    }
    stage.set_busy(true);
    for f in x48.chunks_exact(FRAME_SAMPLES) {
        let mut frame = f.to_vec();
        stage.process(&mut frame);
        std::thread::sleep(Duration::from_micros(500));
    }
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(h.status().line_count, 0, "a busy PC must not run the speech model");
    stage.set_busy(false);
    let t1 = Instant::now();
    while h.status().line_count < 3 && t1.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(100));
    }
    let (lines, _) = h.snapshot(10);
    let text: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
    println!("{text:?}");
    assert_eq!(lines.len(), 3, "every sentence said while busy comes out: {text:?}");
    assert!(text.iter().all(|t| t.contains("insurance")), "{text:?}");
}

/// Two sides: each line is labelled by the stream it came from, in the order said, and the caller's voice leaking
/// quietly into the mic is not transcribed as the agent.
#[test]
fn two_sides_are_labelled_and_echo_is_dropped() {
    use asr_onnx::pipeline::{Side, TranscriptState};
    use dsp_core::{Stage, FRAME_SAMPLES};
    use std::time::{Duration, Instant};
    let Some(dir) = asr_onnx::find_models_dir().or_else(|| {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/asr_models");
        d.join(asr_onnx::model::MODEL_FILE).is_file().then_some(d)
    }) else {
        eprintln!("SKIP: speech model folder not found");
        return;
    };
    let dll = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/onnxruntime.dll");
    let mut cfg = asr_onnx::pipeline::TranscriptConfig::new(dll);
    cfg.models_dir = Some(dir);
    cfg.initial_enabled = true;
    let (mut agent, mut caller, h) = asr_onnx::pipeline::start_two_sided(cfg);
    let t0 = Instant::now();
    while h.status().state != TranscriptState::Listening {
        assert!(t0.elapsed() < Duration::from_secs(60), "model did not load: {}", h.status().line());
        std::thread::sleep(Duration::from_millis(50));
    }
    let speech: Vec<f32> = clip().iter().flat_map(|&s| [s, s, s]).collect(); // 48 kHz
    let quiet = vec![0.0f32; 72_000]; // 1.5 s
    let scaled = |g: f32| speech.iter().map(|s| s * g).collect::<Vec<f32>>();
    // (agent audio, caller audio) played at the same time
    let parts: Vec<(Vec<f32>, Vec<f32>)> = vec![
        (speech.clone(), vec![0.0; speech.len()]), // agent talks
        (quiet.clone(), quiet.clone()),
        (vec![0.0; speech.len()], speech.clone()), // caller talks
        (quiet.clone(), quiet.clone()),
        (scaled(0.1), speech.clone()), // caller talks, a -20 dB leak of it reaches the mic
        (quiet.clone(), quiet.clone()),
    ];
    // real-time pace (10 ms per frame), as in the app: sentence times come from the clock
    let pace = Instant::now();
    let mut n = 0u32;
    for (a, c) in &parts {
        for (fa, fc) in a.chunks_exact(FRAME_SAMPLES).zip(c.chunks_exact(FRAME_SAMPLES)) {
            agent.process(&mut fa.to_vec());
            caller.process(&mut fc.to_vec());
            n += 1;
            if let Some(wait) = (pace + Duration::from_millis(10 * n as u64)).checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
    }
    let t1 = Instant::now();
    while h.status().line_count < 3 && t1.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2)); // a wrongly kept echo line would show up now
    let (lines, _) = h.snapshot(10);
    for l in &lines {
        println!("[{}] {}: {}", l.at_secs, l.side.label(), l.text);
    }
    let sides: Vec<Side> = lines.iter().map(|l| l.side).collect();
    assert_eq!(sides, [Side::Agent, Side::Caller, Side::Caller], "labels and order");
    assert!(lines.iter().all(|l| l.text.contains("insurance")), "every line transcribed");
    assert_eq!(h.echo_dropped(), 1, "the leak was dropped by the echo guard");
}

/// The agent saying something else, quietly, while the caller talks loudly is real speech, not a leak: it is kept
/// (the mic and the call audio have unrelated levels, so a quiet headset must not lose its replies).
#[test]
fn a_quiet_agent_talking_over_the_caller_is_kept() {
    use asr_onnx::pipeline::{Side, TranscriptState};
    use dsp_core::{Stage, FRAME_SAMPLES};
    use std::time::{Duration, Instant};
    let Some(dir) = asr_onnx::find_models_dir().or_else(|| {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/asr_models");
        d.join(asr_onnx::model::MODEL_FILE).is_file().then_some(d)
    }) else {
        eprintln!("SKIP: speech model folder not found");
        return;
    };
    let dll = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/onnxruntime.dll");
    let mut cfg = asr_onnx::pipeline::TranscriptConfig::new(dll);
    cfg.models_dir = Some(dir);
    cfg.initial_enabled = true;
    let (mut agent, mut caller, h) = asr_onnx::pipeline::start_two_sided(cfg);
    let t0 = Instant::now();
    while h.status().state != TranscriptState::Listening {
        assert!(t0.elapsed() < Duration::from_secs(60), "model did not load: {}", h.status().line());
        std::thread::sleep(Duration::from_millis(50));
    }
    let speech: Vec<f32> = clip().iter().flat_map(|&s| [s, s, s]).collect(); // 48 kHz
    let quiet = vec![0.0f32; 72_000]; // 1.5 s
    let scaled = |g: f32| speech.iter().map(|s| s * g).collect::<Vec<f32>>();
    // (agent audio, caller audio) played at the same time
    let backwards: Vec<f32> = speech.iter().rev().copied().collect(); // loud caller sound with other words
    let parts: Vec<(Vec<f32>, Vec<f32>)> = vec![
        (scaled(0.1), backwards), // the agent, 20 dB quieter, over the caller
        (quiet.clone(), quiet.clone()),
    ];
    // real-time pace (10 ms per frame), as in the app: sentence times come from the clock
    let pace = Instant::now();
    let mut n = 0u32;
    for (a, c) in &parts {
        for (fa, fc) in a.chunks_exact(FRAME_SAMPLES).zip(c.chunks_exact(FRAME_SAMPLES)) {
            agent.process(&mut fa.to_vec());
            caller.process(&mut fc.to_vec());
            n += 1;
            if let Some(wait) = (pace + Duration::from_millis(10 * n as u64)).checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
    }
    let t1 = Instant::now();
    while h.status().line_count < 1 && t1.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2)); // a wrongly kept echo line would show up now
    let (lines, _) = h.snapshot(10);
    for l in &lines {
        println!("[{}] {}: {}", l.at_secs, l.side.label(), l.text);
    }
    assert!(lines.iter().any(|l| l.side == Side::Agent && l.text.contains("insurance")), "the agent's words are kept: {lines:?}");
    assert_eq!(h.echo_dropped(), 0);
}

/// Seen on a live test call: the other end heard the agent (same room / loudspeaker) and sent it back, so every
/// sentence came out twice, as Agent and a second later as Caller. The copy on the caller side is dropped.
#[test]
fn the_far_end_sending_the_agent_back_is_not_a_caller_line() {
    use asr_onnx::pipeline::{Side, TranscriptState};
    use dsp_core::{Stage, FRAME_SAMPLES};
    use std::time::{Duration, Instant};
    let Some(dir) = asr_onnx::find_models_dir().or_else(|| {
        let d = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/asr_models");
        d.join(asr_onnx::model::MODEL_FILE).is_file().then_some(d)
    }) else {
        eprintln!("SKIP: speech model folder not found");
        return;
    };
    let dll = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/onnxruntime.dll");
    let mut cfg = asr_onnx::pipeline::TranscriptConfig::new(dll);
    cfg.models_dir = Some(dir);
    cfg.initial_enabled = true;
    let (mut agent, mut caller, h) = asr_onnx::pipeline::start_two_sided(cfg);
    let t0 = Instant::now();
    while h.status().state != TranscriptState::Listening {
        assert!(t0.elapsed() < Duration::from_secs(60), "model did not load: {}", h.status().line());
        std::thread::sleep(Duration::from_millis(50));
    }
    let speech: Vec<f32> = clip().iter().flat_map(|&s| [s, s, s]).collect(); // 48 kHz
    let quiet = vec![0.0f32; 72_000]; // 1.5 s
    // (agent audio, caller audio) played at the same time
    // the same voice back on the call audio 0.8 s later, quieter and muffled like a phone line
    let muffled: Vec<f32> = speech.windows(4).map(|w| 0.4 * (w[0] + w[1] + w[2] + w[3]) / 4.0).collect();
    let delay = vec![0.0f32; 38_400];
    let mut back = delay.clone();
    back.extend(&muffled);
    let mut said = speech.clone();
    said.resize(back.len(), 0.0);
    let parts: Vec<(Vec<f32>, Vec<f32>)> = vec![(said, back), (quiet.clone(), quiet.clone())];
    // real-time pace (10 ms per frame), as in the app: sentence times come from the clock
    let pace = Instant::now();
    let mut n = 0u32;
    for (a, c) in &parts {
        for (fa, fc) in a.chunks_exact(FRAME_SAMPLES).zip(c.chunks_exact(FRAME_SAMPLES)) {
            agent.process(&mut fa.to_vec());
            caller.process(&mut fc.to_vec());
            n += 1;
            if let Some(wait) = (pace + Duration::from_millis(10 * n as u64)).checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
        }
    }
    let t1 = Instant::now();
    while h.status().line_count < 1 && t1.elapsed() < Duration::from_secs(60) {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_secs(2)); // a wrongly kept echo line would show up now
    let (lines, _) = h.snapshot(10);
    for l in &lines {
        println!("[{}] {}: {}", l.at_secs, l.side.label(), l.text);
    }
    let sides: Vec<Side> = lines.iter().map(|l| l.side).collect();
    assert_eq!(sides, [Side::Agent], "only the agent's line: {lines:?}");
    assert_eq!(h.echo_dropped(), 1, "the copy was dropped");
}
