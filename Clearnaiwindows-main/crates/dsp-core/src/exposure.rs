//! Listening-exposure (noise dose) estimate for the audio the agent hears.
//!
//! The NIOSH recommended limit is 85 dB(A) for 8 hours with a 3 dB exchange rate: every +3 dB halves the allowed
//! time. Dose 100% means today's listening reached that limit.
//!
//! The engine only sees digital levels (dBFS). The level at the ear is estimated as
//! `dBFS + Windows volume (dB) + headset sensitivity (dB SPL at 0 dBFS and full volume)`. It is an estimate, and
//! the UI says so. A-weighting is approximated by a 100 Hz high-pass on the measured signal only (the audio itself
//! is never changed).

use crate::SAMPLE_RATE_HZ;

/// Exposure limit level and duration (NIOSH REL).
pub const LIMIT_DB: f64 = 85.0;
pub const LIMIT_SECS: f64 = 8.0 * 3600.0;
pub const EXCHANGE_DB: f64 = 3.0;
/// Frames quieter than this at the ear are not counted as listening (silence between calls).
pub const SILENCE_DB: f64 = 40.0;
/// Typical call-centre headset: 100 dB SPL at 0 dBFS with the Windows volume at maximum.
pub const DEFAULT_HEADSET_DB_SPL: f32 = 100.0;

/// Dose (as a fraction, 1.0 = 100%) for `secs` of listening at a steady `level_db`.
#[inline]
pub fn dose_for(level_db: f64, secs: f64) -> f64 {
    secs / (LIMIT_SECS / 2f64.powf((level_db - LIMIT_DB) / EXCHANGE_DB))
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ExposureTotals {
    /// 1.0 = 100% of the daily limit.
    pub dose: f64,
    /// Seconds of audible listening.
    pub listen_secs: f64,
    /// Sum of seconds * 10^(L/10), for the average level.
    pub energy: f64,
}

impl ExposureTotals {
    /// Average level at the ear over the listening time (equivalent continuous level), or 0 when nothing was heard.
    pub fn avg_db(&self) -> f64 {
        if self.listen_secs <= 0.0 || self.energy <= 0.0 {
            0.0
        } else {
            10.0 * (self.energy / self.listen_secs).log10()
        }
    }
}

pub struct ExposureMeter {
    hp_a: f32,
    hp_x1: f32,
    hp_y1: f32,
    totals: ExposureTotals,
    last_level_db: f32,
}

impl Default for ExposureMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl ExposureMeter {
    pub fn new() -> Self {
        let rc = 1.0 / (2.0 * std::f32::consts::PI * 100.0);
        let dt = 1.0 / SAMPLE_RATE_HZ as f32;
        Self { hp_a: rc / (rc + dt), hp_x1: 0.0, hp_y1: 0.0, totals: ExposureTotals::default(), last_level_db: 0.0 }
    }

    /// Starts from totals carried over (for example from earlier in the same shift).
    pub fn with_totals(totals: ExposureTotals) -> Self {
        let mut m = Self::new();
        m.totals = totals;
        m
    }

    pub fn totals(&self) -> ExposureTotals {
        self.totals
    }

    pub fn set_totals(&mut self, totals: ExposureTotals) {
        self.totals = totals;
    }

    /// Estimated level at the ear of the last frame (0 for silence).
    pub fn last_level_db(&self) -> f32 {
        self.last_level_db
    }

    /// Adds one frame of what the agent hears. `ear_offset_db` is the level at the ear for a 0 dBFS RMS signal
    /// (headset sensitivity + Windows volume in dB); pass `None` when the output is muted.
    /// Allocation-free; safe on the audio thread.
    pub fn add_frame(&mut self, frame: &[f32], ear_offset_db: Option<f32>) {
        let mut sum = 0.0f32;
        for &x in frame {
            let y = self.hp_a * (self.hp_y1 + x - self.hp_x1);
            self.hp_x1 = x;
            self.hp_y1 = y;
            sum += y * y;
        }
        let Some(offset) = ear_offset_db else {
            self.last_level_db = 0.0;
            return;
        };
        if frame.is_empty() || sum <= 0.0 {
            self.last_level_db = 0.0;
            return;
        }
        let dbfs = 10.0 * (sum / frame.len() as f32).log10();
        let level = (dbfs + offset) as f64;
        if level < SILENCE_DB {
            self.last_level_db = 0.0;
            return;
        }
        self.last_level_db = level as f32;
        let secs = frame.len() as f64 / SAMPLE_RATE_HZ as f64;
        self.totals.dose += dose_for(level, secs);
        self.totals.listen_secs += secs;
        self.totals.energy += secs * 10f64.powf(level / 10.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FRAME_SAMPLES;

    fn sine_frames(amp: f32, secs: f64) -> impl Iterator<Item = Vec<f32>> {
        let frames = (secs * SAMPLE_RATE_HZ as f64 / FRAME_SAMPLES as f64) as usize;
        let mut phase = 0usize;
        (0..frames).map(move |_| {
            let f: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|i| amp * (2.0 * std::f32::consts::PI * ((phase + i) % 48) as f32 / 48.0).sin())
                .collect();
            phase = (phase + FRAME_SAMPLES) % 48;
            f
        })
    }

    /// Offset that makes a 1 kHz sine of amplitude `amp` read `target_db` at the ear.
    fn offset_for(amp: f32, target_db: f32) -> f32 {
        let rms_dbfs = 20.0 * (amp / 2f32.sqrt()).log10();
        target_db - rms_dbfs
    }

    #[test]
    fn eight_hours_at_85_db_is_100_percent() {
        let mut m = ExposureMeter::new();
        let off = offset_for(0.1, 85.0);
        for f in sine_frames(0.1, 8.0 * 3600.0) {
            m.add_frame(&f, Some(off));
        }
        let t = m.totals();
        // The 100 Hz weighting filter trims a 1 kHz tone by under 0.1 dB, about 2% of dose.
        assert!((t.dose - 1.0).abs() < 0.03, "dose {}", t.dose);
        assert!((t.avg_db() - 85.0).abs() < 0.2, "avg {}", t.avg_db());
    }

    #[test]
    fn four_hours_at_88_db_is_also_100_percent() {
        assert!((dose_for(88.0, 4.0 * 3600.0) - 1.0).abs() < 1e-9);
        let mut m = ExposureMeter::new();
        let off = offset_for(0.1, 88.0);
        for f in sine_frames(0.1, 4.0 * 3600.0) {
            m.add_frame(&f, Some(off));
        }
        assert!((m.totals().dose - 1.0).abs() < 0.03, "dose {}", m.totals().dose);
    }

    #[test]
    fn silence_and_mute_add_nothing() {
        let mut m = ExposureMeter::new();
        m.add_frame(&[0.0; FRAME_SAMPLES], Some(100.0));
        for f in sine_frames(0.5, 10.0) {
            m.add_frame(&f, None);
        }
        assert_eq!(m.totals().dose, 0.0);
        assert_eq!(m.totals().listen_secs, 0.0);
    }

    #[test]
    fn low_rumble_is_mostly_ignored() {
        // 30 Hz hum at a level that would be 85 dB unweighted reads far lower after the high-pass.
        let mut m = ExposureMeter::new();
        let amp = 0.1f32;
        let mut phase = 0usize;
        for _ in 0..100 {
            let f: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|i| amp * (2.0 * std::f32::consts::PI * 30.0 * (phase + i) as f32 / 48_000.0).sin())
                .collect();
            phase += FRAME_SAMPLES;
            m.add_frame(&f, Some(offset_for(amp, 85.0)));
        }
        assert!(m.last_level_db() < 76.0, "30 Hz read {}", m.last_level_db());
    }
}
