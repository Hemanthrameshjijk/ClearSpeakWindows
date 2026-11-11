//! Offline, device-bound license tokens (format in the license server repo's `docs/token-spec.md`).
//!
//! * [`device`]: the Device Code of a PC and the "2 of 3 components must match" rule.
//! * [`token`]: verify a token signed by the owner's key (the app holds only the public key).
//! * [`status`]: licensed, or not (never signed in / the license no longer works here).
//!
//! There is no free trial in the app: a trial is simply a short license issued by the server.

pub mod device;
pub mod token;

pub use token::{Payload, TokenError};

/// Where a copy stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Licensed(Payload),
    /// A license is stored but no longer works here (expired, made for another PC, damaged) or the clock was turned back.
    Ended,
    /// No license stored: the app asks the user to sign in.
    SignIn,
}

/// A valid token for this PC means licensed; no token means "sign in"; anything else means the license has ended.
pub fn status(token: &str, public_key: &[u8; 32], current: &[String], now: u64) -> Status {
    if token.trim().is_empty() {
        return Status::SignIn;
    }
    match token::verify(token, public_key, current, now) {
        Ok(p) => Status::Licensed(p),
        Err(_) => Status::Ended,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use device::{texts, Components};

    const DAY: u64 = 86_400;

    fn pc() -> Vec<String> {
        texts(&Components { guid: Some("g".into()), bios: Some("b".into()), cpu: Some("c".into()), disk: None })
    }

    #[test]
    fn no_token_asks_for_sign_in_a_valid_one_licenses_and_a_lapsed_one_has_ended() {
        let secret = [5u8; 32];
        let pk = token::public_key_of(&secret);
        let me = pc();
        assert_eq!(status("", &pk, &me, 99), Status::SignIn, "a new install asks for sign-in first, with no trial");
        let start = 1_000_000;
        let p = Payload { v: 1, lid: "l".into(), org: "o".into(), uid: "u".into(), email: String::new(), orgname: String::new(), name: "A".into(), dev: me.clone(), min: 2, feat: vec!["nc".into()], iat: start, exp: start + 90 * DAY };
        let t = token::sign(&p, &secret);
        assert!(matches!(status(&t, &pk, &me, start + 30 * DAY), Status::Licensed(_)));
        assert_eq!(status(&t, &pk, &me, start + 91 * DAY), Status::Ended, "an expired token does not keep the app licensed");
        assert_eq!(status("junk", &pk, &me, start), Status::Ended);
    }
}
