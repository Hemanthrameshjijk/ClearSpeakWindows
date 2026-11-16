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

/// Mic boost choices (settings index): 0 Auto, 1 Off, 2 +6 dB, 3 +12 dB, 4 +18 dB.
pub const MIC_BOOST_DB: [f32; 4] = [0.0, 6.0, 12.0, 18.0];

/// What Auto adds for a Bluetooth headset in call mode. Its mic arrives far quieter than a wired/USB one (live log:
/// output peaks around -25 dBFS with the normal +18 dB AutoGain ceiling), so it needs about this much on top.
pub const BLUETOOTH_AUTO_BOOST_DB: f32 = 12.0;

/// The boost in dB for a mic boost choice. Auto (0, or any unknown index) boosts only a Bluetooth call-mode mic, so
/// wired and USB mics keep exactly their earlier sound.
pub fn mic_boost_db(choice: u8, hands_free: bool) -> f32 {
    match choice {
        1..=4 => MIC_BOOST_DB[choice as usize - 1],
        _ if hands_free => BLUETOOTH_AUTO_BOOST_DB,
        _ => 0.0,
    }
}

pub struct PreGainStage {
    current: f32,
    target: f32,
}

impl PreGainStage {
    pub fn new() -> Self {
        Self { current: 1.0, target: 1.0 }
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
        if self.current == 1.0 && self.target == 1.0 {
            return; // Close: exactly untouched
        }
        let step = (self.target - self.current) / FRAME_SAMPLES as f32;
        let mut g = self.current;
        for s in frame.iter_mut() {
            g += step;
            *s = soft_limit(*s * g);
        }
        self.current = self.target;
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
    fn auto_boosts_only_bluetooth_call_mode() {
        assert_eq!(mic_boost_db(0, false), 0.0);
        assert_eq!(mic_boost_db(0, true), BLUETOOTH_AUTO_BOOST_DB);
        assert_eq!(mic_boost_db(1, true), 0.0, "Off never boosts, even on Bluetooth");
        assert_eq!(mic_boost_db(3, false), 12.0);
        assert_eq!(mic_boost_db(4, true), 18.0);
        assert_eq!(mic_boost_db(9, false), 0.0, "unknown index reads as Auto");
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
}
