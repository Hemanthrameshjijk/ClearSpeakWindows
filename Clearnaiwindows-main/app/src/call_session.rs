//! Automatic call recording: decides once a second when a recording starts, pauses (hold), resumes and stops, from
//! whichever signal is the best available:
//!
//! 1. **VICIdial** (set up and answering): a recording per customer the dialer puts through, named with the
//!    customer's number. Needed when the agent stays on one eyeBeam line to the dialer all shift.
//! 2. **Caller voice** (the call app's speaker goes through ClearSpeak): while a call app holds the mic, a recording
//!    starts when the caller side has sound and stops after [`CALLER_GONE_SECS`] without any. An always-open line
//!    with nobody on it is never recorded.
//! 3. **Microphone**: the call app taking the mic starts a recording; letting it go pauses it (hold) and, after the
//!    hold time, stops it (`call_log::CallTracker`).
//!
//! Pure logic, no I/O: the engine's tick thread carries out the [`Action`]s (`host.rs`).

use crate::call_log::CallEvent;
use crate::vicidial::AgentState;

/// How calls are found, picked in the Call recording tab (`Settings::record_method`). Methods 1 and 3 both give a
/// VICIdial agent state (read from the agent's browser page, or asked from the dialer's API); without one (page not
/// open, API not set up) the caller's voice and the microphone are used.
pub const METHOD_SCREEN: u8 = 1;
pub const METHOD_VOICE: u8 = 2;
pub const METHOD_API: u8 = 3;
/// eyeBeam without VICIdial: a recording per eyeBeam call, from eyeBeam opening and closing the microphone. Other
/// call apps are ignored.
pub const METHOD_EYEBEAM: u8 = 4;
pub const METHODS: [u8; 4] = [METHOD_SCREEN, METHOD_VOICE, METHOD_API, METHOD_EYEBEAM];

pub fn method_name(m: u8) -> &'static str {
    match m {
        METHOD_SCREEN => "VICIdial agent screen",
        METHOD_API => "VICIdial API",
        METHOD_EYEBEAM => "eyeBeam call (no VICIdial)",
        _ => "Caller's voice",
    }
}

/// The softphone programs "eyeBeam call" listens to (CounterPath's eyeBeam, X-Lite and Bria). `CLEARSPEAK_CALL_APPS`
/// (names separated by ',') replaces the list, for another softphone or a test.
pub fn is_softphone(app: &str) -> bool {
    let a = app.to_ascii_lowercase();
    match std::env::var("CLEARSPEAK_CALL_APPS") {
        Ok(list) => list.split(',').map(|s| s.trim().to_ascii_lowercase()).any(|s| !s.is_empty() && a.contains(&s)),
        Err(_) => ["eyebeam", "xlite", "x-lite", "bria", "counterpath"].iter().any(|s| a.contains(s)),
    }
}

/// Caller side silent this long (no speech, no hold music): that customer is gone.
pub const CALLER_GONE_SECS: u32 = 60;
/// VICIdial stopped answering during a customer's call: the recording is kept this long (or until the call app lets
/// go of the mic), so a network hiccup does not cut the call in two.
pub const DIALER_LOST_SECS: u32 = 120;

/// Seconds from before the start that go into the recording. The dialer knows the exact moment (only its 1 s polling
/// delay to cover); the mic is recognised 3 s late and the caller's first sound comes after the agent's greeting.
pub const PRE_ROLL_VICIDIAL_S: usize = 2;
pub const PRE_ROLL_S: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// `app`: where the call is (for the file name); `number`: the customer's number when known; `pre_roll_secs`: how
    /// much from before this moment the recording keeps.
    Start { app: String, number: Option<String>, pre_roll_secs: usize },
    Pause,
    Resume,
    Stop,
}

/// What is known this second.
pub struct Inputs<'a> {
    pub record_calls: bool,
    /// From `CallTracker::take_events` this second.
    pub mic_events: &'a [CallEvent],
    /// A call app holds the mic (or did until moments ago and may come back: on hold).
    pub mic_in_call: bool,
    pub mic_apps: &'a [String],
    /// VICIdial's fresh answer; `None` = not set up or not answering.
    pub vicidial: Option<&'a AgentState>,
    /// Call audio reaches ClearSpeak.
    pub caller_routed: bool,
    pub caller_heard_secs_ago: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Vicidial,
    CallerVoice,
    Mic,
}

struct Open {
    source: Source,
    call_id: Option<String>,
    paused: bool,
    /// Caller-voice recordings: seconds without caller sound, counted only while not on hold (reset on resume).
    quiet: u32,
}

/// VICIdial answered this recently: its line is open all shift, so the fallbacks must not start an idle-line
/// recording just because the page was hidden for a moment.
const VICIDIAL_RECENT_SECS: u32 = 600;

#[derive(Default)]
pub struct Session {
    open: Option<Open>,
    /// Seconds VICIdial has not answered during a VICIdial recording.
    dialer_lost: u32,
    /// Seconds since VICIdial last answered (`None` = not this session).
    vicidial_seen_ago: Option<u32>,
}

impl Session {
    pub fn recording(&self) -> bool {
        self.open.is_some()
    }

    /// Ends a recording at once (the way calls are found was changed mid-call).
    pub fn stop_now(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        self.stop(&mut out);
        out
    }

    pub fn tick(&mut self, i: &Inputs) -> Vec<Action> {
        let mut out = Vec::new();
        if !i.record_calls {
            self.stop(&mut out);
            self.vicidial_seen_ago = None;
            return out;
        }
        if let Some(state) = i.vicidial {
            self.dialer_lost = 0;
            self.vicidial_seen_ago = Some(0);
            self.vicidial(state, &mut out);
            return out;
        }
        if let Some(s) = self.vicidial_seen_ago.as_mut() {
            *s += 1;
        }
        let vicidial_recent = self.vicidial_seen_ago.is_some_and(|s| s < VICIDIAL_RECENT_SECS);
        let mic_ended = i.mic_events.iter().any(|e| matches!(e, CallEvent::Ended { .. }));
        // A VICIdial recording whose dialer stopped answering (or whose page is hidden): kept until the call app lets
        // go of the line (a real end, not "no mic signal": that may not be readable on every PC). After a while
        // without an answer it is handed to the microphone rules, which end it with the call app.
        if self.open.as_ref().is_some_and(|o| o.source == Source::Vicidial) {
            self.dialer_lost += 1;
            if mic_ended {
                self.stop(&mut out);
            } else if self.dialer_lost >= DIALER_LOST_SECS {
                if let Some(o) = self.open.as_mut() {
                    o.source = Source::Mic; // keeps its call ID: if VICIdial comes back with it, the same file goes on
                }
            }
            return out;
        }
        let app = || i.mic_apps.first().map(|a| a.trim_end_matches(".exe").trim_end_matches(".EXE").to_string()).unwrap_or_else(|| "call".into());

        if i.caller_routed && self.open.as_ref().is_none_or(|o| o.source == Source::CallerVoice) {
            let caller_now = i.caller_heard_secs_ago.is_some_and(|s| s <= 1);
            if let Some(o) = self.open.as_mut() {
                o.quiet = if caller_now || o.paused { 0 } else { o.quiet + 1 };
            }
            match &self.open {
                None if i.mic_in_call && caller_now && !vicidial_recent => self.start(Source::CallerVoice, app(), None, &mut out),
                Some(o) if mic_ended || (!i.mic_in_call && !o.paused) || o.quiet >= CALLER_GONE_SECS => self.stop(&mut out),
                _ => self.hold_events(i.mic_events, &mut out),
            }
            return out;
        }

        for e in i.mic_events {
            match e {
                CallEvent::Started { .. } if self.open.is_none() && !vicidial_recent => self.start(Source::Mic, app(), None, &mut out),
                CallEvent::Ended { .. } => self.stop(&mut out),
                _ => self.hold_events(std::slice::from_ref(e), &mut out),
            }
        }
        // The mic call was already going when recording was switched on: start now.
        if self.open.is_none() && i.mic_in_call && i.mic_events.is_empty() && !i.mic_apps.is_empty() && !vicidial_recent {
            self.start(Source::Mic, app(), None, &mut out);
        }
        out
    }

    fn vicidial(&mut self, state: &AgentState, out: &mut Vec<Action>) {
        match state {
            AgentState::InCall { call_id, phone, parked } => {
                // The same call ID is the same call, also after the dialer was lost for a while.
                let same = self.open.as_ref().is_some_and(|o| o.call_id.as_ref() == Some(call_id));
                if same {
                    if let Some(o) = self.open.as_mut() {
                        o.source = Source::Vicidial;
                    }
                } else {
                    self.stop(out);
                    let number = (!phone.is_empty()).then(|| phone.clone());
                    self.start(Source::Vicidial, "VICIdial".into(), number, out);
                    if let Some(o) = self.open.as_mut() {
                        o.call_id = Some(call_id.clone());
                    }
                }
                self.set_paused(*parked, out);
            }
            AgentState::NoCustomer(_) => self.stop(out),
        }
    }

    fn hold_events(&mut self, events: &[CallEvent], out: &mut Vec<Action>) {
        for e in events {
            match e {
                CallEvent::Held => self.set_paused(true, out),
                CallEvent::Resumed => self.set_paused(false, out),
                _ => {}
            }
        }
    }

    fn set_paused(&mut self, paused: bool, out: &mut Vec<Action>) {
        if let Some(o) = self.open.as_mut() {
            if o.paused != paused {
                o.paused = paused;
                out.push(if paused { Action::Pause } else { Action::Resume });
            }
        }
    }

    fn start(&mut self, source: Source, app: String, number: Option<String>, out: &mut Vec<Action>) {
        self.open = Some(Open { source, call_id: None, paused: false, quiet: 0 });
        let pre_roll_secs = if source == Source::Vicidial { PRE_ROLL_VICIDIAL_S } else { PRE_ROLL_S };
        out.push(Action::Start { app, number, pre_roll_secs });
    }

    fn stop(&mut self, out: &mut Vec<Action>) {
        if self.open.take().is_some() {
            out.push(Action::Stop);
        }
    }
}

/// File-name part for the app: letters, digits and '-' only ("eyeBeam.exe" -> "eyeBeam").
pub fn app_part(app: &str) -> String {
    let a = app.trim_end_matches(".exe").trim_end_matches(".EXE");
    let s: String = a.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(24).collect();
    if s.is_empty() {
        "call".into()
    } else {
        s
    }
}

/// File-name part for the number: digits and a leading '+' only; `None` if no digits.
pub fn number_part(number: &str) -> Option<String> {
    let digits: String = number.chars().filter(char::is_ascii_digit).take(20).collect();
    if digits.is_empty() {
        return None;
    }
    Some(if number.trim_start().starts_with('+') { format!("+{digits}") } else { digits })
}

/// `<stamp>_call_<app>[_<number>]`: the shared name of a call's .wav, .txt and .jsonl.
pub fn file_stem(stamp: &str, app: &str, number: Option<&str>) -> String {
    match number.and_then(number_part) {
        Some(n) => format!("{stamp}_call_{}_{n}", app_part(app)),
        None => format!("{stamp}_call_{}", app_part(app)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apps(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    struct T {
        s: Session,
        record: bool,
        in_call: bool,
        apps: Vec<String>,
        vici: Option<AgentState>,
        routed: bool,
        heard: Option<u32>,
    }

    impl T {
        fn new() -> Self {
            Self { s: Session::default(), record: true, in_call: false, apps: vec![], vici: None, routed: false, heard: None }
        }
        fn tick(&mut self, events: &[CallEvent]) -> Vec<Action> {
            self.s.tick(&Inputs {
                record_calls: self.record,
                mic_events: events,
                mic_in_call: self.in_call,
                mic_apps: &self.apps,
                vicidial: self.vici.as_ref(),
                caller_routed: self.routed,
                caller_heard_secs_ago: self.heard,
            })
        }
    }

    fn start(app: &str, number: Option<&str>) -> Action {
        let pre_roll_secs = if app == "VICIdial" { PRE_ROLL_VICIDIAL_S } else { PRE_ROLL_S };
        Action::Start { app: app.into(), number: number.map(String::from), pre_roll_secs }
    }

    #[test]
    fn mic_only_start_hold_resume_stop() {
        let mut t = T::new();
        t.in_call = true;
        t.apps = apps(&["eyeBeam.exe"]);
        assert_eq!(t.tick(&[CallEvent::Started { apps: t.apps.clone() }]), [start("eyeBeam", None)]);
        assert!(t.tick(&[]).is_empty());
        assert_eq!(t.tick(&[CallEvent::Held]), [Action::Pause]);
        assert_eq!(t.tick(&[CallEvent::Resumed]), [Action::Resume]);
        t.in_call = false;
        assert_eq!(t.tick(&[CallEvent::Ended { secs: 100 }]), [Action::Stop]);
        assert!(!t.s.recording());
    }

    #[test]
    fn vicidial_one_recording_per_customer_on_an_always_open_line() {
        let mut t = T::new();
        t.in_call = true; // eyeBeam holds the line to the dialer all shift
        t.apps = apps(&["eyeBeam.exe"]);
        t.vici = Some(AgentState::NoCustomer("READY".into()));
        assert!(t.tick(&[CallEvent::Started { apps: t.apps.clone() }]).is_empty(), "logged in, waiting: nothing recorded");
        t.vici = Some(AgentState::InCall { call_id: "M1".into(), phone: "+919876543210".into(), parked: false });
        assert_eq!(t.tick(&[]), [start("VICIdial", Some("+919876543210"))]);
        assert!(t.tick(&[]).is_empty());
        t.vici = Some(AgentState::InCall { call_id: "M1".into(), phone: "+919876543210".into(), parked: true });
        assert_eq!(t.tick(&[]), [Action::Pause], "parked = on hold, same file");
        t.vici = Some(AgentState::InCall { call_id: "M1".into(), phone: "+919876543210".into(), parked: false });
        assert_eq!(t.tick(&[]), [Action::Resume]);
        // Next customer put straight through: a new file.
        t.vici = Some(AgentState::InCall { call_id: "M2".into(), phone: "5551234".into(), parked: false });
        assert_eq!(t.tick(&[]), [Action::Stop, start("VICIdial", Some("5551234"))]);
        t.vici = Some(AgentState::NoCustomer("INCALL DISPO".into()));
        assert_eq!(t.tick(&[]), [Action::Stop]);
    }

    #[test]
    fn a_dialer_that_stops_answering_does_not_cut_the_call() {
        let mut t = T::new();
        t.in_call = true;
        t.apps = apps(&["eyeBeam.exe"]);
        t.vici = Some(AgentState::InCall { call_id: "M1".into(), phone: String::new(), parked: false });
        assert_eq!(t.tick(&[]), [start("VICIdial", None)]);
        t.vici = None;
        assert!(t.tick(&[]).is_empty(), "kept while the line is open");
        t.in_call = false;
        assert!(t.tick(&[]).is_empty(), "no mic signal is not the end of the call (it may not be readable)");
        assert_eq!(t.tick(&[CallEvent::Ended { secs: 50 }]), [Action::Stop]);
    }

    #[test]
    fn a_dialer_that_answers_again_continues_the_same_recording_and_one_gone_for_good_ends_it() {
        let mut t = T::new();
        let incall = AgentState::InCall { call_id: "M1".into(), phone: "5551234".into(), parked: false };
        t.vici = Some(incall.clone());
        assert_eq!(t.tick(&[]), [start("VICIdial", Some("5551234"))]);
        t.vici = None;
        for _ in 0..10 {
            assert!(t.tick(&[]).is_empty());
        }
        t.vici = Some(incall.clone());
        assert!(t.tick(&[]).is_empty(), "same call ID: the same file goes on");
        t.vici = None;
        let actions: usize = (0..DIALER_LOST_SECS + 30).map(|_| t.tick(&[]).len()).sum();
        assert_eq!(actions, 0, "a long dialer loss hands the call to the mic rules, it is not cut");
        t.vici = Some(incall);
        assert!(t.tick(&[]).is_empty(), "the dialer back with the same call: still the same file");
        t.vici = None;
        for _ in 0..DIALER_LOST_SECS {
            t.tick(&[]);
        }
        assert_eq!(t.tick(&[CallEvent::Ended { secs: 500 }]), [Action::Stop], "ended by the call app");
    }

    #[test]
    fn a_hidden_vicidial_page_does_not_start_an_idle_line_recording() {
        let mut t = T::new();
        t.in_call = true; // the all-shift line to the dialer
        t.apps = apps(&["eyeBeam.exe"]);
        t.vici = Some(AgentState::NoCustomer("READY".into()));
        assert!(t.tick(&[]).is_empty());
        t.vici = None; // the agent switched to another browser tab
        for _ in 0..60 {
            assert!(t.tick(&[]).is_empty(), "the mic fallback must not record the idle line");
        }
    }

    #[test]
    fn caller_voice_a_long_silent_hold_stays_one_recording() {
        let mut t = T::new();
        t.routed = true;
        t.in_call = true;
        t.apps = apps(&["eyeBeam.exe"]);
        t.heard = Some(0);
        assert_eq!(t.tick(&[]), [start("eyeBeam", None)]);
        assert_eq!(t.tick(&[CallEvent::Held]), [Action::Pause]);
        t.heard = Some(10);
        for s in 0..180 {
            t.heard = Some(10 + s);
            assert!(t.tick(&[]).is_empty(), "silent hold, 3 min");
        }
        assert_eq!(t.tick(&[CallEvent::Resumed]), [Action::Resume]);
        t.heard = Some(200);
        for _ in 0..CALLER_GONE_SECS - 2 {
            assert!(t.tick(&[]).is_empty(), "the quiet time counts from the resume");
        }
    }

    #[test]
    fn stop_now_ends_an_open_recording_once() {
        let mut t = T::new();
        t.in_call = true;
        t.apps = apps(&["ms-teams.exe"]);
        assert_eq!(t.tick(&[]), [start("ms-teams", None)]);
        assert_eq!(t.s.stop_now(), [Action::Stop]);
        assert!(t.s.stop_now().is_empty());
    }

    #[test]
    fn caller_voice_splits_customers_on_an_always_open_line() {
        let mut t = T::new();
        t.routed = true;
        t.in_call = true;
        t.apps = apps(&["eyeBeam.exe"]);
        t.heard = None;
        assert!(t.tick(&[CallEvent::Started { apps: t.apps.clone() }]).is_empty(), "line open, no caller yet");
        t.heard = Some(0);
        assert_eq!(t.tick(&[]), [start("eyeBeam", None)]);
        for s in 1..CALLER_GONE_SECS {
            t.heard = Some(s + 1);
            assert!(t.tick(&[]).is_empty(), "a pause in the conversation ({s} s)");
        }
        t.heard = Some(CALLER_GONE_SECS + 1);
        assert_eq!(t.tick(&[]), [Action::Stop], "the customer is gone");
        t.heard = Some(0);
        assert_eq!(t.tick(&[]), [start("eyeBeam", None)], "the next customer");
    }

    #[test]
    fn switching_recording_off_stops_and_on_again_starts_a_call_in_progress() {
        let mut t = T::new();
        t.in_call = true;
        t.apps = apps(&["Zoom.exe"]);
        assert_eq!(t.tick(&[]), [start("Zoom", None)]);
        t.record = false;
        assert_eq!(t.tick(&[]), [Action::Stop]);
        assert!(t.tick(&[]).is_empty());
        t.record = true;
        assert_eq!(t.tick(&[]), [start("Zoom", None)]);
    }

    #[test]
    fn only_counterpath_softphones_count_as_eyebeam() {
        if std::env::var("CLEARSPEAK_CALL_APPS").is_ok() {
            return; // overridden on this machine
        }
        assert!(is_softphone("eyeBeam.exe") && is_softphone("X-Lite.exe") && is_softphone("Bria.exe"));
        assert!(!is_softphone("ms-teams.exe") && !is_softphone("Zoom.exe") && !is_softphone("msedge.exe"));
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_stem("2026-11-26_10-15-02", "eyeBeam.exe", Some("+91 98765-43210")), "2026-11-26_10-15-02_call_eyeBeam_+919876543210");
        assert_eq!(file_stem("2026-11-26_10-15-02", "MSTeams", Some("anonymous")), "2026-11-26_10-15-02_call_MSTeams");
        assert_eq!(file_stem("s", "..\\evil/app", None), "s_call_evilapp");
        assert_eq!(app_part(""), "call");
    }
}
