//! Voice detector, pitch and formants at 16 kHz. Pitch and formants are exact ports of `tools/train_gender.py`
//! (`pitch_track`, `formants`), because the trained pitch + formant classifier only fits numbers made the same way.

use realfft::RealFftPlanner;
use std::collections::VecDeque;

pub const SR: usize = 16_000;
/// 30 ms analysis frames, 10 ms hop (voice detector and pitch).
pub const FRAME: usize = 480;
pub const HOP: usize = 160;

/// Streaming voice detector: passes on the 10 ms hops whose 30 ms energy is within 30 dB of the loud speech of the
/// last ~10 s (and above -60 dB). The training script does the same over a whole file at once.
pub struct VoiceGate {
    pending: Vec<f32>,
    recent_db: VecDeque<f32>,
    loud_db: f32,
    hops_since_update: usize,
}

const RECENT_HOPS: usize = 1000;

impl Default for VoiceGate {
    fn default() -> Self {
        Self { pending: Vec::new(), recent_db: VecDeque::with_capacity(RECENT_HOPS), loud_db: f32::NEG_INFINITY, hops_since_update: 0 }
    }
}

impl VoiceGate {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Feeds 16 kHz audio; appends the voiced hops to `out`.
    pub fn push(&mut self, x: &[f32], out: &mut Vec<f32>) {
        self.pending.extend_from_slice(x);
        let mut start = 0;
        while self.pending.len() - start >= FRAME {
            let fr = &self.pending[start..start + FRAME];
            let db = 10.0 * ((fr.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / FRAME as f64) + 1e-10).log10();
            let db = db as f32;
            if self.recent_db.len() == RECENT_HOPS {
                self.recent_db.pop_front();
            }
            self.recent_db.push_back(db);
            self.hops_since_update += 1;
            if self.hops_since_update >= 100 || !self.loud_db.is_finite() {
                self.hops_since_update = 0;
                self.loud_db = percentile(self.recent_db.iter().copied(), 95.0);
            }
            if db > (self.loud_db - 30.0).max(-60.0) {
                out.extend_from_slice(&self.pending[start..start + HOP]);
            }
            start += HOP;
        }
        self.pending.drain(..start);
    }
}

/// numpy's default (linear) percentile.
fn percentile(v: impl Iterator<Item = f32>, p: f64) -> f32 {
    let mut s: Vec<f32> = v.collect();
    if s.is_empty() {
        return f32::NEG_INFINITY;
    }
    s.sort_by(|a, b| a.total_cmp(b));
    let pos = p / 100.0 * (s.len() - 1) as f64;
    let (i, f) = (pos.floor() as usize, pos - pos.floor());
    let hi = s[(i + 1).min(s.len() - 1)];
    (s[i] as f64 + f * (hi as f64 - s[i] as f64)) as f32
}

/// numpy's median (mean of the two middle values for an even count). `None` when empty.
pub fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

/// Median F0 (Hz) of the voiced frames (normalised autocorrelation peak above 0.55, 60-400 Hz); `None` with 20 or
/// fewer voiced frames.
pub fn median_pitch(x: &[f32]) -> Option<f64> {
    let (lo, hi) = (SR / 400, SR / 60);
    let mut f0s = Vec::new();
    let mut fr = vec![0.0f64; FRAME];
    let mut s = 0;
    while s + FRAME <= x.len() {
        let mean = x[s..s + FRAME].iter().map(|&v| v as f64).sum::<f64>() / FRAME as f64;
        for (d, &v) in fr.iter_mut().zip(&x[s..s + FRAME]) {
            *d = v as f64 - mean;
        }
        let ac0 = fr.iter().map(|v| v * v).sum::<f64>() + 1e-12;
        let (mut best, mut best_lag) = (f64::NEG_INFINITY, lo);
        for lag in lo..hi {
            let ac: f64 = fr[..FRAME - lag].iter().zip(&fr[lag..]).map(|(a, b)| a * b).sum();
            let v = ac / ac0;
            if v > best {
                best = v;
                best_lag = lag;
            }
        }
        if best > 0.55 {
            f0s.push(SR as f64 / best_lag as f64);
        }
        s += HOP;
    }
    if f0s.len() > 20 { median(&mut f0s) } else { None }
}

const LPC_ORDER: usize = 18;
const LPC_N: usize = 400;
const LPC_FFT: usize = 1024;

/// Median F1, F2, F3 (Hz) over every 4th 25 ms frame: the first three peaks (200-4500 Hz) of the LPC envelope.
pub fn median_formants(x: &[f32]) -> Option<[f64; 3]> {
    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(LPC_FFT);
    let mut inp = fft.make_input_vec();
    let mut spec = fft.make_output_vec();
    let win: Vec<f64> =
        (0..LPC_N).map(|n| 0.54 - 0.46 * (2.0 * std::f64::consts::PI * n as f64 / (LPC_N - 1) as f64).cos()).collect();
    let (lo, hi) = ((200 * LPC_FFT / SR).max(1), (4500 * LPC_FFT / SR).min(LPC_FFT / 2));
    let mut found: [Vec<f64>; 3] = Default::default();
    let mut f = vec![0.0f64; LPC_N];
    let mut s = 0;
    let mut k_frame = 0usize;
    while s + LPC_N <= x.len() {
        let take = k_frame % 4 == 0;
        k_frame += 1;
        let start = s;
        s += HOP;
        if !take {
            continue;
        }
        let fr = &x[start..start + LPC_N];
        f[0] = fr[0] as f64 * win[0];
        for i in 1..LPC_N {
            f[i] = (fr[i] as f64 - 0.97 * fr[i - 1] as f64) * win[i];
        }
        if f.iter().map(|v| v * v).sum::<f64>() / (LPC_N as f64) < 1e-7 {
            continue;
        }
        let r: Vec<f64> = (0..=LPC_ORDER).map(|k| f[..LPC_N - k].iter().zip(&f[k..]).map(|(a, b)| a * b).sum()).collect();
        let Some(a) = levinson(&r, LPC_ORDER) else { continue };
        inp.iter_mut().for_each(|v| *v = 0.0);
        inp[..a.len()].copy_from_slice(&a);
        if fft.process(&mut inp, &mut spec).is_err() {
            continue;
        }
        let env: Vec<f64> = spec.iter().map(|c| -(c.norm() + 1e-12).ln()).collect();
        let peaks: Vec<usize> = (lo..hi.min(env.len() - 1)).filter(|&k| env[k] > env[k - 1] && env[k] >= env[k + 1]).take(3).collect();
        if peaks.len() == 3 {
            for (j, &k) in peaks.iter().enumerate() {
                found[j].push(k as f64 * SR as f64 / LPC_FFT as f64);
            }
        }
    }
    let [mut a, mut b, mut c] = found;
    Some([median(&mut a)?, median(&mut b)?, median(&mut c)?])
}

/// Levinson-Durbin, written exactly like the training script's.
fn levinson(r: &[f64], order: usize) -> Option<Vec<f64>> {
    let mut a = vec![0.0; order + 1];
    a[0] = 1.0;
    let mut e = r[0];
    if e <= 0.0 {
        return None;
    }
    for i in 1..=order {
        let acc: f64 = (1..i).map(|j| a[j] * r[i - j]).sum();
        let k = -(r[i] + acc) / e;
        let old = a.clone();
        for j in 1..i {
            a[j] = old[j] + k * old[i - j];
        }
        a[i] = k;
        e *= 1.0 - k * k;
        if e <= 0.0 {
            return None;
        }
    }
    Some(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(f0: f64, secs: f64) -> Vec<f32> {
        // A voice-like buzz: harmonics of f0 with falling level.
        (0..(secs * SR as f64) as usize)
            .map(|i| {
                let t = i as f64 / SR as f64;
                (1..12).map(|h| (2.0 * std::f64::consts::PI * f0 * h as f64 * t).sin() / h as f64).sum::<f64>() as f32 * 0.1
            })
            .collect()
    }

    #[test]
    fn pitch_of_low_and_high_voices() {
        let m = median_pitch(&tone(110.0, 1.0)).unwrap();
        let f = median_pitch(&tone(220.0, 1.0)).unwrap();
        assert!((m - 110.0).abs() < 3.0, "male-like {m}");
        assert!((f - 220.0).abs() < 6.0, "female-like {f}");
        assert_eq!(median_pitch(&vec![0.0; 16_000]), None, "silence has no pitch");
    }

    #[test]
    fn formants_come_out_in_order() {
        let fm = median_formants(&tone(120.0, 1.0)).unwrap();
        assert!(fm[0] < fm[1] && fm[1] < fm[2] && fm[0] >= 200.0 && fm[2] <= 4500.0, "{fm:?}");
    }

    #[test]
    fn gate_keeps_speech_and_drops_silence() {
        let mut g = VoiceGate::default();
        let mut x = tone(150.0, 2.0);
        x.extend(vec![0.0; 32_000]);
        x.extend(tone(150.0, 1.0));
        let mut out = Vec::new();
        for c in x.chunks(160) {
            g.push(c, &mut out);
        }
        let voiced_s = out.len() as f64 / SR as f64;
        assert!((2.6..=3.1).contains(&voiced_s), "about 3 s of the 5 s are voice, got {voiced_s}");
    }

    #[test]
    fn median_matches_numpy() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(median(&mut []), None);
    }
}
