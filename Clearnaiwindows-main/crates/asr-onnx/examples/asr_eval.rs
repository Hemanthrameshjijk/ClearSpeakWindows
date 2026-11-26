//! Accuracy check for the live transcript. For each 48 kHz mono PCM16 WAV in a list (`wav<TAB>id<TAB>reference`)
//! prints `id<TAB>mode<TAB>text` for:
//! - `whole`: the utterance decoded in one go (the model's own accuracy),
//! - `live`: the live path (10 ms frames -> causal 3x downsampler -> Segmenter -> finals joined),
//! - `skip`: the live path with 1 s of every 4 s missing, like the transcript tap while the PC is under load.
//! `cargo run --release -p asr-onnx --example asr_eval -- list.tsv > out.tsv`
use accent_onnx::causal_resampler::CausalDownsampler3x;
use asr_onnx::segmenter::{Event, Segmenter};
use std::path::Path;

fn read_wav_48k(path: &str) -> Vec<f32> {
    let b = std::fs::read(path).unwrap();
    let mut i = 12;
    while i + 8 <= b.len() {
        let n = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
        if &b[i..i + 4] == b"data" {
            let end = (i + 8 + n).min(b.len());
            return b[i + 8..end].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect();
        }
        i += 8 + n + (n & 1);
    }
    panic!("no data chunk in {path}");
}

fn live(m: &mut asr_onnx::AsrModel, x: &[f32], skip: bool) -> String {
    let mut down = CausalDownsampler3x::new(97);
    let mut seg = Segmenter::new();
    let mut out: Vec<String> = Vec::new();
    // 0.5 s of quiet before, 1.5 s after: a pause in a conversation.
    let mut stream = vec![0.0f32; 24_000];
    stream.extend_from_slice(x);
    stream.extend(std::iter::repeat(0.0).take(72_000));
    for (k, frame) in stream.chunks_exact(480).enumerate() {
        if skip && k % 400 >= 300 {
            continue; // tap skipped: these frames never reach the transcript
        }
        for ev in seg.push(&down.push(frame)) {
            if let Event::Final(a) = ev {
                let t = m.transcribe(&a).unwrap();
                if !t.is_empty() {
                    out.push(t);
                }
            }
        }
    }
    out.join(" ")
}

fn main() {
    let list = std::env::args().nth(1).expect("usage: asr_eval <list.tsv>");
    let dir = asr_onnx::find_models_dir().expect("speech model folder not found");
    let dll = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/onnxruntime.dll");
    accent_onnx::ort_util::init_ort(&dll).expect("onnxruntime.dll");
    let mut m = asr_onnx::AsrModel::load(&dir, 2).expect("load");
    for line in std::fs::read_to_string(&list).unwrap().lines().filter(|l| !l.trim().is_empty()) {
        let mut it = line.split('\t');
        let (wav, id) = (it.next().unwrap(), it.next().unwrap());
        let x = read_wav_48k(wav);
        let mut down = CausalDownsampler3x::new(97);
        let mut x16 = Vec::new();
        for f in x.chunks_exact(480) {
            x16.extend(down.push(f));
        }
        println!("{id}\twhole\t{}", m.transcribe(&x16).unwrap());
        println!("{id}\tlive\t{}", live(&mut m, &x, false));
        println!("{id}\tskip\t{}", live(&mut m, &x, true));
    }
}
