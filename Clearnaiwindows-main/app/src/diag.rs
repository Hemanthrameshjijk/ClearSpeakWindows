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
];

/// Names of `EngineHandles::presets`, in the same order (`ipc::PRESET_COUNT`).
pub const PRESET_NAMES: [&str; crate::ipc::PRESET_COUNT] = [
    "mic studio preset",
    "caller studio preset",
    "mic BVC model",
    "caller BVC model",
    "noise reduction strength",
    "mic boost dB",
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
        "[session] ClearNAI {} (build {}{}), {}, {} logical cores",
        env!("CARGO_PKG_VERSION"),
        env!("CLEARNAI_GIT_COMMIT"),
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

#[cfg(test)]
mod tests {
    use super::*;

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
