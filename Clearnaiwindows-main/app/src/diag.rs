//! Diagnostics for the log: a start-up header (version, build, Windows, CPU, settings, devices), readable names for
//! every switch and selector the window can change, and a settings diff, so a support log shows what the user did and
//! what the app was set to when an issue happened.
//!
//! Never logged: the license token and the user's name, email and organisation.

use crate::settings::Settings;
use crate::shared::DeviceList;

/// Names of `EngineHandles::toggles`, in the same order (`ipc::TOGGLE_COUNT`).
pub const TOGGLE_NAMES: [&str; crate::ipc::TOGGLE_COUNT] = [
    "mic noise (RNNoise)",
    "quiet nearby voices (voice gate)",
    "echo removal",
    "mic voices/noise (BVC/Hush)",
    "mic studio",
    "caller noise",
    "caller voices (BVC)",
    "caller studio",
    "mic master (unmuted)",
    "caller master",
    "monitor",
    "virtual microphone",
    "loud-sound protection",
    "save CPU automatically",
];

/// Names of `EngineHandles::presets`, in the same order (`ipc::PRESET_COUNT`).
pub const PRESET_NAMES: [&str; crate::ipc::PRESET_COUNT] = [
    "mic studio preset",
    "caller studio preset",
    "mic BVC model",
    "caller BVC model",
    "noise reduction strength",
    "mic boost dB",
    "CPU-saving step",
];

pub fn role_name(role: u8) -> &'static str {
    match role {
        crate::ipc::role::PHYSICAL_MIC => "microphone",
        crate::ipc::role::VIRTUAL_MIC_TARGET => "send cleaned audio to",
        crate::ipc::role::VIRTUAL_SPEAKER_SOURCE => "call audio source",
        crate::ipc::role::REAL_SPEAKER_OUTPUT => "call audio output",
        crate::ipc::role::MONITOR => "monitor output",
        _ => "unknown device role",
    }
}

/// Settings keys that are personal or secret: never written to the log.
fn private_key(key: &str) -> bool {
    key.contains("license") || key.contains("email") || key.starts_with("user_") || key.contains("password")
}

fn as_map(s: &Settings) -> serde_json::Map<String, serde_json::Value> {
    match serde_json::to_value(s) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    }
}

/// "key: old -> new" for every setting that changed (private keys left out), in key order.
pub fn settings_diff(old: &Settings, new: &Settings) -> Vec<String> {
    let (a, b) = (as_map(old), as_map(new));
    let mut keys: Vec<&String> = b.keys().filter(|k| !private_key(k)).collect();
    keys.sort();
    keys.into_iter()
        .filter_map(|k| {
            let (x, y) = (a.get(k), &b[k]);
            (x != Some(y)).then(|| format!("{k}: {} -> {y}", x.map_or("(none)".to_string(), |v| v.to_string())))
        })
        .collect()
}

/// Every setting (private keys left out) on one line, for the start-up header.
pub fn settings_line(s: &Settings) -> String {
    let m = as_map(s);
    let mut keys: Vec<&String> = m.keys().filter(|k| !private_key(k)).collect();
    keys.sort();
    keys.into_iter().map(|k| format!("{k}={}", m[k])).collect::<Vec<_>>().join(", ")
}

#[cfg(windows)]
fn windows_version() -> String {
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
    let mut v = OSVERSIONINFOW { dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    // SAFETY: RtlGetVersion fills the struct whose size is set above.
    let ok = unsafe { windows::Wdk::System::SystemServices::RtlGetVersion(&mut v) }.is_ok();
    if ok {
        format!("Windows {}.{} build {}", v.dwMajorVersion, v.dwMinorVersion, v.dwBuildNumber)
    } else {
        "Windows (version unknown)".to_string()
    }
}

#[cfg(not(windows))]
fn windows_version() -> String {
    std::env::consts::OS.to_string()
}

/// The start-up header: what is running, on what, set how, with which devices.
pub fn log_session_header(settings: &Settings, devices: &DeviceList) {
    crate::log_info!(
        "[session] ClearSpeak {} (build {}{}), {}, {} logical cores",
        env!("CARGO_PKG_VERSION"),
        env!("CLEARSPEAK_GIT_COMMIT"),
        if cfg!(feature = "dev-code") { ", test build" } else { "" },
        windows_version(),
        std::thread::available_parallelism().map_or(0, |n| n.get())
    );
    crate::log_info!("[session] settings: {}", settings_line(settings));
    let list = |v: &[crate::shared::DeviceOption], default: &Option<String>| {
        v.iter()
            .map(|d| if default.as_deref() == Some(d.id.as_str()) { format!("{} (default)", d.label) } else { d.label.clone() })
            .collect::<Vec<_>>()
            .join(" | ")
    };
    crate::log_info!("[session] microphones: {}", list(&devices.capture, &devices.default_capture));
    crate::log_info!("[session] outputs: {}", list(&devices.render, &devices.default_render));
}

fn dbfs(peak: f32) -> f32 {
    if peak <= 1e-6 {
        -120.0
    } else {
        20.0 * peak.log10()
    }
}

/// Per-minute health of one audio path (mic or caller): fed once per 10 ms frame with plain arithmetic (no
/// allocation, no locks), turned into one log line a minute.
#[derive(Default)]
pub struct PathHealth {
    frames: u64,
    sum_ns: u64,
    max_ns: u64,
    peak_in: f32,
    peak_out: f32,
    /// `shared::counters_to_array` at the previous line, for per-minute deltas.
    last: [u64; 8],
}

impl PathHealth {
    pub fn frame(&mut self, elapsed_ns: u64, peak_in: f32, peak_out: f32) {
        self.frames += 1;
        self.sum_ns += elapsed_ns;
        self.max_ns = self.max_ns.max(elapsed_ns);
        self.peak_in = self.peak_in.max(peak_in);
        self.peak_out = self.peak_out.max(peak_out);
    }

    /// The line for the minute just ended (`counters` = `shared::counters_to_array` now); starts a new minute.
    pub fn take_line(&mut self, counters: [u64; 8]) -> String {
        let d = |i: usize| counters[i].saturating_sub(self.last[i]);
        let avg_ms = if self.frames > 0 { self.sum_ns as f64 / self.frames as f64 / 1e6 } else { 0.0 };
        let line = format!(
            "{} frames, processing avg {avg_ms:.2} ms / worst {:.2} ms (budget 10 ms), late {}, dropped {}, \
             input under/over {}/{}, output under/over {}/{}, peak in {:.1} dBFS / out {:.1} dBFS",
            self.frames,
            self.max_ns as f64 / 1e6,
            d(2),
            d(1),
            d(3),
            d(4),
            d(5),
            d(6),
            dbfs(self.peak_in),
            dbfs(self.peak_out),
        );
        *self = PathHealth { last: counters, ..Default::default() };
        line
    }
}

/// The license state in one line, without the token, name, email or organisation.
pub fn license_line(status: &license::Status, now: u64) -> String {
    match status {
        license::Status::Licensed(p) => format!(
            "licensed (license {}, features {}, {} day(s) left)",
            p.lid,
            if p.feat.is_empty() { "none".to_string() } else { p.feat.join(",") },
            p.exp.saturating_sub(now) / 86_400
        ),
        license::Status::Ended => "license ended (expired, another PC, or clock turned back): processing off".into(),
        license::Status::SignIn => "not signed in: processing off".into(),
    }
}

/// Watches the slower engine states (accent, transcript, Hush watchdog, Bluetooth call mode) and logs each change
/// with how long the previous state lasted, so "loading took 9 s" or "Hush stalled at 14:02" are in the log. Polled
/// from a plain thread twice a second, never from an audio thread.
pub struct StateWatch {
    accent: Option<(accent_onnx::pipeline::AccentState, std::time::Instant)>,
    transcript: Option<(asr_onnx::pipeline::TranscriptState, std::time::Instant)>,
    hush_bypassed: bool,
    bpo_bypassed: bool,
    hands_free: bool,
    gaps_seen: u32,
    gaps_logged_at: Option<std::time::Instant>,
}

impl Default for StateWatch {
    fn default() -> Self {
        StateWatch {
            accent: None,
            transcript: None,
            hush_bypassed: false,
            bpo_bypassed: false,
            hands_free: false,
            gaps_seen: 0,
            gaps_logged_at: None,
        }
    }
}

/// At most one "accent gap" line per this window; the line carries how many gaps there were since the last one.
const GAP_LOG_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

impl StateWatch {
    pub fn check(&mut self, h: &crate::shared::EngineHandles) {
        let now = std::time::Instant::now();
        let acc = h.accent.status();
        match self.accent {
            Some((prev, since)) if prev != acc.state => {
                crate::log_info!(
                    "[accent] {prev:?} -> {:?} after {:.1} s{}",
                    acc.state,
                    now.duration_since(since).as_secs_f32(),
                    if acc.message.is_empty() { String::new() } else { format!(": {}", acc.message) }
                );
                self.accent = Some((acc.state, now));
            }
            None => self.accent = Some((acc.state, now)),
            _ => {}
        }
        // Each underrun is a moment the listener may hear a short gap; log when they happen (bounded).
        if acc.underruns < self.gaps_seen {
            self.gaps_seen = acc.underruns; // the stage restarted (voice changed)
        }
        if acc.underruns > self.gaps_seen && self.gaps_logged_at.is_none_or(|t| now.duration_since(t) >= GAP_LOG_EVERY) {
            crate::log_info!(
                "[accent] {} new gap(s) (total {}): speed {:.2}, cushion {} ms, delay {:.0} ms, catch-ups {}, dropped input {}",
                acc.underruns - self.gaps_seen,
                acc.underruns,
                acc.rtf,
                acc.jitter_ms,
                acc.latency_ms,
                acc.catchups,
                acc.dropped_input
            );
            self.gaps_seen = acc.underruns;
            self.gaps_logged_at = Some(now);
        }
        let ts = h.transcript.status();
        match self.transcript {
            Some((prev, since)) if prev != ts.state => {
                crate::log_info!(
                    "[transcript] {prev:?} -> {:?} after {:.1} s{}",
                    ts.state,
                    now.duration_since(since).as_secs_f32(),
                    if ts.message.is_empty() { String::new() } else { format!(": {}", ts.message) }
                );
                self.transcript = Some((ts.state, now));
            }
            None => self.transcript = Some((ts.state, now)),
            _ => {}
        }
        let hush = h.mic_hush_status.as_ref().is_some_and(|s| s.is_bypassed());
        if hush != self.hush_bypassed {
            crate::log_error!("[hush] mic Hush {}", if hush { "STALLED and is now bypassed (voice passes uncleaned)" } else { "running again" });
            self.hush_bypassed = hush;
        }
        let bpo = h.mic_bpo_v1_status.as_ref().is_some_and(|s| s.is_bypassed());
        if bpo != self.bpo_bypassed {
            crate::log_error!("[hush] mic Hush bpo_v1 {}", if bpo { "STALLED and is now bypassed" } else { "running again" });
            self.bpo_bypassed = bpo;
        }
        let hf = h.counters.hands_free.load(std::sync::atomic::Ordering::Relaxed);
        if hf != self.hands_free {
            crate::log_info!(
                "[device] microphone {} Bluetooth call mode",
                if hf { "entered" } else { "left" }
            );
            self.hands_free = hf;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn license_line_never_names_the_person() {
        let p = license::token::Payload {
            v: 1,
            lid: "L42".into(),
            org: "o1".into(),
            uid: "u1".into(),
            name: "Asha Person".into(),
            email: "asha@example.com".into(),
            orgname: "Some Org".into(),
            dev: vec![],
            min: 0,
            feat: vec!["nc".into(), "bvc".into()],
            iat: 0,
            exp: 10 * 86_400,
        };
        let line = license_line(&license::Status::Licensed(p), 86_400);
        assert_eq!(line, "licensed (license L42, features nc,bvc, 9 day(s) left)");
        assert!(!line.contains("Asha") && !line.contains("example.com") && !line.contains("Some Org"));
    }

    #[test]
    fn health_line_reports_the_minute_and_resets() {
        let mut h = PathHealth::default();
        h.frame(2_000_000, 0.1, 0.5);
        h.frame(4_000_000, 0.2, 1.0);
        let line = h.take_line([2, 0, 1, 0, 0, 3, 0, 0]);
        assert!(line.starts_with("2 frames, processing avg 3.00 ms / worst 4.00 ms"), "{line}");
        assert!(line.contains("late 1") && line.contains("output under/over 3/0"), "{line}");
        assert!(line.contains("peak in -14.0 dBFS / out 0.0 dBFS"), "{line}");
        // Next minute: counters as deltas, nothing carried over.
        let line = h.take_line([2, 0, 1, 0, 0, 5, 0, 0]);
        assert!(line.starts_with("0 frames") && line.contains("late 0") && line.contains("output under/over 2/0"), "{line}");
    }

    #[test]
    fn diff_lists_only_changes_and_never_private_fields() {
        let old = Settings::default();
        let mut new = old.clone();
        new.mic_boost_on = true;
        new.mic_boost_db = 14;
        new.license_key = "secret-token".into();
        new.user_email = "someone@example.com".into();
        let diff = settings_diff(&old, &new);
        assert_eq!(diff, vec!["mic_boost_db: 12 -> 14".to_string(), "mic_boost_on: false -> true".to_string()]);
        let all = settings_line(&new);
        assert!(!all.contains("secret-token") && !all.contains("someone@example.com"));
        assert!(all.contains("mic_boost_db=14"));
    }
}
