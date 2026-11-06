//! Offline license keys.
//!
//! A key is `CLNAI-<payload>.<signature>`, both URL-safe base64. The payload is JSON `{name, email, exp}` (`exp` = unix
//! seconds, 0 = never expires) and the signature is Ed25519 over the payload bytes. The app only ever holds the public
//! key, so it can verify a key but nobody can make one without the owner's private key.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

const PREFIX: &str = "CLNAI-";

/// What a valid key says about its owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Licensee {
    pub name: String,
    pub email: String,
    /// Unix seconds the key stops working; 0 = never.
    pub exp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseError {
    /// Not in the `CLNAI-<payload>.<signature>` shape, or the parts are not valid base64/JSON.
    Malformed,
    /// The signature does not match: the key was altered or was not made by the owner.
    BadSignature,
    /// A valid key whose expiry date has passed.
    Expired(Licensee),
}

impl std::fmt::Display for LicenseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LicenseError::Malformed => write!(f, "That is not a ClearNAI license key."),
            LicenseError::BadSignature => write!(f, "This license key is not valid."),
            LicenseError::Expired(l) => write!(f, "This license key expired (it was issued to {}).", l.name),
        }
    }
}

/// Checks `key` against `public_key` at `now` (unix seconds).
pub fn verify(key: &str, public_key: &[u8; 32], now: u64) -> Result<Licensee, LicenseError> {
    let rest = key.trim().strip_prefix(PREFIX).ok_or(LicenseError::Malformed)?;
    let (payload_b64, sig_b64) = rest.split_once('.').ok_or(LicenseError::Malformed)?;
    let payload = B64.decode(payload_b64).map_err(|_| LicenseError::Malformed)?;
    let sig_bytes: [u8; 64] = B64.decode(sig_b64).map_err(|_| LicenseError::Malformed)?.try_into().map_err(|_| LicenseError::Malformed)?;
    let licensee: Licensee = serde_json::from_slice(&payload).map_err(|_| LicenseError::Malformed)?;
    let vk = VerifyingKey::from_bytes(public_key).map_err(|_| LicenseError::BadSignature)?;
    vk.verify(&payload, &Signature::from_bytes(&sig_bytes)).map_err(|_| LicenseError::BadSignature)?;
    if licensee.exp != 0 && now >= licensee.exp {
        return Err(LicenseError::Expired(licensee));
    }
    Ok(licensee)
}

/// Makes a key (used by `licensegen`; the app never calls this).
pub fn sign(licensee: &Licensee, secret: &[u8; 32]) -> String {
    let payload = serde_json::to_vec(licensee).expect("licensee serialises");
    let sig = SigningKey::from_bytes(secret).sign(&payload);
    format!("{PREFIX}{}.{}", B64.encode(&payload), B64.encode(sig.to_bytes()))
}

/// The public half of a secret key.
pub fn public_key_of(secret: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(secret).verifying_key().to_bytes()
}

/// Where a user stands: licensed, in the free trial, or neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Licensed(Licensee),
    Trial { days_left: u32 },
    TrialOver,
}

pub const TRIAL_DAYS: u64 = 14;
const DAY: u64 = 86_400;

/// `key` empty or invalid means "no license": the 14-day trial counts from `first_run` (unix seconds, 0 = not started).
pub fn status(key: &str, public_key: &[u8; 32], first_run: u64, now: u64) -> Status {
    if let Ok(l) = verify(key, public_key, now) {
        return Status::Licensed(l);
    }
    if first_run == 0 {
        return Status::Trial { days_left: TRIAL_DAYS as u32 };
    }
    let end = first_run + TRIAL_DAYS * DAY;
    if now < end {
        Status::Trial { days_left: ((end - now).div_ceil(DAY)) as u32 }
    } else {
        Status::TrialOver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [7u8; 32];

    fn licensee(exp: u64) -> Licensee {
        Licensee { name: "Asha Rao".into(), email: "asha@example.com".into(), exp }
    }

    #[test]
    fn a_signed_key_verifies_and_returns_the_owner() {
        let pk = public_key_of(&SECRET);
        let key = sign(&licensee(0), &SECRET);
        assert_eq!(verify(&key, &pk, 1_000_000), Ok(licensee(0)));
        assert_eq!(verify(&format!("  {key}\n"), &pk, 1), Ok(licensee(0)), "pasted whitespace is fine");
    }

    #[test]
    fn a_changed_name_or_a_foreign_key_is_rejected() {
        let pk = public_key_of(&SECRET);
        let key = sign(&licensee(0), &SECRET);
        let other = sign(&licensee(0), &[9u8; 32]);
        assert_eq!(verify(&other, &pk, 1), Err(LicenseError::BadSignature));
        // Swap the payload for another owner's payload but keep this signature.
        let forged_payload = B64.encode(serde_json::to_vec(&Licensee { name: "Eve".into(), email: "e@x".into(), exp: 0 }).unwrap());
        let sig = key.split_once('.').unwrap().1;
        assert_eq!(verify(&format!("{PREFIX}{forged_payload}.{sig}"), &pk, 1), Err(LicenseError::BadSignature));
    }

    #[test]
    fn garbage_is_malformed_not_a_panic() {
        let pk = public_key_of(&SECRET);
        for bad in ["", "hello", "CLNAI-", "CLNAI-abc", "CLNAI-abc.def", "CLNAI-!!.!!", "clnai-x.y"] {
            assert_eq!(verify(bad, &pk, 1), Err(LicenseError::Malformed), "{bad:?}");
        }
    }

    #[test]
    fn a_key_stops_working_at_its_expiry() {
        let pk = public_key_of(&SECRET);
        let key = sign(&licensee(5_000), &SECRET);
        assert!(verify(&key, &pk, 4_999).is_ok());
        assert_eq!(verify(&key, &pk, 5_000), Err(LicenseError::Expired(licensee(5_000))));
    }

    #[test]
    fn the_trial_counts_down_from_the_first_run_and_a_key_ends_it() {
        let pk = public_key_of(&SECRET);
        assert_eq!(status("", &pk, 0, 99), Status::Trial { days_left: 14 }, "not started yet");
        let start = 1_000_000;
        assert_eq!(status("", &pk, start, start), Status::Trial { days_left: 14 });
        assert_eq!(status("", &pk, start, start + 13 * DAY + 1), Status::Trial { days_left: 1 });
        assert_eq!(status("", &pk, start, start + 14 * DAY), Status::TrialOver);
        let key = sign(&licensee(0), &SECRET);
        assert!(matches!(status(&key, &pk, start, start + 99 * DAY), Status::Licensed(_)));
        assert_eq!(status("junk", &pk, start, start + 99 * DAY), Status::TrialOver);
    }
}
