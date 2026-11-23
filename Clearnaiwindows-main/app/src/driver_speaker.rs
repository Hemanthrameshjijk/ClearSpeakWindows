//! Call audio from the ClearSpeak virtual audio driver: what call apps play into "ClearSpeak Speaker".
//!
//! The call app's speaker is set to "ClearSpeak Speaker" once. The driver keeps what is played into it in a ring, and
//! this reader pulls it with `IOCTL_CLEARSPEAK_READ` from its own thread into the speaker pipeline's input ring (the
//! same place a second virtual cable's capture feeds), so caller noise removal works without a second VB-CABLE pair.
//! The speaker pipeline then plays the cleaned call on the real headset as before.
//!
//! In the device pickers this source is the capture "device" [`DRIVER_SPEAKER_ID`]; it is only listed when the
//! installed driver answers the read call (an older driver without it is skipped).
//!
//! Wire contract (must match `driver/ClearSpeak/sysvad/EndpointsCommon/clearspeakpipe.h` in the driver project):
//! `IOCTL_CLEARSPEAK_READ` returns at once with up to the output buffer's size of PCM16, mono, 48 kHz (0 bytes when
//! nothing is buffered). The driver drops the oldest audio when nobody reads, and resets after 250 ms without reads.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{anyhow, Result};
use dsp_core::{FrameCounters, Producer};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

/// CTL_CODE(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_READ_ACCESS).
const IOCTL_CLEARSPEAK_READ: u32 = (0x22 << 16) | (1 << 14) | (0x802 << 2);

/// Device-picker id and label of this source (not a Windows endpoint id, so it can never collide with one).
pub const DRIVER_SPEAKER_ID: &str = "clearspeak-driver:speaker";
pub const DRIVER_SPEAKER_LABEL: &str = "ClearSpeak Speaker (virtual, from the call app)";

/// Largest read: 100 ms of 48 kHz mono PCM16.
const MAX_READ_SAMPLES: usize = 4800;
/// Poll period. Well under the 250 ms after which the driver treats the reader as gone.
const POLL: Duration = Duration::from_millis(5);

/// True when `id` is this source rather than a Windows capture endpoint.
pub fn is_driver_speaker(id: &str) -> bool {
    id == DRIVER_SPEAKER_ID
}

/// The installed driver can hand back call audio (it is loaded and knows `IOCTL_CLEARSPEAK_READ`).
pub fn available() -> bool {
    let Some(h) = open_device() else { return false };
    let mut buf = [0i16; 1];
    let ok = read(h, &mut buf).is_some();
    unsafe {
        let _ = CloseHandle(h);
    }
    ok
}

pub struct DriverSpeakerSource {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl DriverSpeakerSource {
    /// Opens the driver and starts the reader thread, which fills `producer` (the speaker pipeline's input).
    pub fn start(mut producer: Producer<f32>, counters: Arc<FrameCounters>) -> Result<Self> {
        let h = open_device().ok_or_else(|| anyhow!("the ClearSpeak virtual audio driver is not installed or not loaded"))?;
        let mut probe = [0i16; 1];
        if read(h, &mut probe).is_none() {
            unsafe {
                let _ = CloseHandle(h);
            }
            return Err(anyhow!("the installed ClearSpeak driver is too old to pass on call audio (update the driver)"));
        }
        // HANDLE is a raw pointer; move its value into the thread.
        let raw = h.0 as isize;
        let stop = Arc::new(AtomicBool::new(false));
        let t_stop = stop.clone();
        let handle = thread::Builder::new()
            .name("clearspeak-driver-speaker".into())
            .spawn(move || {
                let _mmcss_guard = audio_io::mmcss::elevate_current_thread();
                let h = HANDLE(raw as *mut _);
                let mut pcm = vec![0i16; MAX_READ_SAMPLES];
                let mut failures = 0u32;
                while !t_stop.load(Ordering::Relaxed) {
                    match read(h, &mut pcm) {
                        Some(n) => {
                            failures = 0;
                            let mut dropped = false;
                            for &s in &pcm[..n] {
                                if producer.push(s as f32 / 32768.0).is_err() {
                                    dropped = true;
                                }
                            }
                            if dropped {
                                counters.input_overruns.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        None => {
                            failures += 1;
                            if failures == 1 {
                                crate::log_error!("[driver] reading call audio from ClearSpeak Speaker failed; retrying");
                            }
                        }
                    }
                    thread::sleep(POLL);
                }
                unsafe {
                    let _ = CloseHandle(h);
                }
            })
            .map_err(|e| anyhow!("starting the call-audio reader thread: {e}"))?;
        crate::log_info!("[driver] reading call audio from ClearSpeak Speaker");
        Ok(Self { stop, handle: Some(handle) })
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for DriverSpeakerSource {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn open_device() -> Option<HANDLE> {
    unsafe {
        CreateFileW(
            w!("\\\\.\\ClearSpeakPipe"),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .ok()
    }
}

/// Samples read into `buf` (0 when nothing is buffered); `None` when the call failed.
fn read(h: HANDLE, buf: &mut [i16]) -> Option<usize> {
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            h,
            IOCTL_CLEARSPEAK_READ,
            None,
            0,
            Some(buf.as_mut_ptr() as *mut _),
            (buf.len() * 2) as u32,
            Some(&mut returned),
            None,
        )
        .ok()?;
    }
    Some(returned as usize / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CTL_CODE(DeviceType, Function, Method, Access) = (DeviceType << 16) | (Access << 14) | (Function << 2) | Method.
    const fn ctl_code(device: u32, function: u32, method: u32, access: u32) -> u32 {
        (device << 16) | (access << 14) | (function << 2) | method
    }

    #[test]
    fn read_ioctl_matches_the_driver_header() {
        // clearspeakpipe.h: CTL_CODE(FILE_DEVICE_UNKNOWN, 0x802, METHOD_BUFFERED, FILE_READ_ACCESS)
        assert_eq!(IOCTL_CLEARSPEAK_READ, ctl_code(0x22, 0x802, 0, 1));
        assert_eq!(IOCTL_CLEARSPEAK_READ, 0x0022_6008);
    }

    #[test]
    fn the_driver_source_id_is_not_a_windows_endpoint_id() {
        assert!(is_driver_speaker(DRIVER_SPEAKER_ID));
        assert!(!is_driver_speaker("{0.0.1.00000000}.{277a1101-e681-4249-8c7e-ce2c9bd4c1d4}"));
        assert!(DRIVER_SPEAKER_LABEL.to_ascii_lowercase().contains("virtual"), "device rules treat it as virtual");
    }
}
