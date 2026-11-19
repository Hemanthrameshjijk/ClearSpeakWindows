//! Accent voice "Auto" (engine side): the male / female detector (`gender` crate) runs while Accent is on in Auto, and
//! the voice of the detected gender with the agent's chosen number is used (male 2 <-> female 2). The answer is saved
//! per license holder (`voice_gender.json`), so the next start, or the next agent on a shared PC, starts right.
//!
//! Only ever touches a voice that is already on: Accent off, or switched off by the license (`license_gate::enforce`
//! sets voice 0), is left alone. The switch itself waits for a pause (`accent_onnx` does that for every voice change).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use gender::Gender;

use crate::settings::Settings;
use crate::shared::EngineHandles;

const FILE: &str = "voice_gender.json";
const TICK: Duration = Duration::from_millis(500);

fn store_path() -> PathBuf {
    crate::setup::app_data_dir().join(FILE)
}

/// Saved answers by license holder name ("" = nobody signed in).
fn load_store() -> BTreeMap<String, String> {
    std::fs::read_to_string(store_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_answer(person: &str, g: Gender) {
    let mut m = load_store();
    m.insert(person.to_string(), g.label().to_string());
    if let Ok(text) = serde_json::to_string_pretty(&m) {
        if let Err(e) = std::fs::write(store_path(), text) {
            crate::log_error!("[voice] could not save the detected voice: {e}");
        }
    }
}

fn saved_answer(person: &str) -> Option<Gender> {
    match load_store().get(person).map(String::as_str) {
        Some("male") => Some(Gender::Male),
        Some("female") => Some(Gender::Female),
        _ => None,
    }
}

/// The voice to use for these settings and this answer (`None` = keep the hand-picked one).
pub fn auto_voice(s: &Settings, answer: Option<Gender>) -> Option<u8> {
    if !s.accent_voice_auto {
        return None;
    }
    answer.map(|g| g.voice_like(s.accent_voice_index()))
}

/// Runs for the life of the engine. `settings` returns the current settings.
pub fn run(handles: EngineHandles, settings: impl Fn() -> Settings) {
    let va = handles.voice_auto.clone();
    let Some(det) = va.detector.lock().ok().and_then(|d| d.clone()) else { return };
    let mut last_key = None::<String>;
    let mut person = String::new();
    let mut seen_changes = det.changes();
    let mut last_state = u8::MAX;
    let mut first = true;
    loop {
        std::thread::sleep(TICK);
        let s = settings();
        // Whose answer applies: re-read only when the stored license changes (sign-in / sign-out).
        if last_key.as_deref() != Some(s.license_key.as_str()) {
            last_key = Some(s.license_key.clone());
            let who = crate::license_gate::holder_name(&crate::dpapi::unprotect(&s.license_key)).unwrap_or_default();
            if first || who != person {
                first = false;
                person = who;
                let known = saved_answer(&person);
                det.reset(known);
                if let Some(g) = known {
                    crate::log_info!("[voice] Auto: using the saved answer ({}) until the voice says otherwise", g.label());
                }
            }
        }
        let voice = handles.accent.voice();
        det.set_enabled(s.accent_voice_auto && voice != 0);
        let state = det.state();
        va.state.store(state, Relaxed);
        va.gender.store(det.gender().map_or(0, Gender::code), Relaxed);
        if state != last_state {
            if state == gender::pipeline::S_UNAVAILABLE {
                crate::log_error!("[voice] Auto unavailable: {}", det.message());
            }
            last_state = state;
        }
        let changes = det.changes();
        if changes != seen_changes {
            seen_changes = changes;
            if let Some(g) = det.gender() {
                let (voiced_ms, ecapa, f0) = det.stats();
                crate::log_info!(
                    "[voice] Auto: detected {} after {:.1} s of speech (fingerprint score {ecapa:+.2}, pitch {f0} Hz)",
                    g.label(),
                    voiced_ms as f32 / 1000.0
                );
                save_answer(&person, g);
            }
        }
        if voice != 0 {
            if let Some(target) = auto_voice(&s, det.gender()) {
                if target != voice {
                    crate::log_info!("[voice] Auto: voice {voice} -> {target}");
                    handles.accent.set_voice(target);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_keeps_the_number_and_manual_is_left_alone() {
        let mut s = Settings::default();
        s.accent_voice = 2;
        s.accent_voice_auto = true;
        assert_eq!(auto_voice(&s, Some(Gender::Female)), Some(5), "male 2 -> female 2");
        assert_eq!(auto_voice(&s, None), None, "nothing detected yet: keep the current voice");
        s.accent_voice_auto = false;
        assert_eq!(auto_voice(&s, Some(Gender::Female)), None, "a hand-picked voice is never changed");
    }

    #[test]
    fn new_installs_use_auto_and_old_settings_keep_their_voice() {
        assert!(Settings::default().accent_voice_auto);
        // A settings.json written before Auto existed: everything but the new key.
        let mut before = Settings::default();
        before.accent_voice = 4;
        let mut v = serde_json::to_value(before).unwrap();
        v.as_object_mut().unwrap().remove("accent_voice_auto");
        let old: Settings = serde_json::from_value(v).unwrap();
        assert!(!old.accent_voice_auto, "older settings.json: Auto off, the hand-picked voice stays");
        assert_eq!(old.accent_voice, 4);
    }
}
