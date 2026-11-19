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

    /// The Accent voice (1..=6: male 1-3, female 1-3) of this gender with the same number (1-3) as `voice`.
    pub fn voice_like(self, voice: u8) -> u8 {
        let n = if (1..=6).contains(&voice) { (voice - 1) % 3 + 1 } else { 1 };
        match self {
            Gender::Male => n,
            Gender::Female => n + 3,
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
        assert_eq!(Gender::from_code(Gender::Female.code()), Some(Gender::Female));
        assert_eq!(Gender::from_code(0), None);
    }
}
