//! `clearspeak.exe` - the ClearSpeak window. Talks to `clearspeak-engine.exe` (see `clearspeak_app::remote`).

#![windows_subsystem = "windows"]

fn main() -> iced::Result {
    #[cfg(windows)]
    return clearspeak_app::remote::run_window();
    #[cfg(not(windows))]
    Ok(())
}
