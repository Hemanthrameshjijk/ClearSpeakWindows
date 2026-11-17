//! `clearspeak-engine.exe` - the background engine (audio, tray, hotkey, settings). No window; see `clearspeak_app::host`.

#![windows_subsystem = "windows"]

fn main() {
    #[cfg(windows)]
    if std::env::args().any(|a| a == "--voice-test") {
        // The "Test my voice" helper: records, builds each version, exits. Never the engine itself.
        clearspeak_app::voice_test::run_helper();
    } else {
        clearspeak_app::host::run_engine();
    }
}
