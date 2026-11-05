//! Pure, platform-independent logic behind the tabbed window: tab list, header status chip, the three profiles,
//! transcript search/format helpers. Nothing here touches iced, the engine or the OS, so it is all unit-tested.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Home,
    Microphone,
    CallAudio,
    Transcript,
    Devices,
    Advanced,
}

impl Tab {
    pub const ALL: [Tab; 6] = [Tab::Home, Tab::Microphone, Tab::CallAudio, Tab::Transcript, Tab::Devices, Tab::Advanced];

    pub const fn label(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::Microphone => "Microphone",
            Tab::CallAudio => "Call audio",
            Tab::Transcript => "Transcript",
            Tab::Devices => "Devices",
            Tab::Advanced => "Advanced",
        }
    }
}

/// The one-word state shown next to the logo (and in the compact bar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusChip {
    Muted,
    Paused,
    CallAudioNotSetUp,
    Protected,
}

impl StatusChip {
    pub const fn label(self) -> &'static str {
        match self {
            StatusChip::Muted => "Mic muted",
            StatusChip::Paused => "Paused",
            StatusChip::CallAudioNotSetUp => "Call audio not set up",
            StatusChip::Protected => "Protected",
        }
    }
}

/// Priority order: a muted mic beats everything, then "nothing is processing", then the call-audio warning.
/// `any_mic_stage_on`: at least one of noise / background voices / quiet-voice gate / studio / accent is on.
pub fn status_chip(mic_enabled: bool, any_mic_stage_on: bool, speaker_ready: bool) -> StatusChip {
    if !mic_enabled {
        StatusChip::Muted
    } else if !any_mic_stage_on {
        StatusChip::Paused
    } else if !speaker_ready {
        StatusChip::CallAudioNotSetUp
    } else {
        StatusChip::Protected
    }
}

/// A fixed combination of EXISTING switches (no new processing). Applied through the same message handlers the
/// individual controls use, so it persists and takes effect live. Accent is left as it is. The `Option` fields are
/// `None` for a profile that leaves that switch alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub name: &'static str,
    pub noise: bool,
    pub voice_gate: bool,
    pub studio: bool,
    pub studio_preset: &'static str,
    pub bvc_backend: &'static str,
    pub echo: Option<bool>,
    pub transcript: Option<bool>,
    /// The whole speaker side (call audio).
    pub call_audio: Option<bool>,
    /// "Hear my processed mic" on the speakers.
    pub monitor: Option<bool>,
    /// Point the virtual microphone at "CABLE Input" when exactly one is installed (a one-time routing step, not
    /// something the profile keeps checking).
    pub cable_mic: bool,
}

/// - Default: everything that cleans the microphone on, the transcript off, call audio and the monitor off, and the
///   virtual microphone on CABLE Input when it is there. Accent stays as it is.
/// - Office: quiet room, light touch - noise removal + Hush + Natural studio.
/// - Busy floor: many people talking nearby - adds the quiet-voice gate, studio off to save CPU.
/// - Presenting: you are the only voice that matters - noise removal + Hush + a fuller Balanced studio tone.
pub const PROFILES: [Profile; 4] = [
    Profile {
        name: "Default",
        noise: true,
        voice_gate: false,
        studio: true,
        studio_preset: "Natural",
        bvc_backend: "Hush",
        echo: Some(true),
        transcript: Some(false),
        call_audio: Some(false),
        monitor: Some(false),
        cable_mic: true,
    },
    Profile { name: "Office", noise: true, voice_gate: false, studio: true, studio_preset: "Natural", bvc_backend: "Hush", echo: None, transcript: None, call_audio: None, monitor: None, cable_mic: false },
    Profile { name: "Busy floor", noise: true, voice_gate: true, studio: false, studio_preset: "Natural", bvc_backend: "Hush", echo: None, transcript: None, call_audio: None, monitor: None, cable_mic: false },
    Profile { name: "Presenting", noise: true, voice_gate: false, studio: true, studio_preset: "Balanced", bvc_backend: "Hush", echo: None, transcript: None, call_audio: None, monitor: None, cable_mic: false },
];

pub const PROFILE_NAMES: [&str; 4] = [PROFILES[0].name, PROFILES[1].name, PROFILES[2].name, PROFILES[3].name];

pub fn profile_by_name(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name == name)
}

/// What the mic path is doing right now, for deciding whether a saved profile still describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicSnapshot {
    pub noise: bool,
    pub voice_gate: bool,
    pub studio: bool,
    pub studio_preset: &'static str,
    pub bvc_on: bool,
    pub bvc_backend: &'static str,
    pub echo: bool,
    pub transcript: bool,
    pub call_audio: bool,
    pub monitor: bool,
}

impl Profile {
    pub fn matches(&self, cur: &MicSnapshot) -> bool {
        let same = |want: Option<bool>, now: bool| want.map_or(true, |w| w == now);
        self.noise == cur.noise
            && self.voice_gate == cur.voice_gate
            && self.studio == cur.studio
            && self.studio_preset == cur.studio_preset
            && cur.bvc_on
            && self.bvc_backend == cur.bvc_backend
            && same(self.echo, cur.echo)
            && same(self.transcript, cur.transcript)
            && same(self.call_audio, cur.call_audio)
            && same(self.monitor, cur.monitor)
    }
}

/// The profile to show as selected: the saved one, but only while the switches still match it (change a switch by
/// hand and the picker goes back to its placeholder instead of claiming a profile that is no longer true).
pub fn active_profile(saved: &str, cur: &MicSnapshot) -> Option<&'static str> {
    profile_by_name(saved).filter(|p| p.matches(cur)).map(|p| p.name)
}

/// Noise reduction strengths of the mic path, in the order of `bvc_hush::STRENGTH_ATTEN_LIM_DB`.
pub const NR_STRENGTH_NAMES: [&str; 3] = ["Maximum", "Balanced", "Gentle"];

/// The label of a strength index (unknown indexes read as Maximum, which is what the engine uses for them).
pub fn nr_strength_name(strength: u8) -> &'static str {
    NR_STRENGTH_NAMES.get(strength as usize).copied().unwrap_or(NR_STRENGTH_NAMES[0])
}

pub fn nr_strength_from_name(name: &str) -> Option<u8> {
    NR_STRENGTH_NAMES.iter().position(|n| *n == name).map(|i| i as u8)
}

/// Quick choices next to the sensitivity slider: label and the gain in dB it sets.
pub const DISTANCE_NAMES: [&str; 3] = ["Close", "Normal", "Far"];

/// The label for a gain that matches a quick choice exactly; `None` (shown as "Custom") for any other slider value.
pub fn distance_name(gain_db: u8) -> Option<&'static str> {
    DISTANCE_NAMES.iter().copied().find(|n| distance_from_name(n) == Some(gain_db))
}

/// The gain in dB a quick choice sets.
pub fn distance_from_name(name: &str) -> Option<u8> {
    let i = DISTANCE_NAMES.iter().position(|n| *n == name)?;
    Some(dsp_core::distance_gain_db(i as u8) as u8)
}

/// Healthy raw mic peak for speech; the gain suggestions aim for it.
const TARGET_SPEECH_PEAK: f32 = 0.25;

/// How long "Set for my mic" listens, in Tick (250 ms) counts.
pub const CALIBRATION_TICKS: u32 = 20;

/// The gain (dB) for a measured voice: `speech_peak` is the raw mic peak while speaking normally (0..1, before any
/// gain). The gain brings it up to a healthy level, rounded to whole dB and limited to the slider's range. `None` when
/// nothing was heard (peak under 0.004), so a silent test never changes the setting.
pub fn gain_db_for_speech_peak(speech_peak: f32) -> Option<u8> {
    if speech_peak < 0.004 {
        return None;
    }
    let db = 20.0 * (TARGET_SPEECH_PEAK / speech_peak).log10();
    Some(db.round().clamp(0.0, dsp_core::MAX_GAIN_DB as f32) as u8)
}

/// Mic-distance test in progress: the loudest raw peaks seen so far (one per Tick) and how many Ticks are left.
#[derive(Debug, Clone, Default)]
pub struct Calibration {
    pub peaks: Vec<f32>,
    pub ticks_left: u32,
}

impl Calibration {
    pub fn start() -> Self {
        Self { peaks: Vec::new(), ticks_left: CALIBRATION_TICKS }
    }

    /// Record one Tick's raw peak. Returns the finished measurement (the level of the louder speech, i.e. the 80th
    /// percentile of the peaks, which ignores pauses) once the time is up.
    pub fn tick(&mut self, raw_peak: f32) -> Option<f32> {
        self.peaks.push(raw_peak);
        self.ticks_left = self.ticks_left.saturating_sub(1);
        if self.ticks_left > 0 {
            return None;
        }
        let mut sorted = self.peaks.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let i = ((sorted.len() as f32) * 0.8) as usize;
        Some(sorted[i.min(sorted.len() - 1)])
    }
}

/// Case-insensitive substring filter; an empty (or blank) query keeps every line.
pub fn filter_lines<'a>(lines: &'a [(u32, String)], query: &str) -> Vec<&'a (u32, String)> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return lines.iter().collect();
    }
    lines.iter().filter(|(_, t)| t.to_lowercase().contains(&q)).collect()
}

/// `[mm:ss] text` per line, the same shape the live transcript file uses.
pub fn format_transcript(lines: &[(u32, String)]) -> String {
    let mut out = String::new();
    for (secs, text) in lines {
        out.push_str(&format!("[{}] {}\n", asr_onnx::pipeline::fmt_time(*secs), text));
    }
    out
}

/// A problem the user should see right now (banner on every tab and in the collapsed bar) and that is also written
/// to the log once when it appears. `key` identifies the problem so it is logged once, not every tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub key: &'static str,
    pub text: String,
}

/// What the alert rules look at. Each field is only set when the matching feature is switched on.
#[derive(Debug, Default, Clone)]
pub struct AlertInput {
    /// Accent is on but fell back to the cleaned voice because the CPU could not keep up (worker real-time factor).
    pub accent_paused_rtf: Option<f32>,
    /// Accent is on but could not start (reason from the engine).
    pub accent_unavailable: Option<String>,
    /// Background voice removal is on but its model did not load.
    pub bvc_unavailable: bool,
    /// Background voice removal is on but stopped responding and was switched off for this session.
    pub bvc_stopped: bool,
    /// Processing has been over its time budget for several seconds.
    pub cpu_struggling: bool,
    /// The window lost its connection to the background engine (it reconnects by itself).
    pub engine_disconnected: bool,
    /// Accent is on, the mic distance is still Close, and the last seconds of mic input were audible but quiet.
    pub mic_too_quiet: bool,
}

/// Ticks (250 ms each) of mic input looked at before deciding the mic is too quiet for the accent.
pub const QUIET_WINDOW_TICKS: u32 = 40;

/// The loudness of the voice in a window of raw mic peaks (one per Tick), or `None` when nobody was speaking.
/// Speech is told apart from a steady noise floor by its swing: the loud peaks must stand well above the typical
/// (median) one. The 90th percentile ignores one-off clicks.
pub fn speech_level(peaks: &[f32]) -> Option<f32> {
    if peaks.is_empty() {
        return None;
    }
    let mut sorted = peaks.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    let voice = sorted[(sorted.len() * 9 / 10).min(sorted.len() - 1)];
    (voice >= 0.01 && voice >= median * 3.0).then_some(voice)
}

/// True when someone spoke during the window but the voice is much weaker than the gain set so far can bring up: the
/// signature of a far or earbud mic, where the accent model gets a weak voice.
pub fn mic_too_quiet_for_accent(peaks: &[f32], accent_on: bool, gain_db: u8) -> bool {
    accent_on
        && speech_level(peaks)
            .and_then(gain_db_for_speech_peak)
            .is_some_and(|needed| needed > gain_db.saturating_add(6))
}

/// Auto never goes above this; more than +18 dB mostly raises the noise (the slider still allows 30 for manual use).
pub const AUTO_MAX_DB: u8 = 18;

/// Auto sensitivity: the next gain for a voice measured at `voice_peak`. Moves toward the gain that gives a healthy
/// level, at most 3 dB at a time, and not at all when already within 2 dB (so it never hunts back and forth).
pub fn auto_gain_next(current_db: u8, voice_peak: f32) -> Option<u8> {
    let needed = (gain_db_for_speech_peak(voice_peak)? as i16).min(AUTO_MAX_DB as i16);
    let diff = needed - current_db as i16;
    if diff.abs() <= 2 {
        return None;
    }
    Some((current_db as i16 + diff.clamp(-3, 3)).clamp(0, dsp_core::MAX_GAIN_DB as i16) as u8)
}

pub fn alerts(i: &AlertInput) -> Vec<Alert> {
    let mut out = Vec::new();
    if i.mic_too_quiet {
        out.push(Alert {
            key: "mic-quiet",
            text: "Your microphone is quiet, so the accent will sound worse. Raise Mic sensitivity, or tap Set for my mic and speak for 5 seconds.".into(),
        });
    }
    if i.engine_disconnected {
        out.push(Alert { key: "engine-disconnected", text: "Lost contact with the ClearNAI background engine. Reconnecting; audio keeps running if the engine is still alive.".into() });
    }
    if i.bvc_unavailable {
        out.push(Alert { key: "bvc-unavailable", text: "Background voice removal could not start, so other voices are not being removed.".into() });
    }
    if i.bvc_stopped {
        out.push(Alert { key: "bvc-stopped", text: "Background voice removal stopped responding and is off for this session. Restart ClearNAI to fix it.".into() });
    }
    if let Some(rtf) = i.accent_paused_rtf {
        out.push(Alert {
            key: "accent-paused",
            text: format!("Accent is paused: this PC cannot keep up (load {rtf:.2}). You are heard in your own voice. Switch off Transcript or close other apps."),
        });
    }
    if let Some(reason) = &i.accent_unavailable {
        out.push(Alert { key: "accent-unavailable", text: format!("Accent could not start: {reason}") });
    }
    if i.cpu_struggling {
        out.push(Alert { key: "cpu", text: "This PC is struggling; audio may crackle. Try the Low CPU option in Advanced.".into() });
    }
    out
}

/// An engine warning reads "Title - details": the title is shown in bold and the details below it, starting with a
/// capital. A warning without " - " is all details.
pub fn split_warning(w: &str) -> (Option<&str>, String) {
    let (title, rest) = match w.split_once(" - ") {
        Some((a, b)) => (Some(a), b),
        None => (None, w),
    };
    let mut chars = rest.chars();
    let rest = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    };
    (title, rest)
}

/// The password that opens the full window from the quick panel. It keeps casual clicks out of the advanced
/// settings; it is not a security boundary (it sits in the binary).
const SETTINGS_PASSWORD: &str = "AdminLemonpeak";

pub fn settings_password_ok(input: &str) -> bool {
    input == SETTINGS_PASSWORD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_gain_moves_toward_the_needed_gain_in_small_steps_and_does_not_hunt() {
        // A voice at 0.025 needs 20 dB.
        assert_eq!(auto_gain_next(0, 0.025), Some(3));
        assert_eq!(auto_gain_next(18, 0.025), None, "within 2 dB: leave it alone");
        assert_eq!(auto_gain_next(21, 0.025), Some(18), "Auto stops at 18 dB");
        assert_eq!(auto_gain_next(18, 0.01), None, "needs 28 dB but Auto is capped at 18");
        assert_eq!(auto_gain_next(29, 0.25), Some(26), "a louder voice lowers the gain, 3 dB at a time");
        assert_eq!(auto_gain_next(0, 0.5), None, "never below zero");
        assert_eq!(auto_gain_next(10, 0.0), None, "no voice measured: unchanged");
        let mut db = 0u8;
        for _ in 0..20 {
            if let Some(n) = auto_gain_next(db, 0.025) {
                db = n;
            }
        }
        assert_eq!(db, 18, "settles within 2 dB of the needed 20");
    }

    #[test]
    fn noise_reduction_strengths_round_trip_and_unknown_means_maximum() {
        for (i, name) in NR_STRENGTH_NAMES.iter().enumerate() {
            assert_eq!(nr_strength_from_name(name), Some(i as u8));
            assert_eq!(nr_strength_name(i as u8), *name);
        }
        assert_eq!(nr_strength_from_name("Nope"), None);
        assert_eq!(nr_strength_name(9), "Maximum");
    }

    #[test]
    fn speech_level_ignores_a_steady_noise_floor() {
        assert_eq!(speech_level(&vec![0.02f32; 40]), None);
        let speech: Vec<f32> = (0..40).map(|i| if i % 4 == 0 { 0.03 } else { 0.005 }).collect();
        assert_eq!(speech_level(&speech), Some(0.03));
    }

    #[test]
    fn quiet_mic_warning_needs_accent_on_close_distance_and_audible_but_weak_sound() {
        // Speech at 0.03 over a floor of 0.005: 10 quiet ticks in 40.
        let quiet_speech: Vec<f32> = (0..40).map(|i| if i % 4 == 0 { 0.03 } else { 0.005 }).collect();
        assert!(mic_too_quiet_for_accent(&quiet_speech, true, 0));
        assert!(!mic_too_quiet_for_accent(&quiet_speech, false, 0), "accent off: no warning");
        assert!(!mic_too_quiet_for_accent(&quiet_speech, true, 18), "gain already raised enough");
        let noise_floor = vec![0.02f32; 40];
        assert!(!mic_too_quiet_for_accent(&noise_floor, true, 0), "steady noise without speech is not a quiet voice");
        assert!(!mic_too_quiet_for_accent(&[0.0; 40], true, 0), "silence is not a quiet mic");
        let healthy: Vec<f32> = (0..40).map(|i| if i % 4 == 0 { 0.4 } else { 0.01 }).collect();
        assert!(!mic_too_quiet_for_accent(&healthy, true, 0), "healthy level");
        let on = AlertInput { mic_too_quiet: true, ..Default::default() };
        assert!(alerts(&on).iter().any(|a| a.key == "mic-quiet"));
    }

    #[test]
    fn quiet_voices_get_more_gain_and_silence_changes_nothing() {
        assert_eq!(gain_db_for_speech_peak(0.0), None);
        assert_eq!(gain_db_for_speech_peak(0.25), Some(0));
        assert_eq!(gain_db_for_speech_peak(0.5), Some(0), "never below zero");
        assert_eq!(gain_db_for_speech_peak(0.025), Some(20));
        assert_eq!(gain_db_for_speech_peak(0.005), Some(30), "limited to the slider's range");
    }

    #[test]
    fn quick_choices_set_known_gains_and_other_values_are_custom() {
        assert_eq!(distance_from_name("Close"), Some(0));
        assert_eq!(distance_from_name("Normal"), Some(9));
        assert_eq!(distance_from_name("Far"), Some(18));
        assert_eq!(distance_name(9), Some("Normal"));
        assert_eq!(distance_name(11), None, "an in-between slider value is Custom");
        assert_eq!(distance_from_name("Nope"), None);
    }

    #[test]
    fn calibration_reports_the_speech_level_after_its_time_and_ignores_pauses() {
        let mut c = Calibration::start();
        let mut result = None;
        for i in 0..CALIBRATION_TICKS {
            // Speaking at 0.05 for most of the time, with pauses at 0.001.
            let peak = if i % 4 == 0 { 0.001 } else { 0.05 };
            result = c.tick(peak);
            if i + 1 < CALIBRATION_TICKS {
                assert!(result.is_none(), "not done before the time is up");
            }
        }
        let level = result.expect("done after the full time");
        assert!((level - 0.05).abs() < 1e-6, "pauses must not pull the level down, got {level}");
        assert_eq!(gain_db_for_speech_peak(level), Some(14));
    }

    #[test]
    fn only_the_exact_password_opens_settings() {
        assert!(settings_password_ok("AdminLemonpeak"));
        for bad in ["", "12345", "adminlemonpeak", " AdminLemonpeak", "AdminLemonpeak ", "AdminLemon"] {
            assert!(!settings_password_ok(bad), "{bad:?} must not unlock");
        }
    }

    #[test]
    fn chip_priority_is_muted_then_paused_then_call_audio_then_protected() {
        assert_eq!(status_chip(false, true, true), StatusChip::Muted);
        assert_eq!(status_chip(false, false, false), StatusChip::Muted);
        assert_eq!(status_chip(true, false, true), StatusChip::Paused);
        assert_eq!(status_chip(true, false, false), StatusChip::Paused);
        assert_eq!(status_chip(true, true, false), StatusChip::CallAudioNotSetUp);
        assert_eq!(status_chip(true, true, true), StatusChip::Protected);
    }

    #[test]
    fn chip_labels_match_the_prototype() {
        assert_eq!(StatusChip::Protected.label(), "Protected");
        assert_eq!(StatusChip::Muted.label(), "Mic muted");
        assert_eq!(StatusChip::Paused.label(), "Paused");
        assert_eq!(StatusChip::CallAudioNotSetUp.label(), "Call audio not set up");
    }

    #[test]
    fn every_profile_uses_names_the_real_handlers_accept() {
        for p in PROFILES {
            assert!(crate::shared::studio_preset_from_name(p.studio_preset).is_some(), "{}", p.name);
            assert!(tse_engine::BvcBackend::ALL.iter().any(|b| b.name() == p.bvc_backend), "{}", p.name);
            assert!(profile_by_name(p.name).is_some());
        }
        assert_eq!(PROFILE_NAMES, ["Default", "Office", "Busy floor", "Presenting"]);
    }

    #[test]
    fn profiles_are_distinct() {
        for i in 0..PROFILES.len() {
            for j in i + 1..PROFILES.len() {
                assert_ne!(PROFILES[i], PROFILES[j], "{} vs {}", PROFILES[i].name, PROFILES[j].name);
            }
        }
    }

    fn snapshot_of(p: &Profile) -> MicSnapshot {
        MicSnapshot {
            noise: p.noise,
            voice_gate: p.voice_gate,
            studio: p.studio,
            studio_preset: p.studio_preset,
            bvc_on: true,
            bvc_backend: p.bvc_backend,
            echo: p.echo.unwrap_or(true),
            transcript: p.transcript.unwrap_or(false),
            call_audio: p.call_audio.unwrap_or(false),
            monitor: p.monitor.unwrap_or(false),
        }
    }

    #[test]
    fn default_profile_turns_cleaning_on_and_everything_else_off() {
        let d = profile_by_name("Default").unwrap();
        assert!(d.noise && d.studio && d.echo == Some(true) && d.cable_mic);
        assert_eq!((d.transcript, d.call_audio, d.monitor), (Some(false), Some(false), Some(false)));
        let mut cur = snapshot_of(d);
        assert_eq!(active_profile("Default", &cur), Some("Default"));
        cur.call_audio = true; // call audio switched on by hand: no longer the Default profile
        assert_eq!(active_profile("Default", &cur), None);
        cur.call_audio = false;
        cur.transcript = true;
        assert_eq!(active_profile("Default", &cur), None);
    }

    #[test]
    fn profiles_that_leave_a_switch_alone_ignore_it() {
        let office = profile_by_name("Office").unwrap();
        let mut cur = snapshot_of(office);
        cur.transcript = true;
        cur.call_audio = true;
        cur.echo = false;
        assert_eq!(active_profile("Office", &cur), Some("Office"));
    }

    #[test]
    fn saved_profile_is_shown_only_while_it_still_matches() {
        let office = profile_by_name("Office").unwrap();
        let mut cur = snapshot_of(office);
        assert_eq!(active_profile("Office", &cur), Some("Office"));
        cur.voice_gate = true;
        assert_eq!(active_profile("Office", &cur), None);
        assert_eq!(active_profile("", &snapshot_of(office)), None);
        assert_eq!(active_profile("Nope", &snapshot_of(office)), None);
        let mut bvc_off = snapshot_of(office);
        bvc_off.bvc_on = false;
        assert_eq!(active_profile("Office", &bvc_off), None);
    }

    #[test]
    fn transcript_filter_is_case_insensitive_and_blank_keeps_all() {
        let lines = vec![(1, "Hello World".to_string()), (2, "second line".to_string()), (3, "HELLO again".to_string())];
        assert_eq!(filter_lines(&lines, "").len(), 3);
        assert_eq!(filter_lines(&lines, "   ").len(), 3);
        let hits = filter_lines(&lines, "hello");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[1].0, 3);
        assert!(filter_lines(&lines, "zzz").is_empty());
    }

    #[test]
    fn transcript_format_is_one_timestamped_line_each() {
        let lines = vec![(5, "a".to_string()), (65, "b".to_string())];
        let out = format_transcript(&lines);
        assert_eq!(out.lines().count(), 2);
        assert!(out.lines().next().unwrap().ends_with("] a"));
        assert!(out.ends_with("b\n"));
        assert_eq!(format_transcript(&[]), "");
    }

    #[test]
    fn alerts_are_empty_when_all_is_well_and_one_per_problem_otherwise() {
        assert!(alerts(&AlertInput::default()).is_empty());
        let a = alerts(&AlertInput { accent_paused_rtf: Some(0.93), bvc_stopped: true, ..Default::default() });
        let keys: Vec<_> = a.iter().map(|x| x.key).collect();
        assert_eq!(keys, vec!["bvc-stopped", "accent-paused"]);
        assert!(a[1].text.contains("0.93"));
    }

    #[test]
    fn warnings_split_into_a_bold_title_and_capitalised_details() {
        let (t, r) = split_warning("No speaker-source device selected - incoming call audio is off.");
        assert_eq!(t, Some("No speaker-source device selected"));
        assert_eq!(r, "Incoming call audio is off.");
        let (t, r) = split_warning("plain warning");
        assert_eq!((t, r.as_str()), (None, "Plain warning"));
        assert_eq!(split_warning("").1, "");
    }
}
