//! This PC's identity for license tokens: MachineGuid, BIOS serial, processor id and (if readable) the system disk serial.
//! Read once, in the background (it asks Windows through PowerShell, which takes a second or two), then cached. Only the
//! hashes made by `license::device` ever leave this module's callers; the raw values are never shown, stored or sent.

use license::device::{self, Components};
use std::sync::OnceLock;

static COMPONENTS: OnceLock<Components> = OnceLock::new();

/// Starts reading the hardware values on a background thread (call once at start-up; harmless to call again).
pub fn start_reading() {
    if COMPONENTS.get().is_some() {
        return;
    }
    std::thread::spawn(|| {
        let _ = COMPONENTS.set(read());
    });
}

/// The values, or `None` while they are still being read.
pub fn components() -> Option<&'static Components> {
    COMPONENTS.get()
}

/// The per-component texts of this PC for `license::token::verify` (empty while still reading).
pub fn texts() -> Vec<String> {
    components().map(device::texts).unwrap_or_default()
}

/// The Device Code to show the user (None while still reading).
pub fn code() -> Option<String> {
    components().map(device::device_code)
}

#[cfg(windows)]
fn read() -> Components {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // One PowerShell call: MachineGuid from the registry, the rest from the system's own hardware inventory.
    let script = "$ErrorActionPreference='SilentlyContinue';\
        $g=(Get-ItemProperty 'HKLM:\\SOFTWARE\\Microsoft\\Cryptography').MachineGuid;\
        $b=(Get-CimInstance Win32_BIOS).SerialNumber;\
        $c=(Get-CimInstance Win32_Processor | Select-Object -First 1).ProcessorId;\
        $d=(Get-CimInstance Win32_DiskDrive | Sort-Object Index | Select-Object -First 1).SerialNumber;\
        @{guid=\"$g\";bios=\"$b\";cpu=\"$c\";disk=\"$d\"} | ConvertTo-Json -Compress";
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) if o.status.success() => parse(&String::from_utf8_lossy(&o.stdout)),
        _ => Components::default(),
    }
}

#[cfg(not(windows))]
fn read() -> Components {
    Components::default()
}

/// The JSON the script prints -> components. Empty strings count as missing (the disk is then left out of the code).
fn parse(json: &str) -> Components {
    let v: serde_json::Value = serde_json::from_str(json.trim()).unwrap_or(serde_json::Value::Null);
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    Components { guid: get("guid"), bios: get("bios"), cpu: get("cpu"), disk: get("disk") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_script_output_and_treats_empty_values_as_missing() {
        let c = parse(r#"{"guid":"abc","bios":"SN1","cpu":"BFEB","disk":""}"#);
        assert_eq!(c.guid.as_deref(), Some("abc"));
        assert_eq!(c.disk, None, "an unreadable disk is left out of the code");
        assert_eq!(parse("garbage"), Components::default());
        assert_eq!(device::texts(&c).len(), 3);
    }

    /// Reads the real PC (a second or two, needs PowerShell): `cargo test -p clearnai-app --lib device_id -- --ignored --nocapture`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn reads_this_pc() {
        let c = read();
        println!("guid present: {}, bios: {:?}, cpu present: {}, disk present: {}", c.guid.is_some(), c.bios.as_deref().map(|b| b.len()), c.cpu.is_some(), c.disk.is_some());
        println!("device code: {}", device::device_code(&c));
        assert!(c.guid.is_some(), "MachineGuid must be readable");
    }
}
