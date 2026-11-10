//! Windows DPAPI for the stored license token: the settings file holds an encrypted blob, not the token text.
//! Machine scope, so every Windows user on this PC can use the one activation (the token is tied to the PC anyway).
//! This hides the token from a casual copy of `settings.json`; it is not a defence against someone who can run code on
//! this PC.

const PREFIX: &str = "dpapi1:";

/// `token` -> the text to store in settings. Falls back to the plain token if encryption is not available.
pub fn protect(token: &str) -> String {
    imp::protect(token.as_bytes()).map(|blob| format!("{PREFIX}{}", b64(&blob))).unwrap_or_else(|| token.to_string())
}

/// Stored text -> the token (also accepts a plain, never-encrypted token).
pub fn unprotect(stored: &str) -> String {
    match stored.strip_prefix(PREFIX) {
        Some(b) => b64d(b).and_then(|blob| imp::unprotect(&blob)).and_then(|v| String::from_utf8(v).ok()).unwrap_or_default(),
        None => stored.to_string(),
    }
}

fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn b64d(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPT_INTEGER_BLOB};

    fn run(data: &[u8], protect: bool) -> Option<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `input` points at `data` for the duration of the call; `output` is filled by Windows and freed below.
        let ok = unsafe {
            if protect {
                CryptProtectData(&input, None, None, None, None, CRYPTPROTECT_LOCAL_MACHINE, &mut output).is_ok()
            } else {
                CryptUnprotectData(&input, None, None, None, None, 0, &mut output).is_ok()
            }
        };
        if !ok || output.pbData.is_null() {
            return None;
        }
        // SAFETY: Windows allocated `cbData` bytes at `pbData`; we copy them out and free the buffer exactly once.
        let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(output.pbData as *mut _)));
        }
        Some(bytes)
    }

    pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, true)
    }
    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, false)
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn protect(_: &[u8]) -> Option<Vec<u8>> {
        None
    }
    pub fn unprotect(_: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn a_token_survives_protect_and_unprotect_and_is_not_stored_in_clear() {
        let token = "CLNAI1.abc.def";
        let stored = protect(token);
        assert!(stored.starts_with("dpapi1:"), "{stored}");
        assert!(!stored.contains("abc"));
        assert_eq!(unprotect(&stored), token);
        assert_eq!(unprotect("plain-old-token"), "plain-old-token", "an unencrypted value is passed through");
        assert_eq!(unprotect("dpapi1:not-base64!!"), "", "a damaged blob gives no token rather than a panic");
    }
}
