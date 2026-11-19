//! The Rust port must give the training script's numbers (`tools/train_gender.py` writes `gender_golden.npz`: 2 male
//! and 2 female test-speaker chunks with Python's ECAPA features, fingerprint, score and pitch).
//!
//! Needs `GENDER_MODELS_DIR` (a folder with `gender.npz`, `ecapa.onnx` and `gender_golden.npz`) and `onnxruntime.dll`
//! (`ORT_DLL`, default app/assets/onnxruntime.dll). Without them the test prints SKIPPED and passes.

use accent_onnx::npz::Npz;
use gender::analysis;
use gender::ecapa::Model;
use std::path::{Path, PathBuf};

#[test]
fn rust_matches_the_training_script() {
    let Some(dir) = std::env::var_os("GENDER_MODELS_DIR").map(PathBuf::from) else {
        eprintln!("SKIPPED: set GENDER_MODELS_DIR");
        return;
    };
    let dll = std::env::var("ORT_DLL")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets/onnxruntime.dll"));
    accent_onnx::ort_util::init_ort(&dll).unwrap();
    let mut m = Model::load(&dir, 2).unwrap();
    let g = Npz::open(&dir.join("gender_golden.npz")).unwrap();
    for k in 0..4 {
        let audio = g.f32s(&format!("audio{k}")).unwrap();
        let (feats, frames) = m.features(&audio);
        let want = g.f32s(&format!("feats{k}")).unwrap();
        assert_eq!(feats.len(), want.len(), "chunk {k}: feature size ({frames} frames)");
        let fd = feats.iter().zip(&want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(fd < 1e-3, "chunk {k}: features differ by {fd}");

        let emb = m.embed(&audio).unwrap();
        let want_emb = g.f32s(&format!("emb{k}")).unwrap();
        let cos: f32 = emb.iter().zip(&want_emb).map(|(a, b)| a * b).sum();
        // Python used the full ECAPA; the shipped one has int8 weights (tools/compress_ecapa.py, similarity >= 0.997 on
        // the test speakers), so allow that much. The full model gives 1.000000 here.
        assert!(cos > 0.997, "chunk {k}: fingerprint similarity {cos}");

        let score = m.ecapa_score(&emb);
        let want_score = g.scalar(&format!("score{k}")).unwrap() as f32;
        assert!((score - want_score).abs() < 0.25, "chunk {k}: score {score} vs {want_score}");
        let female = g.scalar(&format!("female{k}")).unwrap() > 0.5;
        assert_eq!(score > 0.0, female, "chunk {k}: wrong answer");

        let want_f0 = g.scalar(&format!("f0_{k}")).unwrap();
        let f0 = analysis::median_pitch(&audio).unwrap_or(f64::NAN);
        assert!((f0 - want_f0).abs() < 0.5 || (f0.is_nan() && want_f0.is_nan()), "chunk {k}: pitch {f0} vs {want_f0}");

        // Formants and the pitch + formant score (written by tools/eval_int8.py).
        if let Ok(pf) = Npz::open(&dir.join("gender_golden_pf.npz")) {
            let fm = analysis::median_formants(&audio);
            let want_fm = pf.f32s(&format!("formants{k}")).unwrap();
            if let Some(fm) = fm {
                for j in 0..3 {
                    assert!((fm[j] as f32 - want_fm[j]).abs() < 1.0, "chunk {k}: F{} {} vs {}", j + 1, fm[j], want_fm[j]);
                }
            } else {
                assert!(want_fm.iter().all(|v| v.is_nan()), "chunk {k}: Rust found no formants, Python did");
            }
            let s = m.pitch_formant_score(analysis::median_pitch(&audio), fm);
            let want_s = pf.scalar(&format!("pf_score{k}")).unwrap() as f32;
            assert!((s - want_s).abs() < 0.01, "chunk {k}: pitch+formant score {s} vs {want_s}");
        }
        println!("chunk {k}: {} score {score:+.2} (python {want_score:+.2}), pitch {f0:.0} Hz, fingerprint similarity {cos:.6}",
            if female { "female" } else { "male" });
    }
}
