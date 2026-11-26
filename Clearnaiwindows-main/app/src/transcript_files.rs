//! Saved transcripts on disk (`%LOCALAPPDATA%\ClearSpeak\transcripts`): keep-for-N-days cleanup and "Delete all".
//! Only files the app itself writes there are touched (call files, their JSON copies, older per-session files and
//! exports); anything else a user puts in the folder stays.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// The keep choices offered in the Transcript tab (days; 0 = keep forever) and their names.
pub const KEEP_DAYS: [u32; 4] = [7, 30, 90, 0];
pub const KEEP_NAMES: [&str; 4] = ["7 days", "30 days", "90 days", "Forever"];
/// Default for new and older settings files.
pub const DEFAULT_KEEP_DAYS: u32 = 30;

pub fn keep_name(days: u32) -> &'static str {
    KEEP_DAYS.iter().position(|&d| d == days).map_or(KEEP_NAMES[1], |i| KEEP_NAMES[i])
}

pub fn keep_days(name: &str) -> u32 {
    KEEP_NAMES.iter().position(|&n| n == name).map_or(DEFAULT_KEEP_DAYS, |i| KEEP_DAYS[i])
}

/// A file the app wrote: `<date>_call*.txt|jsonl|wav` (a call's transcript and recording), `transcript-<n>.txt`
/// (older per-session files) or `transcript-export-<n>.txt`.
pub fn is_transcript_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".wav") {
        return n.contains("_call");
    }
    let ext_ok = n.ends_with(".txt") || n.ends_with(".jsonl");
    ext_ok && (n.contains("_call") || n.starts_with("transcript-"))
}

fn transcript_files(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| is_transcript_file(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect()
}

/// Deletes transcript files last changed more than `days` days before `now` (`days` 0 = keep all). Returns how many.
pub fn delete_older_than(dir: &Path, days: u32, now: SystemTime) -> usize {
    if days == 0 {
        return 0;
    }
    let max_age = Duration::from_secs(days as u64 * 86_400);
    let mut n = 0;
    for p in transcript_files(dir) {
        let old = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > max_age));
        if old && std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    n
}

/// Deletes every transcript file it can. Returns how many were deleted and, if some could not be (open in another
/// program), the first such error; it goes on past a failure.
pub fn delete_all(dir: &Path) -> (usize, Option<String>) {
    let mut n = 0;
    let mut err = None;
    for p in transcript_files(dir) {
        match std::fs::remove_file(&p) {
            Ok(()) => n += 1,
            Err(e) => {
                err.get_or_insert_with(|| format!("could not delete {}: {e}", p.display()));
            }
        }
    }
    (n, err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("clearspeak-tx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn only_the_apps_own_files_count() {
        assert!(is_transcript_file("2026-11-26_09-41-05_call.txt"));
        assert!(is_transcript_file("2026-11-26_09-41-05_call-2.jsonl"));
        assert!(is_transcript_file("transcript-1794424478.txt"));
        assert!(is_transcript_file("transcript-export-1794424478.txt"));
        assert!(is_transcript_file("2026-11-26_09-41-05_call_eyeBeam_+919876543210.wav"));
        assert!(is_transcript_file("2026-11-26_09-41-05_call_VICIdial_5551234.txt"));
        assert!(!is_transcript_file("notes.txt"));
        assert!(!is_transcript_file("ringtone.wav"));
        assert!(!is_transcript_file("transcript-1.wav"));
        assert!(!is_transcript_file("2026-11-26_09-41-05_call.docx"));
    }

    #[test]
    fn keep_choices_round_trip() {
        for (d, n) in KEEP_DAYS.iter().zip(KEEP_NAMES) {
            assert_eq!(keep_name(*d), n);
            assert_eq!(keep_days(n), *d);
        }
        assert_eq!(keep_name(12), "30 days", "an odd value from a hand-edited file shows the default");
    }

    #[test]
    fn old_call_files_go_and_new_ones_and_other_files_stay() {
        let d = temp_dir("age");
        for f in ["2026-01-01_10-00-00_call.txt", "2026-01-01_10-00-00_call.jsonl", "notes.txt"] {
            std::fs::write(d.join(f), "x").unwrap();
        }
        // "now" 31 days ahead: the files are 31 days old
        let later = SystemTime::now() + Duration::from_secs(31 * 86_400);
        assert_eq!(delete_older_than(&d, 90, later), 0, "kept for 90 days");
        assert_eq!(delete_older_than(&d, 0, later), 0, "forever");
        assert_eq!(delete_older_than(&d, 30, later), 2);
        assert!(d.join("notes.txt").exists(), "a file the app did not write is never deleted");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn delete_all_removes_every_transcript_but_nothing_else() {
        let d = temp_dir("all");
        for f in ["2026-11-26_09-41-05_call.txt", "transcript-export-1.txt", "keep.me"] {
            std::fs::write(d.join(f), "x").unwrap();
        }
        assert_eq!(delete_all(&d), (2, None));
        assert!(d.join("keep.me").exists());
        assert_eq!(delete_all(&d.join("missing")), (0, None), "no folder yet is fine");
        let _ = std::fs::remove_dir_all(&d);
    }
}
