//! `clearnai-engine.exe` - the background engine (audio, tray, hotkey, settings). No window; see `clearnai_app::host`.

#![windows_subsystem = "windows"]

fn main() {
    #[cfg(windows)]
    if std::env::args().any(|a| a == "--voice-test") {
        // The "Test my voice" helper: records, builds each version, exits. Never the engine itself.
        clearnai_app::voice_test::run_helper();
    } else {
        clearnai_app::host::run_engine();
    }
}
