//! The background engine process (`clearspeak-engine.exe`): audio pipelines, tray icon, mute hotkey, autostart,
//! settings, and the pipe the window (`clearspeak.exe`) connects to. It has no window of its own, so closing the window
//! never interrupts the audio. See `ipc.rs` for the messages.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::ipc::{self, role, AccentWire, ClientMsg, Flags, Hello, PipeReader, Polled, ServerMsg, Status, TranscriptWire};
use crate::settings::Settings;
use crate::setup::CompatibilityReport;
use crate::shared::{DeviceList, EngineHandles};
use crate::{device_rules, engine, settings, setup, shared};

/// Everything the engine knows once it has started (or failed to start, in which case `handles` are inert).
pub struct Booted {
    pub settings: Settings,
    pub handles: EngineHandles,
    pub devices: DeviceList,
    pub compat: CompatibilityReport,
    pub bvc_extraction_error: Option<String>,
}

/// Builds the engine (audio threads, models, devices) from the saved settings. Never fails outright: if the engine
/// cannot start, inert handles are returned so the window can still say why.
pub fn bootstrap(settings: &settings::Settings) -> Booted {
    let t = std::time::Instant::now();
    let devices = engine::enumerate_devices();
    crate::log_info!(
        "[startup] listed {} microphone(s) and {} output(s) in {} ms",
        devices.capture.len(),
        devices.render.len(),
        t.elapsed().as_millis()
    );

    // Check the saved device choices against what is installed right now: a wrong or unplugged device falls
    // back to automatic instead of silently misrouting audio (a physical mic saved as the call audio source
    // used to play the user's own voice out of the speakers).
    let resolved = device_rules::resolve(settings, &devices, engine::default_virtual_mic_target(&devices));
    for note in &resolved.notes {
        crate::log_info!("[clearspeakrt] devices: {note}");
    }
    if resolved.changed {
        if let Err(e) = settings::save(&resolved.settings) {
            crate::log_error!("[clearspeakrt] failed to save corrected device settings: {e:#}");
        }
    }
    let mic_target = resolved.mic_target.clone();
    // Like the Monitor, the speaker path is never auto-picked: with a single VB-Cable pair the only candidate is
    // "CABLE Output", the other end of the cable the mic path renders into, so the speaker path would capture
    // the user's own processed voice and play it back out of the speakers. It stays off until a separate cable
    // is picked (see docs/VIRTUAL_DEVICES.md).
    // With "Call audio" switched off the speaker path does not start at all (the saved source is kept for later).
    let speaker_source = if resolved.settings.call_audio_on { resolved.speaker_source.clone() } else { None };
    // `None` means "system default output device".
    let real_speaker_output = resolved.real_output.clone();
    let settings = &resolved.settings;

    // Fast, synchronous checks - both cheap enough to redo on every launch
    // rather than caching/skipping them (see `setup::CompatibilityReport`
    // and `Settings::setup_acknowledged` for why a real failure is never
    // permanently suppressed).
    let t = std::time::Instant::now();
    let compat = setup::check_system_compatibility();
    crate::log_info!("[startup] compatibility check in {} ms: {compat:?}", t.elapsed().as_millis());

    // Self-extract the embedded BVC DLL + model into %LOCALAPPDATA% *before*
    // `engine::start` tries to load them - this used to be a manual/
    // download-based step; it is now synchronous and (bytes already being
    // resident in the binary) fast enough to never need a progress UI. A
    // real failure here (disk full, unwritable %LOCALAPPDATA%, etc.) is
    // surfaced to the setup screen rather than crashing - BVC simply stays
    // unavailable, same graceful-degradation pattern as a missing DLL
    // always used to be.
    let bvc_dir = setup::assets_dir();
    let t = std::time::Instant::now();
    // Installed layout (files shipped next to the exe): nothing to extract, and that folder is not writable anyway.
    let bvc_extraction_error = match if bvc_dir == setup::app_data_dir() {
        setup::ensure_bvc_assets_extracted(&bvc_dir).map(|_| ())
    } else {
        Ok(())
    } {
        Ok(()) => None,
        Err(e) => {
            crate::log_error!("[clearspeakrt] WARNING: failed to extract embedded BVC assets: {e:#}");
            Some(format!("{e:#}"))
        }
    };
    crate::log_info!(
        "[startup] model files ready in {} ms ({})",
        t.elapsed().as_millis(),
        if bvc_dir == setup::app_data_dir() { "extracted to app data" } else { "installed next to the program" }
    );

    match engine::start(
        &bvc_dir,
        settings,
        devices.clone(),
        mic_target.as_deref(),
        speaker_source.as_deref(),
        real_speaker_output.as_deref(),
    ) {
        Ok(outcome) => Booted {
            settings: settings.clone(),
            handles: outcome.handles,
            devices: outcome.devices,
            compat,
            bvc_extraction_error,
        },
        Err(e) => {
            crate::log_error!("[clearspeakrt] FATAL: failed to start audio engine: {e:#}");
            // Still bring up the GUI rather than exiting silently: an inert
            // engine handle set makes the failure visible (status stays
            // Bypass, no audio flows) instead of the whole app vanishing
            // with no explanation.
            let handles = shared::EngineHandles::new_from_settings(
                settings,
                false,
                Some("engine failed to start; see stderr log".to_string()),
                false,
                Some("engine failed to start; see stderr log".to_string()),
                false,
                Some("engine failed to start; see stderr log".to_string()),
                vec![format!("Audio engine failed to start: {e:#}")],
                false,
                Some("engine failed to start; see stderr log".to_string()),
                false,
                Some("engine failed to start; see stderr log".to_string()),
            );
            Booted {
                settings: settings.clone(),
                handles,
                devices,
                compat,
                bvc_extraction_error,
            }
        }
    }
}


/// A mutex named `name` stays alive as long as the process; if it already exists, another copy is running.
pub fn another_instance_running(name: &str) -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(attrs: *const u8, initial_owner: i32, name: *const u16) -> isize;
        fn GetLastError() -> u32;
    }
    const ERROR_ALREADY_EXISTS: u32 = 183;
    let name: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // The handle is intentionally never closed: the OS releases it when the process ends.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    handle != 0 && unsafe { GetLastError() } == ERROR_ALREADY_EXISTS
}

type SharedWriter = Arc<Mutex<File>>;

struct Host {
    handles: EngineHandles,
    settings: Mutex<Settings>,
    devices: DeviceList,
    compat: CompatibilityReport,
    bvc_extraction_error: Option<String>,
    /// Every connected window, so a quit can tell them all.
    writers: Mutex<Vec<SharedWriter>>,
    cpu_tenths: AtomicU32,
}

/// What this connection has already been told about the transcript.
#[derive(Default)]
struct TxState {
    sent_lines: usize,
}

impl Host {
    fn hello(&self) -> Hello {
        let h = &self.handles;
        Hello {
            version: ipc::PROTOCOL_VERSION,
            settings: self.settings.lock().unwrap().clone(),
            devices: self.devices.clone(),
            compat: self.compat.clone(),
            bvc_extraction_error: self.bvc_extraction_error.clone(),
            flags: Flags {
                mic_bvc_available: h.mic_bvc_available,
                mic_bvc_reason: h.mic_bvc_unavailable_reason.clone(),
                speaker_bvc_available: h.speaker_bvc_available,
                speaker_bvc_reason: h.speaker_bvc_unavailable_reason.clone(),
                speaker_deep_filter_available: h.speaker_deep_filter_available,
                speaker_deep_filter_reason: h.speaker_deep_filter_unavailable_reason.clone(),
                mic_deep_filter_available: h.mic_deep_filter_available,
                mic_deep_filter_reason: h.mic_deep_filter_unavailable_reason.clone(),
                mic_bpo_v1_available: h.mic_bpo_v1_available,
                mic_bpo_v1_reason: h.mic_bpo_v1_unavailable_reason.clone(),
                has_hush_status: h.mic_hush_status.is_some(),
                has_bpo_status: h.mic_bpo_v1_status.is_some(),
                warnings: (*h.warnings).clone(),
            },
        }
    }

    fn status(&self, tx: &mut TxState) -> Status {
        use accent_onnx::pipeline::AccentState;
        use asr_onnx::pipeline::TranscriptState;
        let h = &self.handles;
        let acc = h.accent.status();
        let ts = h.transcript.status();

        let mut transcript = TranscriptWire {
            state: match ts.state {
                TranscriptState::Off => 0,
                TranscriptState::Loading => 1,
                TranscriptState::Listening => 2,
                TranscriptState::Transcribing => 3,
                TranscriptState::Unavailable => 4,
            },
            enabled: h.transcript.enabled(),
            message: ts.message.clone(),
            rtf: ts.rtf,
            ..Default::default()
        };
        if ts.line_count < tx.sent_lines {
            // The transcript was cleared (or restarted): tell the window to start over.
            transcript.reset = true;
            tx.sent_lines = 0;
        }
        let (lines, partial) = h.transcript.snapshot(ts.line_count - tx.sent_lines);
        transcript.new_lines = lines.into_iter().map(|l| (l.at_secs, l.text)).collect();
        transcript.partial = partial;
        tx.sent_lines = ts.line_count;

        let cpu = self.cpu_tenths.load(Ordering::Relaxed);
        Status {
            toggles: h.toggles().iter().map(|t| t.is_on()).collect(),
            presets: h.presets().iter().map(|p| p.load()).collect(),
            level: h.level.load(),
            input_level: h.input_level.load(),
            mic_counters: shared::counters_to_array(&h.counters),
            mic_hands_free: h.counters.hands_free.load(std::sync::atomic::Ordering::Relaxed),
            voice_auto: [h.voice_auto.state.load(Ordering::Relaxed), h.voice_auto.gender.load(Ordering::Relaxed)],
            voice_auto_confirmed: h.voice_auto.confirmed.load(Ordering::Relaxed),
            speaker_counters: shared::counters_to_array(&h.speaker_counters),
            mic_pct: dsp_core::compute_percentiles(h.latency_log.snapshot()).into(),
            speaker_pct: dsp_core::compute_percentiles(h.speaker_latency_log.snapshot()).into(),
            hush_bypassed: h.mic_hush_status.as_ref().is_some_and(|s| s.is_bypassed()),
            bpo_bypassed: h.mic_bpo_v1_status.as_ref().is_some_and(|s| s.is_bypassed()),
            accent: AccentWire {
                voice: acc.voice,
                state: match acc.state {
                    AccentState::Off => 0,
                    AccentState::Loading => 1,
                    AccentState::Starting => 2,
                    AccentState::Active => 3,
                    AccentState::Paused => 4,
                    AccentState::Unavailable => 5,
                },
                rtf: acc.rtf,
                latency_ms: acc.latency_ms,
                underruns: acc.underruns,
                jitter_ms: acc.jitter_ms,
                dropped_input: acc.dropped_input,
                catchups: acc.catchups,
                message: acc.message,
            },
            transcript,
            cpu: (cpu != u32::MAX).then(|| cpu as f32 / 10.0),
            exposure: ipc::ExposureWire {
                dose_pct: h.exposure.dose_pct.load(),
                avg_db: h.exposure.avg_db.load(),
                now_db: h.exposure.now_db.load(),
                shocks: h.exposure.shocks.load(Ordering::Relaxed),
                volume_known: h.exposure.volume_known.load(Ordering::Relaxed),
            },
        }
    }

    fn apply(self: &Arc<Self>, msg: ClientMsg, writer: &SharedWriter) {
        let h = &self.handles;
        match msg {
            ClientMsg::Toggle { key, on } => {
                if let Some(t) = h.toggles().get(key as usize) {
                    if t.is_on() != on {
                        let name = crate::diag::TOGGLE_NAMES.get(key as usize).copied().unwrap_or("switch");
                        crate::log_info!("[action] {name}: {}", if on { "on" } else { "off" });
                    }
                    t.set(on);
                }
            }
            ClientMsg::Preset { key, val } => {
                if let Some(p) = h.presets().get(key as usize) {
                    let old = p.load();
                    if old != val {
                        let name = crate::diag::PRESET_NAMES.get(key as usize).copied().unwrap_or("setting");
                        crate::log_info!("[action] {name}: {old} -> {val}");
                    }
                    p.store(val);
                }
            }
            ClientMsg::SaveSettings(new) => {
                if let Err(e) = crate::sysutil::set_autostart(new.start_with_windows) {
                    crate::log_error!("[clearspeakrt] {e}");
                }
                if let Err(e) = settings::save(&new) {
                    crate::log_error!("[clearspeakrt] failed to persist settings: {e:#}");
                }
                let mut current = self.settings.lock().unwrap();
                for change in crate::diag::settings_diff(&current, &new) {
                    crate::log_info!("[setting] {change}");
                }
                *current = *new;
            }
            ClientMsg::SetDevice { role: r, id } => {
                crate::log_info!("[action] {}: {}", crate::diag::role_name(r), id.as_deref().unwrap_or("(none / Windows default)"));
                // Opening a device and joining the old worker can take a while: never on the pipe thread.
                queue_device_switch(h.device_switcher.clone(), r, id);
            }
            ClientMsg::RefreshDevices => {
                crate::log_info!("[action] refresh devices");
                let this = self.clone();
                let writer = writer.clone();
                let _ = thread::Builder::new().name("clearspeak-device-refresh".into()).spawn(move || {
                    let list = this.handles.device_switcher.enumerate_devices();
                    let _ = send(&writer, &ServerMsg::Devices(list));
                });
            }
            ClientMsg::AccentVoice(v) => {
                crate::log_info!("[action] accent voice: {}", if v == 0 { "off".to_string() } else { format!("voice {v}") });
                h.accent.set_voice(v)
            }
            ClientMsg::TranscriptEnabled(on) => {
                crate::log_info!("[action] live transcript: {}", if on { "on" } else { "off" });
                h.transcript.set_enabled(on)
            }
            ClientMsg::VoiceAutoRelisten => {
                crate::log_info!("[action] accent voice Auto: listen again");
                h.voice_auto.relisten.store(true, Ordering::Relaxed);
            }
            ClientMsg::TranscriptClear => {
                crate::log_info!("[action] transcript cleared");
                h.transcript.clear()
            }
            ClientMsg::Quit => {
                crate::log_info!("[action] quit");
                self.quit()
            }
        }
    }

    /// Tells every window to close, then ends the process at once (see `sysutil::terminate_process`).
    fn quit(&self) -> ! {
        crate::log_info!("[clearspeakrt] engine quitting");
        for w in self.writers.lock().unwrap().iter() {
            let _ = send(w, &ServerMsg::Shutdown);
        }
        thread::sleep(Duration::from_millis(200));
        crate::sysutil::terminate_process();
    }
}

/// Writes one message with a single write call, so two threads (status ticks and replies) can never interleave
/// half-lines.
fn send(writer: &SharedWriter, msg: &ServerMsg) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    ipc::write_msg(&mut bytes, msg)?;
    let mut w = writer.lock().unwrap();
    w.write_all(&bytes)?;
    w.flush()
}

/// Starts the window (`clearspeak.exe` next to this exe). If it is already open, the new copy just brings the existing
/// window to the front and exits.
fn open_window(mode: ipc::ShowMode) {
    let Ok(me) = std::env::current_exe() else { return };
    let ui = me.with_file_name("clearspeak.exe");
    let arg = match mode {
        ipc::ShowMode::Panel => "--panel",
        ipc::ShowMode::Card => "--card",
        ipc::ShowMode::Full => "--full",
    };
    if let Err(e) = std::process::Command::new(&ui).arg(arg).spawn() {
        crate::log_error!("[clearspeakrt] could not open the window ({}): {e}", ui.display());
    }
}

fn serve(host: Arc<Host>, pipe: File) {
    let Ok(read_half) = pipe.try_clone() else { return };
    let writer: SharedWriter = Arc::new(Mutex::new(pipe));
    if send(&writer, &ServerMsg::Hello(Box::new(host.hello()))).is_err() {
        return;
    }
    host.writers.lock().unwrap().push(writer.clone());
    crate::log_info!("[clearspeakrt] window connected");

    let alive = Arc::new(AtomicBool::new(true));
    {
        let (host, writer, alive) = (host.clone(), writer.clone(), alive.clone());
        let _ = thread::Builder::new().name("clearspeak-status".into()).spawn(move || {
            let mut tx = TxState::default();
            while alive.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(250));
                let st = host.status(&mut tx);
                if send(&writer, &ServerMsg::Status(Box::new(st))).is_err() {
                    break;
                }
            }
            alive.store(false, Ordering::Relaxed);
        });
    }

    let mut reader = PipeReader::new(read_half);
    while alive.load(Ordering::Relaxed) {
        match reader.poll::<ClientMsg>() {
            Polled::Msg(m) => host.apply(m, &writer),
            Polled::Empty => thread::sleep(Duration::from_millis(20)),
            Polled::Closed => break,
        }
    }
    alive.store(false, Ordering::Relaxed);
    host.writers.lock().unwrap().retain(|w| !Arc::ptr_eq(w, &writer));
    crate::log_info!("[clearspeakrt] window disconnected");
}

fn toggle_mute(host: &Host) {
    let m = &host.handles.mic_master_enabled;
    m.set(!m.is_on());
}

/// Low-end PC mode, once a second: feed the governor what the audio paths and the accent report, apply its step.
type SwitchJob = (Arc<dyn crate::shared::LiveDeviceSwitcher>, u8, Option<String>);

/// Device switches run one at a time, in the order they were asked for, on one worker thread. A thread per switch
/// let two quick clicks interleave (ring from one switch, device from the other), leaving the mic or the virtual mic
/// silent until the next switch.
fn queue_device_switch(sw: Arc<dyn crate::shared::LiveDeviceSwitcher>, r: u8, id: Option<String>) {
    static QUEUE: std::sync::OnceLock<Mutex<std::sync::mpsc::Sender<SwitchJob>>> = std::sync::OnceLock::new();
    let tx = QUEUE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<SwitchJob>();
        let _ = thread::Builder::new().name("clearspeak-device-switch".into()).spawn(move || {
            while let Ok((sw, r, id)) = rx.recv() {
                match r {
                    role::PHYSICAL_MIC => sw.set_physical_mic(id),
                    role::VIRTUAL_MIC_TARGET => sw.set_virtual_mic_target(id),
                    role::VIRTUAL_SPEAKER_SOURCE => sw.set_virtual_speaker_source(id),
                    role::REAL_SPEAKER_OUTPUT => sw.set_real_speaker_output(id),
                    role::MONITOR => sw.set_monitor_device(id),
                    _ => {}
                }
            }
        });
        Mutex::new(tx)
    });
    let _ = tx.lock().unwrap().send((sw, r, id));
}

/// `last` = each path's processed-frame count a second ago (mic, call audio).
fn perf_second(h: &EngineHandles, g: &mut crate::perf_governor::Governor, last: &mut [u64; 2]) {
    use dsp_core::{classify_status, compute_percentiles, RealtimeStatus};
    const DEADLINE_NS: u64 = (dsp_core::FRAME_SAMPLES as u64 * 1_000_000_000) / dsp_core::SAMPLE_RATE_HZ as u64;
    let change = if h.perf_auto.is_on() {
        // Only a path that processed audio in the last second counts: a stopped path (call audio switched off)
        // keeps its last timings, and they kept saying "overloaded" forever.
        let now = [
            h.counters.frames_processed.load(Ordering::Relaxed),
            h.speaker_counters.frames_processed.load(Ordering::Relaxed),
        ];
        let running = [now[0] > last[0], now[1] > last[1]];
        *last = now;
        let over = |log: &dsp_core::LatencyLog| {
            classify_status(&compute_percentiles(log.snapshot()), DEADLINE_NS, false) == RealtimeStatus::Overloaded
        };
        let accent = h.accent.status();
        g.tick(crate::perf_governor::Signals {
            overloaded: (running[0] && over(&h.latency_log)) || (running[1] && over(&h.speaker_latency_log)),
            accent_strained: running[0]
                && accent.state == accent_onnx::pipeline::AccentState::Active
                && accent.rtf >= crate::perf_governor::ACCENT_STRAINED_RTF,
        })
    } else {
        g.reset()
    };
    if let Some((old, new, reason)) = change {
        h.perf_step.store(new);
        crate::log_info!(
            "[perf] step {old} -> {new} because {reason}; paused now: {}",
            crate::perf_governor::step_pauses(new)
        );
    }
}

/// The engine side of the listening-exposure meter: restores the shift at start-up, feeds the speaker pipeline the
/// current ear offset (headset rating + Windows volume) once a second, saves every minute and starts a new shift
/// after a long break.
/// Every switch and setting, for the per-call log (`call_log`).
fn call_features(h: &EngineHandles) -> Vec<crate::call_log::Feature> {
    use crate::call_log::Feature;
    let mut f: Vec<Feature> =
        h.toggles().iter().zip(crate::diag::TOGGLE_NAMES).map(|(t, name)| Feature::switch(name, t.is_on())).collect();
    f.extend(h.presets().iter().zip(crate::diag::PRESET_NAMES).map(|(p, name)| Feature::value(name, p.load().to_string())));
    let voice = h.accent.voice();
    f.push(Feature::switch("accent", voice > 0));
    let voice_name = match voice {
        0 => "off",
        v => accent_onnx::VOICE_NAMES.get(v as usize - 1).copied().unwrap_or("?"),
    };
    f.push(Feature::value("accent voice", voice_name));
    f.push(Feature::value("accent state", format!("{:?}", h.accent.status().state)));
    f.push(Feature::switch("transcript", h.transcript.enabled()));
    f.push(Feature::switch("Bluetooth call mode", h.counters.hands_free.load(Ordering::Relaxed)));
    f.push(Feature::value("microphone", h.counters.capture_device.lock().map(|d| d.clone()).unwrap_or_default()));
    f
}

/// Figures printed only at the start and the end of a call.
fn call_stats(h: &EngineHandles) -> String {
    format!(
        "listening exposure today {:.0}%, {} loud sound(s) blocked today",
        h.exposure.dose_pct.load(),
        h.exposure.shocks.load(Ordering::Relaxed)
    )
}

struct ExposureTick {
    last_volume: Option<audio_io::volume::OutputVolume>,
}

impl ExposureTick {
    fn start(h: &EngineHandles) -> Self {
        use crate::exposure_store as store;
        let saved = store::carry_over(store::load(), store::now_secs());
        let ex = &h.exposure;
        ex.shocks.store(saved.shocks, Ordering::Relaxed);
        ex.last_heard.store(saved.last_heard, Ordering::Relaxed);
        ex.dose_pct.store((saved.dose * 100.0) as f32);
        ex.avg_db.store(saved.totals().avg_db() as f32);
        *ex.totals.lock().unwrap() = saved.totals();
        *ex.replace.lock().unwrap() = Some(saved.totals());
        crate::log_info!(
            "[exposure] shift so far: {:.0}% of the daily limit, {:.0} min of call audio, {} loud sounds blocked",
            saved.dose * 100.0,
            saved.listen_secs / 60.0,
            saved.shocks
        );
        Self { last_volume: None }
    }

    fn second(&mut self, h: &EngineHandles, settings: &Settings, save_now: bool) {
        use crate::exposure_store as store;
        let ex = &h.exposure;
        let volume = audio_io::volume::output_volume(settings.real_speaker_output_device_id.as_deref());
        let key = |v: Option<audio_io::volume::OutputVolume>| v.map(|v| (v.db.round() as i32, v.muted));
        if key(volume) != key(self.last_volume) {
            match volume {
                Some(v) => crate::log_info!(
                    "[exposure] output volume {:.0} dB{}",
                    v.db,
                    if v.muted { " (muted)" } else { "" }
                ),
                None => crate::log_info!("[exposure] output volume unknown; assuming full volume"),
            }
        }
        self.last_volume = volume;
        ex.volume_known.store(volume.is_some(), Ordering::Relaxed);
        ex.ear_offset_db.store(store::ear_offset(settings.headset_db_spl, volume).unwrap_or(f32::NAN));

        let now = store::now_secs();
        let last_heard = ex.last_heard.load(Ordering::Relaxed);
        if last_heard > 0 && store::is_new_shift(last_heard, now) {
            crate::log_info!("[exposure] {} h with no call audio: starting a new shift", store::BREAK_RESET_SECS / 3600);
            *ex.replace.lock().unwrap() = Some(dsp_core::ExposureTotals::default());
            ex.shocks.store(0, Ordering::Relaxed);
            ex.last_heard.store(0, Ordering::Relaxed);
            ex.dose_pct.store(0.0);
            ex.avg_db.store(0.0);
            store::save(&store::Saved::default());
            return;
        }
        if save_now && last_heard > 0 {
            let t = *ex.totals.lock().unwrap();
            store::save(&store::Saved {
                dose: t.dose,
                listen_secs: t.listen_secs,
                energy: t.energy,
                shocks: ex.shocks.load(Ordering::Relaxed),
                last_heard,
            });
        }
    }
}

/// Entry point of `clearspeak-engine.exe`. Never returns.
pub fn run_engine() {
    if another_instance_running("Local\\ClearSpeak-engine") {
        return;
    }
    // Must happen before anything that touches WASAPI on this thread (see `audio_io::initialize_com_for_this_thread`).
    audio_io::initialize_com_for_this_thread();
    crate::logging::init(&setup::app_data_dir());
    // Names this agent's log folder on the share (`<license name>_<PC>`) before the first line is written.
    crate::logging::set_person(
        crate::license_gate::holder_name(&crate::dpapi::unprotect(&settings::load().license_key)).as_deref(),
    );
    crate::crash::install(&setup::app_data_dir(), "clearspeak.log");
    // audio-io reports everything through one channel; route its informational notes to INFO so the log's ERROR
    // lines are real errors only.
    audio_io::set_error_sink(|msg| match msg.split_once("] INFO: ") {
        Some((_, note)) => crate::log_info!("[audio-io] {note}"),
        None => crate::log_error!("{msg}"),
    });
    crate::log_info!("[clearspeakrt] engine starting");
    if let Some(note) = setup::take_migration_note() {
        crate::log_info!("{note}");
    }

    let saved = settings::load();
    let booted = bootstrap(&saved);
    crate::diag::log_session_header(&booted.settings, &booted.devices);
    if let Err(e) = crate::sysutil::set_autostart(booted.settings.start_with_windows) {
        crate::log_error!("[clearspeakrt] {e}");
    }
    crate::sysutil::start_mute_hotkey();
    crate::tray::start();

    let host = Arc::new(Host {
        handles: booted.handles,
        settings: Mutex::new(booted.settings),
        devices: booted.devices,
        compat: booted.compat,
        bvc_extraction_error: booted.bvc_extraction_error,
        writers: Mutex::new(Vec::new()),
        cpu_tenths: AtomicU32::new(u32::MAX),
    });

    // Accent voice "Auto": male / female detection picks the matching voice (see `voice_auto`).
    {
        let host = host.clone();
        let handles = host.handles.clone();
        let _ = thread::Builder::new()
            .name("clearspeak-voice-auto".into())
            .spawn(move || crate::voice_auto::run(handles, move || host.settings.lock().unwrap_or_else(|p| p.into_inner()).clone()));
    }

    // License gate: every few seconds, switch off the features this PC's license does not allow (nothing without a working license).
    {
        let host = host.clone();
        crate::device_id::start_reading();
        let _ = thread::Builder::new().name("clearspeak-license-gate".into()).spawn(move || {
            let mut last_logged = String::new();
            // What the license allowed at the previous check; a feature allowed again is restored from the settings.
            let mut before: Option<crate::license_gate::Allowed> = None;
            loop {
                thread::sleep(Duration::from_secs(5));
                let devices = crate::device_id::texts();
                if devices.is_empty() {
                    continue; // still reading this PC's identity
                }
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let settings = host.settings.lock().unwrap().clone();
                let status = crate::license_gate::status_for(&settings, &devices, now, crate::clock_guard::registry_mark());
                // Signed in or out: the log moves to that agent's folder on the share.
                crate::logging::set_person(
                    crate::license_gate::holder_name(&crate::dpapi::unprotect(&settings.license_key)).as_deref(),
                );
                // Logged once per change (never the token, name or email): explains "why did noise removal switch off".
                let line = crate::diag::license_line(&status, now);
                if line != last_logged {
                    crate::log_info!("[license] {line}");
                    last_logged = line;
                }
                let allowed = crate::license_gate::allowed(&status);
                crate::license_gate::enforce(&host.handles, allowed);
                if let Some(prev) = before {
                    crate::license_gate::restore(&host.handles, prev, allowed, &settings);
                }
                before = Some(allowed);
            }
        });
    }

    // Tray menu, tray icon clicks and the Ctrl+Shift+M hotkey. They only touch the engine's own switches, so they work
    // with no window open.
    {
        let host = host.clone();
        let _ = thread::Builder::new().name("clearspeak-engine-ui-events".into()).spawn(move || {
            let mut cpu = crate::sysutil::CpuSampler::new();
            let mut ticks = 0u32;
            let mut exposure = ExposureTick::start(&host.handles);
            let mut governor = crate::perf_governor::Governor::new();
            let mut perf_frames = [0u64; 2];
            let mut sys_cpu_prev = crate::sysutil::system_cpu_times_100ns();
            let mut calls = crate::call_log::CallTracker::default();
            let mut watch = crate::diag::StateWatch::default();
            let (mut cpu_sum, mut cpu_max, mut cpu_n) = (0.0f32, 0.0f32, 0u32);
            loop {
                thread::sleep(Duration::from_millis(100));
                for action in crate::tray::take_actions() {
                    match action {
                        crate::tray::TrayAction::Open => open_window(ipc::ShowMode::Panel),
                        crate::tray::TrayAction::OpenFull => open_window(ipc::ShowMode::Full),
                        crate::tray::TrayAction::ToggleMute => toggle_mute(&host),
                        crate::tray::TrayAction::Quit => {
                            for line in calls.end(&call_stats(&host.handles)) {
                                crate::log_info!("{line}");
                            }
                            host.quit()
                        }
                    }
                }
                if crate::sysutil::take_hotkey_pressed() {
                    toggle_mute(&host);
                }
                ticks += 1;
                if ticks % 5 == 0 {
                    watch.check(&host.handles);
                }
                if ticks % 10 == 0 {
                    let settings = host.settings.lock().unwrap().clone();
                    exposure.second(&host.handles, &settings, ticks % 600 == 0);
                    perf_second(&host.handles, &mut governor, &mut perf_frames);
                    let apps = crate::call_log::mic_users();
                    for line in calls.tick(&apps, call_features(&host.handles), &call_stats(&host.handles)) {
                        crate::log_info!("{line}");
                    }
                }
                if ticks % 10 == 0 {
                    if let Some(p) = cpu.sample() {
                        host.cpu_tenths.store((p * 10.0) as u32, Ordering::Relaxed);
                        cpu_sum += p;
                        cpu_max = cpu_max.max(p);
                        cpu_n += 1;
                    }
                }
                if ticks % 600 == 0 && cpu_n > 0 {
                    // The whole PC too, and the power state: an accent that slows down while the engine itself is
                    // quiet means other apps, or Windows power saving, took the CPU.
                    let sys_now = crate::sysutil::system_cpu_times_100ns();
                    let pc = match (sys_cpu_prev, sys_now) {
                        (Some(a), Some(b)) => crate::sysutil::system_cpu_percent(a, b),
                        _ => None,
                    };
                    sys_cpu_prev = sys_now;
                    crate::log_info!(
                        "[health] engine CPU last 60s: avg {:.1}% / worst {:.1}% of the whole PC; all apps {}; {}",
                        cpu_sum / cpu_n as f32,
                        cpu_max,
                        pc.map_or("unknown".to_string(), |p| format!("{p:.0}%")),
                        crate::sysutil::power_line(),
                    );
                    (cpu_sum, cpu_max, cpu_n) = (0.0, 0.0, 0);
                }
            }
        });
    }

    // Started by hand or by the installer (no --background): show the card once. Autostart at sign-in passes
    // --background and stays in the tray.
    if !std::env::args().any(|a| a == "--background") {
        open_window(ipc::ShowMode::Card);
    }

    loop {
        match ipc::accept() {
            Ok(pipe) => {
                let host = host.clone();
                let _ = thread::Builder::new().name("clearspeak-client".into()).spawn(move || serve(host, pipe));
            }
            Err(e) => {
                crate::log_error!("[clearspeakrt] pipe accept failed: {e}");
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}
