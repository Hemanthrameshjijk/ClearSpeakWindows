//! Clock-drift compensation for the end of the mic chain.
//!
//! The mic chain runs at the pace of the microphone's clock, while everything it feeds (the ClearNAI driver, a
//! virtual cable) consumes at the system's exact 48 kHz. A Bluetooth headset in call mode runs a little slow and
//! drops the odd packet (live log: 5,965-5,998 frames a minute instead of 6,000), so those buffers slowly run dry and
//! the listener hears short gaps: cut words.
//!
//! This stage compares the audio sent out with the time that has passed (wall clock, which the audio engine also
//! follows) and resamples by at most +/-1 % to close the gap smoothly instead of letting it become silence. Inside a
//! dead band (default 10 ms, entered and left with hysteresis) the input passes through bit-exact, so a mic that
//! keeps time (USB, wired) is untouched. A big jump (device switch, the PC waking up) is accepted as is and the
//! reference is reset rather than "caught up" for minutes.
//!
//! Output comes in the same 480-sample frames, via `pop_frame`; stretching occasionally yields a second frame and
//! squeezing occasionally none.

use crate::{FRAME_SAMPLES, SAMPLE_RATE_HZ};

/// Largest correction: 1 % (about 17 cents of pitch, not noticeable on speech).
const MAX_CORRECTION: f64 = 0.01;
/// Time over which a deficit is closed: a 10 ms deficit becomes a 0.2 % stretch.
const CLOSE_OVER_SECONDS: f64 = 5.0;
/// Start correcting above this smoothed deficit (20 ms) and stop below the inner band (2.5 ms).
const START_SAMPLES: f64 = 960.0;
const STOP_SAMPLES: f64 = 120.0;
/// Larger than this (250 ms) is a real break, not drift: accept it and start a new reference.
const RESET_SAMPLES: f64 = 12_000.0;
/// Smoothing of the measured deficit per frame (~1 s), so the bursty arrival of capture packets does not steer it.
const SMOOTHING: f64 = 0.01;

pub struct DriftCompensator {
    /// Wall-clock seconds at which `emitted` was zero.
    origin: Option<f64>,
    /// Samples handed out since `origin`.
    emitted: f64,
    smoothed_deficit: f64,
    correcting: bool,
    ratio: f64,
    /// Input not yet consumed; `pos` is the fractional read position into it.
    input: Vec<f32>,
    pos: f64,
    output: Vec<f32>,
    // Statistics since the last `take_stats`.
    stat_min_ratio: f64,
    stat_max_ratio: f64,
    stat_resets: u32,
}

/// What the compensator did since the last `take_stats` (for a per-minute log line).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DriftStats {
    pub min_ratio: f64,
    pub max_ratio: f64,
    pub resets: u32,
    /// Current smoothed deficit in milliseconds (positive = behind the clock).
    pub deficit_ms: f64,
}

impl Default for DriftCompensator {
    fn default() -> Self {
        Self::new()
    }
}

impl DriftCompensator {
    pub fn new() -> Self {
        Self {
            origin: None,
            emitted: 0.0,
            smoothed_deficit: 0.0,
            correcting: false,
            ratio: 1.0,
            input: Vec::with_capacity(FRAME_SAMPLES * 4),
            pos: 0.0,
            output: Vec::with_capacity(FRAME_SAMPLES * 4),
            stat_min_ratio: 1.0,
            stat_max_ratio: 1.0,
            stat_resets: 0,
        }
    }

    /// Feeds one processed frame that arrived at wall-clock time `now_secs`.
    pub fn push(&mut self, frame: &[f32], now_secs: f64) {
        let origin = *self.origin.get_or_insert(now_secs);
        let expected = (now_secs - origin) * SAMPLE_RATE_HZ as f64;
        let deficit = expected - self.emitted;
        if deficit.abs() > RESET_SAMPLES {
            self.reset_reference(now_secs);
        } else {
            self.smoothed_deficit += (deficit - self.smoothed_deficit) * SMOOTHING;
        }
        if self.correcting && self.smoothed_deficit.abs() < STOP_SAMPLES {
            self.correcting = false;
        } else if !self.correcting && self.smoothed_deficit.abs() > START_SAMPLES {
            self.correcting = true;
        }
        self.ratio = if self.correcting {
            1.0 + (self.smoothed_deficit / (SAMPLE_RATE_HZ as f64 * CLOSE_OVER_SECONDS)).clamp(-MAX_CORRECTION, MAX_CORRECTION)
        } else {
            1.0
        };
        self.stat_min_ratio = self.stat_min_ratio.min(self.ratio);
        self.stat_max_ratio = self.stat_max_ratio.max(self.ratio);

        self.input.extend_from_slice(frame);
        let before = self.output.len();
        if self.ratio == 1.0 && self.pos.fract() == 0.0 {
            // Bit-exact pass-through.
            let start = self.pos as usize;
            self.output.extend_from_slice(&self.input[start..]);
            self.pos = self.input.len() as f64;
        } else {
            // Each output sample advances the read position by 1/ratio (ratio > 1 stretches: more output).
            let step = 1.0 / self.ratio;
            while self.pos + 2.0 < self.input.len() as f64 {
                let i = self.pos.floor() as usize;
                let t = (self.pos - i as f64) as f32;
                let at = |k: isize| self.input[(i as isize + k).max(0) as usize];
                self.output.push(catmull_rom(at(-1), at(0), at(1), at(2), t));
                self.pos += step;
            }
            if self.ratio == 1.0 && (self.pos - self.pos.round()).abs() < 1e-6 {
                self.pos = self.pos.round(); // back on the grid: pass-through again from here
            }
        }
        self.emitted += (self.output.len() - before) as f64;
        // Keep one sample behind the read position for the interpolation, drop the rest.
        let keep_from = (self.pos.floor() as usize).saturating_sub(1).min(self.input.len());
        if keep_from > 0 {
            self.input.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
    }

    /// Next full output frame, if one is ready.
    pub fn pop_frame(&mut self, out: &mut [f32]) -> bool {
        if self.output.len() < out.len() {
            return false;
        }
        out.copy_from_slice(&self.output[..out.len()]);
        self.output.drain(..out.len());
        true
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    pub fn take_stats(&mut self) -> DriftStats {
        let s = DriftStats {
            min_ratio: self.stat_min_ratio,
            max_ratio: self.stat_max_ratio,
            resets: self.stat_resets,
            deficit_ms: self.smoothed_deficit / SAMPLE_RATE_HZ as f64 * 1000.0,
        };
        self.stat_min_ratio = self.ratio;
        self.stat_max_ratio = self.ratio;
        self.stat_resets = 0;
        s
    }

    fn reset_reference(&mut self, now_secs: f64) {
        self.origin = Some(now_secs - self.output.len() as f64 / SAMPLE_RATE_HZ as f64);
        self.emitted = self.output.len() as f64;
        self.smoothed_deficit = 0.0;
        self.correcting = false;
        self.stat_resets += 1;
    }
}

#[inline]
fn catmull_rom(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let t2 = t * t;
    let t3 = t2 * t;
    0.5 * ((2.0 * p1) + (-p0 + p2) * t + (2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3) * t2 + (-p0 + 3.0 * p1 - 3.0 * p2 + p3) * t3)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_SECS: f64 = FRAME_SAMPLES as f64 / SAMPLE_RATE_HZ as f64;

    fn tone(n: usize, start: usize) -> Vec<f32> {
        (start..start + n).map(|i| (i as f32 * 0.031).sin() * 0.5).collect()
    }

    /// Runs `seconds` of input that arrives `speed` times as fast as the wall clock; returns frames out and in.
    fn run(c: &mut DriftCompensator, seconds: f64, speed: f64, t0: f64) -> (usize, usize) {
        let frames_in = (seconds / FRAME_SECS * speed) as usize;
        let mut out = vec![0.0; FRAME_SAMPLES];
        let mut frames_out = 0;
        for k in 0..frames_in {
            let now = t0 + k as f64 * FRAME_SECS / speed;
            c.push(&tone(FRAME_SAMPLES, k * FRAME_SAMPLES), now);
            while c.pop_frame(&mut out) {
                frames_out += 1;
            }
        }
        (frames_out, frames_in)
    }

    #[test]
    fn a_mic_on_time_passes_through_bit_exact() {
        let mut c = DriftCompensator::new();
        let mut out = vec![0.0; FRAME_SAMPLES];
        for k in 0..2000 {
            let frame = tone(FRAME_SAMPLES, k * FRAME_SAMPLES);
            c.push(&frame, k as f64 * FRAME_SECS);
            assert!(c.pop_frame(&mut out));
            assert_eq!(out, frame, "frame {k} changed");
            assert!(!c.pop_frame(&mut out));
        }
        assert_eq!(c.ratio(), 1.0);
    }

    #[test]
    fn bursty_arrival_on_time_is_not_corrected() {
        // A capture buffer of ~22 ms hands over two frames at once; on average it is on time: stay bit-exact.
        let mut c = DriftCompensator::new();
        let mut out = vec![0.0; FRAME_SAMPLES];
        for k in 0..6000 {
            let frame = tone(FRAME_SAMPLES, k * FRAME_SAMPLES);
            let now = (k / 2 * 2 + 1) as f64 * FRAME_SECS; // pairs arrive together, 20 ms apart
            c.push(&frame, now);
            assert!(c.pop_frame(&mut out));
            assert_eq!(out, frame, "frame {k} changed");
        }
        assert_eq!(c.take_stats().max_ratio, 1.0);
    }

    #[test]
    fn a_slow_bluetooth_clock_is_stretched_back_to_real_time() {
        // 0.5 % slow: 60 s gives 5,970 frames in; uncorrected the listener would be 300 ms short.
        let mut c = DriftCompensator::new();
        let (out, inp) = run(&mut c, 60.0, 0.995, 0.0);
        assert!(inp < 5980);
        let missing_ms = (6000.0 - out as f64) * 10.0;
        assert!(missing_ms < 60.0, "still {missing_ms} ms behind after a minute ({out} frames out of {inp} in)");
        assert!(c.ratio() > 1.0 && c.ratio() <= 1.0 + MAX_CORRECTION);
    }

    #[test]
    fn a_fast_clock_is_squeezed() {
        let mut c = DriftCompensator::new();
        let (out, _) = run(&mut c, 60.0, 1.005, 0.0);
        let extra_ms = (out as f64 - 6000.0) * 10.0;
        assert!(extra_ms < 60.0, "{extra_ms} ms too much after a minute");
        assert!(c.ratio() < 1.0);
    }

    #[test]
    fn a_big_break_resets_instead_of_catching_up() {
        let mut c = DriftCompensator::new();
        run(&mut c, 5.0, 1.0, 0.0);
        // Two seconds with no input (device switch), then on-time input again: no long stretch afterwards.
        run(&mut c, 5.0, 1.0, 7.0);
        assert_eq!(c.take_stats().resets, 1);
        assert_eq!(c.ratio(), 1.0);
    }

    #[test]
    fn stretching_keeps_the_waveform_smooth() {
        let mut c = DriftCompensator::new();
        let mut out = vec![0.0; FRAME_SAMPLES];
        let mut all = Vec::new();
        for k in 0..3000 {
            c.push(&tone(FRAME_SAMPLES, k * FRAME_SAMPLES), k as f64 * FRAME_SECS / 0.99);
            while c.pop_frame(&mut out) {
                all.extend_from_slice(&out);
            }
        }
        let max_step = all.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        // The source tone moves at most 0.5 * 0.031 per sample; no clicks means no step much larger than that.
        assert!(max_step < 0.02, "a jump of {max_step} in the output");
    }
}
