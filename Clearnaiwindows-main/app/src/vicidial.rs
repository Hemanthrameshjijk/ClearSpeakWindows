//! VICIdial: whether a customer is connected to this agent right now, and the customer's number.
//!
//! With VICIdial the agent's eyeBeam usually holds one call to the dialer for the whole shift and customers are put
//! through one after another, so the call app alone cannot tell where one customer ends and the next begins. The
//! dialer knows: its Non-Agent API (`function=agent_status`) says INCALL while a customer is connected, with the
//! call's ID and the customer's number. A background thread asks once a second ([`VicidialWatch`]).
//!
//! Only this agent's status is read. Nothing is sent but the API user, its password and the agent's user ID. The
//! number is never written to the log (the log is copied to the shared drive).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// e.g. `http://dialer.local/vicidial/non_agent_api.php`
    pub url: String,
    pub api_user: String,
    pub api_pass: String,
    pub agent_user: String,
}

impl Config {
    /// `None` unless all four are filled in.
    pub fn from_settings(s: &crate::settings::Settings) -> Option<Self> {
        let c = Self {
            url: s.vicidial_url.trim().to_string(),
            api_user: s.vicidial_api_user.trim().to_string(),
            api_pass: crate::dpapi::unprotect(&s.vicidial_api_pass),
            agent_user: s.vicidial_agent_user.trim().to_string(),
        };
        (!c.url.is_empty() && !c.api_user.is_empty() && !c.api_pass.is_empty() && !c.agent_user.is_empty()).then_some(c)
    }

    pub fn request_url(&self) -> String {
        let sep = if self.url.contains('?') { '&' } else { '?' };
        format!(
            "{}{sep}source=clearspeak&function=agent_status&stage=pipe&header=YES&user={}&pass={}&agent_user={}",
            self.url,
            url_encode(&self.api_user),
            url_encode(&self.api_pass),
            url_encode(&self.agent_user)
        )
    }
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentState {
    /// A customer is connected. `call_id` is VICIdial's ID of this call (a new one = a new customer); `phone` may be
    /// empty; `parked` = the customer is on hold (VICIdial "PARK").
    InCall { call_id: String, phone: String, parked: bool },
    /// No customer: READY, PAUSED, wrap-up, the customer hung up, or the agent is not logged in. The text is
    /// VICIdial's status, for the log.
    NoCustomer(String),
}

/// Reads an `agent_status` reply (`stage=pipe&header=YES`: a header line, then a value line).
pub fn parse_agent_status(body: &str) -> Result<AgentState, String> {
    let body = body.trim();
    if body.starts_with("ERROR:") {
        // "ERROR: agent_status AGENT NOT LOGGED IN - ..." is an answer, not a failure.
        if body.contains("NOT LOGGED IN") {
            return Ok(AgentState::NoCustomer("not logged in".into()));
        }
        // The rest of the line can repeat the API user and its password ("ERROR: ...: |api|secret|BAD|"): keep only
        // the reason, before any '|', ' - ' or ':' detail.
        let reason = body.split('|').next().unwrap_or(body);
        let reason = reason.split(" - ").next().unwrap_or(reason);
        let reason = reason.trim_start_matches("ERROR:").split(':').next().unwrap_or("").trim();
        return Err(format!("ERROR: {reason}"));
    }
    let mut lines = body.lines().map(str::trim).filter(|l| !l.is_empty());
    let (Some(head), Some(vals)) = (lines.next(), lines.next()) else {
        return Err("unexpected reply from VICIdial (no status line)".into());
    };
    let names: Vec<String> = head.split('|').map(|h| h.trim().to_ascii_lowercase()).collect();
    if !names.iter().any(|n| n == "status") {
        return Err("unexpected reply from VICIdial (no status column)".into());
    }
    let vals: Vec<&str> = vals.split('|').map(str::trim).collect();
    let get = |want: &[&str]| -> String {
        names.iter().position(|n| want.contains(&n.as_str())).and_then(|i| vals.get(i)).map(|v| v.to_string()).unwrap_or_default()
    };
    let status = get(&["status"]).to_ascii_uppercase();
    let sub = get(&["real_time_sub_status"]).to_ascii_uppercase();
    let call_id = get(&["callerid", "call_id"]);
    let phone = get(&["phone_number"]);
    // INCALL with sub-status DEAD (the customer hung up) or DISPO (wrap-up): the customer is gone.
    if status == "INCALL" && sub != "DEAD" && sub != "DISPO" {
        Ok(AgentState::InCall { call_id, phone, parked: sub == "PARK" })
    } else {
        Ok(AgentState::NoCustomer(if sub.is_empty() { status } else { format!("{status} {sub}") }))
    }
}

/// The latest answer, kept fresh by a background thread once a second.
#[derive(Clone, Default)]
pub struct VicidialWatch(Arc<Mutex<WatchState>>);

#[derive(Default)]
struct WatchState {
    config: Option<Config>,
    /// The last good answer. A failed request does not replace it: one lost reply must not end a call.
    latest: Option<(Instant, AgentState)>,
    running: bool,
}

/// An answer older than this is not trusted (the dialer or the network is down).
const FRESH: Duration = Duration::from_secs(5);

impl VicidialWatch {
    /// Sets (or clears) what to ask; starts the thread the first time there is something to ask.
    pub fn set_config(&self, cfg: Option<Config>) {
        let mut st = self.0.lock().unwrap();
        if st.config != cfg {
            st.latest = None;
        }
        st.config = cfg;
        if st.config.is_some() && !st.running {
            st.running = true;
            let me = self.clone();
            let _ = std::thread::Builder::new().name("clearspeak-vicidial".into()).spawn(move || me.run());
        }
    }

    pub fn configured(&self) -> bool {
        self.0.lock().unwrap().config.is_some()
    }

    /// The agent's state if VICIdial answered within the last few seconds; `None` when not set up or not answering.
    pub fn state(&self) -> Option<AgentState> {
        match &self.0.lock().unwrap().latest {
            Some((at, s)) if at.elapsed() < FRESH => Some(s.clone()),
            _ => None,
        }
    }

    fn run(self) {
        crate::sysutil::lower_current_thread_priority();
        let mut last_logged = String::new();
        loop {
            let Some(cfg) = self.0.lock().unwrap().config.clone() else {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            };
            let t0 = Instant::now();
            let res = http_get(&cfg.request_url()).and_then(|body| parse_agent_status(&body));
            // One log line per change (never the number or the password).
            let line = match &res {
                Ok(AgentState::InCall { parked: true, .. }) => "customer on hold (park)".to_string(),
                Ok(AgentState::InCall { .. }) => "customer connected".to_string(),
                Ok(AgentState::NoCustomer(s)) => format!("no customer ({s})"),
                Err(e) => format!("not answering: {e}"),
            };
            // Second guard: never let the password reach the log, whatever the dialer sent back.
            let line = if cfg.api_pass.is_empty() {
                line
            } else {
                line.replace(&cfg.api_pass, "***").replace(&url_encode(&cfg.api_pass), "***")
            };
            if line != last_logged {
                crate::log_info!("[vicidial] {line}");
                last_logged = line;
            }
            if let Ok(state) = res {
                let mut st = self.0.lock().unwrap();
                if st.config.as_ref() == Some(&cfg) {
                    st.latest = Some((Instant::now(), state));
                }
            }
            std::thread::sleep(Duration::from_secs(1).saturating_sub(t0.elapsed()));
        }
    }
}

/// `http[s]://host[:port]/path?query` -> (secure, host, port, path with query).
pub fn split_url(url: &str) -> Result<(bool, String, u16, String), String> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err("the VICIdial address must start with http:// or https://".into());
    };
    let (hostport, path) = rest.find(['/', '?']).map_or((rest, "/".to_string()), |i| {
        let p = &rest[i..];
        (&rest[..i], if p.starts_with('?') { format!("/{p}") } else { p.to_string() })
    });
    // "user:pass@host" - the credentials are not used (the API takes them in the query).
    let hostport = hostport.rsplit_once('@').map_or(hostport, |(_, h)| h);
    let default_port = if secure { 443 } else { 80 };
    let bad_port = || "bad port in the VICIdial address".to_string();
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        // IPv6 literal: [::1] or [::1]:8080
        let end = v6.find(']').ok_or("bad IPv6 address in the VICIdial address")?;
        let after = &v6[end + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| bad_port())?,
            None if after.is_empty() => default_port,
            None => return Err(bad_port()),
        };
        (&v6[..end], port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().map_err(|_| bad_port())?),
            None => (hostport, default_port),
        }
    };
    if host.is_empty() {
        return Err("no server name in the VICIdial address".into());
    }
    Ok((secure, host.to_string(), port, path))
}

#[cfg(windows)]
fn http_get(url: &str) -> Result<String, String> {
    win::get(url)
}

#[cfg(not(windows))]
fn http_get(_url: &str) -> Result<String, String> {
    Err("not supported on this platform".into())
}

#[cfg(windows)]
mod win {
    type Handle = isize;

    #[link(name = "winhttp")]
    extern "system" {
        fn WinHttpOpen(agent: *const u16, access: u32, proxy: *const u16, bypass: *const u16, flags: u32) -> Handle;
        fn WinHttpSetTimeouts(h: Handle, resolve: i32, connect: i32, send: i32, receive: i32) -> i32;
        fn WinHttpConnect(session: Handle, server: *const u16, port: u16, reserved: u32) -> Handle;
        fn WinHttpOpenRequest(
            connect: Handle,
            verb: *const u16,
            object: *const u16,
            version: *const u16,
            referrer: *const u16,
            accept: *const *const u16,
            flags: u32,
        ) -> Handle;
        fn WinHttpSendRequest(req: Handle, headers: *const u16, headers_len: u32, opt: *const u8, opt_len: u32, total: u32, ctx: usize) -> i32;
        fn WinHttpReceiveResponse(req: Handle, reserved: *mut u8) -> i32;
        fn WinHttpQueryHeaders(req: Handle, level: u32, name: *const u16, buf: *mut u8, len: *mut u32, index: *mut u32) -> i32;
        fn WinHttpReadData(req: Handle, buf: *mut u8, to_read: u32, read: *mut u32) -> i32;
        fn WinHttpCloseHandle(h: Handle) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLastError() -> u32;
    }

    const WINHTTP_ACCESS_TYPE_DEFAULT_PROXY: u32 = 0;
    const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
    const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
    const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
    /// Replies are a couple of lines; anything bigger is not VICIdial.
    const MAX_BODY: usize = 64 * 1024;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct H(Handle);
    impl Drop for H {
        fn drop(&mut self) {
            if self.0 != 0 {
                // SAFETY: a handle WinHTTP returned, closed once.
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    fn check(h: Handle, what: &str) -> Result<H, String> {
        if h == 0 {
            // SAFETY: no arguments.
            Err(format!("{what} failed (error {})", unsafe { GetLastError() }))
        } else {
            Ok(H(h))
        }
    }

    fn ok(r: i32, what: &str) -> Result<(), String> {
        if r == 0 {
            // SAFETY: no arguments. 12002 = timeout, 12007 = name not resolved, 12029 = cannot connect.
            Err(format!("{what} failed (error {})", unsafe { GetLastError() }))
        } else {
            Ok(())
        }
    }

    pub fn get(url: &str) -> Result<String, String> {
        let (secure, host, port, path) = super::split_url(url)?;
        let null = std::ptr::null();
        // SAFETY: every pointer is a NUL-terminated UTF-16 string that outlives the call, or null where allowed;
        // handles are closed by `H` in reverse order of creation.
        unsafe {
            let session = check(WinHttpOpen(wide("ClearSpeak").as_ptr(), WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, null, null, 0), "open")?;
            WinHttpSetTimeouts(session.0, 2000, 2000, 2000, 3000);
            let conn = check(WinHttpConnect(session.0, wide(&host).as_ptr(), port, 0), "connect")?;
            let flags = if secure { WINHTTP_FLAG_SECURE } else { 0 };
            let req = check(
                WinHttpOpenRequest(conn.0, wide("GET").as_ptr(), wide(&path).as_ptr(), null, null, std::ptr::null(), flags),
                "request",
            )?;
            ok(WinHttpSendRequest(req.0, null, 0, std::ptr::null(), 0, 0, 0), "send")?;
            ok(WinHttpReceiveResponse(req.0, std::ptr::null_mut()), "receive")?;
            let mut code = 0u32;
            let mut len = 4u32;
            ok(
                WinHttpQueryHeaders(
                    req.0,
                    WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                    null,
                    &mut code as *mut u32 as *mut u8,
                    &mut len,
                    std::ptr::null_mut(),
                ),
                "status",
            )?;
            if code != 200 {
                return Err(format!("HTTP {code}"));
            }
            let mut body = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let mut n = 0u32;
                ok(WinHttpReadData(req.0, buf.as_mut_ptr(), buf.len() as u32, &mut n), "read")?;
                if n == 0 || body.len() > MAX_BODY {
                    break;
                }
                body.extend_from_slice(&buf[..n as usize]);
            }
            drop(req);
            drop(conn);
            drop(session);
            Ok(String::from_utf8_lossy(&body).into_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "status|callerid|lead_id|campaign_id|calls_today|full_name|user_group|user_level|pause_code|real_time_sub_status|phone_number|vendor_lead_code|session_id";

    #[test]
    fn a_connected_customer_with_number_and_call_id() {
        let r = format!("{HEAD}\nINCALL|M1126101502000123456|42|SALES|7|Asha K|AGENTS|1|||9198765432|V1|8600051\n");
        assert_eq!(
            parse_agent_status(&r),
            Ok(AgentState::InCall { call_id: "M1126101502000123456".into(), phone: "9198765432".into(), parked: false })
        );
    }

    #[test]
    fn columns_are_found_by_name_not_position() {
        let r = "phone_number|status|call_id\n5551234|INCALL|Y99\n";
        assert_eq!(parse_agent_status(r), Ok(AgentState::InCall { call_id: "Y99".into(), phone: "5551234".into(), parked: false }));
    }

    #[test]
    fn ready_paused_hung_up_and_wrap_up_are_no_customer() {
        for (status, sub) in [("READY", ""), ("PAUSED", "LUNCH"), ("INCALL", "DEAD"), ("INCALL", "DISPO"), ("QUEUE", "")] {
            let r = format!("{HEAD}\n{status}|M1|42|SALES|7|Asha K|AGENTS|1||{sub}|9198765432||\n");
            assert!(matches!(parse_agent_status(&r), Ok(AgentState::NoCustomer(_))), "{status} {sub}");
        }
    }

    #[test]
    fn a_parked_customer_is_still_the_same_call() {
        let r = format!("{HEAD}\nINCALL|M1|42|SALES|7|Asha K|AGENTS|1||PARK|9198765432||\n");
        assert!(matches!(parse_agent_status(&r), Ok(AgentState::InCall { parked: true, .. })));
    }

    #[test]
    fn errors_say_why_without_the_rest_of_the_line() {
        assert_eq!(parse_agent_status("ERROR: agent_status AGENT NOT LOGGED IN - apiuser|1001"), Ok(AgentState::NoCustomer("not logged in".into())));
        assert_eq!(parse_agent_status("ERROR: agent_status INVALID USER ID - apiuser|1001"), Err("ERROR: agent_status INVALID USER ID".into()));
        assert!(parse_agent_status("<html>login</html>").is_err());
        let e = parse_agent_status("ERROR: Invalid Username/Password: |api|secret|BAD|").unwrap_err();
        assert!(!e.contains("secret") && !e.contains("api|"), "{e}");
    }

    #[test]
    fn ipv6_user_info_and_a_bare_query() {
        assert_eq!(split_url("http://[fd00::5]:8080/api").unwrap(), (false, String::from("fd00::5"), 8080, String::from("/api")));
        assert_eq!(split_url("https://[::1]/api").unwrap().2, 443);
        assert_eq!(split_url("http://u:p@dialer/api").unwrap().1, "dialer");
        assert_eq!(split_url("http://dialer?x=1").unwrap().3, "/?x=1");
    }

    #[test]
    fn the_request_escapes_the_password() {
        let c = Config { url: "http://dialer/vicidial/non_agent_api.php".into(), api_user: "api".into(), api_pass: "p&ss w".into(), agent_user: "1001".into() };
        assert_eq!(
            c.request_url(),
            "http://dialer/vicidial/non_agent_api.php?source=clearspeak&function=agent_status&stage=pipe&header=YES&user=api&pass=p%26ss%20w&agent_user=1001"
        );
    }

    #[test]
    fn urls_split_into_server_port_and_path() {
        assert_eq!(
            split_url("http://10.0.0.5/vicidial/non_agent_api.php?x=1").unwrap(),
            (false, String::from("10.0.0.5"), 80, String::from("/vicidial/non_agent_api.php?x=1"))
        );
        assert_eq!(split_url("https://dialer.bpo.local:8443/api").unwrap(), (true, String::from("dialer.bpo.local"), 8443, String::from("/api")));
        assert!(split_url("dialer/api").is_err());
    }
}
