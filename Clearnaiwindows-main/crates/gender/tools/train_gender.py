"""Trains and checks ClearSpeak's male / female detector.

Signals (all from the same 3 s chunks of voiced 16 kHz speech the app collects):
  1. pitch     median F0 of the voiced frames
  2. formants  median F1..F3 (vocal-tract resonances, LPC)
  3. ECAPA     192-number voice fingerprint (the app's ecapa.onnx) -> logistic regression

Trains on LibriSpeech dev-clean speakers and tests on test-clean speakers (never seen in training), then writes
gender.npz (the classifier the app loads) and golden test data for the Rust port.

usage: python train_gender.py <LibriSpeech dir> <app_models dir with ecapa.onnx/.npz> <out dir>
"""
import sys
from pathlib import Path

import numpy as np
import onnxruntime as ort
import soundfile as sf

SR = 16_000
CHUNK_S = 3.0                 # voiced seconds per decision chunk (same as the app)
CHUNKS_PER_SPEAKER = 20
FRAME = 480                   # 30 ms analysis frames for the voice detector / pitch
HOP = 160                     # 10 ms


# ------------------------------------------------------------------ ECAPA (same maths as the app will use)
class Ecapa:
    def __init__(self, folder):
        d = np.load(Path(folder) / "ecapa.npz")
        self.n_fft, self.hop = int(d["n_fft"]), int(d["hop"])
        self.window = d["window"].astype(np.float64)
        self.center, self.pad_mode = bool(d["center"]), str(d["pad_mode"])
        self.fbank = d["fbank"].astype(np.float64)
        self.mult, self.amin, self.db_mult, self.top_db = (float(d["multiplier"]), float(d["amin"]),
                                                           float(d["db_multiplier"]), float(d["top_db"]))
        o = ort.SessionOptions()
        o.intra_op_num_threads = 4
        self.sess = ort.InferenceSession(str(Path(folder) / "ecapa.onnx"), o, providers=["CPUExecutionProvider"])
        self.params = dict(n_fft=self.n_fft, hop=self.hop, center=self.center, pad_mode=self.pad_mode,
                           multiplier=self.mult, amin=self.amin, db_multiplier=self.db_mult, top_db=self.top_db,
                           window_len=len(self.window))

    def features(self, x16):
        x = np.asarray(x16, np.float64)
        if self.center:
            x = np.pad(x, self.n_fft // 2, mode=self.pad_mode)
        win = np.zeros(self.n_fft)
        left = (self.n_fft - len(self.window)) // 2
        win[left:left + len(self.window)] = self.window
        fr = np.lib.stride_tricks.sliding_window_view(x, self.n_fft)[::self.hop]
        spec = np.fft.rfft(fr * win, axis=1)
        fb = (spec.real ** 2 + spec.imag ** 2) @ self.fbank
        db = self.mult * np.log10(np.maximum(fb, self.amin)) - self.mult * self.db_mult
        db = np.maximum(db, db.max() - self.top_db)
        return db - db.mean(0, keepdims=True)

    def __call__(self, x16):
        f = self.features(x16).astype(np.float32)[None]
        e = self.sess.run(None, {"feats": f})[0].reshape(-1).astype(np.float64)
        return e / max(np.linalg.norm(e), 1e-12)


# ------------------------------------------------------------------ voice detector, pitch, formants
def frames(x, n=FRAME, hop=HOP):
    if len(x) < n:
        return np.zeros((0, n))
    return np.lib.stride_tricks.sliding_window_view(x, n)[::hop]


def voiced_audio(x):
    """Keep 10 ms hops whose 30 ms energy is within 30 dB of the loud speech (the app's simple voice detector)."""
    fr = frames(x)
    if len(fr) == 0:
        return x[:0]
    db = 10 * np.log10(np.mean(fr ** 2, axis=1) + 1e-10)
    keep = db > max(np.percentile(db, 95) - 30, -60)
    out = [x[i * HOP:(i + 1) * HOP] for i in np.nonzero(keep)[0]]
    return np.concatenate(out) if out else x[:0]


def pitch_track(x):
    """Normalised autocorrelation F0 per 30 ms frame, 60-400 Hz; returns voiced F0s."""
    fr = frames(x)
    fr = fr - fr.mean(1, keepdims=True)
    lo, hi = SR // 400, SR // 60
    n = FRAME
    spec = np.fft.rfft(fr, 2 * n, axis=1)
    ac = np.fft.irfft(np.abs(spec) ** 2, axis=1)[:, :n]
    e = ac[:, :1] + 1e-12
    nac = ac / e
    seg = nac[:, lo:hi]
    lag = seg.argmax(1) + lo
    peak = seg.max(1)
    f0 = SR / lag
    return f0[peak > 0.55]


LPC_ORDER = 18                # 2 + fs/1000 at 16 kHz
LPC_N = 400                   # 25 ms frames
LPC_FFT = 1024                # envelope resolution: 15.6 Hz per bin


def formants(x, every=4):
    """Median F1..F3 over voiced frames: the first three peaks (200-4500 Hz) of the LPC spectral envelope.
    Peak picking on the envelope instead of polynomial roots, so the Rust port is exact and simple."""
    fr = frames(x, LPC_N, 160)[::every]
    if len(fr) == 0:
        return np.full(3, np.nan)
    win = 0.54 - 0.46 * np.cos(2 * np.pi * np.arange(LPC_N) / (LPC_N - 1))
    fr = np.append(fr[:, :1], fr[:, 1:] - 0.97 * fr[:, :-1], axis=1) * win
    lo, hi = int(200 * LPC_FFT / SR), int(4500 * LPC_FFT / SR)
    res = []
    for f in fr:
        if np.mean(f ** 2) < 1e-7:
            continue
        r = np.array([np.dot(f[:LPC_N - k], f[k:]) for k in range(LPC_ORDER + 1)])
        a = levinson(r, LPC_ORDER)
        if a is None:
            continue
        env = -np.log(np.abs(np.fft.rfft(a, LPC_FFT)) + 1e-12)
        pk = [k for k in range(max(lo, 1), min(hi, len(env) - 1)) if env[k] > env[k - 1] and env[k] >= env[k + 1]]
        if len(pk) >= 3:
            res.append(np.array(pk[:3], np.float64) * SR / LPC_FFT)
    return np.median(res, 0) if res else np.full(3, np.nan)


def levinson(r, order):
    a = np.zeros(order + 1)
    a[0] = 1.0
    e = r[0]
    if e <= 0:
        return None
    for i in range(1, order + 1):
        k = -(r[i] + np.dot(a[1:i], r[i - 1:0:-1])) / e
        a[1:i] = a[1:i] + k * a[i - 1:0:-1]
        a[i] = k
        e *= 1 - k * k
        if e <= 0:
            return None
    return a


# ------------------------------------------------------------------ data
def speakers(root, subset):
    sex = {}
    for line in (Path(root) / "SPEAKERS.TXT").read_text(encoding="utf-8").splitlines():
        if line.startswith(";"):
            continue
        p = [s.strip() for s in line.split("|")]
        if len(p) >= 3 and p[2] == subset:
            sex[p[0]] = p[1]
    return sex


def speaker_chunks(folder, n):
    """Up to n chunks of CHUNK_S voiced seconds, from the speaker's files in order."""
    need = int(CHUNK_S * SR)
    buf, out = np.zeros(0), []
    for f in sorted(Path(folder).rglob("*.flac")):
        x, sr = sf.read(f, dtype="float64")
        assert sr == SR
        buf = np.concatenate([buf, voiced_audio(x)])
        while len(buf) >= need and len(out) < n:
            out.append(buf[:need])
            buf = buf[need:]
        if len(out) >= n:
            break
    return out


def measure(root, subset, ecapa, cache_dir):
    """All three signals for every chunk of every speaker in `subset`; cached in <cache_dir>/rows_<subset>.npz."""
    cache = Path(cache_dir) / f"rows_{subset}.npz"
    if cache.exists():
        z = np.load(cache)
        return dict(spk=z["spk"], female=z["female"], emb=z["emb"], f0=z["f0"], fm=z["fm"])
    rows = dict(spk=[], female=[], emb=[], f0=[], fm=[])
    for spk, s in sorted(speakers(root, subset).items()):
        d = Path(root) / subset / spk
        if not d.exists():
            continue
        n = 0
        for c in speaker_chunks(d, CHUNKS_PER_SPEAKER):
            f0 = pitch_track(c)
            rows["spk"].append(f"{subset}/{spk}")
            rows["female"].append(int(s == "F"))
            rows["emb"].append(ecapa(c))
            rows["f0"].append(float(np.median(f0)) if len(f0) > 20 else np.nan)
            rows["fm"].append(formants(c))
            n += 1
        print(f"  {subset} {spk} {s}: {n} chunks", flush=True)
    out = {k: np.array(v) for k, v in rows.items()}
    np.savez(cache, **out)
    return out


# ------------------------------------------------------------------ classifiers
def fit_logreg(X, y, l2=1e-2, steps=3000, lr=0.5):
    w, b = np.zeros(X.shape[1]), 0.0
    for _ in range(steps):
        p = 1 / (1 + np.exp(-(X @ w + b)))
        g = p - y
        w -= lr * (X.T @ g / len(y) + l2 * w)
        b -= lr * g.mean()
    return w, b


def acc(pred, y):
    return float(np.mean(pred == y))


def by_speaker(rows, score):
    """Speaker-level answer from the average score of their first 2 chunks (= ~6 s of speech)."""
    out = {}
    for r, s in zip(rows, score):
        out.setdefault(r["spk"], (r["female"], []))[1].append(s)
    return [(f, np.mean(v[:2])) for f, v in out.values()]


SUBSETS = ["dev-clean", "test-clean", "dev-other", "test-other"]


def pf_matrix(r):
    f0, fm = r["f0"], r["fm"]
    return np.column_stack([np.log(np.nan_to_num(f0, nan=150.0)), np.log(np.nan_to_num(fm, nan=1000.0))])


def lda_direction(X, y, spk, shrink):
    """Speaker-mean LDA: each speaker counts once (so 20 chunks of one voice cannot dominate), shrunk covariance."""
    ids = np.unique(spk)
    M = np.stack([X[spk == s].mean(0) for s in ids])
    ym = np.array([y[spk == s][0] for s in ids])
    mf, mm = M[ym == 1].mean(0), M[ym == 0].mean(0)
    C = np.cov((M - np.where(ym[:, None] == 1, mf, mm)).T)
    C = (1 - shrink) * C + shrink * np.eye(len(C)) * np.trace(C) / len(C)
    w = np.linalg.solve(C, mf - mm)
    return w


def calibrate(s, y):
    """1-D logistic on a raw score, so 'sure' (|score| >= 1) means roughly 73% and up."""
    a, c = fit_logreg(s[:, None] / (np.std(s) + 1e-9), y, l2=1e-4, steps=2000, lr=1.0)
    return a[0] / (np.std(s) + 1e-9), c


def methods():
    """name -> fit(X_emb, X_pf, y, spk) -> score(X_emb, X_pf)."""
    def logreg_emb(l2):
        def fit(E, P, y, spk):
            w, b = fit_logreg(E, y, l2=l2, steps=3000, lr=2.0)
            return lambda E2, P2: E2 @ w + b
        return fit

    def lda_emb(shrink):
        def fit(E, P, y, spk):
            w = lda_direction(E, y, spk, shrink)
            a, c = calibrate(E @ w, y)
            return lambda E2, P2: (E2 @ w) * a + c
        return fit

    def pitch(E, P, y, spk):
        t = max(np.arange(120, 220, 1.0), key=lambda t: np.mean((np.exp(P[:, 0]) > t) == y))
        return lambda E2, P2: (np.exp(P2[:, 0]) - t) / 15.0

    def pf(E, P, y, spk):
        mu, sd = P.mean(0), P.std(0) + 1e-9
        w, b = fit_logreg((P - mu) / sd, y, l2=1e-2)
        return lambda E2, P2: (P2 - mu) / sd @ w + b

    m = {"pitch only": pitch, "pitch + formants": pf}
    for l2 in (1e-3, 1e-2, 1e-1):
        m[f"ECAPA logreg l2={l2}"] = logreg_emb(l2)
    for sh in (0.1, 0.5, 0.9):
        m[f"ECAPA speaker-LDA shrink={sh}"] = lda_emb(sh)
    return m


def cross_validate(r, folds=8):
    """Speakers split into `folds` groups; each group is tested by a model trained on the others."""
    E, P, y, spk = r["emb"], pf_matrix(r), r["female"], r["spk"]
    ids = np.unique(spk)
    rng = np.random.default_rng(0)
    order = rng.permutation(ids)
    scores = {name: np.zeros(len(y)) for name in methods()}
    for k in range(folds):
        test_ids = set(order[k::folds])
        te = np.array([s in test_ids for s in spk])
        for name, fit in methods().items():
            f = fit(E[~te], P[~te], y[~te], spk[~te])
            scores[name][te] = f(E[te], P[te])
    return scores


def speaker_accuracy(score, y, spk, n=2):
    """Per speaker, the average of the first n chunks (n=2: ~6 s of speech), as the app decides."""
    res = []
    for s in np.unique(spk):
        i = np.nonzero(spk == s)[0][:n]
        res.append((np.mean(score[i]) > 0) == bool(y[i[0]]))
    return float(np.mean(res))


def main():
    root, models, out = Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
    out.mkdir(parents=True, exist_ok=True)
    ecapa = Ecapa(models)
    parts = []
    for sub in SUBSETS:
        if (root / sub).exists():
            print(f"measuring {sub} ...", flush=True)
            parts.append(measure(root, sub, ecapa, out))
    r = {k: np.concatenate([p[k] for p in parts]) for k in parts[0]}
    y, spk = r["female"], r["spk"]
    n_spk = len(np.unique(spk))
    report = [f"{n_spk} speakers ({len(np.unique(spk[y == 1]))} female), {len(y)} chunks of {CHUNK_S:.0f} s; "
              f"8-fold cross-validation by speaker (no speaker is ever in both training and test)"]
    scores = cross_validate(r)
    sure_line = {}
    for name, s in scores.items():
        sure = np.abs(s) >= 1
        sure_line[name] = (np.mean(sure), np.mean(((s > 0) == y)[sure]) if sure.any() else float("nan"))
        report.append(f"{name:32s} chunk {acc((s > 0).astype(int), y):.3f}   speaker (~6 s) {speaker_accuracy(s, y, spk):.3f}"
                      f"   sure on {sure_line[name][0]:.0%} of chunks, {sure_line[name][1]:.3f} right when sure")

    # Pick the best ECAPA method by speaker-level accuracy; combine with pitch + formants as the app does.
    ecapa_names = [n for n in scores if n.startswith("ECAPA")]
    best = max(ecapa_names, key=lambda n: (speaker_accuracy(scores[n], y, spk), acc((scores[n] > 0).astype(int), y)))
    se, sp = scores[best], scores["pitch + formants"]
    comb = np.where(np.abs(se) >= 1, se, np.where(np.abs(sp) >= 1, np.sign(sp), se))
    report.append(f"combined ({best} + pitch/formants when unsure): chunk {acc((comb > 0).astype(int), y):.3f}   "
                  f"speaker (~6 s) {speaker_accuracy(comb, y, spk):.3f}")
    report.append(f"chosen: {best}")

    # Final model on every speaker.
    E, P = r["emb"], pf_matrix(r)
    if "LDA" in best:
        sh = float(best.split("=")[1])
        w = lda_direction(E, y, spk, sh)
        a, c = calibrate(E @ w, y)
        we, be = w * a, c
    else:
        l2 = float(best.split("=")[1])
        we, be = fit_logreg(E, y, l2=l2, steps=3000, lr=2.0)
    mu, sd = P.mean(0), P.std(0) + 1e-9
    wp, bp = fit_logreg((P - mu) / sd, y, l2=1e-2)

    v = np.load(models / "american_voices.npz")
    for name in sorted(v.files):
        e = v[name].astype(np.float64).reshape(-1)
        e /= np.linalg.norm(e)
        report.append(f"  {name}: score {e @ we + be:+.2f} -> {'female' if e @ we + be > 0 else 'male'}")

    assert ecapa.center and ecapa.pad_mode == "constant", (ecapa.center, ecapa.pad_mode)
    np.savez(out / "gender.npz", ecapa_w=we.astype(np.float32), ecapa_b=np.float32(be),
             pf_mu=mu.astype(np.float32), pf_sd=sd.astype(np.float32), pf_w=wp.astype(np.float32),
             pf_b=np.float32(bp), sure=np.float32(1.0), chunk_s=np.float32(CHUNK_S),
             fe_window=ecapa.window.astype(np.float32), fe_fbank=ecapa.fbank.astype(np.float32),
             fe_n_fft=np.int64(ecapa.n_fft), fe_hop=np.int64(ecapa.hop), fe_multiplier=np.float32(ecapa.mult),
             fe_amin=np.float32(ecapa.amin), fe_db_multiplier=np.float32(ecapa.db_mult),
             fe_top_db=np.float32(ecapa.top_db))

    # Golden data for the Rust port: the first chunk of 2 male and 2 female test-clean speakers.
    gold, k = {}, 0
    sex = speakers(root, "test-clean")
    for want in ("M", "M", "F", "F"):
        spk_id = sorted(s for s, x in sex.items() if x == want)[k % 2]
        c = speaker_chunks(root / "test-clean" / spk_id, 1)[0]
        e = ecapa(c)
        f0 = pitch_track(c)
        gold[f"audio{k}"] = c.astype(np.float32)
        gold[f"feats{k}"] = ecapa.features(c).astype(np.float32)
        gold[f"emb{k}"] = e.astype(np.float32)
        gold[f"score{k}"] = np.float32(e @ we + be)
        gold[f"female{k}"] = np.int32(want == "F")
        gold[f"f0_{k}"] = np.float32(float(np.median(f0)) if len(f0) > 20 else np.nan)
        k += 1
    np.savez(out / "gender_golden.npz", **gold)

    text = "\n".join(report)
    (out / "gender_report.txt").write_text(text + "\n", encoding="utf-8")
    print(text)


if __name__ == "__main__":
    main()
