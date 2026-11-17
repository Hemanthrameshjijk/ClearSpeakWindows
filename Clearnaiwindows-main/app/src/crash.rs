//! Writes the cause of a crash into the log. The release build aborts on panic and the programs have no console, so a
//! panic or a native fault (an access violation inside a DLL, say) used to end the process with nothing in the log.
//! The normal logger hands lines to a background thread, which would never get to run, so these lines are appended to
//! the file directly.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

static PATH: OnceLock<PathBuf> = OnceLock::new();

fn write_now(line: &str) {
    let Some(path) = PATH.get() else { return };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{}.{:03}] [ERROR] [clearspeakrt] {line}", now.as_secs(), now.subsec_millis());
    }
}

/// Call once after the logger is set up; `file_name` is the log this process writes.
pub fn install(dir: &std::path::Path, file_name: &str) {
    if PATH.set(dir.join(file_name)).is_err() {
        return;
    }
    std::panic::set_hook(Box::new(|info| {
        let place = info.location().map_or(String::new(), |l| format!(" at {}:{}", l.file(), l.line()));
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(no message)".into());
        let thread = std::thread::current();
        write_now(&format!("CRASH: panic on thread '{}'{place}: {msg}", thread.name().unwrap_or("?")));
    }));
    #[cfg(windows)]
    win::install();
}

#[cfg(windows)]
mod win {
    use super::write_now;

    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        record: *mut ExceptionRecord,
        address: *mut core::ffi::c_void,
        number_parameters: u32,
        info: [usize; 15],
    }

    #[repr(C)]
    struct ExceptionPointers {
        record: *mut ExceptionRecord,
        context: *mut core::ffi::c_void,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn SetUnhandledExceptionFilter(
            filter: Option<unsafe extern "system" fn(*const ExceptionPointers) -> i32>,
        ) -> *mut core::ffi::c_void;
        fn GetModuleHandleExW(flags: u32, address: *const core::ffi::c_void, module: *mut isize) -> i32;
        fn GetModuleFileNameW(module: isize, name: *mut u16, size: u32) -> u32;
    }

    const FROM_ADDRESS_UNCHANGED_REFCOUNT: u32 = 0x4 | 0x2;

    unsafe extern "system" fn filter(p: *const ExceptionPointers) -> i32 {
        if let Some(rec) = p.as_ref().and_then(|p| p.record.as_ref()) {
            let mut module = 0isize;
            let mut name = String::from("unknown module");
            if GetModuleHandleExW(FROM_ADDRESS_UNCHANGED_REFCOUNT, rec.address, &mut module) != 0 {
                let mut buf = [0u16; 260];
                let n = GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) as usize;
                name = String::from_utf16_lossy(&buf[..n]);
                let offset = rec.address as usize - module as usize;
                name = format!("{name}+0x{offset:x}");
            }
            let extra = if rec.code == 0xC000_0005 && rec.number_parameters >= 2 {
                let kind = match rec.info[0] {
                    0 => "read",
                    1 => "write",
                    _ => "execute",
                };
                format!(" ({kind} of 0x{:x})", rec.info[1])
            } else {
                String::new()
            };
            write_now(&format!("CRASH: native exception 0x{:08X}{extra} in {name}", rec.code));
        }
        0 // EXCEPTION_CONTINUE_SEARCH: let Windows end the process as before
    }

    pub fn install() {
        unsafe { SetUnhandledExceptionFilter(Some(filter)) };
    }
}
