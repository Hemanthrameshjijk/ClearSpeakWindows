//! VICIdial without a password: reads the agent's own VICIdial page in the browser (Chrome / Edge / Firefox expose a
//! page's text to Windows UI Automation, the accessibility interface screen readers use). Nothing is clicked or
//! changed, and nothing goes over the network.
//!
//! What is read, once a second, from the browser window whose title mentions VICIdial:
//! - the "Live Call" indicator (the agent screen shows "Live Call" while a customer is connected, "No Live Call"
//!   otherwise),
//! - the customer's number from the page's `phone_number` field.
//!
//! The VICIdial page must be the browser's visible tab: browsers only expose the tab on screen. While it is not, the
//! answer is "unknown" and a call being recorded goes on (`call_session::DIALER_LOST_SECS`).
//!
//! The exact names come from VICIdial's standard agent screen and are checked with `tools/eyebeam_probe.ps1` on an
//! agent PC; they are all in [`LIVE_NAMES`], [`IDLE_NAMES`] and [`PHONE_IDS`].

use crate::vicidial::AgentState;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Browser window titles (the visible tab's page title) that are the VICIdial agent screen.
pub const TITLE_WORDS: [&str; 2] = ["vicidial", "agent web client"];
/// Element names shown while a customer is connected / not connected.
pub const LIVE_NAMES: [&str; 1] = ["Live Call"];
pub const IDLE_NAMES: [&str; 1] = ["No Live Call"];
/// The page's customer phone field (HTML id, which browsers give as the UI Automation id).
pub const PHONE_IDS: [&str; 1] = ["phone_number"];

/// What the page shows right now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenFacts {
    pub live: bool,
    pub phone: String,
}

/// Gives each customer its own call ID (the page has none): a new one when a live call starts or the number changes.
#[derive(Default)]
pub struct Tracker {
    in_call: bool,
    calls: u64,
    phone: String,
}

impl Tracker {
    pub fn next(&mut self, facts: Option<&ScreenFacts>) -> Option<AgentState> {
        let f = facts?;
        if !f.live {
            self.in_call = false;
            self.phone.clear(); // the next customer must not be compared with this one
            return Some(AgentState::NoCustomer("no live call".into()));
        }
        let new_number = !f.phone.is_empty() && !self.phone.is_empty() && f.phone != self.phone;
        if !self.in_call || new_number {
            self.calls += 1;
            self.phone = f.phone.clone();
        }
        self.in_call = true;
        if !f.phone.is_empty() {
            self.phone = f.phone.clone();
        }
        Some(AgentState::InCall { call_id: format!("screen-{}", self.calls), phone: f.phone.clone(), parked: false })
    }
}

/// The latest reading, kept fresh by a background thread while switched on.
#[derive(Clone, Default)]
pub struct ScreenWatch(Arc<Mutex<WatchState>>);

#[derive(Default)]
struct WatchState {
    on: bool,
    running: bool,
    latest: Option<(Instant, Option<AgentState>)>,
}

impl ScreenWatch {
    pub fn set_on(&self, on: bool) {
        let mut st = self.0.lock().unwrap();
        st.on = on;
        if !on {
            st.latest = None;
        }
        if on && !st.running {
            st.running = true;
            let me = self.clone();
            let _ = std::thread::Builder::new().name("clearspeak-vicidial-screen".into()).spawn(move || me.run());
        }
    }

    /// The agent's state if the VICIdial page was read within the last few seconds; `None` = page not found.
    pub fn state(&self) -> Option<AgentState> {
        match &self.0.lock().unwrap().latest {
            Some((at, s)) if at.elapsed() < Duration::from_secs(5) => s.clone(),
            _ => None,
        }
    }

    fn run(self) {
        crate::sysutil::lower_current_thread_priority();
        let mut tracker = Tracker::default();
        let mut reader = uia::Reader::new();
        let mut last_logged = String::new();
        loop {
            let t0 = Instant::now();
            if !self.0.lock().unwrap().on {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            let facts = reader.as_mut().ok().and_then(|r| r.read());
            let state = tracker.next(facts.as_ref());
            // One log line per change (never the number).
            let line = match (&reader, &state) {
                (Err(e), _) => format!("cannot read the screen: {e}"),
                (_, None) => "VICIdial page not found (open it in the browser, on the visible tab)".into(),
                (_, Some(AgentState::InCall { .. })) => "customer connected".into(),
                (_, Some(AgentState::NoCustomer(_))) => "no customer".into(),
            };
            if line != last_logged {
                crate::log_info!("[vicidial screen] {line}");
                last_logged = line;
            }
            self.0.lock().unwrap().latest = Some((Instant::now(), state));
            std::thread::sleep(Duration::from_secs(1).saturating_sub(t0.elapsed()));
        }
    }
}

#[cfg(windows)]
mod uia {
    use super::*;
    use windows::core::BSTR;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
    use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BSTR};

    /// A string VARIANT (freed with `VariantClear` by the caller).
    fn bstr_variant(s: &str) -> VARIANT {
        let mut v = VARIANT::default();
        // SAFETY: a zeroed VARIANT is VT_EMPTY; setting the type and the matching union field makes a valid VT_BSTR.
        unsafe {
            let inner = &mut *v.Anonymous.Anonymous;
            inner.vt = VT_BSTR;
            inner.Anonymous.bstrVal = std::mem::ManuallyDrop::new(BSTR::from(s));
        }
        v
    }
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationValuePattern, TreeScope_Children, TreeScope_Descendants,
        UIA_AutomationIdPropertyId, UIA_NamePropertyId, UIA_ValuePatternId,
    };

    pub struct Reader {
        uia: IUIAutomation,
    }

    impl Reader {
        pub fn new() -> Result<Self, String> {
            // SAFETY: plain COM set-up for this thread; the object is used on this thread only.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?;
                Ok(Self { uia })
            }
        }

        fn find(&self, scope: &IUIAutomationElement, prop: windows::Win32::UI::Accessibility::UIA_PROPERTY_ID, value: &str) -> Option<IUIAutomationElement> {
            // SAFETY: COM calls on live interfaces; failures come back as errors.
            unsafe {
                let mut v = bstr_variant(value);
                let cond = self.uia.CreatePropertyCondition(prop, &v);
                let _ = VariantClear(&mut v);
                scope.FindFirst(TreeScope_Descendants, &cond.ok()?).ok()
            }
        }

        /// `None` when no browser window shows the VICIdial agent screen.
        pub fn read(&mut self) -> Option<ScreenFacts> {
            // SAFETY: as above.
            unsafe {
                let root = self.uia.GetRootElement().ok()?;
                let all = self.uia.CreateTrueCondition().ok()?;
                let windows = root.FindAll(TreeScope_Children, &all).ok()?;
                for i in 0..windows.Length().unwrap_or(0) {
                    let Ok(w) = windows.GetElement(i) else { continue };
                    let title = w.CurrentName().map(|b| b.to_string().to_lowercase()).unwrap_or_default();
                    if !TITLE_WORDS.iter().any(|t| title.contains(t)) {
                        continue;
                    }
                    let live = LIVE_NAMES.iter().any(|n| self.find(&w, UIA_NamePropertyId, n).is_some());
                    let idle = IDLE_NAMES.iter().any(|n| self.find(&w, UIA_NamePropertyId, n).is_some());
                    let phone_el = PHONE_IDS.iter().find_map(|id| self.find(&w, UIA_AutomationIdPropertyId, id));
                    if !live && !idle && phone_el.is_none() {
                        continue; // a page about VICIdial, not the agent screen
                    }
                    let phone = phone_el
                        .and_then(|e| e.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId).ok())
                        .and_then(|v| v.CurrentValue().ok())
                        .map(|b| b.to_string().trim().to_string())
                        .unwrap_or_default();
                    // Names are matched whole, so "No Live Call" never counts as "Live Call". The indicator may not
                    // change its name on every VICIdial version (only its picture), so a filled-in customer number
                    // also means a live call, unless the page says "No Live Call".
                    return Some(ScreenFacts { live: !idle && (live || !phone.is_empty()), phone });
                }
                None
            }
        }
    }
}

#[cfg(not(windows))]
mod uia {
    use super::*;
    pub struct Reader;
    impl Reader {
        pub fn new() -> Result<Self, String> {
            Err("not supported on this platform".into())
        }
        pub fn read(&mut self) -> Option<ScreenFacts> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(live: bool, phone: &str) -> Option<ScreenFacts> {
        Some(ScreenFacts { live, phone: phone.into() })
    }

    fn id(s: Option<AgentState>) -> String {
        match s {
            Some(AgentState::InCall { call_id, .. }) => call_id,
            other => format!("{other:?}"),
        }
    }

    #[test]
    fn each_customer_gets_a_new_call_id() {
        let mut t = Tracker::default();
        assert!(matches!(t.next(facts(false, "").as_ref()), Some(AgentState::NoCustomer(_))));
        let a = id(t.next(facts(true, "9198765432").as_ref()));
        assert_eq!(id(t.next(facts(true, "9198765432").as_ref())), a, "the same call while it stays live");
        assert_eq!(id(t.next(facts(true, "").as_ref())), a, "the number briefly blank is still the same call");
        t.next(facts(false, "9198765432").as_ref());
        let b = id(t.next(facts(true, "9198765432").as_ref()));
        assert_ne!(a, b, "a new live call, even from the same number (a callback)");
        let c = id(t.next(facts(true, "5551234").as_ref()));
        assert_ne!(b, c, "put straight through to the next customer");
    }

    #[test]
    fn a_number_shown_late_is_not_taken_for_the_previous_customer() {
        let mut t = Tracker::default();
        let a = id(t.next(facts(true, "111").as_ref()));
        t.next(facts(false, "").as_ref());
        let b = id(t.next(facts(true, "").as_ref())); // the page fills the number in a moment later
        assert_eq!(id(t.next(facts(true, "222").as_ref())), b, "still the same (second) call");
        assert_ne!(a, b);
    }

    #[test]
    fn no_page_is_unknown_not_no_customer() {
        let mut t = Tracker::default();
        assert_eq!(t.next(None), None);
    }
}
