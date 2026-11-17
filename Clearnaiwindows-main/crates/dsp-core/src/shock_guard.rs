//! Acoustic-shock protection for the audio the agent hears (the caller path).
//!
//! Three layers, all with zero look-ahead so no delay is added:
//! 1. **Peak ceiling**: an instant-attack peak limiter. No sample ever leaves above `PEAK_CEILING_DB`.
//! 2. **Loudness cap**: a fast short-term RMS limiter. Tones, screeches, feedback whistles and fax/modem noise have
//!    a low crest factor (their RMS is close to their peak), so they hit this cap. Speech has quiet gaps between
//!    peaks, so at the same peak level its RMS stays well under the cap.
//! 3. **Shock duck**: if the loudness cap has to cut more than `SHOCK_TRIGGER_DB`, the sound is clearly not normal
//!    speech. Everything is then lowered by another `SHOCK_DUCK_DB` for `SHOCK_HOLD_MS` and faded back in, and one
//!    event is counted.
//!
//! Normal speech passes through bit-for-bit (gain stays exactly 1.0).

use crate::{Stage, SAMPLE_RATE_HZ};

/// Highest peak sent to the headset.
pub const PEAK_CEILING_DB: f32 = -6.0;
/// Highest short-term (about 15 ms) RMS sent to the headset.
pub const RMS_CEILING_DB: f32 = -12.0;
/// A loudness cut deeper than this counts as a shock and starts the duck.
pub const SHOCK_TRIGGER_DB: f32 = 6.0;
/// Extra reduction while a shock is held.
pub const SHOCK_DUCK_DB: f32 = 12.0;
pub const SHOCK_HOLD_MS: f32 = 300.0;

const PEAK_RELEASE_MS: f32 = 80.0;
const RMS_TAU_MS: f32 = 15.0;
const GAIN_ATTACK_MS: f32 = 1.0;
const GAIN_RELEASE_MS: f32 = 150.0;
const DUCK_RELEASE_MS: f32 = 250.0;

#[inline]
fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[inline]
fn coeff(ms: f32) -> f32 {
    // One-pole smoothing coefficient for a time constant in milliseconds at the engine rate.
    1.0 - (-1.0 / (ms * 0.001 * SAMPLE_RATE_HZ as f32)).exp()
}

pub struct ShockGuard {
    peak_ceiling: f32,
    rms_ceiling: f32,
    trigger_gain: f32,
    duck_gain: f64,
    hold_samples: u32,

    peak_rel: f32,
    rms_a: f32,
    gain_att: f64,
    gain_rel: f64,
    duck_rel: f64,

    env: f32,
    ms: f32,
    // Gains are smoothed in f64: with release coefficients near 1e-4, f32 rounding would leave them stuck a hair
    // below 1.0 and normal audio would never be bit-exact again.
    g_rms: f64,
    duck: f64,
    hold_left: u32,

    events: u64,
    max_cut_db: f32,
}

impl Default for ShockGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl ShockGuard {
    pub fn new() -> Self {
        Self {
            peak_ceiling: db_to_lin(PEAK_CEILING_DB),
            rms_ceiling: db_to_lin(RMS_CEILING_DB),
            trigger_gain: db_to_lin(-SHOCK_TRIGGER_DB),
            duck_gain: db_to_lin(-SHOCK_DUCK_DB) as f64,
            hold_samples: (SHOCK_HOLD_MS * 0.001 * SAMPLE_RATE_HZ as f32) as u32,
            peak_rel: (-1.0 / (PEAK_RELEASE_MS * 0.001 * SAMPLE_RATE_HZ as f32)).exp(),
            rms_a: coeff(RMS_TAU_MS),
            gain_att: coeff(GAIN_ATTACK_MS) as f64,
            gain_rel: coeff(GAIN_RELEASE_MS) as f64,
            duck_rel: coeff(DUCK_RELEASE_MS) as f64,
            env: 0.0,
            ms: 0.0,
            g_rms: 1.0,
            duck: 1.0,
            hold_left: 0,
            events: 0,
            max_cut_db: 0.0,
        }
    }

    /// Shock events since the last `take_stats` call.
    pub fn events(&self) -> u64 {
        self.events
    }

    /// Returns (events, deepest cut in dB) since the last call and resets both.
    pub fn take_stats(&mut self) -> (u64, f32) {
        let out = (self.events, self.max_cut_db);
        self.events = 0;
        self.max_cut_db = 0.0;
        out
    }

    #[inline]
    fn step(&mut self, x: f32) -> f32 {
        let a = x.abs();

        // Peak envelope: instant attack, so env >= |x| and the peak gain below always holds the ceiling.
        self.env = if a > self.env { a } else { self.env * self.peak_rel };
        let g_peak = if self.env > self.peak_ceiling { self.peak_ceiling / self.env } else { 1.0 };

        // Short-term loudness and its target gain.
        self.ms += (x * x - self.ms) * self.rms_a;
        let rms = self.ms.sqrt();
        let target = if rms > self.rms_ceiling { self.rms_ceiling / rms } else { 1.0 };
        let t64 = target as f64;
        let c = if t64 < self.g_rms { self.gain_att } else { self.gain_rel };
        self.g_rms += (t64 - self.g_rms) * c;
        if self.g_rms > 0.999 && target >= 1.0 {
            self.g_rms = 1.0; // settle exactly, so speech after a shock is bit-exact again
        }

        // Shock: the loudness cap is cutting hard, so start (or extend) the duck.
        if target < self.trigger_gain {
            if self.hold_left == 0 && self.duck > 0.999 {
                self.events += 1;
            }
            self.hold_left = self.hold_samples;
        }
        if self.hold_left > 0 {
            self.hold_left -= 1;
            self.duck += (self.duck_gain - self.duck) * self.gain_att;
        } else if self.duck < 1.0 {
            self.duck += (1.0 - self.duck) * self.duck_rel;
            if self.duck > 0.999 {
                self.duck = 1.0; // a 0.009 dB step, inaudible
            }
        }

        let g = ((self.g_rms * self.duck) as f32).min(g_peak);
        if g < 1.0 {
            let cut = -20.0 * g.max(1e-6).log10();
            if cut > self.max_cut_db {
                self.max_cut_db = cut;
            }
        }
        x * g
    }
}

impl Stage for ShockGuard {
    fn name(&self) -> &'static str {
        "shock_guard"
    }

    fn process(&mut self, frame: &mut [f32]) {
        for s in frame.iter_mut() {
            *s = self.step(*s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_SAMPLES;

    const SR: f32 = SAMPLE_RATE_HZ as f32;

    fn tone(freq: f32, amp: f32, secs: f32) -> Vec<f32> {
        (0..(secs * SR) as usize).map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin()).collect()
    }

    /// Speech-like: a 200 Hz carrier gated on/off every 60 ms (bursts with gaps), peak `amp`.
    fn speechy(amp: f32, secs: f32) -> Vec<f32> {
        (0..(secs * SR) as usize)
            .map(|i| {
                let t = i as f32 / SR;
                let gate = if ((t / 0.06) as u32) % 2 == 0 { 1.0 } else { 0.05 };
                // a sharp peak on top of a quieter body gives speech-like crest factor
                let body = 0.3 * (2.0 * std::f32::consts::PI * 200.0 * t).sin();
                let spike = if i % 240 == 0 { 0.7 } else { 0.0 };
                amp * gate * (body + spike)
            })
            .collect()
    }

    fn run(g: &mut ShockGuard, sig: &mut [f32]) {
        for chunk in sig.chunks_mut(FRAME_SAMPLES) {
            g.process(chunk);
        }
    }

    fn peak(s: &[f32]) -> f32 {
        s.iter().fold(0.0, |m, x| m.max(x.abs()))
    }

    #[test]
    fn normal_level_audio_is_bit_exact() {
        let mut g = ShockGuard::new();
        let mut sig = tone(1000.0, 0.1, 2.0); // -20 dBFS peak
        let before = sig.clone();
        run(&mut g, &mut sig);
        assert_eq!(sig, before);
        assert_eq!(g.events(), 0);
    }

    #[test]
    fn loud_speech_peaks_are_not_treated_as_a_shock() {
        let mut g = ShockGuard::new();
        let mut sig = speechy(0.5, 3.0); // peaks around -6 dBFS
        run(&mut g, &mut sig);
        assert_eq!(g.events(), 0, "speech must not trigger the duck");
    }

    #[test]
    fn full_scale_screech_is_capped_and_ducked() {
        let mut g = ShockGuard::new();
        let mut sig = tone(1000.0, 0.05, 1.0);
        sig.extend(tone(2000.0, 1.0, 1.0)); // sudden 0 dBFS screech
        run(&mut g, &mut sig);

        let ceiling = db_to_lin(PEAK_CEILING_DB) + 1e-4;
        assert!(peak(&sig) <= ceiling, "peak {} above ceiling", peak(&sig));

        // After the first 20 ms the duck holds the screech far below the ceiling.
        let settled = &sig[(1.02 * SR) as usize..(2.0 * SR) as usize];
        let settled_db = 20.0 * peak(settled).log10();
        assert!(settled_db < RMS_CEILING_DB - 6.0, "screech still at {settled_db} dBFS");
        assert_eq!(g.events(), 1);
        let (ev, cut) = g.take_stats();
        assert_eq!(ev, 1);
        assert!(cut > 20.0, "deepest cut {cut} dB");
        assert_eq!(g.events(), 0);
    }

    #[test]
    fn the_first_millisecond_is_already_under_the_ceiling() {
        let mut g = ShockGuard::new();
        let mut sig = vec![0.0; 480];
        sig.extend(tone(3000.0, 1.0, 0.002));
        run(&mut g, &mut sig);
        assert!(peak(&sig) <= db_to_lin(PEAK_CEILING_DB) + 1e-4);
    }

    #[test]
    fn gain_recovers_after_the_shock() {
        let mut g = ShockGuard::new();
        let mut sig = tone(2000.0, 1.0, 0.5);
        run(&mut g, &mut sig);
        let mut quiet = tone(1000.0, 0.1, 3.0);
        run(&mut g, &mut quiet);
        // About 2 s after the shock (300 ms hold + fade back in) normal audio is bit-exact again.
        let tail = &quiet[(2.5 * SR) as usize..];
        let reference = &tone(1000.0, 0.1, 3.0)[(2.5 * SR) as usize..];
        assert_eq!(tail, reference);
    }
}
