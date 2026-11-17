//! Keeps today's listening exposure across engine restarts (`%LOCALAPPDATA%\ClearSpeak\exposure.json`), and starts a
//! new shift after a long break with no call audio. A break, not midnight, so a night shift is counted as one shift.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use dsp_core::ExposureTotals;

/// No call audio for this long starts a new shift.
pub const BREAK_RESET_SECS: u64 = 6 * 3600;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Saved {
    pub dose: f64,
    pub listen_secs: f64,
    pub energy: f64,
    pub shocks: u64,
    /// Seconds since the epoch when call audio was last heard.
    pub last_heard: u64,
}

impl Saved {
    pub fn totals(&self) -> ExposureTotals {
        ExposureTotals { dose: self.dose, listen_secs: self.listen_secs, energy: self.energy }
    }
}

/// The saved shift is over when nothing was heard for `BREAK_RESET_SECS`.
pub fn is_new_shift(last_heard: u64, now: u64) -> bool {
    last_heard == 0 || now.saturating_sub(last_heard) >= BREAK_RESET_SECS
}

/// What to start from now: the saved shift, or a fresh one after a long break.
pub fn carry_over(saved: Option<Saved>, now: u64) -> Saved {
    match saved {
        Some(s) if !is_new_shift(s.last_heard, now) => s,
        _ => Saved::default(),
    }
}

fn path() -> PathBuf {
    crate::setup::app_data_dir().join("exposure.json")
}

pub fn load() -> Option<Saved> {
    serde_json::from_str(&std::fs::read_to_string(path()).ok()?).ok()
}

/// Write-then-rename, so a crash never leaves half a file.
pub fn save(s: &Saved) {
    let p = path();
    let tmp = p.with_extension("json.tmp");
    let ok = serde_json::to_string(s)
        .ok()
        .and_then(|t| std::fs::write(&tmp, t).ok())
        .and_then(|_| std::fs::rename(&tmp, &p).ok());
    if ok.is_none() {
        crate::log_error!("[exposure] could not save {}", p.display());
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Level at the ear for a 0 dBFS signal; `None` when muted. Unknown volume counts as full volume (the safe side).
pub fn ear_offset(headset_db_spl: u8, volume: Option<audio_io::volume::OutputVolume>) -> Option<f32> {
    match volume {
        Some(v) if v.muted => None,
        Some(v) => Some(headset_db_spl as f32 + v.db.min(0.0)),
        None => Some(headset_db_spl as f32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_io::volume::OutputVolume;

    #[test]
    fn a_short_break_keeps_the_shift_and_a_long_one_starts_a_new_one() {
        let s = Saved { dose: 0.4, listen_secs: 3600.0, energy: 1.0, shocks: 2, last_heard: 1_000_000 };
        assert_eq!(carry_over(Some(s), 1_000_000 + 3600), s, "1 h lunch break: same shift");
        assert_eq!(carry_over(Some(s), 1_000_000 + BREAK_RESET_SECS), Saved::default(), "6 h off: new shift");
        assert_eq!(carry_over(None, 5), Saved::default());
    }

    #[test]
    fn ear_offset_follows_volume_and_mute() {
        let v = |db, muted| Some(OutputVolume { db, muted });
        assert_eq!(ear_offset(100, v(-20.0, false)), Some(80.0));
        assert_eq!(ear_offset(100, v(-20.0, true)), None);
        assert_eq!(ear_offset(100, None), Some(100.0), "unknown volume errs on the safe side");
        assert_eq!(ear_offset(100, v(3.0, false)), Some(100.0), "never above the headset rating");
    }
}
