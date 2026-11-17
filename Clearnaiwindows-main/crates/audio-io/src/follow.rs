//! Which device a capture/render thread should be using *right now*, so headsets (Bluetooth earbuds in particular)
//! are followed when they connect, disconnect or come back under a new id.
//!
//! Before this, "Default microphone" was resolved once at start (plugging in earbuds was never followed), and a saved
//! device that went away was retried forever on its stale id: `GetDevice` succeeds for an endpoint that is no longer
//! active, then `Activate` fails with 0x88890004 (seen ~1,100 times in a row in a real log).

/// What a worker thread was asked to use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Whatever Windows' default device is at the moment (followed while running).
    Default,
    /// A specific device chosen by the user.
    Id(String),
}

/// What the thread should open now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub id: String,
    /// The saved device is not connected and the Windows default is used in its place.
    pub fallback: bool,
}

/// A snapshot of the devices, so the decision itself is plain logic (and testable without hardware).
pub struct Snapshot<'a> {
    /// (id, friendly name) of every *active* device in this direction.
    pub active: &'a [(String, String)],
    pub default_id: Option<&'a str>,
    /// Friendly name of the saved device (readable even when it is unplugged), for rematching a new id.
    pub saved_name: Option<&'a str>,
}

/// Decides what to open. `allow_fallback`: may a missing saved device be replaced by the default? Yes for the
/// microphone (talking on the laptop mic beats silence); no for render targets like the virtual-mic cable, where the
/// default would be the real speakers.
pub fn choose(target: &Target, snap: &Snapshot, allow_fallback: bool) -> Option<Choice> {
    // For the real microphone (`allow_fallback`), the Windows default may itself be a virtual device: call apps are
    // set to "ClearSpeak Microphone" or "CABLE Output", which is often the default too. Recording that would make
    // ClearSpeak listen to its own output (seen live: a feedback loop, input backlog trimmed every 1-2 s, cut words), so
    // the first real microphone is used instead.
    let default_for_mic = || -> Option<String> {
        let def = snap.default_id?;
        let def_name = snap.active.iter().find(|(id, _)| id == def).map(|(_, n)| n.as_str());
        if !allow_fallback || !def_name.is_some_and(is_virtual_device) {
            return Some(def.to_string());
        }
        snap.active.iter().find(|(_, n)| !is_virtual_device(n)).map(|(id, _)| id.clone())
    };
    match target {
        Target::Default => default_for_mic().map(|id| Choice { id, fallback: false }),
        Target::Id(saved) => {
            if snap.active.iter().any(|(id, _)| id == saved) {
                return Some(Choice { id: saved.clone(), fallback: false });
            }
            // The same headset back under another id (other USB port, Bluetooth re-pair, driver reinstall).
            if let Some(name) = snap.saved_name.filter(|n| !n.is_empty()) {
                if let Some((id, _)) = snap.active.iter().find(|(_, n)| n == name) {
                    return Some(Choice { id: id.clone(), fallback: false });
                }
            }
            if allow_fallback {
                default_for_mic().map(|id| Choice { id, fallback: true })
            } else {
                None
            }
        }
    }
}

/// Bluetooth earbuds/headsets in their call mode ("Hands-Free"): mic and earbud sound are phone quality (8-16 kHz).
pub fn is_hands_free(name: &str, sample_rate: u32) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("hands-free") || n.contains("handsfree") || (sample_rate > 0 && sample_rate <= 16_000)
}

/// A virtual endpoint (ClearSpeak's own device, VB-CABLE and similar), never a real microphone.
pub fn is_virtual_device(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.contains("clearspeak") || l.contains("clearnai") || l.contains("cable") || l.contains("vb-audio") || l.contains("virtual")
}

/// Two endpoints of the same physical device: Windows names them "Headset (realme Buds T310)" and
/// "Headphones (realme Buds T310)", so the text in the last brackets matches.
pub fn same_physical_device(a: &str, b: &str) -> bool {
    fn key(n: &str) -> Option<&str> {
        let open = n.rfind('(')?;
        let close = n.rfind(')')?;
        (close > open + 1).then(|| n[open + 1..close].trim())
    }
    matches!((key(a), key(b)), (Some(x), Some(y)) if !x.is_empty() && x.eq_ignore_ascii_case(y))
}

#[cfg(windows)]
mod live {
    use super::{choose, Choice, Snapshot, Target};
    use wasapi::{DeviceEnumerator, Direction};

    fn active_devices(e: &DeviceEnumerator, direction: &Direction) -> Vec<(String, String)> {
        let Ok(collection) = e.get_device_collection(direction) else { return Vec::new() };
        let mut out = Vec::new();
        for d in &collection {
            let Ok(d) = d else { continue };
            if let (Ok(id), Ok(name)) = (d.get_id(), d.get_friendlyname()) {
                out.push((id, name));
            }
        }
        out
    }

    /// Looks at the devices now and decides (see [`choose`]). `None`: nothing usable is connected.
    pub fn resolve(direction: &Direction, target: &Target, allow_fallback: bool) -> Option<Choice> {
        let e = DeviceEnumerator::new().ok()?;
        let default_id = e.get_default_device(direction).ok().and_then(|d| d.get_id().ok());
        let (active, saved_name) = match target {
            // The real mic following the default needs the list, to skip a virtual default (see `choose`).
            Target::Default if allow_fallback => (active_devices(&e, direction), None),
            Target::Default => (Vec::new(), None),
            Target::Id(id) => {
                let name = e.get_device(id).ok().and_then(|d| d.get_friendlyname().ok());
                (active_devices(&e, direction), name)
            }
        };
        let snap = Snapshot { active: &active, default_id: default_id.as_deref(), saved_name: saved_name.as_deref() };
        choose(target, &snap, allow_fallback)
    }

    /// True when the thread should reopen because a better device is available (the default changed, the saved
    /// headset came back, or it returned under a new id). Cheap enough to call about once a second.
    pub fn should_switch(direction: &Direction, target: &Target, current: &Choice, allow_fallback: bool) -> bool {
        match target {
            // The real mic may sit on a real device while the default is virtual: compare full decisions.
            Target::Default if allow_fallback => resolve(direction, target, allow_fallback).is_some_and(|c| c != *current),
            // Fast path: one COM call, no enumeration.
            Target::Default => {
                let now = DeviceEnumerator::new().ok().and_then(|e| e.get_default_device(direction).ok()).and_then(|d| d.get_id().ok());
                now.is_some_and(|id| id != current.id)
            }
            Target::Id(saved) if !current.fallback && &current.id == saved => {
                // On the saved device: only leave it when it is gone (the stream errors out on its own anyway).
                let state = DeviceEnumerator::new().ok().and_then(|e| e.get_device(saved).ok()).and_then(|d| d.get_state().ok());
                !matches!(state, Some(wasapi::DeviceState::Active) | None)
            }
            Target::Id(_) => resolve(direction, target, allow_fallback).is_some_and(|c| c != *current),
        }
    }
}

#[cfg(windows)]
pub use live::{resolve, should_switch};

/// Runs [`should_switch`] once a second on its own thread, so the audio thread only reads a flag. The check lists
/// devices and reads their names (tens of ms, more with Bluetooth), which on the audio thread itself could make a
/// small glitch every second.
#[cfg(windows)]
pub struct SwitchWatch {
    due: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(windows)]
impl SwitchWatch {
    pub fn start(direction: wasapi::Direction, target: Target, current: Choice, allow_fallback: bool) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
        use std::sync::Arc;
        let due = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (d, s) = (due.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("audio-io-follow".into()).spawn(move || {
            let _ = wasapi::initialize_mta();
            while !s.load(Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if s.load(Relaxed) {
                    break;
                }
                if should_switch(&direction, &target, &current, allow_fallback) {
                    d.store(true, Relaxed);
                    break;
                }
            }
        });
        Self { due, stop }
    }

    /// A better device is available: the audio thread should reopen.
    pub fn due(&self) -> bool {
        self.due.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(windows)]
impl Drop for SwitchWatch {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devs() -> Vec<(String, String)> {
        vec![
            ("mic-laptop".into(), "Microphone Array (Realtek(R) Audio)".into()),
            ("mic-buds-2".into(), "Headset (OnePlus Buds Hands-Free)".into()),
        ]
    }

    #[test]
    fn default_follows_windows() {
        let a = devs();
        let s = Snapshot { active: &a, default_id: Some("mic-buds-2"), saved_name: None };
        assert_eq!(choose(&Target::Default, &s, true), Some(Choice { id: "mic-buds-2".into(), fallback: false }));
    }

    #[test]
    fn a_saved_device_that_is_present_is_used() {
        let a = devs();
        let s = Snapshot { active: &a, default_id: Some("mic-buds-2"), saved_name: Some("Microphone Array (Realtek(R) Audio)") };
        assert_eq!(choose(&Target::Id("mic-laptop".into()), &s, true).unwrap().id, "mic-laptop");
    }

    #[test]
    fn a_headset_back_under_a_new_id_is_found_by_name() {
        let a = devs();
        let s = Snapshot { active: &a, default_id: Some("mic-laptop"), saved_name: Some("Headset (OnePlus Buds Hands-Free)") };
        assert_eq!(choose(&Target::Id("mic-buds-OLD".into()), &s, false), Some(Choice { id: "mic-buds-2".into(), fallback: false }));
    }

    #[test]
    fn a_missing_mic_falls_back_to_the_default_but_a_missing_cable_does_not() {
        let a = devs();
        let s = Snapshot { active: &a, default_id: Some("mic-laptop"), saved_name: Some("EPOS PC 8 USB") };
        assert_eq!(choose(&Target::Id("gone".into()), &s, true), Some(Choice { id: "mic-laptop".into(), fallback: true }));
        assert_eq!(choose(&Target::Id("gone".into()), &s, false), None, "never render the virtual mic into the speakers");
    }

    #[test]
    fn the_real_mic_never_falls_back_to_a_virtual_default() {
        let active = vec![
            ("clearspeak".to_string(), "ClearSpeak Microphone (ClearSpeak Virtual Audio Device)".to_string()),
            ("cable".to_string(), "CABLE Output (VB-Audio Virtual Cable)".to_string()),
            ("laptop".to_string(), "Microphone Array (Realtek(R) Audio)".to_string()),
        ];
        let snap = Snapshot { active: &active, default_id: Some("clearspeak"), saved_name: Some("Headset (realme Buds T310)") };
        // Saved earbuds gone, default is ClearSpeak's own mic: use the real laptop mic, not a feedback loop.
        let c = choose(&Target::Id("buds".into()), &snap, true).unwrap();
        assert_eq!((c.id.as_str(), c.fallback), ("laptop", true));
        // Following the default: same rule.
        assert_eq!(choose(&Target::Default, &snap, true).unwrap().id, "laptop");
        // A virtual device the user picked on purpose is still used.
        assert_eq!(choose(&Target::Id("cable".into()), &snap, true).unwrap().id, "cable");
        // Call-audio capture (no fallback) keeps following a virtual default.
        assert_eq!(choose(&Target::Default, &snap, false).unwrap().id, "clearspeak");
        // Only virtual devices connected: nothing real to record.
        let only_virtual = vec![active[0].clone()];
        let snap = Snapshot { active: &only_virtual, default_id: Some("clearspeak"), saved_name: None };
        assert!(choose(&Target::Default, &snap, true).is_none());
    }

    #[test]
    fn same_physical_device_matches_the_bracketed_name() {
        assert!(same_physical_device("Headset (realme Buds T310)", "Headphones (realme Buds T310)"));
        assert!(!same_physical_device("Headset (realme Buds T310)", "CABLE Input (VB-Audio Virtual Cable)"));
        assert!(!same_physical_device("Headset", "Headphones"));
        assert!(!same_physical_device("Headset ()", "Headphones ()"));
    }

    #[test]
    fn hands_free_is_recognised() {
        assert!(is_hands_free("Headset (OnePlus Buds Hands-Free)", 48_000));
        assert!(is_hands_free("Headset Microphone", 16_000));
        assert!(!is_hands_free("Microphone (EPOS PC 8 USB)", 48_000));
        assert!(!is_hands_free("Microphone (EPOS PC 8 USB)", 0));
    }
}
