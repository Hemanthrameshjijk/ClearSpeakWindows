//! Male / female detection from the agent's voice, so Accent can pick a matching American voice.
//!
//! Three signals from the same 3 s chunks of voiced 16 kHz speech (see `tools/train_gender.py`, which trains the
//! numbers in `gender.npz` and checks them on speakers it never saw):
//! 1. pitch: median F0 of the voiced frames;
//! 2. formants: median F1..F3, the resonances of the throat and mouth;
//! 3. ECAPA: the 192-number voice fingerprint (`ecapa.onnx`), scored by a small trained classifier.
//!
//! ECAPA decides; when it is unsure, pitch + formants decide if they are sure ([`detector`]).
//! [`pipeline`] taps the mic chain (the frame is never changed) and does all the work on its own low-priority thread.

pub mod analysis;
pub mod detector;
pub mod ecapa;
pub mod pipeline;

/// What the detector concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gender {
    Male,
    Female,
}

impl Gender {
    /// `1` male, `2` female (`0` is used for "not known yet" wherever a plain number is stored).
    pub fn code(self) -> u8 {
        match self {
            Gender::Male => 1,
            Gender::Female => 2,
        }
    }

    pub fn from_code(c: u8) -> Option<Self> {
        match c {
            1 => Some(Gender::Male),
            2 => Some(Gender::Female),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Gender::Male => "male",
            Gender::Female => "female",
        }
    }

    /// The Accent voice of this gender in the same position as `voice` (male 2 <-> female 2, male 4 <-> female 4; see
    /// `accent_onnx::MALE_VOICES` / `FEMALE_VOICES`). An unknown `voice` counts as position 1.
    pub fn voice_like(self, voice: u8) -> u8 {
        use accent_onnx::{FEMALE_VOICES, MALE_VOICES};
        let p = MALE_VOICES.iter().chain(FEMALE_VOICES.iter()).position(|&v| v == voice).unwrap_or(0) % MALE_VOICES.len();
        match self {
            Gender::Male => MALE_VOICES[p],
            Gender::Female => FEMALE_VOICES[p],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_number_is_kept_across_genders() {
        assert_eq!(Gender::Female.voice_like(2), 5, "male 2 -> female 2");
        assert_eq!(Gender::Male.voice_like(6), 3, "female 3 -> male 3");
        assert_eq!(Gender::Male.voice_like(1), 1);
        assert_eq!(Gender::Female.voice_like(0), 4, "no voice yet -> female 1");
        assert_eq!(Gender::Female.voice_like(7), 9, "male 4 -> female 4");
        assert_eq!(Gender::Male.voice_like(10), 8, "female 5 -> male 5");
        assert_eq!(Gender::Male.voice_like(8), 8);
        // The lists match the voice names, so Auto never picks a voice of the wrong gender.
        for (&m, &f) in accent_onnx::MALE_VOICES.iter().zip(accent_onnx::FEMALE_VOICES.iter()) {
            assert!(accent_onnx::VOICE_NAMES[m as usize - 1].contains("_male_"), "voice {m}");
            assert!(accent_onnx::VOICE_NAMES[f as usize - 1].contains("_female_"), "voice {f}");
        }
        assert_eq!(accent_onnx::MALE_VOICES.len() + accent_onnx::FEMALE_VOICES.len(), accent_onnx::VOICE_NAMES.len());
        assert_eq!(Gender::from_code(Gender::Female.code()), Some(Gender::Female));
        assert_eq!(Gender::from_code(0), None);
    }
}
