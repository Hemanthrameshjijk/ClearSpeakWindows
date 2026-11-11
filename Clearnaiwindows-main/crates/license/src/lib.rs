//! Offline, device-bound license tokens (format in the license server repo's `docs/token-spec.md`).
//!
//! * [`device`]: the Device Code of a PC and the "2 of 3 components must match" rule.
//! * [`token`]: verify a token signed by the owner's key (the app holds only the public key).
//! * [`status`]: licensed, in the 14-day free trial, or neither.

pub mod device;
pub mod token;

pub use token::{Payload, TokenError};

/// Where a copy stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Licensed(Payload),
    Trial { days_left: u32 },
    TrialOver,
    /// No license and the free trial was never started: the app asks the user to sign in first.
    SignIn,
}

pub const TRIAL_DAYS: u64 = 14;
const DAY: u64 = 86_400;

/// A valid token for this PC means licensed. Otherwise (no token, expired, other PC, ...) the 14-day trial counts from
/// `first_run` (unix seconds); 0 = the trial was never started, so the user must sign in (or start the trial) first.
pub fn status(token: &str, public_key: &[u8; 32], current: &[String], first_run: u64, now: u64) -> Status {
    if let Ok(p) = token::verify(token, public_key, current, now) {
        return Status::Licensed(p);
    }
    if first_run == 0 {
        return Status::SignIn;
    }
    let end = first_run + TRIAL_DAYS * DAY;
    if now < end {
        Status::Trial { days_left: (end - now).div_ceil(DAY) as u32 }
    } else {
        Status::TrialOver
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use device::{texts, Components};

    fn pc() -> Vec<String> {
        texts(&Components { guid: Some("g".into()), bios: Some("b".into()), cpu: Some("c".into()), disk: None })
    }

    #[test]
    fn the_trial_counts_down_and_a_valid_token_ends_it() {
        let secret = [5u8; 32];
        let pk = token::public_key_of(&secret);
        let me = pc();
        assert_eq!(status("", &pk, &me, 0, 99), Status::SignIn, "a new install asks for sign-in first");
        let start = 1_000_000;
        assert_eq!(status("", &pk, &me, start, start + 13 * DAY + 1), Status::Trial { days_left: 1 });
        assert_eq!(status("", &pk, &me, start, start + 14 * DAY), Status::TrialOver);
        let p = Payload { v: 1, lid: "l".into(), org: "o".into(), uid: "u".into(), name: "A".into(), dev: me.clone(), min: 2, feat: vec!["nc".into()], iat: start, exp: start + 90 * DAY };
        let t = token::sign(&p, &secret);
        assert!(matches!(status(&t, &pk, &me, start, start + 30 * DAY), Status::Licensed(_)));
        assert_eq!(status(&t, &pk, &me, start, start + 91 * DAY), Status::TrialOver, "an expired token does not keep the app licensed");
        assert_eq!(status("junk", &pk, &me, start, start + 99 * DAY), Status::TrialOver);
    }
}
