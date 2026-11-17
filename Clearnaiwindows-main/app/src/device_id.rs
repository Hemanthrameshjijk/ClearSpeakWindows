//! This PC's identity for license tokens: MachineGuid, BIOS (system) serial, processor id and (if readable) the system
//! disk serial. Read natively (registry, SMBIOS table, CPUID, a storage query), once in the background, then cached.
//! Only the hashes made by `license::device` ever leave this module's callers; the raw values are never shown, stored
//! or sent.

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

fn read() -> Components {
    let clean = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    Components { guid: clean(imp::machine_guid()), bios: clean(imp::system_serial()), cpu: clean(cpu_id()), disk: clean(imp::disk_serial()) }
}

/// The processor id the way Windows reports it (CPUID leaf 1: EDX then EAX).
#[cfg(target_arch = "x86_64")]
fn cpu_id() -> Option<String> {
    // CPUID leaf 1 exists on every x86-64 processor.
    let r = std::arch::x86_64::__cpuid(1);
    Some(format!("{:08X}{:08X}", r.edx, r.eax))
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu_id() -> Option<String> {
    None
}

/// The "system serial number" string from an SMBIOS table (a Type 1 structure), if there is one.
/// `table` is the raw structure data after the 8-byte header Windows puts in front of it.
pub fn smbios_system_serial(table: &[u8]) -> Option<String> {
    let mut i = 0usize;
    while i + 4 <= table.len() {
        let (kind, len) = (table[i], table[i + 1] as usize);
        if len < 4 || i + len > table.len() {
            return None;
        }
        // The strings follow the formatted area, each NUL-terminated, the set ending with an extra NUL.
        let mut end = i + len;
        while end + 1 < table.len() && !(table[end] == 0 && table[end + 1] == 0) {
            end += 1;
        }
        if kind == 1 && len > 7 {
            let index = table[i + 7] as usize; // serial number: string number in the strings area
            if index > 0 {
                let strings = &table[i + len..end.min(table.len())];
                return strings.split(|b| *b == 0).nth(index - 1).map(|s| String::from_utf8_lossy(s).into_owned());
            }
            return None;
        }
        if kind == 127 {
            return None; // end-of-table
        }
        i = end + 2;
    }
    None
}

/// A NUL-terminated string at `offset` inside a storage descriptor buffer (0 = none).
pub fn string_at(buf: &[u8], offset: usize) -> Option<String> {
    if offset == 0 || offset >= buf.len() {
        return None;
    }
    let rest = &buf[offset..];
    let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

#[cfg(windows)]
mod imp {
    use super::{smbios_system_serial, string_at};
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ};
    use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RRF_SUBKEY_WOW6464KEY};
    use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, FIRMWARE_TABLE_PROVIDER};

    pub fn machine_guid() -> Option<String> {
        let mut size = 0u32;
        // SAFETY: first call only asks for the size; second call fills a buffer of that size.
        unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, w!("SOFTWARE\\Microsoft\\Cryptography"), w!("MachineGuid"), RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY, None, None, Some(&mut size))
                .ok()
                .ok()?;
            let mut buf = vec![0u16; (size as usize).div_ceil(2)];
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                w!("SOFTWARE\\Microsoft\\Cryptography"),
                w!("MachineGuid"),
                RRF_RT_REG_SZ | RRF_SUBKEY_WOW6464KEY,
                None,
                Some(buf.as_mut_ptr() as *mut _),
                Some(&mut size),
            )
            .ok()
            .ok()?;
            let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
            Some(String::from_utf16_lossy(&buf[..end]))
        }
    }

    pub fn system_serial() -> Option<String> {
        const RSMB: u32 = 0x5253_4D42; // 'RSMB'
        let provider = FIRMWARE_TABLE_PROVIDER(RSMB);
        // SAFETY: standard two-call pattern: ask for the size, then read into a buffer of that size.
        unsafe {
            let size = GetSystemFirmwareTable(provider, 0, None);
            if size < 8 {
                return None;
            }
            let mut buf = vec![0u8; size as usize];
            let got = GetSystemFirmwareTable(provider, 0, Some(&mut buf));
            if got < 8 || got as usize > buf.len() {
                return None;
            }
            // RawSMBIOSData: calling method, major, minor, revision, then a u32 length, then the structures.
            let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
            let table = buf.get(8..(8 + len).min(buf.len()))?;
            smbios_system_serial(table)
        }
    }

    pub fn disk_serial() -> Option<String> {
        use windows::Win32::System::Ioctl::{IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_PROPERTY_QUERY, StorageDeviceProperty};
        // SAFETY: opens the first physical drive for a property query only (no read access needed), then closes it.
        unsafe {
            let handle = CreateFileW(PCWSTR(w!("\\\\.\\PhysicalDrive0").0), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, Default::default(), None).ok()?;
            let _ = GENERIC_READ; // (no access rights are requested on purpose)
            let query = STORAGE_PROPERTY_QUERY { PropertyId: StorageDeviceProperty, QueryType: PropertyStandardQuery, AdditionalParameters: [0] };
            let mut out = vec![0u8; 1024];
            let mut returned = 0u32;
            let ok = DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                Some(&query as *const _ as *const _),
                std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
                Some(out.as_mut_ptr() as *mut _),
                out.len() as u32,
                Some(&mut returned),
                None,
            )
            .is_ok();
            let _ = CloseHandle(handle);
            if !ok || (returned as usize) < 28 {
                return None;
            }
            // STORAGE_DEVICE_DESCRIPTOR: SerialNumberOffset is the u32 at byte 24.
            let offset = u32::from_le_bytes([out[24], out[25], out[26], out[27]]) as usize;
            string_at(&out[..returned as usize], offset)
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn machine_guid() -> Option<String> {
        None
    }
    pub fn system_serial() -> Option<String> {
        None
    }
    pub fn disk_serial() -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny SMBIOS table: a Type 0 structure, then a Type 1 whose serial is string 2, then the end marker.
    fn table() -> Vec<u8> {
        let mut t = vec![0, 4, 0, 0]; // type 0, length 4, handle
        t.extend_from_slice(b"BIOS\0\0");
        let mut t1 = vec![1u8, 8, 1, 0, 1, 2, 0, 2]; // type 1, length 8: strings at byte 4,5,6, serial = string 2
        t1.extend_from_slice(b"ACME\0SN-12345\0\0");
        t.extend(t1);
        t.extend_from_slice(&[127, 4, 9, 0, 0, 0]);
        t
    }

    #[test]
    fn finds_the_system_serial_in_an_smbios_table() {
        assert_eq!(smbios_system_serial(&table()).as_deref(), Some("SN-12345"));
        assert_eq!(smbios_system_serial(&[]), None);
        assert_eq!(smbios_system_serial(&[1, 2, 3]), None, "a damaged table gives nothing instead of a panic");
        assert_eq!(smbios_system_serial(&[127, 4, 0, 0, 0, 0]), None);
    }

    #[test]
    fn reads_a_string_at_an_offset() {
        let buf = b"\0\0\0\0abc\0def\0";
        assert_eq!(string_at(buf, 4).as_deref(), Some("abc"));
        assert_eq!(string_at(buf, 8).as_deref(), Some("def"));
        assert_eq!(string_at(buf, 0), None);
        assert_eq!(string_at(buf, 99), None);
    }

    /// Reads the real PC: `cargo test -p clearspeak-app --lib device_id -- --ignored --nocapture`.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn reads_this_pc() {
        let c = read();
        println!("guid present: {}, bios: {:?}, cpu: {:?}, disk: {:?}", c.guid.is_some(), c.bios.as_deref().map(str::len), c.cpu, c.disk.as_deref().map(str::len));
        println!("device code: {}", device::device_code(&c));
        assert!(c.guid.is_some(), "MachineGuid must be readable");
        assert!(c.cpu.is_some());
    }
}
