//! Fixed "mic distance" gain, applied to the raw mic *before* the noise stages.
//!
//! A mic that is far from the mouth (earbud or earpiece mic instead of a boom) delivers a quiet voice that sits
//! close to the room's noise floor, and the noise stages then cut speech along with the noise. Unlike
//! `AutoGainStage` (adaptive, runs after Hush, deliberately kept off the front of the chain because it lifts quiet
//! background voices during pauses), this gain is constant: it raises voice and background by the same factor, so the
//! contrast the noise stages rely on is untouched and only the level they see is healthy.
//!
//! A soft knee keeps loud words from hard-clipping once boosted.

use crate::{Stage, FRAME_SAMPLES};

/// Gain in dB for each distance step: Close (mic at the mouth), Normal, Far.
pub const DISTANCE_GAIN_DB: [f32; 3] = [0.0, 9.0, 18.0];

/// Largest gain the slider offers.
pub const MAX_GAIN_DB: u8 = 30;

/// Above this magnitude the signal is compressed instead of clipped.
const KNEE: f32 = 0.8;

#[inline]
pub(crate) fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        let room = 1.0 - KNEE;
        x.signum() * (KNEE + room * ((a - KNEE) / room).tanh())
    }
}

/// The gain for a distance step (out-of-range steps fall back to Close, no gain).
pub fn distance_gain_db(step: u8) -> f32 {
    DISTANCE_GAIN_DB.get(step as usize).copied().unwrap_or(0.0)
}

/// Ceiling for `AutoGainStage::set_max_gain` for a slider value. The pre-gain already provides the boost for a far
/// mic, so the adaptive stage after the noise stages keeps its normal ceiling: a higher one lifted the leftover hiss
/// in pauses (audible as rising noise) and clipped loud word starts.
pub fn agc_max_gain_for_db(_db: u8) -> f32 {
    8.0
}

/// Top of the Mic boost slider. A Bluetooth headset in call mode needs about +12 dB (live log: its voice reached the
/// output around -40 dBFS); the soft knee keeps the top of the range from clipping.
pub const MIC_BOOST_MAX_DB: u8 = 24;

/// Where the slider starts the first time: enough for a Bluetooth call-mode mic.
pub const MIC_BOOST_DEFAULT_DB: u8 = 12;

/// The boost the mic chain applies, in dB: the slider value while the switch is on, else 0 (exactly untouched).
pub fn mic_boost_effective(on: bool, db: u8) -> u8 {
    if on {
        db.min(MIC_BOOST_MAX_DB)
    } else {
        0
    }
}

/// The boost never drives the voice's recent peak above this (-6 dBFS), well under the soft-limit knee.
const HEADROOM: f32 = 0.5;
/// How fast the level memory fades (per 10 ms frame): about 1 s, so the gain does not pump between words but a
/// single knock or cough does not hold the boost down for long.
const PEAK_RELEASE_PER_FRAME: f32 = 0.990_050; // exp(-10 ms / 1000 ms)
/// Speech peaks sit about this far above the 10 ms RMS: the level is judged on RMS x this, so a click lasting a few
/// milliseconds counts far less than real loud speech.
const CREST: f32 = 3.0;

/// Mic boost: lifts a quiet mic (Bluetooth call mode, far-away laptop mic) by the slider amount, but backs off by
/// itself when the voice is already loud. A fixed +12 dB on a mic that already peaks near full scale pushed whole
/// sentences into the soft limiter, and the noise removal and accent then turned that distortion into garbled words.
pub struct PreGainStage {
    current: f32,
    target: f32,
    /// Remembered speech level: only levels lasting two frames (20 ms) go in, then it fades over about 1 s.
    env: f32,
    /// Level of the previous frame.
    prev: f32,
}

impl PreGainStage {
    pub fn new() -> Self {
        Self { current: 1.0, target: 1.0, env: 0.0, prev: 0.0 }
    }

    /// Set the gain for the next frame. Cheap (no allocation); the change is ramped across the frame so it never clicks.
    pub fn set_db(&mut self, db: f32) {
        self.target = 10f32.powf(db / 20.0);
    }
}

impl Default for PreGainStage {
    fn default() -> Self {
        Self::new()
    }
}

impl Stage for PreGainStage {
    fn name(&self) -> &'static str {
        "pre_gain"
    }

    fn process(&mut self, frame: &mut [f32]) {
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len().max(1) as f32).sqrt();
        let level = rms * CREST;
        // This frame is always protected; only a level that lasted two frames (speech, not a knock) is remembered.
        self.env = level.min(self.prev).max(self.env * PEAK_RELEASE_PER_FRAME);
        self.prev = level;
        let judged = self.env.max(level);
        // Only as much boost as keeps the speech peak at HEADROOM, and never below unity (it only ever lifts).
        let allowed = if judged > 0.0 { HEADROOM / judged } else { f32::INFINITY };
        let target = self.target.min(allowed).max(1.0);
        if self.current == 1.0 && target == 1.0 {
            return; // no boost: exactly untouched
        }
        // Going down happens at once (this frame is already the loud one: sliding down across it overdrove the start
        // of the first word after a pause); going up is ramped across the frame so it never clicks.
        if target < self.current {
            self.current = target;
        }
        let step = (target - self.current) / FRAME_SAMPLES as f32;
        let mut g = self.current;
        for s in frame.iter_mut() {
            g += step;
            *s = soft_limit(*s * g);
        }
        self.current = target;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(peak: f32) -> Vec<f32> {
        (0..FRAME_SAMPLES).map(|i| (i as f32 * 0.07).sin() * peak).collect()
    }

    fn peak(frame: &[f32]) -> f32 {
        frame.iter().fold(0.0f32, |m, &s| m.max(s.abs()))
    }

    #[test]
    fn mic_boost_is_the_slider_only_while_on() {
        assert_eq!(mic_boost_effective(false, 18), 0, "off never boosts");
        assert_eq!(mic_boost_effective(true, 12), 12);
        assert_eq!(mic_boost_effective(true, 0), 0);
        assert_eq!(mic_boost_effective(true, 200), MIC_BOOST_MAX_DB, "capped at the slider top");
    }

    #[test]
    fn close_leaves_the_frame_exactly_untouched() {
        let mut stage = PreGainStage::new();
        stage.set_db(distance_gain_db(0));
        let mut frame = tone(0.3);
        let before = frame.clone();
        stage.process(&mut frame);
        assert_eq!(frame, before);
    }

    #[test]
    fn each_step_boosts_by_its_amount_once_settled() {
        for step in 1..=2u8 {
            let mut stage = PreGainStage::new();
            stage.set_db(distance_gain_db(step));
            let mut frame = tone(0.02);
            stage.process(&mut frame); // ramp frame
            let mut frame = tone(0.02);
            stage.process(&mut frame);
            let expected = 0.02 * 10f32.powf(distance_gain_db(step) / 20.0);
            assert!((peak(&frame) - expected).abs() < expected * 0.02, "step {step}: {} vs {expected}", peak(&frame));
        }
    }

    #[test]
    fn output_never_exceeds_full_scale_even_for_a_loud_input() {
        let mut stage = PreGainStage::new();
        stage.set_db(distance_gain_db(2));
        for _ in 0..3 {
            let mut frame = tone(1.0);
            stage.process(&mut frame);
            assert!(frame.iter().all(|s| s.abs() <= 1.0), "must stay within full scale");
        }
    }

    #[test]
    fn silence_stays_silent() {
        let mut stage = PreGainStage::new();
        stage.set_db(distance_gain_db(2));
        let mut frame = vec![0.0f32; FRAME_SAMPLES];
        stage.process(&mut frame);
        assert!(frame.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn a_gain_change_ramps_instead_of_jumping() {
        let mut stage = PreGainStage::new();
        stage.set_db(18.0);
        let mut frame = vec![0.01f32; FRAME_SAMPLES];
        stage.process(&mut frame);
        assert!(frame[0] < 0.011 * 1.2, "first sample is nearly unchanged, got {}", frame[0]);
        assert!(frame[FRAME_SAMPLES - 1] > 0.07, "last sample has the full boost, got {}", frame[FRAME_SAMPLES - 1]);
    }

    #[test]
    fn unknown_steps_mean_no_gain() {
        assert_eq!(distance_gain_db(9), 0.0);
        assert_eq!(agc_max_gain_for_db(0), 8.0);
        assert_eq!(agc_max_gain_for_db(9), 8.0, "no extra post-noise boost");
        assert_eq!(agc_max_gain_for_db(30), 8.0);
    }

    #[test]
    fn boost_backs_off_on_a_voice_that_is_already_loud() {
        // An earbud that already peaks near full scale: +12 dB must not drive it into the limiter.
        let mut stage = PreGainStage::new();
        stage.set_db(12.0);
        let mut first = tone(0.6);
        stage.process(&mut first);
        for _ in 0..20 {
            let mut frame = tone(0.6);
            stage.process(&mut frame);
            assert!(peak(&frame) <= 0.6 + 1e-4, "no boost on a loud voice, got peak {}", peak(&frame));
        }
    }

    #[test]
    fn boost_lifts_a_quiet_voice_but_keeps_headroom() {
        let mut stage = PreGainStage::new();
        stage.set_db(24.0); // x15.8
        for _ in 0..5 {
            let mut frame = tone(0.1);
            stage.process(&mut frame);
        }
        let mut frame = tone(0.1);
        stage.process(&mut frame);
        // A steady tone has a lower crest than speech, so it lands under the headroom, still boosted well above 0.1.
        assert!(peak(&frame) > 0.2 && peak(&frame) <= HEADROOM, "lifted but under the headroom, got {}", peak(&frame));
    }

    #[test]
    fn the_first_word_after_a_pause_is_not_overdriven() {
        let mut stage = PreGainStage::new();
        stage.set_db(12.0);
        // A long pause: the peak memory fades and the boost climbs back to the full +12 dB.
        for _ in 0..2000 {
            let mut frame = tone(0.001);
            stage.process(&mut frame);
        }
        // The first frame of a loud word must already be at the backed-off gain.
        let mut frame = tone(0.6);
        stage.process(&mut frame);
        assert!(peak(&frame) <= 0.6 + 1e-4, "first loud frame boosted to {}", peak(&frame));
    }

    #[test]
    fn a_knock_does_not_hold_the_boost_down() {
        let mut stage = PreGainStage::new();
        stage.set_db(24.0);
        for _ in 0..100 {
            let mut frame = tone(0.01);
            stage.process(&mut frame);
        }
        // A desk knock: 2 ms at 0.9 in an otherwise quiet frame.
        let mut knock = tone(0.01);
        for s in knock.iter_mut().take(96) {
            *s = 0.9;
        }
        stage.process(&mut knock);
        // 2 s later the quiet voice is boosted again (it used to take about 10 s).
        for _ in 0..200 {
            let mut frame = tone(0.01);
            stage.process(&mut frame);
        }
        let mut frame = tone(0.01);
        stage.process(&mut frame);
        assert!(peak(&frame) > 0.01 * 10.0, "boost is back, got peak {}", peak(&frame));
    }
}
