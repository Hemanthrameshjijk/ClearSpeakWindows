"""Checks the int8 ECAPA against the full one with the trained classifier, on the test speakers (test-clean), and
writes pitch + formant reference numbers for the Rust test.

usage: python eval_int8.py <LibriSpeech dir> <app_models dir (full ecapa)> <out dir (gender.npz, ecapa_int8.onnx)>
"""
import sys
from pathlib import Path

import numpy as np
import onnxruntime as ort

sys.path.insert(0, str(Path(__file__).parent))
import train_gender as tg  # noqa: E402


def main():
    root, models, out = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
    full = tg.Ecapa(models)
    small = tg.Ecapa(models)
    o = ort.SessionOptions()
    o.intra_op_num_threads = 2
    small.sess = ort.InferenceSession(str(out / "ecapa_int8.onnx"), o, providers=["CPUExecutionProvider"])
    g = np.load(out / "gender.npz")
    w, b = g["ecapa_w"].astype(np.float64), float(g["ecapa_b"])

    rows = []
    for spk, s in sorted(tg.speakers(root, "test-clean").items()):
        for c in tg.speaker_chunks(root / "test-clean" / spk, 10):
            rows.append((s == "F", full(c) @ w + b, small(c) @ w + b, full(c) @ small(c)))
    y = np.array([r[0] for r in rows])
    sf, si, cos = (np.array([r[i] for r in rows]) for i in (1, 2, 3))
    lines = [
        f"test chunks: {len(rows)} from 40 unseen speakers",
        f"full ECAPA accuracy {np.mean((sf > 0) == y):.3f}",
        f"int8 ECAPA accuracy {np.mean((si > 0) == y):.3f}",
        f"same answer full vs int8: {np.mean((sf > 0) == (si > 0)):.3f}; fingerprint similarity mean {cos.mean():.4f} min {cos.min():.4f}",
        f"int8 'sure' (|score| >= 1): {np.mean(np.abs(si) >= 1):.3f} of chunks, accuracy when sure {np.mean(((si > 0) == y)[np.abs(si) >= 1]):.3f}",
    ]

    # Reference pitch + formant numbers for the Rust test.
    gold = np.load(out / "gender_golden.npz")
    ref = {}
    for k in range(4):
        a = gold[f"audio{k}"].astype(np.float64)
        f0 = tg.pitch_track(a)
        f0m = float(np.median(f0)) if len(f0) > 20 else np.nan
        fm = tg.formants(a)
        v = np.array([np.log(np.nan_to_num(f0m, nan=150.0))] + list(np.log(np.nan_to_num(fm, nan=1000.0))))
        ref[f"formants{k}"] = fm.astype(np.float32)
        ref[f"pf_score{k}"] = np.float32((v - g["pf_mu"]) / g["pf_sd"] @ g["pf_w"] + g["pf_b"])
    np.savez(out / "gender_golden_pf.npz", **ref)
    text = "\n".join(lines)
    (out / "int8_report.txt").write_text(text + "\n", encoding="utf-8")
    print(text)


if __name__ == "__main__":
    main()
