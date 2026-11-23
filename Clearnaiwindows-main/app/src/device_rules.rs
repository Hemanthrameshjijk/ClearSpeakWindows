//! Startup sanity check of the saved device choices against what is actually installed, so a stale or wrong
//! saved id never silently routes audio the wrong way (e.g. the physical mic chosen as the "call audio source",
//! which plays your own voice out of the speakers).

use crate::settings::Settings;
use crate::shared::{DeviceList, DeviceOption};

/// True for virtual-cable style endpoints (VB-CABLE and similar).
/// A virtual cable or virtual audio device (by its name), not real speakers/mic.
pub fn is_virtual_label(label: &str) -> bool {
    is_virtual(label)
}

fn is_virtual(label: &str) -> bool {
    let l = label.to_ascii_lowercase();
    l.contains("cable") || l.contains("vb-audio") || l.contains("virtual")
}

/// Our own virtual microphone driver, under its current name or the one it had before the rename.
fn is_our_driver(label: &str) -> bool {
    let l = label.to_ascii_lowercase();
    l.contains("clearspeak virtual audio device") || l.contains("clearnai virtual audio device")
}

/// "CABLE Input (VB-Audio Virtual Cable)" ->"cable output (vb-audio virtual cable)": the other end of the same
/// cable. Whatever we render into one end comes out of the other, so that end must never feed the speaker path.
fn paired_capture_label(render_label: &str) -> String {
    render_label.to_ascii_lowercase().replacen("input", "output", 1)
}

fn find<'a>(list: &'a [DeviceOption], id: &str) -> Option<&'a DeviceOption> {
    list.iter().find(|d| d.id == id)
}

/// Picker choices: devices that are (`virtual_wanted`) or are not virtual cables. A real mic must not be a cable
/// and the speakers must not be a cable; the mic target and call source must be.
pub fn only_virtual(options: &[DeviceOption], virtual_wanted: bool) -> Vec<DeviceOption> {
    options.iter().filter(|d| is_virtual(&d.label) == virtual_wanted).cloned().collect()
}

/// The microphone of a Bluetooth headset. Opening it puts the earbuds into call mode, which drops what you hear to
/// phone quality too. Known by "Hands-Free" in the name, or as Windows names earbuds: a "Headset (X)" mic next to a
/// "Headphones (X)" output of the same device (a USB headset's mic is "Microphone (X)").
pub fn is_bluetooth_mic(label: &str, devices: &DeviceList) -> bool {
    audio_io::follow::is_hands_free(label, 0)
        || (label.to_ascii_lowercase().starts_with("headset")
            && devices.render.iter().any(|r| {
                r.label.to_ascii_lowercase().starts_with("headphones") && audio_io::follow::same_physical_device(label, &r.label)
            }))
}

/// A real, non-Bluetooth microphone to talk on, so Bluetooth earbuds can stay in full-quality listening mode: a USB
/// or wired headset mic first, then the PC's built-in mic. `None` when there is no other real mic.
pub fn talk_mic_instead(devices: &DeviceList) -> Option<DeviceOption> {
    let built_in = |l: &str| {
        let l = l.to_ascii_lowercase();
        l.contains("array") || l.contains("realtek") || l.contains("internal") || l.contains("built-in")
    };
    only_virtual(&devices.capture, false)
        .into_iter()
        .filter(|d| !is_bluetooth_mic(&d.label, devices) && !audio_io::follow::is_virtual_device(&d.label))
        .min_by_key(|d| built_in(&d.label))
}

/// The "CABLE Input" render endpoint a virtual microphone should go to, when exactly one is installed (the usual
/// VB-CABLE setup). Two or none means there is no safe pick, so `None`.
pub fn cable_input_target(devices: &DeviceList) -> Option<DeviceOption> {
    let mut found = devices.render.iter().filter(|d| d.label.to_ascii_lowercase().contains("cable input"));
    let only = found.next()?;
    found.next().is_none().then(|| only.clone())
}

/// "Send cleaned audio to" choices with friendlier names: the ClearSpeak driver's render end ("Speakers (ClearSpeak
/// Virtual Audio Device)") is shown as "ClearSpeak Microphone input", since sending there is what feeds the ClearSpeak
/// Microphone. Only the label changes; the id (what is saved and used) stays the same.
pub fn virtual_target_choices(render: &[DeviceOption]) -> Vec<DeviceOption> {
    only_virtual(render, true)
        .into_iter()
        .map(|mut d| {
            if is_our_driver(&d.label) {
                d.label = "ClearSpeak Microphone input".to_string();
            }
            d
        })
        .collect()
}

/// The microphone a meeting app must pick to hear ClearSpeak's cleaned voice: the capture end of the same virtual
/// device the cleaned audio is sent to ("Speakers (ClearSpeak Virtual Audio Device)" pairs with
/// "ClearSpeak Microphone (ClearSpeak Virtual Audio Device)"; "CABLE Input (VB-Audio Virtual Cable)" with "CABLE Output (...)").
pub fn meeting_mic_label(devices: &DeviceList, mic_target_id: &str) -> Option<String> {
    let target = find(&devices.render, mic_target_id)?;
    let start = target.label.rfind(" (")?;
    let suffix = &target.label[start..];
    let mut found = devices.capture.iter().filter(|d| d.label.ends_with(suffix));
    let only = found.next()?;
    found.next().is_none().then(|| only.label.clone())
}

/// Where the Default profile sends the cleaned voice: the ClearSpeak driver ("ClearSpeak Microphone input") when it is
/// installed, otherwise the single VB-CABLE input.
pub fn preferred_mic_target(devices: &DeviceList) -> Option<DeviceOption> {
    devices
        .render
        .iter()
        .find(|d| is_our_driver(&d.label))
        .cloned()
        .or_else(|| cable_input_target(devices))
}

/// Call-audio source choices: virtual capture ends, minus the end paired with the mic target's cable.
pub fn call_source_choices(devices: &DeviceList, mic_target_id: Option<&str>) -> Vec<DeviceOption> {
    let paired = mic_target_id
        .and_then(|id| find(&devices.render, id))
        .map(|d| paired_capture_label(&d.label));
    only_virtual(&devices.capture, true)
        .into_iter()
        .filter(|d| paired.as_deref() != Some(d.label.to_ascii_lowercase().as_str()))
        .collect()
}

/// What to actually use this run, and which saved choices were never valid (and so are cleared in settings).
pub struct Resolved {
    pub settings: Settings,
    pub mic_target: Option<String>,
    pub speaker_source: Option<String>,
    pub real_output: Option<String>,
    /// Plain-language notes for the log.
    pub notes: Vec<String>,
    /// Settings were changed in a way that should be saved (a choice that could never work was cleared).
    pub changed: bool,
}

pub fn resolve(saved: &Settings, devices: &DeviceList, auto_mic_target: Option<String>) -> Resolved {
    let mut s = saved.clone();
    let mut notes = Vec::new();
    let mut changed = false;
    let no_enumeration = devices.render.is_empty() && devices.capture.is_empty();

    // Virtual mic target: a present virtual render endpoint, else the single auto-detected CABLE Input.
    let mic_target = match saved.virtual_mic_target_id.as_deref() {
        Some(id) if no_enumeration || find(&devices.render, id).is_some_and(|d| is_virtual(&d.label)) => Some(id.to_string()),
        Some(_) => {
            notes.push("saved virtual microphone target is missing or not a virtual cable; auto-detecting".into());
            auto_mic_target
        }
        None => auto_mic_target,
    };
    let mic_target_label = mic_target.as_deref().and_then(|id| find(&devices.render, id)).map(|d| d.label.clone());

    // Call audio source: must be a virtual cable's capture end, and not the end paired with the mic target.
    let mut clear_source_for_good = false;
    let speaker_source = match saved.virtual_speaker_source_id.as_deref() {
        Some(id) if no_enumeration => Some(id.to_string()),
        Some(id) => match find(&devices.capture, id) {
            Some(d) if is_virtual(&d.label) => {
                let loops_back = mic_target_label
                    .as_deref()
                    .is_some_and(|m| paired_capture_label(m) == d.label.to_ascii_lowercase());
                if loops_back {
                    notes.push(format!("call audio source '{}' is the other end of the mic cable (it would echo your voice); cleared", d.label));
                    clear_source_for_good = true;
                    None
                } else {
                    Some(id.to_string())
                }
            }
            Some(d) => {
                notes.push(format!("call audio source '{}' is a real microphone, not a virtual cable; cleared", d.label));
                clear_source_for_good = true;
                None
            }
            None => {
                notes.push("saved call audio source is not connected; call audio cleanup is off".into());
                None
            }
        },
        None => None,
    };
    if clear_source_for_good {
        s.virtual_speaker_source_id = None;
        changed = true;
    }

    // Real microphone must not be a virtual cable (that would feed the cable back into itself).
    if let Some(d) = saved.physical_mic_device_id.as_deref().and_then(|id| find(&devices.capture, id)) {
        if is_virtual(&d.label) {
            notes.push(format!("saved microphone '{}' is a virtual cable; using the default microphone", d.label));
            s.physical_mic_device_id = None;
            changed = true;
        }
    }

    // Real speakers: a missing device means "system default" for this run (kept in settings for when it returns);
    // a virtual cable would feed call audio back into the mic path.
    let real_output = match saved.real_speaker_output_device_id.as_deref() {
        Some(id) if no_enumeration => Some(id.to_string()),
        Some(id) => match find(&devices.render, id) {
            Some(d) if is_virtual(&d.label) => {
                notes.push(format!("speaker output '{}' is a virtual cable; using the default speakers", d.label));
                s.real_speaker_output_device_id = None;
                changed = true;
                None
            }
            Some(_) => Some(id.to_string()),
            None => {
                notes.push("saved speaker output is not connected; using the default speakers".into());
                None
            }
        },
        None => None,
    };

    Resolved { settings: s, mic_target, speaker_source, real_output, notes, changed }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, label: &str) -> DeviceOption {
        DeviceOption { id: id.into(), label: label.into() }
    }

    fn list() -> DeviceList {
        DeviceList {
            render: vec![
                dev("realtek-out", "Speakers (Realtek(R) Audio)"),
                dev("cable-in", "CABLE Input (VB-Audio Virtual Cable)"),
                dev("epos-out", "Speakers (EPOS PC 8 USB)"),
            ],
            capture: vec![
                dev("realtek-mic", "Microphone Array (Realtek(R) Audio)"),
                dev("cable-out", "CABLE Output (VB-Audio Virtual Cable)"),
                dev("epos-mic", "Microphone (EPOS PC 8 USB)"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn the_driver_speaker_is_a_valid_call_audio_source() {
        // Listed by the engine as a capture "device" when the driver can pass call audio on (engine::enumerate_devices).
        let mut l = list();
        l.capture.push(dev("clearspeak-driver:speaker", "ClearSpeak Speaker (virtual, from the call app)"));
        let choices = call_source_choices(&l, Some("cable-in"));
        assert!(choices.iter().any(|d| d.id == "clearspeak-driver:speaker"), "offered as a call audio source");
        assert!(!only_virtual(&l.capture, false).iter().any(|d| d.id == "clearspeak-driver:speaker"), "never offered as a mic");
        let mut s = Settings::default();
        s.virtual_speaker_source_id = Some("clearspeak-driver:speaker".into());
        let r = resolve(&s, &l, None);
        assert_eq!(r.speaker_source.as_deref(), Some("clearspeak-driver:speaker"), "kept at start");
        // The driver's own render endpoint is virtual, so it is never offered as the real headset (no loop).
        assert!(is_virtual_label("ClearSpeak Speaker (ClearSpeak Virtual Audio Device)"));
    }

    #[test]
    fn the_meeting_mic_is_the_other_end_of_the_chosen_virtual_device() {
        let mut l = list();
        l.render.push(dev("clearspeak-out", "Speakers (ClearSpeak Virtual Audio Device)"));
        l.capture.push(dev("clearspeak-mic", "ClearSpeak Microphone (ClearSpeak Virtual Audio Device)"));
        assert_eq!(meeting_mic_label(&l, "clearspeak-out").as_deref(), Some("ClearSpeak Microphone (ClearSpeak Virtual Audio Device)"));
        assert_eq!(meeting_mic_label(&l, "cable-in").as_deref(), Some("CABLE Output (VB-Audio Virtual Cable)"));
        assert_eq!(meeting_mic_label(&l, "missing"), None);
        let choices = virtual_target_choices(&l.render);
        let ours = choices.iter().find(|d| d.id == "clearspeak-out").unwrap();
        assert_eq!(ours.label, "ClearSpeak Microphone input", "renamed for the picker, same id");
        assert!(choices.iter().any(|d| d.label == "CABLE Input (VB-Audio Virtual Cable)"));
        assert_eq!(preferred_mic_target(&l).map(|d| d.id).as_deref(), Some("clearspeak-out"), "ClearSpeak driver first");
        assert_eq!(preferred_mic_target(&list()).map(|d| d.id).as_deref(), Some("cable-in"), "else VB-CABLE");
    }

    #[test]
    fn cable_input_is_picked_only_when_there_is_exactly_one() {
        assert_eq!(cable_input_target(&list()).map(|d| d.id), Some("cable-in".to_string()));
        let mut two = list();
        two.render.push(dev("cable2-in", "CABLE Input 2 (VB-Audio Virtual Cable)"));
        assert!(cable_input_target(&two).is_none(), "ambiguous: no safe pick");
        let mut none = list();
        none.render.retain(|d| d.id != "cable-in");
        assert!(cable_input_target(&none).is_none());
    }

    #[test]
    fn physical_mic_as_call_source_is_cleared() {
        let mut s = Settings::default();
        s.virtual_speaker_source_id = Some("realtek-mic".into());
        let r = resolve(&s, &list(), None);
        assert!(r.speaker_source.is_none());
        assert!(r.changed && r.settings.virtual_speaker_source_id.is_none());
    }

    #[test]
    fn other_end_of_the_mic_cable_is_not_a_call_source() {
        let mut s = Settings::default();
        s.virtual_mic_target_id = Some("cable-in".into());
        s.virtual_speaker_source_id = Some("cable-out".into());
        assert!(resolve(&s, &list(), None).speaker_source.is_none());
    }

    #[test]
    fn a_separate_cable_is_a_valid_call_source() {
        let mut devices = list();
        devices.render.push(dev("b-in", "CABLE-B Input (VB-Audio Cable B)"));
        devices.capture.push(dev("b-out", "CABLE-B Output (VB-Audio Cable B)"));
        let mut s = Settings::default();
        s.virtual_mic_target_id = Some("cable-in".into());
        s.virtual_speaker_source_id = Some("b-out".into());
        assert_eq!(resolve(&s, &devices, None).speaker_source.as_deref(), Some("b-out"));
    }

    #[test]
    fn missing_devices_fall_back_without_forgetting_the_saved_speaker() {
        let mut s = Settings::default();
        s.real_speaker_output_device_id = Some("gone".into());
        s.virtual_mic_target_id = Some("gone-too".into());
        let r = resolve(&s, &list(), Some("cable-in".into()));
        assert!(r.real_output.is_none());
        assert_eq!(r.mic_target.as_deref(), Some("cable-in"));
        assert_eq!(r.settings.real_speaker_output_device_id.as_deref(), Some("gone"));
    }

    #[test]
    fn cable_as_real_speakers_or_mic_is_cleared() {
        let mut s = Settings::default();
        s.real_speaker_output_device_id = Some("cable-in".into());
        s.physical_mic_device_id = Some("cable-out".into());
        let r = resolve(&s, &list(), None);
        assert!(r.real_output.is_none() && r.settings.physical_mic_device_id.is_none() && r.changed);
    }

    #[test]
    fn empty_enumeration_keeps_saved_choices() {
        let mut s = Settings::default();
        s.virtual_speaker_source_id = Some("x".into());
        assert_eq!(resolve(&s, &DeviceList::empty(), None).speaker_source.as_deref(), Some("x"));
    }

    fn earbuds_and_pc() -> DeviceList {
        DeviceList {
            render: vec![
                dev("bt-hp", "Headphones (realme Buds T310)"),
                dev("bt-hs-out", "Headset (realme Buds T310)"),
                dev("realtek-out", "Speakers (Realtek(R) Audio)"),
                dev("epos-out", "Speakers (EPOS PC 8 USB)"),
                dev("clearspeak-in", "Speakers (ClearSpeak Virtual Audio Device)"),
            ],
            capture: vec![
                dev("bt-mic", "Headset (realme Buds T310)"),
                dev("realtek-mic", "Microphone Array (Realtek(R) Audio)"),
                dev("epos-mic", "Microphone (EPOS PC 8 USB)"),
                dev("clearspeak-mic", "Microphone (ClearSpeak Virtual Audio Device)"),
                dev("cable-out", "CABLE Output (VB-Audio Virtual Cable)"),
            ],
            ..DeviceList::empty()
        }
    }

    #[test]
    fn earbud_mic_is_bluetooth_usb_and_built_in_are_not() {
        let d = earbuds_and_pc();
        assert!(is_bluetooth_mic("Headset (realme Buds T310)", &d));
        assert!(is_bluetooth_mic("Headset (Galaxy Buds2 Hands-Free AG Audio)", &DeviceList::empty()));
        assert!(!is_bluetooth_mic("Microphone (EPOS PC 8 USB)", &d));
        assert!(!is_bluetooth_mic("Microphone Array (Realtek(R) Audio)", &d));
        // A wired "Headset" with no matching "Headphones" output is not Bluetooth.
        assert!(!is_bluetooth_mic("Headset (USB Audio Device)", &d));
    }

    #[test]
    fn talk_mic_prefers_usb_then_built_in_never_bluetooth_or_virtual() {
        let mut d = earbuds_and_pc();
        assert_eq!(talk_mic_instead(&d).map(|m| m.id).as_deref(), Some("epos-mic"));
        d.capture.retain(|m| m.id != "epos-mic");
        assert_eq!(talk_mic_instead(&d).map(|m| m.id).as_deref(), Some("realtek-mic"));
        d.capture.retain(|m| m.id != "realtek-mic");
        assert_eq!(talk_mic_instead(&d), None, "only the earbuds and virtual mics are left");
    }
}
