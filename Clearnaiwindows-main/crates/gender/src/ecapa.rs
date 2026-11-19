//! ECAPA voice fingerprint (`ecapa.onnx`) and the two trained classifiers (`gender.npz`, from `tools/train_gender.py`).
//!
//! Front end = speechbrain's (as ported in the accent project's `VoiceEmbedder.features`): centred 400-point STFT with
//! zero padding, power, 80 mel filters, 10*log10 with an 80 dB floor below the loudest value, mean removed per filter.

use accent_onnx::npz::Npz;
use accent_onnx::ort_util::{make_session, run, view, Feed};
use ort::session::Session;
use realfft::RealFftPlanner;
use std::borrow::Cow;
use std::path::Path;

use crate::analysis;

pub const GENDER_NPZ: &str = "gender.npz";
pub const ECAPA_ONNX: &str = "ecapa.onnx";

/// Scores for one chunk. Positive = female, negative = male; the larger, the surer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChunkScores {
    pub ecapa: f32,
    pub pitch_formants: f32,
    /// Median pitch of the chunk (Hz), for the log.
    pub f0: Option<f32>,
}

pub struct Model {
    n_fft: usize,
    hop: usize,
    window: Vec<f64>,
    fbank: Vec<f64>, // [n_fft/2+1, n_mels] row-major
    n_mels: usize,
    multiplier: f64,
    amin: f64,
    db_multiplier: f64,
    top_db: f64,
    ecapa_w: Vec<f32>,
    ecapa_b: f32,
    pf_mu: Vec<f32>,
    pf_sd: Vec<f32>,
    pf_w: Vec<f32>,
    pf_b: f32,
    /// A chunk score at least this far from 0 counts as sure.
    pub sure: f32,
    /// Voiced seconds per chunk.
    pub chunk_s: f32,
    sess: Session,
}

impl Model {
    /// `dir` holds `gender.npz` and `ecapa.onnx`. `init_ort` must have run.
    pub fn load(dir: &Path, threads: usize) -> Result<Self, String> {
        let z = Npz::open(&dir.join(GENDER_NPZ))?;
        let fbank = z.get("fe_fbank")?;
        if fbank.shape.len() != 2 {
            return Err("gender.npz: fe_fbank must be 2-D".into());
        }
        let n_fft = z.scalar("fe_n_fft")? as usize;
        if fbank.shape[0] != n_fft / 2 + 1 {
            return Err(format!("gender.npz: fe_fbank has {} rows, expected {}", fbank.shape[0], n_fft / 2 + 1));
        }
        let win = z.get("fe_window")?.data.clone();
        if win.len() > n_fft {
            return Err("gender.npz: window longer than n_fft".into());
        }
        let mut window = vec![0.0; n_fft];
        let left = (n_fft - win.len()) / 2;
        window[left..left + win.len()].copy_from_slice(&win);
        let ecapa_w = z.f32s("ecapa_w")?;
        let pf_w = z.f32s("pf_w")?;
        let sess = make_session(&dir.join(ECAPA_ONNX), threads)?;
        Ok(Self {
            n_fft,
            hop: z.scalar("fe_hop")? as usize,
            window,
            n_mels: fbank.shape[1],
            fbank: fbank.data.clone(),
            multiplier: z.scalar("fe_multiplier")?,
            amin: z.scalar("fe_amin")?,
            db_multiplier: z.scalar("fe_db_multiplier")?,
            top_db: z.scalar("fe_top_db")?,
            ecapa_b: z.scalar("ecapa_b")? as f32,
            pf_mu: z.f32s("pf_mu")?,
            pf_sd: z.f32s("pf_sd")?,
            pf_b: z.scalar("pf_b")? as f32,
            sure: z.scalar("sure")? as f32,
            chunk_s: z.scalar("chunk_s")? as f32,
            ecapa_w,
            pf_w,
            sess,
        })
    }

    /// ECAPA input features [frames, n_mels] (row-major) for 16 kHz audio.
    pub fn features(&self, x: &[f32]) -> (Vec<f32>, usize) {
        let pad = self.n_fft / 2;
        let mut xp = vec![0.0f64; x.len() + 2 * pad];
        for (d, &s) in xp[pad..].iter_mut().zip(x) {
            *d = s as f64;
        }
        let frames = if xp.len() >= self.n_fft { (xp.len() - self.n_fft) / self.hop + 1 } else { 0 };
        let fft = RealFftPlanner::<f64>::new().plan_fft_forward(self.n_fft);
        let mut inp = fft.make_input_vec();
        let mut spec = fft.make_output_vec();
        let bins = self.n_fft / 2 + 1;
        let mut db = vec![0.0f64; frames * self.n_mels];
        let mut power = vec![0.0f64; bins];
        for t in 0..frames {
            let s = t * self.hop;
            for i in 0..self.n_fft {
                inp[i] = xp[s + i] * self.window[i];
            }
            let _ = fft.process(&mut inp, &mut spec);
            for (p, c) in power.iter_mut().zip(&spec) {
                *p = c.re * c.re + c.im * c.im;
            }
            let row = &mut db[t * self.n_mels..(t + 1) * self.n_mels];
            for (m, out) in row.iter_mut().enumerate() {
                let mut acc = 0.0;
                for (b, p) in power.iter().enumerate() {
                    acc += p * self.fbank[b * self.n_mels + m];
                }
                *out = self.multiplier * acc.max(self.amin).log10() - self.multiplier * self.db_multiplier;
            }
        }
        let top = db.iter().cloned().fold(f64::NEG_INFINITY, f64::max) - self.top_db;
        db.iter_mut().for_each(|v| *v = v.max(top));
        for m in 0..self.n_mels {
            let mean = (0..frames).map(|t| db[t * self.n_mels + m]).sum::<f64>() / frames.max(1) as f64;
            for t in 0..frames {
                db[t * self.n_mels + m] -= mean;
            }
        }
        (db.into_iter().map(|v| v as f32).collect(), frames)
    }

    /// Unit-length 192-number fingerprint of 16 kHz audio.
    pub fn embed(&mut self, x: &[f32]) -> Result<Vec<f32>, String> {
        let (feats, frames) = self.features(x);
        if frames == 0 {
            return Err("audio too short".into());
        }
        let shape = [1i64, frames as i64, self.n_mels as i64];
        let feed: Feed = vec![(Cow::Borrowed("feats"), view(&shape, &feats)?)];
        let out = run(&mut self.sess, feed)?;
        let e = out.into_iter().next().ok_or("ecapa: no output")?.data;
        let norm = e.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>().sqrt().max(1e-12);
        Ok(e.iter().map(|&v| (v as f64 / norm) as f32).collect())
    }

    pub fn ecapa_score(&self, emb: &[f32]) -> f32 {
        emb.iter().zip(&self.ecapa_w).map(|(a, b)| a * b).sum::<f32>() + self.ecapa_b
    }

    /// Pitch + formants score, exactly as trained: log of [F0, F1, F2, F3] (150 / 1000 Hz when missing), standardised.
    pub fn pitch_formant_score(&self, f0: Option<f64>, fm: Option<[f64; 3]>) -> f32 {
        let fm = fm.unwrap_or([1000.0; 3]);
        let v = [f0.unwrap_or(150.0).ln(), fm[0].ln(), fm[1].ln(), fm[2].ln()];
        (0..4).map(|i| ((v[i] as f32 - self.pf_mu[i]) / self.pf_sd[i]) * self.pf_w[i]).sum::<f32>() + self.pf_b
    }

    /// All three signals for one chunk of voiced 16 kHz speech.
    pub fn score_chunk(&mut self, x: &[f32]) -> Result<ChunkScores, String> {
        let emb = self.embed(x)?;
        let f0 = analysis::median_pitch(x);
        let fm = analysis::median_formants(x);
        Ok(ChunkScores { ecapa: self.ecapa_score(&emb), pitch_formants: self.pitch_formant_score(f0, fm), f0: f0.map(|v| v as f32) })
    }
}
