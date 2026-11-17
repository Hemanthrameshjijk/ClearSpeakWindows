//! Connection to the ClearSpeak virtual audio driver (the separate `ClearnaiWindows-Diver` project).
//!
//! Additive and removable: the mic pipeline hands each final processed frame to a [`DriverSink`], which forwards it
//! to the driver's control device `\\.\ClearSpeakPipe` from its own thread, so the real-time audio thread never makes a
//! system call. Apps then pick "ClearSpeak Microphone" as their input. If the driver is not installed the sink just
//! keeps retrying once a second and drops the frames; nothing else in the app is affected (the VB-CABLE route keeps
//! working as before).
//!
//! Wire contract (must match `driver/ClearSpeak/sysvad/EndpointsCommon/clearspeakpipe.h` in the driver project):
//! `IOCTL_CLEARSPEAK_WRITE` takes PCM16, mono, 48 kHz, at most 100 ms per call.
//!
//! To remove this connection: delete this file, the `pub mod driver_pipe;` line in `lib.rs`, the extra `windows`
//! features in `Cargo.toml`, and the three `driver_pipe` references in `engine.rs`
//! (backups of the originals: `ClearnaiWindows-Diver/integration/backup-before-connect`).

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use dsp_core::{make_ring_buffer, Producer, FRAME_SAMPLES};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

/// CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_WRITE_DATA).
const IOCTL_CLEARSPEAK_WRITE: u32 = (0x22 << 16) | (2 << 14) | (0x800 << 2);

/// Frames of headroom between the pipeline thread and the sender thread (~0.5 s).
const QUEUE_SECONDS: f32 = 0.5;

pub struct DriverSink {
    producer: Producer<f32>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    /// Frames dropped because the queue to the sender thread was full (counted on the audio thread, logged by the sender).
    dropped: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
}

/// How often the sender thread writes a one-line summary (frames sent, dropped, loudest level) to the log.
const SUMMARY_EVERY: Duration = Duration::from_secs(60);

impl DriverSink {
    /// Starts the sender thread. Never fails: a missing driver only means frames are dropped.
    pub fn start() -> Self {
        let (producer, mut consumer) = make_ring_buffer(QUEUE_SECONDS);
        let stop = Arc::new(AtomicBool::new(false));
        let connected = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let (t_stop, t_connected, t_dropped) = (stop.clone(), connected.clone(), dropped.clone());

        let handle = thread::Builder::new()
            .name("clearspeak-driver-pipe".into())
            .spawn(move || {
                let mut device: Option<HANDLE> = None;
                let mut last_try = Instant::now() - Duration::from_secs(10);
                let mut pcm = vec![0i16; FRAME_SAMPLES];
                let (mut sent, mut failed, mut peak) = (0u64, 0u64, 0i16);
                let mut last_summary = Instant::now();
                let mut told_missing = false;
                while !t_stop.load(Ordering::Relaxed) {
                    if last_summary.elapsed() >= SUMMARY_EVERY {
                        let lost = t_dropped.swap(0, Ordering::Relaxed);
                        if device.is_some() || sent > 0 {
                            crate::log_info!(
                                "[driver] last {}s: {sent} frames sent to ClearSpeak Microphone, {failed} write errors, {lost} dropped (queue full), loudest {:.1} dBFS",
                                SUMMARY_EVERY.as_secs(),
                                if peak <= 0 { -120.0 } else { 20.0 * (peak as f32 / 32767.0).log10() }
                            );
                        }
                        (sent, failed, peak) = (0, 0, 0);
                        last_summary = Instant::now();
                    }
                    if consumer.slots() < FRAME_SAMPLES {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    for p in pcm.iter_mut() {
                        let s = consumer.pop().unwrap_or(0.0);
                        *p = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        peak = peak.max(p.saturating_abs());
                    }
                    if device.is_none() && last_try.elapsed() >= Duration::from_secs(1) {
                        last_try = Instant::now();
                        device = open_device();
                        if device.is_some() {
                            t_connected.store(true, Ordering::Relaxed);
                            told_missing = false;
                            crate::log_info!("[driver] connected to the ClearSpeak virtual microphone driver");
                        } else if !std::mem::replace(&mut told_missing, true) {
                            crate::log_info!("[driver] ClearSpeak virtual microphone driver not found (not installed or not loaded); retrying every second");
                        }
                    }
                    if let Some(h) = device {
                        if write_frame(h, &pcm) {
                            sent += 1;
                        } else {
                            failed += 1;
                            unsafe {
                                let _ = CloseHandle(h);
                            }
                            device = None;
                            t_connected.store(false, Ordering::Relaxed);
                            crate::log_error!("[driver] write to the ClearSpeak virtual microphone driver failed; reconnecting");
                        }
                    }
                }
                if let Some(h) = device {
                    unsafe {
                        let _ = CloseHandle(h);
                    }
                }
            })
            .ok();

        Self { producer, stop, connected, dropped, handle }
    }

    /// Queues one final processed frame (`FRAME_SAMPLES` mono f32). Never blocks; drops the frame if the queue is full.
    pub fn push_frame(&mut self, frame: &[f32]) -> bool {
        if self.producer.slots() < frame.len() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        for &s in frame {
            let _ = self.producer.push(s);
        }
        true
    }

    /// True while the driver's control device is open.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

impl Drop for DriverSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn open_device() -> Option<HANDLE> {
    // A driver installed before the rename still uses the old device name.
    open_named(w!("\\\\.\\ClearSpeakPipe")).or_else(|| open_named(w!("\\\\.\\ClearNAIPipe")))
}

fn open_named(name: windows::core::PCWSTR) -> Option<HANDLE> {
    unsafe {
        CreateFileW(
            name,
            GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .ok()
    }
}

fn write_frame(h: HANDLE, pcm: &[i16]) -> bool {
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            h,
            IOCTL_CLEARSPEAK_WRITE,
            Some(pcm.as_ptr() as *const _),
            (pcm.len() * 2) as u32,
            None,
            0,
            Some(&mut returned),
            None,
        )
        .is_ok()
    }
}
