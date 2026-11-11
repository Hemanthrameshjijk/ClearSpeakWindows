//! Dev / pilot tool: makes ClearNAI device-bound tokens without the license server (the server is the normal way).
//! Run it on your own PC only; never ship it or commit the secret key. The production signing key lives with the
//! license server (an encrypted file made by its `scripts/newkey.py`), not here.
//!
//!   licensegen newkey <secret-file>                                  create a dev secret key, print the public key bytes
//!   licensegen pub <secret-file>                                     print the public key bytes again
//!   licensegen make <secret-file> <name> <device-code> <days> [features]   make a token (features e.g. nc,bvc,accent)
use license::{device, token};
use rand::RngCore;
use std::time::{SystemTime, UNIX_EPOCH};

fn read_secret(path: &str) -> [u8; 32] {
    let hex = std::fs::read_to_string(path).expect("cannot read the secret key file");
    let hex = hex.trim();
    assert_eq!(hex.len(), 64, "the secret key file must hold 64 hex characters");
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("secret key is not hex");
    }
    out
}

fn show_public(secret: &[u8; 32]) {
    let pk = token::public_key_of(secret);
    println!("public key (paste into app/src/license_key.rs):");
    println!("[{}]", pk.iter().map(|b| format!("0x{b:02x}")).collect::<Vec<_>>().join(", "));
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("newkey") if a.len() == 3 => {
            assert!(!std::path::Path::new(&a[2]).exists(), "{} already exists; refusing to overwrite a secret key", a[2]);
            let mut secret = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut secret);
            std::fs::write(&a[2], secret.iter().map(|b| format!("{b:02x}")).collect::<String>()).unwrap();
            println!("secret key written to {} - keep it private and back it up", a[2]);
            show_public(&secret);
        }
        Some("pub") if a.len() == 3 => show_public(&read_secret(&a[2])),
        Some("make") if a.len() >= 6 => {
            let secret = read_secret(&a[2]);
            let dev = device::parse_device_code(&a[4]).unwrap_or_else(|e| panic!("bad device code: {e:?}"));
            let days: u64 = a[5].parse().expect("days must be a number");
            let feat: Vec<String> = a.get(6).map(|f| f.split(',').map(str::to_string).collect()).unwrap_or_else(|| vec!["nc".into(), "bvc".into(), "accent".into()]);
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
            let p = token::Payload {
                v: 1,
                lid: format!("dev-{now}"),
                org: "dev".into(),
                uid: "dev".into(),
                email: String::new(), orgname: String::new(),
                name: a[3].clone(),
                dev,
                min: 2,
                feat,
                iat: now,
                exp: now + days * 86_400,
            };
            println!("{}", token::sign(&p, &secret));
        }
        _ => eprintln!("usage:\n  licensegen newkey <secret-file>\n  licensegen pub <secret-file>\n  licensegen make <secret-file> <name> <device-code> <days> [features]"),
    }
}
