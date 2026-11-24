//! Replays a 48 kHz mono 16-bit WAV through the same path as the live transcript worker (10 ms frames ->
//! causal 3x downsampler -> Segmenter -> model) and prints every partial/final, then the whole file transcribed in
//! one go for comparison: `cargo run --release -p asr-onnx --example live_replay -- file.wav`
use accent_onnx::causal_resampler::CausalDownsampler3x;
use asr_onnx::segmenter::{Event, Segmenter};
use std::path::Path;

fn read_wav_48k(path: &str) -> Vec<f32> {
    let b = std::fs::read(path).unwrap();
    // Find the "data" chunk (plain PCM16 mono 48 kHz expected).
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let n = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
        if id == b"data" {
            let end = (i + 8 + n).min(b.len());
            return b[i + 8..end].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
        }
        i += 8 + n + (n & 1);
    }
    panic!("no data chunk");
}

fn main() {
    let path = std::env::args().nth(1).expect("usage: live_replay <48k mono pcm16 wav> [gain]");
    let gain: f32 = std::env::args().nth(2).and_then(|g| g.parse().ok()).unwrap_or(1.0);
    let dir = asr_onnx::find_models_dir().expect("speech model folder not found");
    let dll = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/onnxruntime.dll");
    accent_onnx::ort_util::init_ort(&dll).expect("onnxruntime.dll");
    let mut m = asr_onnx::AsrModel::load(&dir, 1).expect("load");
    let x: Vec<f32> = read_wav_48k(&path).into_iter().map(|v| v * gain).collect();

    let mut down = CausalDownsampler3x::new(97);
    let mut seg = Segmenter::new();
    let mut whole16 = Vec::new();
    for (k, frame) in x.chunks_exact(480).enumerate() {
        let x16 = down.push(frame);
        whole16.extend_from_slice(&x16);
        let events = seg.push(&x16);
        let n = events.len();
        for (i, ev) in events.into_iter().enumerate() {
            let t = (k + 1) as f32 * 0.01;
            match ev {
                Event::Final(a) => println!("{t:6.2}s FINAL   ({:.1} s) {}", a.len() as f32 / 16000.0, m.transcribe(&a).unwrap()),
                Event::Partial(a) if i + 1 == n => {
                    println!("{t:6.2}s partial ({:.1} s) {}", a.len() as f32 / 16000.0, m.transcribe(&a).unwrap())
                }
                Event::Partial(_) => {}
            }
        }
    }
    // Flush: 1 s of silence closes an open segment.
    for ev in seg.push(&vec![0.0; 16000]) {
        if let Event::Final(a) = ev {
            println!("   end FINAL   ({:.1} s) {}", a.len() as f32 / 16000.0, m.transcribe(&a).unwrap());
        }
    }
    println!("--- whole file in one go:\n{}", m.transcribe(&whole16).unwrap());
}
