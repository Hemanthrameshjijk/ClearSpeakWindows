//! Turns per-chunk scores into one steady answer.
//!
//! - Each 3 s chunk gives a vote: ECAPA's score when it is sure; otherwise pitch + formants if *they* are sure;
//!   otherwise ECAPA's (weak) score.
//! - First answer: after 2 chunks (~6 s of speech) whose average is sure, or 1 chunk that is very sure.
//! - After that the answer only changes if the last 10 chunks (~30 s of speech) clearly say the other gender (average
//!   well past "sure" and at least 8 of the 10 agree): a different person on a shared PC, never a single odd chunk.

use std::collections::VecDeque;

use crate::ecapa::ChunkScores;
use crate::Gender;

const RECHECK_CHUNKS: usize = 10;
const RECHECK_AGREE: usize = 8;
/// Past this many unsure chunks before the first answer, only the latest ones are averaged.
const FIRST_WINDOW: usize = 10;
const VOTE_LIMIT: f32 = 6.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Change {
    pub gender: Gender,
    /// `true` for the first answer, `false` when an earlier answer was overturned.
    pub first: bool,
    /// Average vote that decided it.
    pub strength: f32,
}

pub struct Detector {
    sure: f32,
    answer: Option<Gender>,
    /// Votes before the first answer.
    early: VecDeque<f32>,
    /// Votes since the answer was given (for re-checking).
    recent: VecDeque<f32>,
}

impl Detector {
    /// `known`: the answer saved for this person earlier (used straight away, still re-checked).
    pub fn new(sure: f32, known: Option<Gender>) -> Self {
        Self { sure, answer: known, early: VecDeque::new(), recent: VecDeque::new() }
    }

    pub fn answer(&self) -> Option<Gender> {
        self.answer
    }

    pub fn vote(&self, s: &ChunkScores) -> f32 {
        let e = s.ecapa.clamp(-VOTE_LIMIT, VOTE_LIMIT);
        if e.abs() >= self.sure {
            e
        } else if s.pitch_formants.abs() >= self.sure {
            self.sure * s.pitch_formants.signum()
        } else {
            e
        }
    }

    pub fn push(&mut self, s: &ChunkScores) -> Option<Change> {
        let v = self.vote(s);
        match self.answer {
            None => {
                self.early.push_back(v);
                if self.early.len() > FIRST_WINDOW {
                    self.early.pop_front();
                }
                let avg = mean(&self.early);
                let decided = (self.early.len() >= 2 && avg.abs() >= self.sure) || (self.early.len() == 1 && v.abs() >= 3.0 * self.sure);
                if decided {
                    let g = sign_gender(avg);
                    self.answer = Some(g);
                    self.early.clear();
                    return Some(Change { gender: g, first: true, strength: avg });
                }
                None
            }
            Some(g) => {
                self.recent.push_back(v);
                if self.recent.len() > RECHECK_CHUNKS {
                    self.recent.pop_front();
                }
                if self.recent.len() < RECHECK_CHUNKS {
                    return None;
                }
                let avg = mean(&self.recent);
                let other = sign_gender(avg);
                let agree = self.recent.iter().filter(|&&x| sign_gender(x) == other && x != 0.0).count();
                if other != g && avg.abs() >= 1.5 * self.sure && agree >= RECHECK_AGREE {
                    self.answer = Some(other);
                    self.recent.clear();
                    return Some(Change { gender: other, first: false, strength: avg });
                }
                None
            }
        }
    }
}

fn mean(v: &VecDeque<f32>) -> f32 {
    if v.is_empty() { 0.0 } else { v.iter().sum::<f32>() / v.len() as f32 }
}

fn sign_gender(v: f32) -> Gender {
    if v > 0.0 { Gender::Female } else { Gender::Male }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(ecapa: f32, pf: f32) -> ChunkScores {
        ChunkScores { ecapa, pitch_formants: pf, f0: None }
    }

    #[test]
    fn decides_after_two_sure_chunks() {
        let mut d = Detector::new(1.0, None);
        assert_eq!(d.push(&s(2.0, 0.0)), None, "one fairly sure chunk is not enough");
        let c = d.push(&s(1.5, 0.0)).unwrap();
        assert_eq!((c.gender, c.first), (Gender::Female, true));
        assert_eq!(d.answer(), Some(Gender::Female));
    }

    #[test]
    fn a_very_sure_first_chunk_decides_at_once() {
        let mut d = Detector::new(1.0, None);
        assert_eq!(d.push(&s(-4.0, 0.0)).map(|c| c.gender), Some(Gender::Male));
    }

    #[test]
    fn pitch_and_formants_decide_only_when_ecapa_is_unsure() {
        let d = Detector::new(1.0, None);
        assert_eq!(d.vote(&s(0.3, -2.0)), -1.0, "ECAPA unsure, pitch+formants sure: theirs");
        assert_eq!(d.vote(&s(2.0, -3.0)), 2.0, "ECAPA sure: ECAPA wins");
        assert_eq!(d.vote(&s(0.3, 0.5)), 0.3, "both unsure: ECAPA's weak score");
    }

    #[test]
    fn stays_unsure_on_mixed_evidence() {
        let mut d = Detector::new(1.0, None);
        for v in [0.5, -0.5, 0.4, -0.6, 0.2] {
            assert_eq!(d.push(&s(v, 0.0)), None);
        }
        assert_eq!(d.answer(), None);
    }

    #[test]
    fn one_odd_chunk_never_flips_the_answer() {
        let mut d = Detector::new(1.0, Some(Gender::Male));
        for i in 0..40 {
            let v = if i % 5 == 0 { 3.0 } else { -2.0 };
            assert_eq!(d.push(&s(v, 0.0)), None, "chunk {i}");
        }
        assert_eq!(d.answer(), Some(Gender::Male));
    }

    #[test]
    fn a_different_person_flips_it_after_about_30_s() {
        let mut d = Detector::new(1.0, Some(Gender::Male));
        let mut flipped_at = None;
        for i in 0..12 {
            if let Some(c) = d.push(&s(2.5, 0.0)) {
                assert_eq!((c.gender, c.first), (Gender::Female, false));
                flipped_at = Some(i);
                break;
            }
        }
        assert_eq!(flipped_at, Some(9), "on the 10th chunk of clear opposite evidence");
    }
}
