//! Transcribes a call recording with who-spoke-when, on this PC (no cloud):
//! `cargo run --release -p asr-onnx --example diarize_file -- call.mp3 [--speakers 2] [--channels auto|split|mix]`
//!
//! - Any format ffmpeg reads (wav, mp3, m4a, ...), any sample rate; decoded to 16 kHz.
//! - Stereo with different channels (agent on one side, caller on the other): each channel is its own speaker
//!   (exact). Otherwise all channels are mixed and voices are told apart by their fingerprints (`asr_onnx::diarize`).
//! - Writes `<file>.ours.txt` (`[mm:ss] Speaker N: text`) and `<file>.ours.json` (words with times and speakers,
//!   lines), and prints the lines.
//!
//! Needs ffmpeg/ffprobe on PATH, the speech model (ASR_MODELS_DIR or the usual places) and the voice models
//! (APP_MODELS_DIR, default app/assets/app_models).
use asr_onnx::diarize::{self, Turn};
use asr_onnx::segmenter::{Event, Segmenter};
use asr_onnx::Word;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

const SR: usize = 16_000;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn channels(file: &str) -> usize {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=channels", "-of", "csv=p=0", file])
        .output()
        .expect("ffprobe (part of ffmpeg) must be on PATH");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(1)
}

/// The file at 16 kHz, one Vec per channel (`mono`: all channels mixed into one).
fn decode(file: &str, n_ch: usize, mono: bool) -> Vec<Vec<f32>> {
    let ac = if mono { 1 } else { n_ch };
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i", file, "-f", "f32le", "-ac", &ac.to_string(), "-ar", &SR.to_string(), "-"])
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    let x: Vec<f32> = out.stdout.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    (0..ac).map(|c| x.iter().skip(c).step_by(ac).copied().collect()).collect()
}

/// Are the two channels really different recordings (not the same mono audio twice)?
fn channels_differ(a: &[f32], b: &[f32]) -> bool {
    let (mut ab, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        ab += (*x as f64) * (*y as f64);
        aa += (*x as f64).powi(2);
        bb += (*y as f64).powi(2);
    }
    let corr = ab / (aa.sqrt() * bb.sqrt()).max(1e-12);
    println!("channel correlation {corr:.2}");
    corr < 0.8 && aa > 1e-6 && bb > 1e-6
}

/// Stretches of speech: (start s, end s, audio).
fn speech(x: &[f32]) -> Vec<(f32, f32, Vec<f32>)> {
    let mut seg = Segmenter::new();
    let mut out = Vec::new();
    let take = |seg: &Segmenter, ev: Vec<Event>, out: &mut Vec<(f32, f32, Vec<f32>)>| {
        for e in ev {
            if let Event::Final(a) = e {
                let end = seg.last_final_end();
                let start = end.saturating_sub(a.len());
                out.push((start as f32 / SR as f32, end as f32 / SR as f32, a));
            }
        }
    };
    for f in x.chunks(320) {
        let ev = seg.push(f);
        take(&seg, ev, &mut out);
    }
    let ev = seg.push(&vec![0.0; SR]); // closes the last stretch
    take(&seg, ev, &mut out);
    out
}

fn words_of(m: &mut asr_onnx::AsrModel, segs: &[(f32, f32, Vec<f32>)]) -> Vec<Word> {
    let mut all = Vec::new();
    for (s0, _, a) in segs {
        for mut w in m.transcribe_words(a).unwrap() {
            w.start += s0;
            w.end += s0;
            all.push(w);
        }
    }
    all
}

fn fmt(t: f32) -> String {
    let s = t.max(0.0) as u32;
    format!("{:02}:{:02}", s / 60, s % 60)
}

fn json_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = args.first().filter(|a| !a.starts_with("--")).cloned().expect("usage: diarize_file <audio> [--speakers N] [--channels auto|split|mix] [--threshold T]");
    let speakers: Option<usize> = arg(&args, "--speakers").and_then(|v| v.parse().ok());
    let threshold: Option<f32> = arg(&args, "--threshold").and_then(|v| v.parse().ok());
    let ch_mode = arg(&args, "--channels").unwrap_or_else(|| "auto".into());
    // --embs: also save the voice fingerprints (<file>.embs.bin); --embs-only: just that, no speech model run.
    let embs_only = args.iter().any(|a| a == "--embs-only");
    let embs_out = embs_only || args.iter().any(|a| a == "--embs");

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    accent_onnx::ort_util::init_ort(&root.join("app/assets/onnxruntime.dll")).expect("onnxruntime.dll");
    let asr_dir = asr_onnx::find_models_dir().unwrap_or_else(|| root.join("app/assets/asr_models"));
    let mut asr = asr_onnx::AsrModel::load(&asr_dir, 4).expect("speech model");
    let t0 = Instant::now();

    let n_ch = channels(&file);
    let mut split = false;
    let mut chans = decode(&file, n_ch, false);
    if n_ch == 2 && ch_mode != "mix" {
        split = ch_mode == "split" || channels_differ(&chans[0], &chans[1]);
    }
    if !split && chans.len() > 1 {
        chans = decode(&file, n_ch, true);
    }
    let dur = chans[0].len() as f32 / SR as f32;
    println!("{file}: {:.1} min, {n_ch} channel(s), {}", dur / 60.0, if split { "one speaker per channel" } else { "voices told apart by fingerprint" });

    let (mut words, mut who): (Vec<Word>, Vec<usize>) = (Vec::new(), Vec::new());
    if split {
        for (c, x) in chans.iter().enumerate() {
            let w = words_of(&mut asr, &speech(x));
            who.extend(std::iter::repeat(c).take(w.len()));
            words.extend(w);
        }
        let mut idx: Vec<usize> = (0..words.len()).collect();
        idx.sort_by(|&a, &b| words[a].start.total_cmp(&words[b].start));
        words = idx.iter().map(|&i| words[i].clone()).collect();
        who = idx.iter().map(|&i| who[i]).collect();
    } else {
        let x = &chans[0];
        let segs = speech(x);
        if !embs_only {
            words = words_of(&mut asr, &segs);
        }
        let vdir = std::env::var_os("APP_MODELS_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("app/assets/app_models"));
        let mut ecapa = gender::ecapa::Model::load(&vdir, 4).expect("voice model (ecapa.onnx, gender.npz)");
        let mut wins: Vec<(f32, f32)> = Vec::new();
        let mut embs: Vec<Vec<f32>> = Vec::new();
        for (s0, s1, a) in &segs {
            for (w0, w1) in diarize::windows(*s0, *s1) {
                let (i0, i1) = (((w0 - s0) * SR as f32) as usize, (((w1 - s0) * SR as f32) as usize).min(a.len()));
                if let Ok(e) = ecapa.embed(&a[i0..i1]) {
                    wins.push((w0, w1));
                    embs.push(e);
                }
            }
        }
        if embs_out {
            // For tuning the clustering offline: n, dim, then per window start, end, fingerprint (all f32 LE).
            let dim = embs.first().map_or(0, |e| e.len());
            let mut b: Vec<u8> = Vec::new();
            b.extend((embs.len() as u32).to_le_bytes());
            b.extend((dim as u32).to_le_bytes());
            for (w, e) in wins.iter().zip(&embs) {
                for v in [w.0, w.1].iter().chain(e) {
                    b.extend(v.to_le_bytes());
                }
            }
            std::fs::write(PathBuf::from(&file).with_extension("embs.bin"), b).unwrap();
            if embs_only {
                println!("--- wrote fingerprints only ({} windows), {:.1} s", embs.len(), t0.elapsed().as_secs_f32());
                return;
            }
        }
        // Averaging each window with its neighbours (~3 s of voice) was tried: 92.0% -> 91.7% on 8 real calls (one
        // call's gain in the Python bench did not hold in Rust), so each window is grouped on its own.
        let labels = diarize::cluster(&embs, speakers, threshold);
        let lw: Vec<(f32, f32, usize)> = wins.iter().zip(&labels).map(|(w, &l)| (w.0, w.1, l)).collect();
        who = diarize::label_words(&words, &lw);
        println!("{} fingerprint windows, {} speaker(s)", embs.len(), labels.iter().max().map_or(0, |m| m + 1));
    }
    let lines: Vec<Turn> = diarize::turns(&words, &who);

    let mut txt = String::new();
    for t in &lines {
        txt.push_str(&format!("[{}] Speaker {}: {}\n", fmt(t.start), t.speaker + 1, t.text));
    }
    print!("{txt}");
    let base = PathBuf::from(&file);
    let out_txt = base.with_extension("ours.txt");
    std::fs::write(&out_txt, &txt).unwrap();
    let mut j = String::from("{\n  \"words\": [\n");
    for (i, (w, s)) in words.iter().zip(&who).enumerate() {
        j.push_str(&format!(
            "    {{\"word\": {}, \"start\": {:.2}, \"end\": {:.2}, \"speaker\": {}}}{}\n",
            json_str(&w.text),
            w.start,
            w.end,
            s,
            if i + 1 < words.len() { "," } else { "" }
        ));
    }
    j.push_str("  ],\n  \"lines\": [\n");
    for (i, t) in lines.iter().enumerate() {
        j.push_str(&format!(
            "    {{\"speaker\": {}, \"start\": {:.2}, \"end\": {:.2}, \"text\": {}}}{}\n",
            t.speaker,
            t.start,
            t.end,
            json_str(&t.text),
            if i + 1 < lines.len() { "," } else { "" }
        ));
    }
    j.push_str("  ]\n}\n");
    std::fs::write(base.with_extension("ours.json"), j).unwrap();
    println!("--- {:.1} s for {:.1} min of audio; wrote {}", t0.elapsed().as_secs_f32(), dur / 60.0, out_txt.display());
}
