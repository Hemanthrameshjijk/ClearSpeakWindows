//! Real-time microphone capture via WASAPI shared-mode, event-driven
//! capture, feeding fixed `dsp_core::FRAME_SAMPLES` frames into a
//! `dsp_core::Producer<f32>`.
//!
//! Real-time-priority gap: the `wasapi` crate (as of 0.24, confirmed by
//! reading its source) does not expose any thread/MMCSS priority helper
//! (no wrapper around `AvSetMmThreadCharacteristicsW` or
//! `SetThreadPriority`). This module spawns a plain OS thread and does
//! *not* fake priority elevation - if the engine needs MMCSS "Pro Audio"
//! characteristics for this thread, that has to be done separately (e.g.
//! via a small `windows-sys` call wrapped around this thread's body), which
//! is out of scope for what this crate/dependency can honestly provide.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{bail, Context, Result};
use dsp_core::{FrameCounters, Producer, FRAME_SAMPLES};
use wasapi::{initialize_mta, DeviceEnumerator, Direction, StreamMode};

use crate::devices::{find_capture_device_by_name, open_capture_device_by_id};
use crate::follow::{self, Choice, Target};
use crate::{
    engine_wave_format, sleep_respecting_stop, warn_if_buffer_size_mismatched, RECONNECT_BACKOFF,
    REQUESTED_BUFFER_DURATION_HNS,
};

// NOTE: `wasapi::Device` wraps a raw COM pointer (`IMMDevice`) and is not
// `Send` - confirmed by `cargo check` (E0277 on the very first draft of this
// module). WASAPI/COM objects are meant to be activated on the thread that
// uses them anyway (each thread needs its own COM apartment via
// `initialize_mta`), so instead of moving a `Device` into the capture
// thread, we move its `String` id across and re-open it with a fresh
// `DeviceEnumerator` from inside the thread.

/// Owns the mic-capture OS thread. Dropping (or calling `stop`) signals the
/// thread to stop and joins it.
pub struct MicCapture {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MicCapture {
    /// Opens the system default capture device and starts streaming
    /// 480-sample frames into `producer`. `counters` receives live
    /// overrun/drop telemetry (see `capture_loop`/`push_frame`) - callers
    /// that don't care about this yet can pass `Arc::new(FrameCounters::default())`
    /// and simply never read it.
    pub fn start_default(producer: Producer<f32>, counters: Arc<FrameCounters>) -> Result<Self> {
        // Resolve the device id on the calling thread just to validate a
        // default device exists and fail fast; the id (not the Device
        // itself - see note above) is what actually crosses into the
        // capture thread.
        let enumerator = DeviceEnumerator::new().context("creating WASAPI device enumerator")?;
        enumerator.get_default_device(&Direction::Capture).context("opening default capture device")?;
        // Followed while running: earbuds that connect later (and become the Windows default) are picked up.
        Self::spawn(Target::Default, true, producer, counters)
    }

    /// Opens the capture device whose friendly name contains
    /// `name_substr` (case-insensitive) and starts streaming.
    pub fn start_by_name(name_substr: &str, producer: Producer<f32>, counters: Arc<FrameCounters>) -> Result<Self> {
        let device = find_capture_device_by_name(name_substr)?
            .with_context(|| format!("no capture device matching '{name_substr}'"))?;
        let device_id = device.get_id().context("reading capture device id")?;
        Self::start_on_device_id(&device_id, producer, counters)
    }

    /// Opens a specific capture device by its enumerated device id (see
    /// `devices::list_capture_devices`) and starts streaming.
    /// A microphone that is unplugged is replaced by the Windows default until it is back.
    pub fn start_on_device_id(device_id: &str, producer: Producer<f32>, counters: Arc<FrameCounters>) -> Result<Self> {
        Self::spawn(Target::Id(device_id.to_string()), true, producer, counters)
    }

    /// Like [`start_on_device_id`](Self::start_on_device_id) but never falls back to the default microphone: for a
    /// capture that is not the user's voice (the call-audio source), the real mic would play your own voice back.
    pub fn start_on_device_id_no_fallback(device_id: &str, producer: Producer<f32>, counters: Arc<FrameCounters>) -> Result<Self> {
        Self::spawn(Target::Id(device_id.to_string()), false, producer, counters)
    }

    fn spawn(target: Target, allow_fallback: bool, producer: Producer<f32>, counters: Arc<FrameCounters>) -> Result<Self> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_flag.clone();

        let handle = thread::Builder::new()
            .name("audio-io-mic-capture".into())
            .spawn(move || {
                let mut producer = producer;
                // Best-effort: see `mmcss.rs` - failure is silent and
                // harmless (thread just stays at normal priority). Held for
                // the whole thread's lifetime, across reconnects, not
                // re-acquired per `capture_loop` call below.
                let _mmcss_guard = crate::mmcss::elevate_current_thread();
                // See render.rs's identical reconnect loop: a WASAPI failure
                // here (device disabled/removed, sleep/wake) used to
                // permanently kill mic capture until the app was restarted.
                // Retry with a backoff instead.
                let mut failed_attempts: u32 = 0;
                let mut last_opened: Option<String> = None;
                while !thread_stop.load(Ordering::Relaxed) {
                    let started = std::time::Instant::now();
                    let result = capture_loop(&target, allow_fallback, &mut last_opened, &mut producer, &thread_stop, &counters);
                    if let Ok(true) = result {
                        // A better device appeared (earbuds connected, default changed): reopen right away.
                        failed_attempts = 0;
                        continue;
                    }
                    if let Err(e) = result {
                        // A session that streamed for a while before dying starts the count over; a device
                        // that never opens (unplugged, stale id) fails identically each time, so say so on
                        // the first failure and then about once a minute.
                        if started.elapsed() > std::time::Duration::from_secs(10) {
                            failed_attempts = 0;
                        }
                        failed_attempts += 1;
                        if failed_attempts == 1 || failed_attempts % 12 == 0 {
                            crate::report_error(&format!(
                                "[audio-io] mic capture thread error (attempt {failed_attempts}), retrying: {e:#}"
                            ));
                        }
                    }
                    if thread_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    // Quick retries first (recovers a brief glitch within 2 s), then every 5 s for a device
                    // that is simply gone.
                    let backoff = if failed_attempts <= 3 { RECONNECT_BACKOFF } else { RECONNECT_BACKOFF * 5 / 2 };
                    sleep_respecting_stop(backoff, &thread_stop);
                }
            })
            .context("spawning mic capture thread")?;

        Ok(Self {
            stop_flag,
            handle: Some(handle),
        })
    }

    /// Signals the capture thread to stop and blocks until it exits.
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for MicCapture {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// How often a streaming thread looks for a better device (default changed, saved headset back).
const FOLLOW_EVERY: std::time::Duration = std::time::Duration::from_secs(1);
/// First wait for audio after opening: a Bluetooth headset switching into call mode can take several seconds.
const FIRST_EVENT_TIMEOUT_MS: u32 = 5000;

/// Streams until stopped (`Ok(false)`), until a better device should be used (`Ok(true)`), or until the device fails.
fn capture_loop(
    target: &Target,
    allow_fallback: bool,
    last_opened: &mut Option<String>,
    producer: &mut Producer<f32>,
    stop_flag: &Arc<AtomicBool>,
    counters: &FrameCounters,
) -> Result<bool> {
    // Every thread that touches WASAPI needs its own COM apartment, and
    // COM objects (like the `Device`/`IAudioClient` below) should be
    // activated on the thread that will use them - hence re-resolving the
    // device from its id here rather than receiving a `Device` from the
    // spawning thread.
    let _ = initialize_mta();

    // A saved mic that is unplugged is replaced by the Windows default (better than silence) and taken back as soon
    // as it returns; one that came back under a new id is found by name.
    let choice: Choice = follow::resolve(&Direction::Capture, target, allow_fallback)
        .context("the capture device is not connected")?;
    let device_id = choice.id.as_str();
    let device = open_capture_device_by_id(device_id)
        .with_context(|| format!("re-opening capture device '{device_id}' on capture thread"))?;
    let name = device.get_friendlyname().unwrap_or_else(|_| "unnamed microphone".to_string());
    let mut audio_client = device
        .get_iaudioclient()
        .with_context(|| format!("activating IAudioClient on capture device '{name}'"))?;
    let device_rate = audio_client.get_mixformat().map(|f| f.get_samplespersec()).unwrap_or(0);
    counters.hands_free.store(follow::is_hands_free(&name, device_rate), Ordering::Relaxed);
    if last_opened.as_deref() != Some(device_id) {
        let mut note = format!("[audio-io] INFO: microphone: '{name}' ({device_rate} Hz)");
        if choice.fallback {
            note += "; the chosen microphone is not connected, using the Windows default until it is back";
        }
        if follow::is_hands_free(&name, device_rate) {
            note += "; Bluetooth call mode (phone quality): for best sound talk on the PC or USB mic";
        }
        crate::report_error(&note);
        *last_opened = Some(device_id.to_string());
    }

    let format = engine_wave_format();

    // Explicitly request a 10ms (480-sample) buffer rather than trusting
    // whatever `get_device_period()`'s default/min period happens to be -
    // see `REQUESTED_BUFFER_DURATION_HNS` docs in `lib.rs` for why (the
    // reference PipeWire port silently got a 256ms buffer until it did the
    // analogous thing explicitly).
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: REQUESTED_BUFFER_DURATION_HNS,
    };

    audio_client
        .initialize_client(&format, &Direction::Capture, &mode)
        .context("initializing capture AudioClient (shared, event-driven)")?;

    // Speculative defense, unverified without real hardware: confirm WASAPI
    // actually honored the request above before we start streaming.
    warn_if_buffer_size_mismatched(&audio_client, "mic capture");

    let event_handle = audio_client
        .set_get_eventhandle()
        .context("getting capture event handle")?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .context("getting IAudioCaptureClient")?;

    audio_client
        .start_stream()
        .context("starting capture stream")?;

    // Mono f32: 4 bytes a frame. Grown if a device ever hands over a bigger packet.
    let mut scratch: Vec<u8> = vec![0; 48_000 * 4 / 2];
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES);
    let mut timeout_ms = FIRST_EVENT_TIMEOUT_MS;
    let mut last_follow_check = std::time::Instant::now();

    while !stop_flag.load(Ordering::Relaxed) {
        // A stalled event most likely means the device was removed/disabled
        // out from under us; surface that as a real error rather than
        // spinning silently forever.
        if event_handle.wait_for_event(timeout_ms).is_err() {
            bail!("capture event wait failed or timed out; capture device likely became unavailable");
        }
        timeout_ms = 1000;
        if last_follow_check.elapsed() >= FOLLOW_EVERY {
            last_follow_check = std::time::Instant::now();
            if follow::should_switch(&Direction::Capture, target, &choice, allow_fallback) {
                audio_client.stop_stream().ok();
                return Ok(true);
            }
        }

        loop {
            // Shared mode reports `Some(0)`, not `None`, when nothing is
            // waiting - so this must end on 0 too. Ending only on `None`
            // spun on GetBuffer at 100% of a core, never went back to wait
            // for the event and never saw `stop_flag`: a live device switch
            // then hung joining this thread, and the abandoned thread kept
            // filling its old ring ("mic capture ring buffer full" and the
            // dropped-frames counter climbing at 100/s, forever).
            let frames = match capture_client
                .get_next_packet_size()
                .context("polling capture packet size")?
            {
                Some(frames) if frames > 0 => frames as usize,
                _ => break,
            };
            if scratch.len() < frames * 4 {
                scratch.resize(frames * 4, 0);
            }

            // `read_from_device` returns errors; the deque variant unwraps ReleaseBuffer, which aborts the whole
            // engine (panic = abort) when the device disappears in the middle of a read.
            let (got, info) = capture_client
                .read_from_device(&mut scratch)
                .context("reading captured audio from device")?;

            for c in scratch[..got as usize * 4].chunks_exact(4) {
                // A packet marked silent (Bluetooth glitch, profile switch) is silence whatever its bytes say.
                let sample = if info.flags.silent { 0.0 } else { f32::from_le_bytes([c[0], c[1], c[2], c[3]]) };
                pending.push(sample);
                if pending.len() == FRAME_SAMPLES {
                    push_frame(producer, &pending, counters);
                    pending.clear();
                }
            }
        }

        if stop_flag.load(Ordering::Relaxed) {
            break;
        }
    }

    audio_client.stop_stream().ok();
    Ok(false)
}

/// Pushes one full frame into the ring buffer. If the consumer isn't
/// keeping up and the ring buffer is full, the whole frame is dropped
/// (never block the realtime capture thread waiting for space) - this is a
/// real audible glitch under overload, and preferable to blocking WASAPI's
/// callback thread indefinitely. Increments `counters.input_overruns` and
/// `counters.dropped_frames` (both plain atomic adds, realtime-safe) so this
/// is queryable telemetry, not just a log line.
fn push_frame(producer: &mut Producer<f32>, frame: &[f32], counters: &FrameCounters) {
    debug_assert_eq!(frame.len(), FRAME_SAMPLES);
    for &sample in frame {
        if producer.push(sample).is_err() {
            // Ring buffer full: drop the remainder of this frame rather
            // than blocking. Downstream will simply see a short gap.
            counters.input_overruns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            counters.dropped_frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::report_error("[audio-io] mic capture ring buffer full, dropping frame");
            return;
        }
    }
}
