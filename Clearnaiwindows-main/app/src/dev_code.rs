//! Developer/test sign-in: typing `1111111111` on the sign-in page unlocks every feature for 365 days on that PC.
//!
//! Only in builds made with `--features dev-code`. Customer installers are built without it, and there
//! [`status`] always returns `None`, so a stored dev token is then just an invalid license ("sign in again").

/// What to type on the sign-in page.
pub const CODE: &str = "1111111111";
/// How long a dev sign-in lasts, counted from the first time the code was entered on this PC.
pub const DAYS: u64 = 365;
const PREFIX: &str = "CLNAI-DEV:";

/// True in builds that accept the dev code.
pub const ENABLED: bool = cfg!(feature = "dev-code");

/// The token to store for a dev sign-in. Re-entering the code keeps the original start, so it cannot be stretched.
pub fn token(existing: &str, now: u64) -> String {
    let start = started_at(existing).unwrap_or(now);
    format!("{PREFIX}{start}")
}

fn started_at(token: &str) -> Option<u64> {
    token.trim().strip_prefix(PREFIX)?.parse().ok()
}

/// `Some(status)` for a dev token in a dev build; `None` otherwise (the normal license check then decides).
pub fn status(token: &str, now: u64) -> Option<license::Status> {
    status_in(ENABLED, token, now)
}

fn status_in(enabled: bool, token: &str, now: u64) -> Option<license::Status> {
    if !enabled {
        return None;
    }
    let start = started_at(token)?;
    let exp = start + DAYS * 86_400;
    if now >= exp {
        return Some(license::Status::Ended);
    }
    Some(license::Status::Licensed(license::token::Payload {
        v: 1,
        lid: "dev".into(),
        org: "dev".into(),
        uid: "dev".into(),
        name: "Developer (test build)".into(),
        email: String::new(),
        orgname: "ClearNAI test".into(),
        dev: Vec::new(),
        min: 0,
        feat: vec!["nc".into(), "bvc".into(), "accent".into()],
        iat: start,
        exp,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dev_code_gives_365_days_only_in_a_dev_build() {
        let t = token("", 1_000);
        assert!(matches!(status_in(true, &t, 1_000 + 364 * 86_400), Some(license::Status::Licensed(ref p)) if p.feat.len() == 3));
        assert_eq!(status_in(true, &t, 1_000 + 365 * 86_400), Some(license::Status::Ended));
        assert_eq!(status_in(false, &t, 1_000), None, "customer builds ignore it");
        assert_eq!(status_in(true, "CLNAI1.real.token", 1_000), None, "real licenses are not touched");
        assert_eq!(token(&t, 99_999), t, "entering the code again keeps the first start");
    }
}
