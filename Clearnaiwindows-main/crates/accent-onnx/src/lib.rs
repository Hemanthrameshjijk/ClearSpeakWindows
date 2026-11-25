//! Real-time accent conversion (Indian -> American English) with ONNX Runtime: a Rust port of the reference
//! engine in github.com/KarthikGavini/Accent-Converter (code/engine.py + dsp.py). The math is ported 1:1 and
//! verified against fixtures produced by the reference (see `tests/golden.rs`, `tools/make_golden.py`).
//!
//! Signal path of [`AccentStream`]:  16 kHz mono -> [`logmel::StreamingLogMel`] -> [`converter::StreamingConverterV5`]
//! -> [`vocoder::CausalStreamer`] -> 24 kHz mono. Ten fixed American voices, or "My voice" ([`own_speaker`]).

pub mod causal_resampler;
pub mod converter;
pub mod logmel;
pub mod npz;
pub mod ort_util;
pub mod pipeline;
pub mod resample;
pub mod vocoder;

use converter::StreamingConverterV5;
use logmel::{LogMel, StreamingLogMel};
use npz::Npz;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use vocoder::CausalStreamer;

/// Voice index `i + 1` is `VOICE_NAMES[i]` (the key in `american_voices.npz`). Saved settings store the index, so
/// new voices are only ever added at the end: 1-3 male 1-3, 4-6 female 1-3, 7-8 male 4-5, 9-10 female 4-5.
pub const VOICE_NAMES: [&str; 10] = [
    "american_male_1",
    "american_male_2",
    "american_male_3",
    "american_female_1",
    "american_female_2",
    "american_female_3",
    "american_male_4",
    "american_male_5",
    "american_female_4",
    "american_female_5",
];

/// Voice picker labels (without "Off"): choice `i` is voice index `i + 1`.
pub const VOICE_CHOICES: [&str; 10] = [
    "American male 1",
    "American male 2",
    "American male 3",
    "American female 1",
    "American female 2",
    "American female 3",
    "American male 4",
    "American male 5",
    "American female 4",
    "American female 5",
];

/// Voice indexes by gender, in picker order; position `p` in one list pairs with position `p` in the other.
pub const MALE_VOICES: [u8; 5] = [1, 2, 3, 7, 8];
pub const FEMALE_VOICES: [u8; 5] = [4, 5, 6, 9, 10];

/// Files that make up the model bundle (what `setup.rs` ships / compares). ECAPA and RNNoise are not needed.
pub const BUNDLE_FILES: [&str; 8] = [
    "catalog.json",
    "features.npz",
    "american_voices.npz",
    "converter_best_v6dmel.npz",
    "converter_best_v6dmel.onnx",
    "converter_best_v6dmel.dec.onnx",
    "vocoder_causal_hifigan_best.npz",
    "vocoder_causal_hifigan_best.onnx",
];

/// "My voice": how much of the agent's own fingerprint goes into the speaker vector; the rest is the nearest of the
/// trained American voices. Own-voice test (24 Nov, 3 speakers, ECAPA similarity of the output to the speaker): for
/// a speaker far from every trained voice, nearest voice +0.27, 70% own +0.40, 100% own +0.54; speakers close to a
/// trained voice scored ~+0.9 either way. 70% keeps more of the person while staying near voices the model knows.
pub const OWN_VOICE_SHARE: f32 = 0.7;
/// Length of a voice fingerprint (ECAPA).
pub const FINGERPRINT_LEN: usize = 192;

/// `share` of the unit-length `own` fingerprint plus the rest of the most similar trained voice, made unit-length
/// again (the trained voices are unit-length too). Same recipe as the own-voice test.
pub fn own_speaker(own: &[f32], voices: &BTreeMap<String, Vec<f32>>, share: f32) -> Result<Vec<f32>, String> {
    if own.len() != FINGERPRINT_LEN || !own.iter().all(|v| v.is_finite()) {
        return Err(format!("voice fingerprint must be {FINGERPRINT_LEN} numbers"));
    }
    let unit = |v: &[f32]| -> Vec<f32> {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
        v.iter().map(|x| x / n).collect()
    };
    let own = unit(own);
    let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
    let nearest = voices
        .values()
        .filter(|v| v.len() == FINGERPRINT_LEN)
        .max_by(|a, b| dot(a, &own).total_cmp(&dot(b, &own)))
        .ok_or("no trained voices")?;
    let near = unit(nearest);
    Ok(unit(&own.iter().zip(&near).map(|(o, n)| share * o + (1.0 - share) * n).collect::<Vec<_>>()))
}

/// The trained voice (index, 1-based as in [`VOICE_NAMES`]) most like the `own` fingerprint: the one My voice is
/// blended with, and the comparison voice in the My voice preview.
pub fn nearest_voice(models: &Path, own: &[f32]) -> Result<u8, String> {
    let vz = Npz::open(&models.join("american_voices.npz"))?;
    let dot = |a: &[f32]| a.iter().zip(own).map(|(x, y)| x * y).sum::<f32>();
    let mut best = (0u8, f32::NEG_INFINITY);
    for (i, name) in VOICE_NAMES.iter().enumerate() {
        let d = dot(&vz.f32s(name)?);
        if d > best.1 {
            best = (i as u8 + 1, d);
        }
    }
    Ok(best.0)
}

/// Load `onnxruntime.dll` once for the process.
pub fn init_runtime(dll: &Path) -> Result<(), String> {
    ort_util::init_ort(dll)
}

/// One step of the stream, stage by stage (used by the golden tests; `audio` still contains the vocoder lag).
pub struct StepOut {
    pub mel_in: Vec<f32>,
    pub mel_out: Vec<f32>,
    pub audio: Vec<f32>,
}

pub struct AccentStream {
    spec: StreamingLogMel,
    conv: StreamingConverterV5,
    voc: CausalStreamer,
    voices: BTreeMap<String, Vec<f32>>,
    /// Vocoder-lag samples still to drop from the start of the stream.
    lag_left: usize,
}

impl AccentStream {
    pub fn load(models: &Path, threads: usize, voice: &str) -> Result<Self, String> {
        let cat: serde_json::Value = serde_json::from_slice(&std::fs::read(models.join("catalog.json")).map_err(|e| format!("catalog.json: {e}"))?)
            .map_err(|e| format!("catalog.json: {e}"))?;
        let s = |v: &serde_json::Value, k: &str| -> Result<String, String> {
            v.get(k).and_then(|x| x.as_str()).map(str::to_string).ok_or_else(|| format!("catalog.json: '{k}' missing"))
        };
        let conv_entry = cat["converters"].get(0).ok_or("catalog.json: no converters")?;
        let mel = LogMel::from_npz(&Npz::open(&models.join(s(&cat, "features")?))?)?;
        let vz = Npz::open(&models.join("american_voices.npz"))?;
        let mut voices = BTreeMap::new();
        for name in VOICE_NAMES {
            voices.insert(name.to_string(), vz.f32s(name)?);
        }
        let spk = voices.get(voice).ok_or_else(|| format!("unknown voice '{voice}'"))?.clone();
        let conv = StreamingConverterV5::load(models, &s(conv_entry, "npz")?, &s(conv_entry, "onnx")?, threads, &spk)?;
        let voc = CausalStreamer::load(models, &s(&cat["vocoder"], "npz")?, &s(&cat["vocoder"], "onnx")?, threads)?;
        let lag_left = voc.lag();
        Ok(Self { spec: StreamingLogMel::new(mel), conv, voc, voices, lag_left })
    }

    pub fn set_voice(&mut self, voice: &str) -> Result<(), String> {
        let spk = self.voices.get(voice).ok_or_else(|| format!("unknown voice '{voice}'"))?;
        self.conv.set_voice(spk);
        Ok(())
    }

    /// "My voice": the converter speaks with `own` (a recorded ECAPA fingerprint) blended with the nearest trained
    /// voice, see [`own_speaker`]. Costs nothing per frame; only the speaker vector changes.
    pub fn set_own_voice(&mut self, own: &[f32]) -> Result<(), String> {
        let spk = own_speaker(own, &self.voices, OWN_VOICE_SHARE)?;
        self.conv.set_voice(&spk);
        Ok(())
    }

    pub fn reset(&mut self) {
        self.spec.reset();
        self.conv.reset();
        self.voc.reset();
        self.lag_left = self.voc.lag();
    }

    /// Algorithmic delay of the model chain in ms (converter look-ahead + vocoder look-ahead), excluding the
    /// 25 ms mel window reach and any compute / buffering time.
    pub fn algorithmic_latency_ms(&self) -> f32 {
        self.conv.latency_ms() + (self.voc.lag() as f32 / 24.0)
    }

    /// 16 kHz samples in, stage outputs out. `audio` is the raw vocoder output (lag not dropped).
    pub fn push_stages(&mut self, x16: &[f32]) -> Result<StepOut, String> {
        let mel_in = self.spec.push(x16);
        let mel_out = self.conv.push(&mel_in)?;
        let audio = self.voc.push(&mel_out)?;
        Ok(StepOut { mel_in, mel_out, audio })
    }

    pub fn flush_stages(&mut self) -> Result<StepOut, String> {
        let mel_in = self.spec.flush();
        let mut mel_out_all = self.conv.push(&mel_in)?;
        mel_out_all.extend(self.conv.flush()?);
        let mut audio = self.voc.push(&mel_out_all)?;
        audio.extend(self.voc.flush()?);
        Ok(StepOut { mel_in, mel_out: mel_out_all, audio })
    }

    fn drop_lag(&mut self, mut audio: Vec<f32>) -> Vec<f32> {
        let d = self.lag_left.min(audio.len());
        self.lag_left -= d;
        audio.drain(..d);
        audio
    }

    /// 16 kHz in -> 24 kHz audio ready to play (the first `lag` samples of junk are removed).
    pub fn push(&mut self, x16: &[f32]) -> Result<Vec<f32>, String> {
        let a = self.push_stages(x16)?.audio;
        Ok(self.drop_lag(a))
    }

    pub fn flush(&mut self) -> Result<Vec<f32>, String> {
        let a = self.flush_stages()?.audio;
        Ok(self.drop_lag(a))
    }

    /// Time spent inside ONNX Runtime (converter, vocoder) since the last call.
    pub fn take_busy(&mut self) -> (Duration, Duration) {
        (self.conv.take_busy(), self.voc.take_busy())
    }
}

/// Where the model bundle lives: `ACCENT_MODELS_DIR`, else `app_models` next to the exe, else
/// `%LOCALAPPDATA%\ClearSpeak\app_models`.
pub fn find_models_dir() -> Option<PathBuf> {
    let ok = |p: &PathBuf| BUNDLE_FILES.iter().all(|f| p.join(f).exists());
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("ACCENT_MODELS_DIR") {
        cands.push(p.into());
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            cands.push(d.join("app_models"));
        }
    }
    if let Ok(l) = std::env::var("LOCALAPPDATA") {
        cands.push(PathBuf::from(l).join("ClearSpeak").join("app_models"));
    }
    cands.into_iter().find(ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn axis(i: usize) -> Vec<f32> {
        let mut v = vec![0.0; FINGERPRINT_LEN];
        v[i] = 1.0;
        v
    }

    fn voices() -> BTreeMap<String, Vec<f32>> {
        [("a", axis(0)), ("b", axis(1))].into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }

    #[test]
    fn my_voice_is_70_percent_own_plus_the_nearest_trained_voice() {
        // Own voice mostly like "b", partly something no trained voice has.
        let mut own = vec![0.0; FINGERPRINT_LEN];
        own[1] = 0.6;
        own[2] = 0.8;
        let s = own_speaker(&own, &voices(), 0.7).unwrap();
        let norm = s.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "unit length like the trained voices");
        assert_eq!(s[0], 0.0, "the far voice 'a' is not used");
        // before re-normalising: b = 0.7*0.6 + 0.3 = 0.72, own part = 0.7*0.8 = 0.56
        assert!((s[1] / s[2] - 0.72 / 0.56).abs() < 1e-4, "{} {}", s[1], s[2]);
    }

    #[test]
    fn a_voice_equal_to_a_trained_one_stays_that_voice() {
        let s = own_speaker(&axis(0), &voices(), 0.7).unwrap();
        assert!((s[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_broken_fingerprint_is_refused() {
        assert!(own_speaker(&[1.0; 10], &voices(), 0.7).is_err());
        let mut nan = axis(0);
        nan[3] = f32::NAN;
        assert!(own_speaker(&nan, &voices(), 0.7).is_err());
    }
}
