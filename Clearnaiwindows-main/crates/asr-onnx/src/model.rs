//! The speech model: log-mel -> quantised FastConformer (CTC-style logits, 80 ms per token) -> greedy decode.

use crate::features::Features;
use ort::session::{Session, SessionInputValue};
use ort::value::TensorRef;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

pub const MODEL_FILE: &str = "fast_conformer_with_laughter_MatMul_qu8.ort";
pub const VOCAB_FILE: &str = "vocab.txt";
/// Files a model directory must contain.
pub const BUNDLE_FILES: [&str; 2] = [MODEL_FILE, VOCAB_FILE];
const BLANK: usize = 1024;
/// The model was trained on short chunks: past about 8 s the first words of the input start to be cut short
/// ("safe road" -> "sae ro"). Longer audio is decoded in overlapping windows (fpConfig.json: chunk + context on
/// both sides) and only the frames of each window's core are kept.
const SINGLE_PASS_MAX_S: f64 = 8.0;
const CORE_S: f64 = 3.0;
const PAST_S: f64 = 2.0;
const FUTURE_S: f64 = 2.0;
const TOKEN_S: f64 = 0.08; // 80 ms per output frame
const SR: f64 = 16_000.0;

/// ASR_MODELS_DIR, else `asr_models` next to the exe, else %LOCALAPPDATA%\ClearSpeak\asr_models.
pub fn find_models_dir() -> Option<PathBuf> {
    let ok = |d: &Path| BUNDLE_FILES.iter().all(|f| d.join(f).is_file());
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("ASR_MODELS_DIR") {
        cands.push(PathBuf::from(d));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(p) = exe.parent() {
            cands.push(p.join("asr_models"));
        }
    }
    if let Some(l) = std::env::var_os("LOCALAPPDATA") {
        cands.push(PathBuf::from(l).join("ClearSpeak").join("asr_models"));
    }
    cands.into_iter().find(|d| ok(d))
}

pub struct AsrModel {
    sess: Session,
    vocab: Vec<String>,
    feats: Features,
}

impl AsrModel {
    /// `init_ort` must have been called once before.
    pub fn load(dir: &Path, threads: usize) -> Result<Self, String> {
        let vocab: Vec<String> = std::fs::read_to_string(dir.join(VOCAB_FILE))
            .map_err(|e| format!("{}: {e}", dir.join(VOCAB_FILE).display()))?
            .lines()
            .map(|l| l.trim_end_matches('\r').to_string())
            .collect();
        if vocab.len() != BLANK {
            return Err(format!("vocab.txt has {} entries, expected {BLANK}", vocab.len()));
        }
        let sess = accent_onnx::ort_util::make_session(&dir.join(MODEL_FILE), threads)?;
        Ok(Self { sess, vocab, feats: Features::new() })
    }

    /// Speech -> lowercase text (no punctuation). Empty string for silence / too-short audio.
    pub fn transcribe(&mut self, audio16: &[f32]) -> Result<String, String> {
        let ids = self.frame_ids(audio16)?;
        Ok(self.detokenize(&ids))
    }

    /// Like [`transcribe`](Self::transcribe), but word by word with times (seconds from the start of `audio16`).
    pub fn transcribe_words(&mut self, audio16: &[f32]) -> Result<Vec<Word>, String> {
        let ids = self.frame_ids(audio16)?;
        Ok(words_with_times(&self.vocab, &ids))
    }

    /// Greedy token id per 80 ms output frame for the whole of `audio16` (long audio decoded in windows).
    fn frame_ids(&mut self, audio16: &[f32]) -> Result<Vec<usize>, String> {
        // Per-feature normalisation turns near-silence into confident nonsense ("yeah"), so skip it outright.
        if audio16.iter().fold(0.0f32, |m, v| m.max(v.abs())) < 1e-3 {
            return Ok(Vec::new());
        }
        let total = audio16.len() as f64 / SR;
        if total <= SINGLE_PASS_MAX_S {
            return self.token_ids(audio16);
        }
        let mut kept: Vec<usize> = Vec::new();
        let mut pos = 0.0f64;
        while pos < total - 1e-6 {
            let core_end = (pos + CORE_S).min(total);
            let (w0, w1) = ((pos - PAST_S).max(0.0), (core_end + FUTURE_S).min(total));
            let ids = self.token_ids(&audio16[(w0 * SR) as usize..(w1 * SR) as usize])?;
            let f0 = (((pos - w0) / TOKEN_S).round() as usize).min(ids.len());
            let f1 = (((core_end - w0) / TOKEN_S).round() as usize).min(ids.len());
            kept.extend_from_slice(&ids[f0..f1]);
            // Keep the frame count in step with time even if a window came back a frame short.
            let want = ((core_end / TOKEN_S).round() as usize).max(kept.len());
            kept.resize(want, BLANK);
            pos = core_end;
        }
        Ok(kept)
    }

    /// Greedy per-frame token ids (CTC, before collapsing) for one window of audio.
    fn token_ids(&mut self, audio16: &[f32]) -> Result<Vec<usize>, String> {
        let Some((mel, frames)) = self.feats.compute(audio16) else { return Ok(Vec::new()) };
        let shape = vec![1i64, crate::features::N_MELS as i64, frames as i64];
        let len = [frames as i64];
        let feed: Vec<(Cow<str>, SessionInputValue)> = vec![
            (Cow::Borrowed("audio_signal"), SessionInputValue::from(TensorRef::from_array_view((shape, &mel[..])).map_err(|e| e.to_string())?)),
            (Cow::Borrowed("length"), SessionInputValue::from(TensorRef::from_array_view((vec![1i64], &len[..])).map_err(|e| e.to_string())?)),
        ];
        let out = self.sess.run(feed).map_err(|e| e.to_string())?;
        let ids = {
            let (shape, data) = out[1].try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
            let (t, v) = (shape[1] as usize, shape[2] as usize);
            let mut ids = Vec::with_capacity(t);
            for i in 0..t {
                let row = &data[i * v..(i + 1) * v];
                let mut best = 0usize;
                for (k, x) in row.iter().enumerate() {
                    if *x > row[best] {
                        best = k;
                    }
                }
                ids.push(best);
            }
            ids
        };
        drop(out);
        Ok(ids)
    }

    /// CTC collapse + word-piece join (`##x` continues the previous word).
    pub fn detokenize(&self, ids: &[usize]) -> String {
        join_pieces(&self.vocab, ids)
    }
}

/// One recognised word and when it was said (seconds).
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    pub start: f32,
    pub end: f32,
}

/// CTC collapse + word-piece join, keeping each word's first and last frame. Gives the same words as
/// [`join_pieces`].
fn words_with_times(vocab: &[String], ids: &[usize]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut open = false; // the last word can still take "##" pieces
    let mut prev = usize::MAX;
    for (f, &i) in ids.iter().enumerate() {
        let (t0, t1) = (f as f32 * TOKEN_S as f32, (f + 1) as f32 * TOKEN_S as f32);
        if i == prev && open && i != BLANK {
            if let Some(w) = words.last_mut() {
                w.end = t1; // the same token held over several frames
            }
        }
        if i != prev && i != BLANK && i != 0 && i < vocab.len() {
            let tok = &vocab[i];
            match tok.strip_prefix("##") {
                Some(rest) if open => {
                    let w = words.last_mut().unwrap();
                    w.text.push_str(rest);
                    w.end = t1;
                }
                Some(rest) => {
                    words.push(Word { text: rest.to_string(), start: t0, end: t1 });
                    open = true;
                }
                None => {
                    words.push(Word { text: tok.to_string(), start: t0, end: t1 });
                    open = true;
                }
            }
        }
        prev = i;
    }
    // A bare "▁" (or one inside a piece) is only a word break, never a word.
    let mut out = Vec::with_capacity(words.len());
    for w in words {
        for part in w.text.split(|c: char| c.is_whitespace() || c == '\u{2581}').filter(|p| !p.is_empty()) {
            out.push(Word { text: part.to_string(), start: w.start, end: w.end });
        }
    }
    out
}

fn join_pieces(vocab: &[String], ids: &[usize]) -> String {
    let mut text = String::new();
    let mut prev = usize::MAX;
    for &i in ids {
        if i != prev && i != BLANK && i != 0 && i < vocab.len() {
            let tok = &vocab[i];
            match tok.strip_prefix("##") {
                Some(rest) => text.push_str(rest),
                None => {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(tok);
                }
            }
        }
        prev = i;
    }
    // The vocab also holds a bare "▁" piece: a word-boundary marker, not a word. Left in, it showed up in the
    // transcript as "▁" or glued to the next piece ("▁ch"); here it becomes a plain word break.
    text.split(|c: char| c.is_whitespace() || c == '\u{2581}').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_boundary_marker_never_reaches_the_text() {
        let vocab: Vec<String> = ["<unk>", "hello", "##s", "\u{2581}", "##ch", "world"].iter().map(|s| s.to_string()).collect();
        // hello + ##s -> "hellos"; "▁" then "##ch" must not give "▁ch"; a trailing bare "▁" must vanish.
        assert_eq!(join_pieces(&vocab, &[1, 2, 3, 4, 5, 3]), "hellos ch world");
        assert_eq!(join_pieces(&vocab, &[1, 1, BLANK, 1, 5]), "hello hello world"); // CTC: repeats collapse, a blank splits them
    }

    #[test]
    fn words_carry_their_times_and_match_the_plain_text() {
        let vocab: Vec<String> = ["<unk>", "hello", "##s", "\u{2581}", "##ch", "world"].iter().map(|s| s.to_string()).collect();
        let b = BLANK;
        // frame: 0 hello, 1 hello, 2 ##s, 3 blank, 4 blank, 5 ▁, 6 ##ch, 7 blank, 8 world, 9 blank
        let ids = [1, 1, 2, b, b, 3, 4, b, 5, b];
        let w = words_with_times(&vocab, &ids);
        let text: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(text.join(" "), join_pieces(&vocab, &ids));
        assert_eq!(text, ["hellos", "ch", "world"]);
        let close = |a: f32, b: f32| (a - b).abs() < 1e-4;
        assert!(close(w[0].start, 0.0) && close(w[0].end, 0.24), "{:?}", w[0]);
        assert!(close(w[1].start, 0.40) && close(w[1].end, 0.56), "{:?}", w[1]);
        assert!(close(w[2].start, 0.64) && close(w[2].end, 0.72), "{:?}", w[2]);
    }
}
