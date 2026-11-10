//! Device Code: a short code for "this PC", made of one truncated hash per hardware value. Must match the server's
//! `licsrv/tokens.py` exactly (see the license server repo's `docs/token-spec.md`); `tests/vectors.json` checks it.
//! Raw serial numbers never leave the PC: only these hashes are shown, sent to the admin and stored in a token.

use sha2::{Digest, Sha256};

/// Order = position in the Device Code and in a token's `dev` list.
pub const COMPONENTS: [&str; 4] = ["guid", "bios", "cpu", "disk"];
/// guid, bios and disk can identify a PC on their own; the CPU id is shared by identical models.
const STRONG: [usize; 3] = [0, 1, 3];

/// Values some PC makers leave in the BIOS; they identify nothing, so they count as unknown.
const PLACEHOLDERS: [&str; 15] = [
    "", "unknown", "none", "n/a", "na", "default string", "to be filled by o.e.m.", "system serial number", "0", "123456789", "0123456789",
    "00000000", "ffffffff", "not specified", "not applicable",
];

/// The hardware values read from this PC (any of them may be missing).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Components {
    pub guid: Option<String>,
    pub bios: Option<String>,
    pub cpu: Option<String>,
    pub disk: Option<String>,
}

fn base32(bytes: &[u8; 5]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bits: u64 = 0;
    for b in bytes {
        bits = (bits << 8) | *b as u64;
    }
    (0..8).map(|i| ALPHABET[((bits >> (35 - 5 * i)) & 31) as usize] as char).collect()
}

/// 8-character text for one hardware value. Missing or placeholder values give the "unknown" text.
pub fn component_text(name: &str, raw: Option<&str>) -> String {
    let mut value = raw.unwrap_or("").trim().to_lowercase();
    if PLACEHOLDERS.contains(&value.as_str()) {
        value = "unknown".to_string();
    }
    let digest = Sha256::digest(format!("clearnai-device-v1|{name}|{value}").as_bytes());
    let mut five = [0u8; 5];
    five.copy_from_slice(&digest[..5]);
    base32(&five)
}

fn unknown_text(name: &str) -> String {
    component_text(name, Some("unknown"))
}

/// The per-component texts of this PC: guid, bios, cpu, and disk when it could be read (3 or 4 entries).
pub fn texts(c: &Components) -> Vec<String> {
    let mut out = vec![component_text("guid", c.guid.as_deref()), component_text("bios", c.bios.as_deref()), component_text("cpu", c.cpu.as_deref())];
    if c.disk.is_some() {
        out.push(component_text("disk", c.disk.as_deref()));
    }
    out
}

/// The code shown to the user, in groups of four ("ABCD-EFGH-...").
pub fn device_code(c: &Components) -> String {
    group(&texts(c).concat())
}

fn group(flat: &str) -> String {
    flat.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap_or("")).collect::<Vec<_>>().join("-")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceCodeError {
    BadShape,
    NoMachineId,
}

/// Device Code (however typed: dashes, spaces, any case) -> component texts. Used by the dev tool; the app only compares.
pub fn parse_device_code(code: &str) -> Result<Vec<String>, DeviceCodeError> {
    let flat: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect::<String>().to_uppercase();
    let ok = (flat.len() == 24 || flat.len() == 32) && flat.chars().all(|c| matches!(c, 'A'..='Z' | '2'..='7'));
    if !ok {
        return Err(DeviceCodeError::BadShape);
    }
    let parts: Vec<String> = flat.as_bytes().chunks(8).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
    if parts[0] == unknown_text("guid") {
        return Err(DeviceCodeError::NoMachineId);
    }
    Ok(parts)
}

/// At least `minimum` components agree and at least one of them is strong. Unknown components never match.
pub fn matches(token_devs: &[String], current: &[String], minimum: u32) -> bool {
    let mut total = 0u32;
    let mut strong = 0u32;
    for (i, (a, b)) in token_devs.iter().zip(current.iter()).enumerate() {
        let name = COMPONENTS.get(i).copied().unwrap_or("");
        if a == b && *a != unknown_text(name) {
            total += 1;
            if STRONG.contains(&i) {
                strong += 1;
            }
        }
    }
    total >= minimum && strong >= 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pc(guid: &str, bios: &str, cpu: &str) -> Components {
        Components { guid: Some(guid.into()), bios: Some(bios.into()), cpu: Some(cpu.into()), disk: None }
    }

    #[test]
    fn code_round_trips_and_has_the_documented_shape() {
        let c = pc("g1", "b1", "c1");
        let code = device_code(&c);
        assert_eq!(code.replace('-', "").len(), 24);
        assert_eq!(parse_device_code(&code).unwrap(), texts(&c));
        assert_eq!(parse_device_code(&code.to_lowercase().replace('-', " ")).unwrap(), texts(&c));
        let with_disk = Components { disk: Some("d1".into()), ..c };
        assert_eq!(device_code(&with_disk).replace('-', "").len(), 32);
    }

    #[test]
    fn bad_codes_and_a_missing_machine_id_are_refused() {
        assert_eq!(parse_device_code("hello"), Err(DeviceCodeError::BadShape));
        assert_eq!(parse_device_code("1111-1111-1111-1111-1111-1111"), Err(DeviceCodeError::BadShape));
        let no_guid = Components { guid: None, ..pc("", "b", "c") };
        assert_eq!(parse_device_code(&device_code(&no_guid)), Err(DeviceCodeError::NoMachineId));
    }

    #[test]
    fn placeholder_values_are_unknown_and_never_match() {
        assert_eq!(component_text("bios", Some("To Be Filled By O.E.M.")), unknown_text("bios"));
        assert_ne!(component_text("bios", Some("SN-1")), unknown_text("bios"));
        let a = texts(&pc("g1", "default string", "c1"));
        let b = texts(&pc("g2", "Default String", "c1"));
        assert!(!matches(&a, &b, 2));
    }

    #[test]
    fn two_components_with_one_strong_are_needed() {
        let a = texts(&pc("g1", "b1", "c1"));
        assert!(matches(&a, &a, 2));
        assert!(matches(&a, &texts(&pc("g1", "b9", "c1")), 2));
        assert!(!matches(&a, &texts(&pc("g2", "b2", "c1")), 2), "a shared CPU model alone must not match");
        assert!(!matches(&a, &texts(&pc("g1", "b9", "c9")), 2), "one component is not enough");
    }
}
