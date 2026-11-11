//! Deters turning the PC's clock back to stretch a license. The latest time the app has seen is kept in two
//! places (the settings file and the registry); a clock that is more than a day behind it is "wrong" and the license is
//! not honoured until the date is fixed. A deterrent only: someone who edits both places defeats it.

const TOLERANCE_SECS: u64 = 86_400;
/// Writes are throttled so the registry is touched about once a minute, not on every tick.
const WRITE_EVERY_SECS: u64 = 60;

/// True when `now` is well behind the latest time seen before.
pub fn rolled_back(last_seen: u64, now: u64) -> bool {
    last_seen > 0 && now + TOLERANCE_SECS < last_seen
}

/// The latest time to remember after seeing `now`, and whether it is worth writing (rolled-back clocks never lower it).
pub fn next_mark(last_seen: u64, now: u64) -> (u64, bool) {
    if rolled_back(last_seen, now) || now < last_seen + WRITE_EVERY_SECS {
        (last_seen, false)
    } else {
        (now, true)
    }
}

/// The time kept in the registry (0 = none).
pub fn registry_mark() -> u64 {
    imp::read()
}

pub fn write_registry_mark(ts: u64) {
    imp::write(ts);
}

#[cfg(windows)]
mod imp {
    use windows::core::w;
    use windows::Win32::System::Registry::{RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_QWORD, RRF_RT_REG_QWORD};

    pub fn read() -> u64 {
        let mut value = 0u64;
        let mut size = 8u32;
        // SAFETY: `value` is an 8-byte buffer and `size` says so.
        let ok = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, w!("Software\\ClearNAI"), w!("ls"), RRF_RT_REG_QWORD, None, Some(&mut value as *mut u64 as *mut _), Some(&mut size)).is_ok()
        };
        if ok {
            value
        } else {
            0
        }
    }

    pub fn write(ts: u64) {
        // SAFETY: the data pointer and length describe the 8 bytes of `ts`.
        unsafe {
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, w!("Software\\ClearNAI"), w!("ls"), REG_QWORD.0, Some(&ts as *const u64 as *const _), 8);
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn read() -> u64 {
        0
    }
    pub fn write(_: u64) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_clock_more_than_a_day_behind_is_wrong() {
        let day = 86_400;
        assert!(!rolled_back(0, 5), "nothing seen yet");
        assert!(!rolled_back(1_000_000, 1_000_000 - day), "exactly a day behind is tolerated");
        assert!(rolled_back(1_000_000, 1_000_000 - day - 1));
        assert!(!rolled_back(1_000_000, 2_000_000));
    }

    #[test]
    fn the_mark_only_moves_forward_and_writes_are_throttled() {
        assert_eq!(next_mark(1_000, 1_030), (1_000, false), "too soon to write again");
        assert_eq!(next_mark(1_000, 1_060), (1_060, true));
        assert_eq!(next_mark(1_000_000, 10), (1_000_000, false), "a rolled-back clock never lowers the mark");
    }

    #[cfg(windows)]
    #[test]
    fn the_registry_mark_round_trips() {
        let before = registry_mark();
        write_registry_mark(12_345_678);
        assert_eq!(registry_mark(), 12_345_678);
        write_registry_mark(before); // put back whatever was there
    }
}
