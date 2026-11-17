//! One block in the log per call: which app the call is in, which features were on and off when it started, every
//! switch changed during it, and at the end how long it lasted and how much of it each feature was on.
//!
//! A "call" is another app using a microphone (Teams, Zoom, a softphone, a browser call): Windows records that per
//! app under CapabilityAccessManager\ConsentStore\microphone, which works whatever device the app opened (our virtual
//! microphone included). ClearSpeak's own capture is left out.
//!
//! Runs once a second on the engine's tick thread. No call content, names or numbers are logged.

/// The app must hold the mic this long before it counts as a call (short tests and sound checks are not calls).
pub const START_AFTER_SECS: u32 = 3;
/// The mic must be free this long before the call counts as ended (apps reopen the mic when switching devices).
pub const END_AFTER_SECS: u32 = 5;

/// One switch or setting as it is right now.
#[derive(Clone, Debug, PartialEq)]
pub struct Feature {
    pub name: String,
    /// What to print ("on", "off", "American male 2", "12").
    pub value: String,
    /// `Some` for on/off features: these get the "on for N% of the call" summary at the end.
    pub on: Option<bool>,
}

impl Feature {
    pub fn switch(name: &str, on: bool) -> Self {
        Self { name: name.into(), value: if on { "on" } else { "off" }.into(), on: Some(on) }
    }

    pub fn value(name: &str, value: impl Into<String>) -> Self {
        Self { name: name.into(), value: value.into(), on: None }
    }
}

struct Call {
    apps: Vec<String>,
    secs: u32,
    last: Vec<Feature>,
    /// Seconds each on/off feature was on, in the order of `last`.
    on_secs: Vec<u32>,
    /// `secs` and `on_secs` at the last second the mic was in use: the call ends there, not when the wait ends.
    used_secs: u32,
    used_on: Vec<u32>,
}

#[derive(Default)]
pub struct CallTracker {
    call: Option<Call>,
    busy: u32,
    free: u32,
}

/// "on: a, b | off: c | accent: American male 2, mic boost dB: 12".
pub fn summary(features: &[Feature]) -> String {
    let pick = |want: Option<bool>| features.iter().filter(|f| f.on == want).map(|f| f.name.as_str()).collect::<Vec<_>>();
    let values: Vec<String> = features.iter().filter(|f| f.on.is_none()).map(|f| format!("{}: {}", f.name, f.value)).collect();
    let list = |v: Vec<&str>| if v.is_empty() { "none".to_string() } else { v.join(", ") };
    format!("on: {} | off: {} | {}", list(pick(Some(true))), list(pick(Some(false))), values.join(", "))
}

fn duration(secs: u32) -> String {
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min {} s", s / 60, s % 60),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

impl CallTracker {
    pub fn in_call(&self) -> bool {
        self.call.is_some()
    }

    /// Call once a second with the apps using a microphone now, the features as they are now and a few figures that
    /// are only printed at the start and the end (exposure, loud sounds blocked). Returns the lines to log.
    pub fn tick(&mut self, apps: &[String], features: Vec<Feature>, stats: &str) -> Vec<String> {
        let mut out = Vec::new();
        let busy_now = !apps.is_empty();
        self.busy = if busy_now { self.busy + 1 } else { 0 };
        self.free = if busy_now { 0 } else { self.free + 1 };

        match &mut self.call {
            None => {
                if self.busy >= START_AFTER_SECS {
                    out.push(format!("[call] started in {} | {}", apps.join(" + "), summary(&features)));
                    if !stats.is_empty() {
                        out.push(format!("[call] at start: {stats}"));
                    }
                    // The mic was already in use for the START_AFTER_SECS before this line.
                    let on_secs: Vec<u32> = features.iter().map(|f| f.on.unwrap_or(false) as u32 * self.busy).collect();
                    let call = Call { apps: apps.to_vec(), secs: self.busy, last: features, used_secs: self.busy, used_on: on_secs.clone(), on_secs };
                    self.call = Some(call);
                }
            }
            Some(call) => {
                if busy_now && apps != call.apps.as_slice() {
                    out.push(format!("[call] apps using the mic now: {}", apps.join(" + ")));
                    call.apps = apps.to_vec();
                }
                for f in &features {
                    match call.last.iter().position(|l| l.name == f.name) {
                        Some(i) if call.last[i].value != f.value => {
                            out.push(format!(
                                "[call] {} changed: {} -> {} ({} into the call)",
                                f.name,
                                call.last[i].value,
                                f.value,
                                duration(call.secs)
                            ));
                            call.last[i] = f.clone();
                        }
                        Some(_) => {}
                        None => {
                            call.last.push(f.clone());
                            call.on_secs.push(0);
                            call.used_on.push(0);
                        }
                    }
                }
                call.secs += 1;
                for (i, l) in call.last.iter().enumerate() {
                    if l.on == Some(true) {
                        call.on_secs[i] += 1;
                    }
                }
                if busy_now {
                    call.used_secs = call.secs;
                    call.used_on.clone_from(&call.on_secs);
                }
                if self.free >= END_AFTER_SECS {
                    out.extend(self.end(stats));
                }
            }
        }
        out
    }

    /// Ends the call now (engine quitting) and returns its summary lines; nothing when no call is going on.
    pub fn end(&mut self, stats: &str) -> Vec<String> {
        let Some(call) = self.call.take() else { return Vec::new() };
        let secs = call.used_secs.max(1);
        let (mut whole, mut part, mut never) = (Vec::new(), Vec::new(), Vec::new());
        for (i, l) in call.last.iter().enumerate() {
            if l.on.is_none() {
                continue;
            }
            let on = call.used_on.get(i).copied().unwrap_or(0);
            let pct = (on.min(secs) as f64 * 100.0 / secs as f64).round() as u32;
            match pct {
                100.. => whole.push(l.name.clone()),
                0 => never.push(l.name.clone()),
                p => part.push(format!("{} {p}%", l.name)),
            }
        }
        let list = |v: Vec<String>| if v.is_empty() { "none".to_string() } else { v.join(", ") };
        let mut out = vec![
            format!("[call] ended in {} after {}", call.apps.join(" + "), duration(secs)),
            format!(
                "[call] on the whole call: {} | on part of it: {} | off the whole call: {}",
                list(whole),
                list(part),
                list(never)
            ),
        ];
        if !stats.is_empty() {
            out.push(format!("[call] at end: {stats}"));
        }
        out
    }
}

/// "C:#Program Files#...#ms-teams.exe" -> "ms-teams.exe"; "MSTeams_8wekyb3d8bbwe" -> "MSTeams".
pub fn app_name(key: &str, packaged: bool) -> String {
    if packaged {
        key.split('_').next().unwrap_or(key).to_string()
    } else {
        key.rsplit('#').next().unwrap_or(key).to_string()
    }
}

fn is_ours(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.contains("clearspeak") || l.contains("clearnai")
}

/// Apps other than ClearSpeak that have a microphone open right now, sorted. Empty when it cannot be read.
#[cfg(windows)]
pub fn mic_users() -> Vec<String> {
    use reg::*;
    let mut out = Vec::new();
    let Some(root) = Key::open(None, r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone") else {
        return out;
    };
    for sub in root.subkeys() {
        if sub == "NonPackaged" {
            if let Some(np) = Key::open(Some(&root), &sub) {
                for exe in np.subkeys() {
                    if Key::open(Some(&np), &exe).is_some_and(|k| k.in_use()) {
                        out.push(app_name(&exe, false));
                    }
                }
            }
        } else if Key::open(Some(&root), &sub).is_some_and(|k| k.in_use()) {
            out.push(app_name(&sub, true));
        }
    }
    out.retain(|n| !is_ours(n));
    out.sort();
    out.dedup();
    out
}

#[cfg(not(windows))]
pub fn mic_users() -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
mod reg {
    const HKEY_CURRENT_USER: isize = 0x8000_0001u32 as i32 as isize;
    const KEY_READ: u32 = 0x20019;
    const ERROR_SUCCESS: i32 = 0;

    #[link(name = "advapi32")]
    extern "system" {
        fn RegOpenKeyExW(hkey: isize, sub_key: *const u16, options: u32, sam: u32, out: *mut isize) -> i32;
        fn RegEnumKeyExW(
            hkey: isize,
            index: u32,
            name: *mut u16,
            name_len: *mut u32,
            reserved: *mut u32,
            class: *mut u16,
            class_len: *mut u32,
            last_write: *mut u64,
        ) -> i32;
        fn RegQueryValueExW(hkey: isize, name: *const u16, reserved: *mut u32, ty: *mut u32, data: *mut u8, len: *mut u32) -> i32;
        fn RegCloseKey(hkey: isize) -> i32;
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub struct Key(isize);

    impl Key {
        pub fn open(parent: Option<&Key>, path: &str) -> Option<Key> {
            let mut h = 0isize;
            let p = wide(path);
            // SAFETY: `p` is NUL-terminated and `h` receives the handle, closed in `Drop`.
            let rc = unsafe { RegOpenKeyExW(parent.map_or(HKEY_CURRENT_USER, |k| k.0), p.as_ptr(), 0, KEY_READ, &mut h) };
            (rc == ERROR_SUCCESS).then_some(Key(h))
        }

        pub fn subkeys(&self) -> Vec<String> {
            let mut out = Vec::new();
            let mut buf = vec![0u16; 1024];
            for i in 0.. {
                let mut len = buf.len() as u32;
                // SAFETY: `buf` holds `len` u16s; the optional out-parameters are null.
                let rc = unsafe {
                    RegEnumKeyExW(
                        self.0,
                        i,
                        buf.as_mut_ptr(),
                        &mut len,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                };
                if rc != ERROR_SUCCESS {
                    break; // ERROR_NO_MORE_ITEMS, or a name too long to be an app
                }
                out.push(String::from_utf16_lossy(&buf[..len as usize]));
            }
            out
        }

        fn qword(&self, name: &str) -> Option<u64> {
            let n = wide(name);
            let mut v = 0u64;
            let mut len = 8u32;
            // SAFETY: `v` is 8 bytes and `len` says so.
            let rc = unsafe {
                RegQueryValueExW(self.0, n.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), &mut v as *mut u64 as *mut u8, &mut len)
            };
            (rc == ERROR_SUCCESS && len == 8).then_some(v)
        }

        /// Windows sets LastUsedTimeStart when the app opens the mic and LastUsedTimeStop (0 while open) when it closes it.
        pub fn in_use(&self) -> bool {
            self.qword("LastUsedTimeStart").unwrap_or(0) > 0 && self.qword("LastUsedTimeStop") == Some(0)
        }
    }

    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: the handle came from RegOpenKeyExW and is closed once.
            unsafe { RegCloseKey(self.0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feats(bvc: bool, accent: &str) -> Vec<Feature> {
        vec![
            Feature::switch("mic voices", bvc),
            Feature::switch("caller noise", false),
            Feature::switch("accent", accent != "off"),
            Feature::value("accent voice", accent),
        ]
    }

    fn run(t: &mut CallTracker, apps: &[&str], secs: u32, f: &[Feature]) -> Vec<String> {
        let apps: Vec<String> = apps.iter().map(|s| s.to_string()).collect();
        (0..secs).flat_map(|_| t.tick(&apps, f.to_vec(), "")).collect()
    }

    #[test]
    fn a_short_mic_use_is_not_a_call() {
        let mut t = CallTracker::default();
        assert!(run(&mut t, &["ms-teams.exe"], START_AFTER_SECS - 1, &feats(true, "off")).is_empty());
        assert!(run(&mut t, &[], 10, &feats(true, "off")).is_empty());
        assert!(!t.in_call());
    }

    #[test]
    fn logs_start_changes_and_end_with_share_of_the_call() {
        let mut t = CallTracker::default();
        let lines = run(&mut t, &["ms-teams.exe"], START_AFTER_SECS, &feats(true, "off"));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("[call] started in ms-teams.exe | on: mic voices | off: caller noise, accent"), "{}", lines[0]);
        run(&mut t, &["ms-teams.exe"], 47, &feats(true, "off"));
        let lines = run(&mut t, &["ms-teams.exe"], 50, &feats(true, "American male 2"));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("accent changed: off -> on"));
        assert!(lines[1].contains("accent voice changed: off -> American male 2"));
        let end = run(&mut t, &[], END_AFTER_SECS, &feats(true, "American male 2"));
        assert!(end[0].starts_with("[call] ended in ms-teams.exe after 1 min 40 s"), "{}", end[0]);
        assert!(end[1].contains("on the whole call: mic voices"), "{}", end[1]);
        assert!(end[1].contains("on part of it: accent 50%"), "{}", end[1]);
        assert!(end[1].contains("off the whole call: caller noise"), "{}", end[1]);
        assert!(!t.in_call());
    }

    #[test]
    fn a_brief_mic_gap_does_not_end_the_call() {
        let mut t = CallTracker::default();
        run(&mut t, &["Zoom.exe"], 10, &feats(true, "off"));
        assert!(run(&mut t, &[], END_AFTER_SECS - 1, &feats(true, "off")).is_empty());
        run(&mut t, &["Zoom.exe"], 5, &feats(true, "off"));
        assert!(t.in_call());
    }

    #[test]
    fn app_names_and_our_own_capture() {
        assert_eq!(app_name(r"C:#Program Files#WindowsApps#MSTeams#ms-teams.exe", false), "ms-teams.exe");
        assert_eq!(app_name("MSTeams_8wekyb3d8bbwe", true), "MSTeams");
        assert!(is_ours("clearspeak-engine.exe") && is_ours("clearnai-engine.exe") && !is_ours("Zoom.exe"));
    }
}
