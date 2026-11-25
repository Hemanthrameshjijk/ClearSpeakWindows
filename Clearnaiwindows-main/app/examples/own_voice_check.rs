//! "My voice" end to end, the way the app does it: record (voice gate + ECAPA on the first 10 s of speech, as the
//! detection worker does), blend 70% own with the nearest American voice, convert the whole clip, write a WAV.
//! Score the result with the own-voice test's ECAPA similarity to compare with the Python run.
//!
//! usage: own_voice_check <app_models dir> <input wav> <output wav>

use std::path::Path;

use accent_onnx::causal_resampler::CausalDownsampler3x;
use gender::analysis::VoiceGate;

fn read_wav_48k_mono(path: &Path) -> Vec<f32> {
    clearspeak_app::test_input::wav_to_mono_48k(&std::fs::read(path).expect("read wav")).expect("wav")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [models, input, output] = [&args[0], &args[1], &args[2]].map(Path::new);
    let dll = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/onnxruntime.dll");
    accent_onnx::init_runtime(&dll).expect("onnxruntime");

    let x48 = read_wav_48k_mono(input);
    // Record: 48k -> 16k -> voice gate -> first ENROLL_S of speech -> fingerprint (as the detection worker does).
    let mut down = CausalDownsampler3x::new(97);
    let mut gate = VoiceGate::default();
    let mut voiced = Vec::new();
    for c in x48.chunks(480) {
        gate.push(&down.push(c), &mut voiced);
    }
    let need = (gender::pipeline::ENROLL_S * gender::analysis::SR as f32) as usize;
    assert!(voiced.len() >= need, "only {:.1} s of speech in the clip", voiced.len() as f32 / 16_000.0);
    let mut model = gender::ecapa::Model::load(models, 1).expect("ecapa");
    let fp = model.embed(&voiced[..need]).expect("fingerprint");
    println!("recorded {:.1} s of speech -> fingerprint of {} numbers", need as f32 / 16_000.0, fp.len());

    let t = std::time::Instant::now();
    let y48 = accent_onnx::pipeline::convert_offline_with(models, 4, 1, Some(&fp), &x48).expect("convert");
    println!("converted {:.1} s in {:.1} s", x48.len() as f32 / 48_000.0, t.elapsed().as_secs_f32());

    let peak = y48.iter().fold(1e-6f32, |m, v| m.max(v.abs()));
    let y: Vec<f32> = y48.iter().map(|v| v / peak * 0.8).collect();
    clearspeak_app::voice_test::write_wav(output, &y).expect("write wav");
    println!("wrote {}", output.display());
}
