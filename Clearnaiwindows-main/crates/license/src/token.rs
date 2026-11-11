//! Token v1: `CLNAI1.<base64url payload>.<base64url signature>`. The signature covers the exact payload bytes carried in
//! the token, so no JSON canonicalisation is needed. Twin of the license server's `licsrv/tokens.py`; the shared
//! `tests/vectors.json` keeps both in agreement.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::device;

pub const PREFIX: &str = "CLNAI1.";
/// A token issued "tomorrow" by a PC whose clock is a day off still works.
const SKEW_SECS: u64 = 86_400;

/// What a valid token says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payload {
    pub v: u32,
    pub lid: String,
    pub org: String,
    pub uid: String,
    pub name: String,
    /// The person's email and organisation name, as entered on the server. Optional: tokens made before they existed
    /// still verify (the signature covers whatever bytes were issued).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub orgname: String,
    pub dev: Vec<String>,
    pub min: u32,
    pub feat: Vec<String>,
    pub iat: u64,
    pub exp: u64,
}

impl Payload {
    pub fn has_feature(&self, f: &str) -> bool {
        self.feat.iter().any(|x| x == f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    Malformed,
    BadSignature,
    UnsupportedVersion,
    NotYetValid,
    /// Valid token whose time ran out (carried so the screen can still say who it was for).
    Expired(Box<Payload>),
    WrongDevice,
}

impl TokenError {
    /// The short reason the shared vectors use.
    pub fn code(&self) -> &'static str {
        match self {
            TokenError::Malformed => "malformed",
            TokenError::BadSignature => "bad signature",
            TokenError::UnsupportedVersion => "unsupported version",
            TokenError::NotYetValid => "not yet valid",
            TokenError::Expired(_) => "expired",
            TokenError::WrongDevice => "wrong device",
        }
    }
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenError::Malformed => write!(f, "That is not a ClearNAI license."),
            TokenError::BadSignature => write!(f, "This license is not valid."),
            TokenError::UnsupportedVersion => write!(f, "This license is from a newer version of ClearNAI. Update the app."),
            TokenError::NotYetValid => write!(f, "This license is not valid yet. Check that this PC's date and time are right."),
            TokenError::Expired(p) => write!(f, "This license expired (it was issued to {}). Ask your admin for a new one.", p.name),
            TokenError::WrongDevice => write!(f, "This license is for a different computer. Ask your admin for one for this PC."),
        }
    }
}

/// Checks `token` against `public_key` for the PC whose component texts are `current`, at `now` (unix seconds).
pub fn verify(token: &str, public_key: &[u8; 32], current: &[String], now: u64) -> Result<Payload, TokenError> {
    let t = token.trim();
    let rest = t.strip_prefix(PREFIX).ok_or(TokenError::Malformed)?;
    let (body_b64, sig_b64) = rest.split_once('.').ok_or(TokenError::Malformed)?;
    if sig_b64.contains('.') {
        return Err(TokenError::Malformed);
    }
    let body = B64.decode(body_b64).map_err(|_| TokenError::Malformed)?;
    let sig: [u8; 64] = B64.decode(sig_b64).map_err(|_| TokenError::Malformed)?.try_into().map_err(|_| TokenError::Malformed)?;
    let vk = VerifyingKey::from_bytes(public_key).map_err(|_| TokenError::BadSignature)?;
    vk.verify(&body, &Signature::from_bytes(&sig)).map_err(|_| TokenError::BadSignature)?;
    // Only after the signature holds do we trust the bytes enough to parse them.
    let p: Payload = serde_json::from_slice(&body).map_err(|_| TokenError::Malformed)?;
    if p.v != 1 {
        return Err(TokenError::UnsupportedVersion);
    }
    if now + SKEW_SECS < p.iat {
        return Err(TokenError::NotYetValid);
    }
    if now >= p.exp {
        return Err(TokenError::Expired(Box::new(p)));
    }
    if !device::matches(&p.dev, current, p.min) {
        return Err(TokenError::WrongDevice);
    }
    Ok(p)
}

/// Dev/pilot tool (`licensegen`): makes a token. The app never calls this.
pub fn sign(p: &Payload, secret: &[u8; 32]) -> String {
    let body = serde_json::to_vec(p).expect("payload serialises");
    let sig = SigningKey::from_bytes(secret).sign(&body);
    format!("{PREFIX}{}.{}", B64.encode(&body), B64.encode(sig.to_bytes()))
}

pub fn public_key_of(secret: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(secret).verifying_key().to_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{texts, Components};

    const SECRET: [u8; 32] = [3u8; 32];

    /// A PC whose guid and bios serial both follow `g` (the CPU model is the same for every PC here, like real life).
    fn pc(g: &str) -> Components {
        Components { guid: Some(g.into()), bios: Some(format!("sn-{g}")), cpu: Some("cpu".into()), disk: None }
    }

    fn payload(dev: Vec<String>, iat: u64, exp: u64) -> Payload {
        Payload { v: 1, lid: "l".into(), org: "o".into(), uid: "u".into(), email: String::new(), orgname: String::new(), name: "Asha".into(), dev, min: 2, feat: vec!["accent".into(), "nc".into()], iat, exp }
    }

    #[test]
    fn a_token_verifies_on_its_pc_only_and_only_in_time() {
        let pk = public_key_of(&SECRET);
        let me = texts(&pc("g1"));
        let t = sign(&payload(me.clone(), 200_000, 400_000), &SECRET);
        let p = verify(&t, &pk, &me, 250_000).unwrap();
        assert!(p.has_feature("accent") && !p.has_feature("bvc"));
        assert_eq!(verify(&t, &pk, &texts(&pc("g2")), 250_000), Err(TokenError::WrongDevice));
        assert!(matches!(verify(&t, &pk, &me, 400_000), Err(TokenError::Expired(_))));
        assert_eq!(verify(&t, &pk, &me, 200_000 - 86_401), Err(TokenError::NotYetValid));
        assert!(verify(&t, &pk, &me, 200_000 - 86_000).is_ok(), "a clock one day behind is tolerated");
    }

    #[test]
    fn a_foreign_or_edited_token_is_refused_and_garbage_does_not_panic() {
        let pk = public_key_of(&SECRET);
        let me = texts(&pc("g1"));
        let other = sign(&payload(me.clone(), 1, 9_000), &[9u8; 32]);
        assert_eq!(verify(&other, &pk, &me, 5), Err(TokenError::BadSignature));
        for bad in ["", "hello", "CLNAI1.", "CLNAI1.a", "CLNAI1.a.b", "CLNAI1.!!.!!", "CLNAI1.a.b.c", "CLNAI-x.y"] {
            assert_eq!(verify(bad, &pk, &me, 5), Err(TokenError::Malformed), "{bad:?}");
        }
    }
}
