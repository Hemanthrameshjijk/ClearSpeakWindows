//! Accent voice "Auto" (engine side): the male / female detector (`gender` crate) runs while Accent is on in Auto, and
//! the voice of the detected gender with the agent's chosen number is used (male 2 <-> female 2). The answer is saved
//! per license holder (`voice_gender.json`), so the next start, or the next agent on a shared PC, starts right.
//! A saved answer is only a starting guess: the detector still decides from the voice in the first ~6 s of speech and
//! switches if it disagrees (see `gender::detector`).
//!
//! Every time detection starts (Accent switched on in Auto, or Auto picked while Accent is on) it listens again: the last
//! answer is used straight away as a guess and the voice corrects it within ~6 s, so a different agent at the PC is
//! picked up at once instead of after minutes of re-checking. "Listen again" in the window forgets the answer entirely.
//!
//! The developer sign-in (`dev_code`) is shared by everyone testing, so it never loads or saves an answer: one tester's
//! voice would otherwise become everyone's starting point.
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

// ---- "My voice": the agent's recorded voice fingerprint (see `accent_onnx::own_speaker`) ----

const OWN_FILE: &str = "my_voice.json";

/// Whose "My voice" recording applies: the license holder, or for the shared developer sign-in this Windows user on
/// this PC (testers all sign in with the dev code; their recordings must not overwrite each other, nor vanish).
fn own_voice_key(token: &str, holder: Option<String>) -> String {
    match answer_owner(token, holder) {
        Some(p) => p,
        None => format!(
            "dev:{}@{}",
            std::env::var("USERNAME").unwrap_or_default(),
            std::env::var("COMPUTERNAME").unwrap_or_default()
        ),
    }
}

/// Recordings by person, each DPAPI-protected (a voice fingerprint is personal data; only this Windows user on this
/// PC can read it back).
fn load_own_store() -> BTreeMap<String, String> {
    std::fs::read_to_string(crate::setup::app_data_dir().join(OWN_FILE))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn encode_fingerprint(fp: &[f32]) -> String {
    fp.iter().map(|v| format!("{v:.7}")).collect::<Vec<_>>().join(",")
}

fn decode_fingerprint(text: &str) -> Option<Vec<f32>> {
    let fp: Vec<f32> = text.split(',').map(|v| v.trim().parse().ok()).collect::<Option<_>>()?;
    (fp.len() == accent_onnx::FINGERPRINT_LEN && fp.iter().all(|v| v.is_finite())).then_some(fp)
}

fn save_own_voice(key: &str, fp: &[f32]) {
    let mut m = load_own_store();
    m.insert(key.to_string(), crate::dpapi::protect(&encode_fingerprint(fp)));
    match serde_json::to_string_pretty(&m) {
        Ok(text) => {
            if let Err(e) = std::fs::write(crate::setup::app_data_dir().join(OWN_FILE), text) {
                crate::log_error!("[voice] could not save my voice: {e}");
            }
        }
        Err(e) => crate::log_error!("[voice] could not save my voice: {e}"),
    }
}

fn saved_own_voice(key: &str) -> Option<Vec<f32>> {
    load_own_store().get(key).and_then(|p| decode_fingerprint(&crate::dpapi::unprotect(p)))
}

/// Whose saved answer applies for this license token: `None` for the shared developer sign-in (never saved or loaded).
fn answer_owner(token: &str, holder: Option<String>) -> Option<String> {
    if crate::dev_code::status(token, 0).is_some() {
        return None;
    }
    Some(holder.unwrap_or_default())
}

/// Whether detection should start listening afresh this tick, and with what starting guess: `Some(None)` = from
/// scratch ("Listen again" pressed), `Some(Some(g))`/`Some(None)` on start = keep the current answer only as a guess,
/// `None` = carry on.
fn fresh_listen(was_on: bool, now_on: bool, relisten: bool, current: Option<Gender>) -> Option<Option<Gender>> {
    if relisten {
        Some(None)
    } else if now_on && !was_on {
        Some(current)
    } else {
        None
    }
}

/// The voice to use for these settings and this answer (`None` = keep the hand-picked one).
pub fn auto_voice(s: &Settings, answer: Option<Gender>) -> Option<u8> {
    if !s.accent_voice_auto {
        return None;
    }
    answer.map(|g| g.voice_like(s.accent_voice_index()))
}

/// Runs for the life of the engine. `settings` returns the current settings. Every switch of detection on / off is
/// logged with the reason (Auto switch, Accent voice), so "not started" in the window can be explained from the log.
pub fn run(handles: EngineHandles, settings: impl Fn() -> Settings) {
    let va = handles.voice_auto.clone();
    // The detector is created with the audio engine; wait for it instead of giving up.
    let det = loop {
        if let Some(d) = va.detector.lock().ok().and_then(|d| d.clone()) {
            break d;
        }
        std::thread::sleep(TICK);
    };
    let mut logged_on = None::<bool>;
    let mut last_key = None::<String>;
    let mut person: Option<String> = None;
    let mut seen_changes = det.changes();
    let mut last_state = u8::MAX;
    let mut first = true;
    let mut was_on = false;
    // "My voice": whose recording applies, the recording, the last finished recording seen, and whether it is in use.
    let mut own_key = String::new();
    let mut own_fp: Option<Vec<f32>> = None;
    let mut seen_enrollments = det.enrollments();
    let mut own_logged = None::<bool>;
    loop {
        std::thread::sleep(TICK);
        let s = settings();
        // Whose answer applies: re-read only when the stored license changes (sign-in / sign-out).
        if last_key.as_deref() != Some(s.license_key.as_str()) {
            last_key = Some(s.license_key.clone());
            let token = crate::dpapi::unprotect(&s.license_key);
            let holder = crate::license_gate::holder_name(&token);
            let key = own_voice_key(&token, holder.clone());
            if key != own_key || own_fp.is_none() {
                own_fp = saved_own_voice(&key);
                own_key = key;
            }
            let who = answer_owner(&token, holder);
            if first || who != person {
                first = false;
                person = who;
                let known = person.as_deref().and_then(saved_answer);
                det.reset(known);
                if let Some(g) = known {
                    crate::log_info!("[voice] Auto: using the saved answer ({}) until the voice says otherwise", g.label());
                }
            }
        }
        // "Record my voice" pressed, and a recording finished.
        if va.record.swap(false, Relaxed) {
            crate::log_info!("[voice] My voice: recording ({:.0} s of speech)", gender::pipeline::ENROLL_S);
            det.start_enroll();
        }
        let done = det.enrollments();
        if done != seen_enrollments {
            seen_enrollments = done;
            match det.enrolled() {
                Some(fp) => {
                    save_own_voice(&own_key, &fp);
                    own_fp = Some(fp);
                    crate::log_info!("[voice] My voice: recorded and saved");
                }
                None => crate::log_error!("[voice] My voice: recording failed: {}", det.message()),
            }
        }
        let progress = det.enroll_progress();
        va.own_recording.store(progress.is_some(), Relaxed);
        va.own_rec_ms.store(progress.unwrap_or(0), Relaxed);
        va.own_ready.store(own_fp.is_some(), Relaxed);
        // In use only with the switch on and a recording made; it replaces the American voice (and Auto) while on.
        let own_on = s.accent_own_voice && own_fp.is_some();
        handles.accent.set_own_voice(if own_on { own_fp.clone() } else { None });
        if own_logged != Some(own_on) {
            own_logged = Some(own_on);
            crate::log_info!(
                "[voice] My voice {} (switch {}, recording {})",
                if own_on { "in use" } else { "not in use" },
                if s.accent_own_voice { "on" } else { "off" },
                if own_fp.is_some() { "saved" } else { "not made yet" }
            );
        }

        let voice = handles.accent.voice();
        // My voice replaces the American voice, so male / female detection has nothing to choose meanwhile.
        let now_on = s.accent_voice_auto && voice != 0 && !own_on;
        let relisten = va.relisten.swap(false, Relaxed);
        if let Some(guess) = fresh_listen(was_on, now_on, relisten, det.gender()) {
            det.reset(guess);
            if relisten || guess.is_some() {
                crate::log_info!(
                    "[voice] Auto: listening again ({})",
                    if relisten { "Listen again pressed" } else { "Accent switched on" }
                );
            }
        }
        was_on = now_on;
        if logged_on != Some(now_on) {
            logged_on = Some(now_on);
            crate::log_info!(
                "[voice] Auto detection {} (Auto switch {}, Accent voice {voice})",
                if now_on { "on: listening" } else { "off" },
                if s.accent_voice_auto { "on" } else { "off" }
            );
        }
        det.set_enabled(now_on);
        let state = det.state();
        va.state.store(state, Relaxed);
        va.gender.store(det.gender().map_or(0, Gender::code), Relaxed);
        va.confirmed.store(det.confirmed(), Relaxed);
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
                // A quick guess (first ~2 s) is used at once but only saved once the full check confirms it.
                let confirmed = det.confirmed();
                crate::log_info!(
                    "[voice] Auto: {} {} after {:.1} s of speech (fingerprint score {ecapa:+.2}, pitch {f0} Hz)",
                    if confirmed { "detected" } else { "quick guess" },
                    g.label(),
                    voiced_ms as f32 / 1000.0
                );
                if let (true, Some(p)) = (confirmed, person.as_deref()) {
                    save_answer(p, g);
                }
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
    fn my_voice_fingerprint_round_trips_and_bad_text_is_refused() {
        let fp: Vec<f32> = (0..accent_onnx::FINGERPRINT_LEN).map(|i| (i as f32 * 0.37).sin() / 9.8).collect();
        let back = decode_fingerprint(&encode_fingerprint(&fp)).expect("decodes");
        assert!(fp.iter().zip(&back).all(|(a, b)| (a - b).abs() < 1e-6));
        assert_eq!(decode_fingerprint("1,2,3"), None, "wrong length");
        assert_eq!(decode_fingerprint(""), None, "nothing saved / could not be unprotected");
        assert_eq!(decode_fingerprint(&encode_fingerprint(&fp).replacen('0', "x", 1)), None, "damaged");
    }

    #[test]
    fn detection_listens_again_whenever_it_starts() {
        use Gender::{Female, Male};
        assert_eq!(fresh_listen(false, true, false, Some(Male)), Some(Some(Male)), "Accent on: last answer is only a guess");
        assert_eq!(fresh_listen(false, true, false, None), Some(None));
        assert_eq!(fresh_listen(true, true, false, Some(Male)), None, "already listening: carry on");
        assert_eq!(fresh_listen(true, false, false, Some(Female)), None, "switching off keeps the answer");
        assert_eq!(fresh_listen(true, true, true, Some(Male)), Some(None), "Listen again: from scratch");
        assert_eq!(fresh_listen(false, false, true, Some(Male)), Some(None), "Listen again while off: ready for next time");
    }

    #[test]
    fn the_shared_dev_sign_in_never_keeps_an_answer() {
        let dev = crate::dev_code::token("", 1_000);
        let expected = if crate::dev_code::ENABLED { None } else { Some(String::new()) };
        assert_eq!(answer_owner(&dev, None), expected, "dev sign-in (in a dev build): nothing saved or loaded");
        assert_eq!(answer_owner("CLNAI1.real.token", Some("Asha".into())), Some("Asha".into()));
        assert_eq!(answer_owner("", None), Some(String::new()), "nobody signed in keeps its own slot");
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
