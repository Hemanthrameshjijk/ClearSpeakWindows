//! "Test my voice": record 20 seconds from the microphone, then play the same recording back as the original and as
//! each feature would make it sound (noise removed, voices removed, accent, all together), with what was said.
//!
//! The work runs in a helper process (`clearnai-engine.exe --voice-test`) so the live call audio in the engine is never
//! touched: the helper opens the microphone itself (shared mode, next to the engine), loads its own copies of the models,
//! writes WAV files and a transcript per version into `%LOCALAPPDATA%\ClearNAI\voice_test`, and exits (freeing the
//! memory). The window starts it, reads `progress.json` / `result.json` on its tick, and plays the files.
//!
//! The versions use the same stages, order and settings as the live mic path (`engine::spawn_mic_pipeline`): mic boost,
//! then BVC (the chosen backend and strength) or RNNoise, the accent model, AutoGain and the output limiter. Each file
//! is then brought to the same loudness so the comparison is about the sound, not the volume.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Length of the recording.
pub const SECONDS: f32 = 20.0;
/// "Stop" is honoured only after this much audio, so there is something to process.
pub const MIN_SECONDS: f32 = 3.0;
const SR: usize = 48_000;
const FRAME: usize = dsp_core::FRAME_SAMPLES;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Starting,
    Recording,
    Processing,
    Done,
    Failed,
}

/// Written by the helper about 10 times a second while recording, then at every processing step.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub phase: Phase,
    pub recorded_s: f32,
    /// Microphone peak over the last 100 ms, 0..1.
    pub level: f32,
    /// What the helper is preparing now ("Removing noise...").
    pub step: String,
    pub error: String,
}

/// One playable version of the recording.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Version {
    pub key: String,
    pub label: String,
    pub file: PathBuf,
    pub transcript: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Results {
    pub seconds: f32,
    pub versions: Vec<Version>,
    /// Set when the speech model is not installed (the versions still play).
    pub transcript_note: String,
    /// Pre-shift check: one row per measurement (see `preshift`).
    #[serde(default)]
    pub checks: Vec<crate::preshift::Check>,
    #[serde(default)]
    pub verdict: crate::preshift::Verdict,
}

fn history_path() -> PathBuf {
    crate::setup::app_data_dir().join("preshift_history.jsonl")
}

/// One line per pre-shift check, kept for a later report to the supervisor (nothing is sent anywhere today).
#[derive(Serialize)]
struct HistoryLine<'a> {
    at: u64,
    verdict: crate::preshift::Verdict,
    device: &'a str,
    org: &'a str,
    uid: &'a str,
    checks: &'a [crate::preshift::Check],
}

fn append_history(results: &Results, device: &str, who: Option<(String, String)>) {
    use std::io::Write as _;
    let (org, uid) = who.unwrap_or_default();
    let line = HistoryLine {
        at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        verdict: results.verdict,
        device,
        org: &org,
        uid: &uid,
        checks: &results.checks,
    };
    let ok = serde_json::to_string(&line).ok().and_then(|text| {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(history_path()).ok()?;
        writeln!(f, "{text}").ok()
    });
    if ok.is_none() {
        crate::log_error!("[voice-test] could not add to {}", history_path().display());
    }
}

pub fn dir() -> PathBuf {
    crate::setup::app_data_dir().join("voice_test")
}

fn progress_path() -> PathBuf {
    dir().join("progress.json")
}
fn results_path() -> PathBuf {
    dir().join("result.json")
}
fn stop_path() -> PathBuf {
    dir().join("stop")
}

fn write_json<T: Serialize>(path: &Path, v: &T) {
    // Write then rename, so the window never reads half a file.
    let tmp = path.with_extension("tmp");
    if let Ok(bytes) = serde_json::to_vec(v) {
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

// ---------------------------------------------------------------- window side

/// Starts a new test: clears the previous one and launches the helper.
pub fn start() -> Result<(), String> {
    stop_playback();
    let d = dir();
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).map_err(|e| format!("Could not prepare the test folder: {e}"))?;
    write_json(&progress_path(), &Progress::default());
    let exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name("clearnai-engine.exe");
    std::process::Command::new(&exe)
        .arg("--voice-test")
        .spawn()
        .map_err(|e| format!("Could not start the voice test ({e})."))?;
    crate::log_info!("[voice-test] started");
    Ok(())
}

/// Asks the helper to finish recording now (it keeps what it has, if at least `MIN_SECONDS`).
pub fn request_stop() {
    let _ = std::fs::write(stop_path(), b"stop");
}

pub fn read_progress() -> Option<Progress> {
    read_json(&progress_path())
}

pub fn read_results() -> Option<Results> {
    read_json(&results_path())
}

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn PlaySoundW(sound: *const u16, module: isize, flags: u32) -> i32;
}

/// Plays a WAV file on the Windows default output, without waiting. Replaces whatever was playing.
pub fn play(file: &Path) -> bool {
    #[cfg(windows)]
    {
        const SND_ASYNC: u32 = 0x0001;
        const SND_NODEFAULT: u32 = 0x0002;
        const SND_FILENAME: u32 = 0x0002_0000;
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = file.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: a NUL-terminated path; SND_ASYNC copies what it needs before returning.
        unsafe { PlaySoundW(wide.as_ptr(), 0, SND_FILENAME | SND_ASYNC | SND_NODEFAULT) != 0 }
    }
    #[cfg(not(windows))]
    {
        let _ = file;
        false
    }
}

pub fn stop_playback() {
    #[cfg(windows)]
    // SAFETY: a null sound stops any sound this process is playing.
    unsafe {
        PlaySoundW(std::ptr::null(), 0, 0);
    }
}

// ---------------------------------------------------------------- helper process

fn set(p: &mut Progress, f: impl FnOnce(&mut Progress)) {
    f(p);
    write_json(&progress_path(), p);
}

fn fail(p: &mut Progress, msg: String) {
    crate::log_error!("[voice-test] {msg}");
    set(p, |p| {
        p.phase = Phase::Failed;
        p.error = msg;
    });
}

/// Entry point of `clearnai-engine.exe --voice-test`. Returns when the files are written (or on failure).
pub fn run_helper() {
    let app = crate::setup::app_data_dir();
    crate::logging::init_named(&app, "clearnai-voicetest.log");
    crate::crash::install(&app, "clearnai-voicetest.log");
    audio_io::initialize_com_for_this_thread();
    audio_io::set_error_sink(|msg| match msg.split_once("] INFO: ") {
        Some((_, note)) => crate::log_info!("[audio-io] {note}"),
        None => crate::log_error!("{msg}"),
    });
    let _ = std::fs::create_dir_all(dir());
    let mut p = Progress::default();
    if crate::host::another_instance_running("Local\\ClearNAI-voice-test") {
        return fail(&mut p, "A voice test is already running. Wait for it to finish and try again.".into());
    }
    let settings = crate::settings::load();
    crate::device_id::start_reading();
    let t0 = Instant::now();

    let (rec, capture_info) = match record(&settings, &mut p) {
        Ok(r) => r,
        Err(e) => return fail(&mut p, e),
    };
    let mut checks = crate::preshift::analyse(&rec, SR, &capture_info);
    crate::log_info!("[voice-test] recorded {:.1} s in {} ms", rec.len() as f32 / SR as f32, t0.elapsed().as_millis());

    set(&mut p, |p| {
        p.phase = Phase::Processing;
        p.step = "Preparing...".into();
    });
    let (allowed, who) = license_allowed(&settings);
    match process(&settings, allowed, &rec, &mut p, &mut checks) {
        Ok(mut results) => {
            results.verdict = crate::preshift::overall(&checks);
            for c in &checks {
                crate::log_info!("[pre-shift] {}: {} -> {}", c.label, c.value, c.verdict.word());
            }
            crate::log_info!("[pre-shift] {} on '{}'", crate::preshift::overall_line(results.verdict), capture_info.device);
            results.checks = checks;
            append_history(&results, &capture_info.device, who);
            write_json(&results_path(), &results);
            crate::log_info!(
                "[voice-test] done in {} ms: {}",
                t0.elapsed().as_millis(),
                results.versions.iter().map(|v| v.key.as_str()).collect::<Vec<_>>().join(", ")
            );
            set(&mut p, |p| {
                p.phase = Phase::Done;
                p.step.clear();
            });
        }
        Err(e) => fail(&mut p, e),
    }
}

/// What this PC's license allows, read the same way as the engine's license gate (so editing files cannot unlock a
/// feature here). Waits up to 10 s for the PC identity (usually ready long before the recording ends).
fn license_allowed(settings: &crate::settings::Settings) -> (crate::license_gate::Allowed, Option<(String, String)>) {
    let until = Instant::now() + Duration::from_secs(10);
    let mut devices = crate::device_id::texts();
    while devices.is_empty() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
        devices = crate::device_id::texts();
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let status = crate::license_gate::status_for(settings, &devices, now, crate::clock_guard::registry_mark());
    crate::log_info!("[voice-test] {}", crate::diag::license_line(&status, now));
    // Organisation and user ids (not names) for the pre-shift history.
    let who = match &status {
        license::Status::Licensed(p) => Some((p.org.clone(), p.uid.clone())),
        _ => None,
    };
    (crate::license_gate::allowed(&status), who)
}

fn record(settings: &crate::settings::Settings, p: &mut Progress) -> Result<(Vec<f32>, crate::preshift::CaptureInfo), String> {
    let (producer, mut consumer) = dsp_core::make_ring_buffer(2.0);
    let counters = std::sync::Arc::new(dsp_core::FrameCounters::default());
    let capture = match settings.physical_mic_device_id.as_deref() {
        Some(id) => audio_io::capture::MicCapture::start_on_device_id(id, producer, counters.clone()),
        None => audio_io::capture::MicCapture::start_default(producer, counters.clone()),
    }
    .map_err(|e| format!("Could not open the microphone ({e:#})."))?;

    let target = (SECONDS * SR as f32) as usize;
    let mut rec: Vec<f32> = Vec::with_capacity(target);
    let started = Instant::now();
    set(p, |p| p.phase = Phase::Recording);
    let mut last_report = Instant::now();
    let mut peak = 0.0f32;
    while rec.len() < target {
        std::thread::sleep(Duration::from_millis(20));
        while let Ok(s) = consumer.pop() {
            if rec.len() < target {
                rec.push(s);
                peak = peak.max(s.abs());
            }
        }
        if rec.is_empty() && started.elapsed() > Duration::from_secs(6) {
            capture.stop();
            return Err("The microphone sent no sound. Check the microphone choice and try again.".into());
        }
        let secs = rec.len() as f32 / SR as f32;
        if secs >= MIN_SECONDS && stop_path().exists() {
            break;
        }
        if last_report.elapsed() >= Duration::from_millis(100) {
            set(p, |p| {
                p.recorded_s = secs;
                p.level = peak.min(1.0);
            });
            peak = 0.0;
            last_report = Instant::now();
        }
        if started.elapsed() > Duration::from_secs_f32(SECONDS + 15.0) {
            break; // the device delivers too slowly; keep what we have
        }
    }
    capture.stop();
    set(p, |p| {
        p.recorded_s = rec.len() as f32 / SR as f32;
        p.level = 0.0;
    });
    if (rec.len() as f32) < MIN_SECONDS * SR as f32 {
        return Err("The recording is too short. Try again and speak for at least a few seconds.".into());
    }
    use std::sync::atomic::Ordering::Relaxed;
    let info = crate::preshift::CaptureInfo {
        gaps: counters.capture_gaps.load(Relaxed),
        silent_packets: counters.capture_silent_packets.load(Relaxed),
        hands_free: counters.hands_free.load(Relaxed),
        device: counters.capture_device.lock().map(|d| d.clone()).unwrap_or_default(),
        boost_db: dsp_core::mic_boost_effective(settings.mic_boost_on, settings.mic_boost_db) as f64,
    };
    crate::log_info!(
        "[voice-test] capture: {} gap(s), {} silent packet(s){}",
        info.gaps,
        info.silent_packets,
        if info.hands_free { ", Bluetooth call mode" } else { "" }
    );
    Ok((rec, info))
}

/// Runs `stages` over `x` in 10 ms frames, as the live mic thread does.
fn run_frames(x: &[f32], stages: &mut [&mut dyn dsp_core::Stage]) -> Vec<f32> {
    let mut out = Vec::with_capacity(x.len().div_ceil(FRAME) * FRAME);
    let mut frame = [0.0f32; FRAME];
    for c in x.chunks(FRAME) {
        frame[..c.len()].copy_from_slice(c);
        frame[c.len()..].fill(0.0);
        for s in stages.iter_mut() {
            s.process(&mut frame);
        }
        out.extend_from_slice(&frame);
    }
    out.truncate(x.len());
    out
}

/// AutoGain then the output limiter: the end of the live chain.
fn finish(x: &[f32]) -> Vec<f32> {
    let mut agc = dsp_core::AutoGainStage::new();
    let mut lim = dsp_core::OutputLimiter::new();
    run_frames(x, &mut [&mut agc, &mut lim])
}

/// Brings every version to the same speech loudness (about -20 dBFS RMS over the louder half of the 10 ms frames),
/// never past a 0.95 peak, so the comparison is about the sound and not the volume.
pub fn level_match(x: &mut [f32]) {
    let mut energies: Vec<f32> = x.chunks(FRAME).map(|c| c.iter().map(|v| v * v).sum::<f32>() / c.len() as f32).collect();
    if energies.is_empty() {
        return;
    }
    energies.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let loud = &energies[..energies.len().div_ceil(2)];
    let rms = (loud.iter().sum::<f32>() / loud.len() as f32).sqrt();
    if rms < 1e-5 {
        return;
    }
    let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs())).max(1e-6);
    let gain = (0.1 / rms).min(0.95 / peak);
    for v in x.iter_mut() {
        *v *= gain;
    }
}

/// 16-bit PCM mono WAV at 48 kHz.
pub fn write_wav(path: &Path, x: &[f32]) -> std::io::Result<()> {
    let data_len = (x.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + x.len() * 2);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // mono
    b.extend_from_slice(&(SR as u32).to_le_bytes());
    b.extend_from_slice(&(SR as u32 * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for v in x {
        b.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, b)
}

/// The BVC stage the user has chosen (Hush when the choice is Off, since the test is about hearing BVC).
fn load_bvc(settings: &crate::settings::Settings) -> Result<Box<dyn dsp_core::Stage>, String> {
    use tse_engine::BvcBackend;
    let assets = crate::setup::assets_dir();
    let strength = dsp_core::SharedPreset::new(settings.mic_nr_strength);
    match BvcBackend::from_index(settings.mic_bvc_backend) {
        BvcBackend::DeepFilter => noise_engine::load_bundled_deep_filter()
            .map(|e| Box::new(e) as Box<dyn dsp_core::Stage>)
            .map_err(|e| format!("{e:#}")),
        BvcBackend::HushBpoV1 => {
            bvc_hush::HushBvcStage::try_load_with_model(&assets, bvc_hush::DLL_FILE_NAME, bvc_hush::BPO_V1_MODEL_BUNDLE_FILE_NAME)
                .map(|mut s| {
                    s.set_strength_source(strength);
                    Box::new(s) as Box<dyn dsp_core::Stage>
                })
                .map_err(|e| e.to_string())
        }
        _ => bvc_hush::HushBvcStage::try_load(&assets)
            .map(|mut s| {
                s.set_strength_source(strength);
                Box::new(s) as Box<dyn dsp_core::Stage>
            })
            .map_err(|e| e.to_string()),
    }
}

fn process(
    settings: &crate::settings::Settings,
    allowed: crate::license_gate::Allowed,
    rec: &[f32],
    p: &mut Progress,
    checks: &mut Vec<crate::preshift::Check>,
) -> Result<Results, String> {
    let step = |p: &mut Progress, s: &str| {
        crate::log_info!("[voice-test] {s}");
        set(p, |p| p.step = s.to_string());
    };
    // Mic boost, as the live path applies it first.
    let mut boost = dsp_core::PreGainStage::new();
    boost.set_db(dsp_core::mic_boost_effective(settings.mic_boost_on, settings.mic_boost_db) as f32);
    let pre = run_frames(rec, &mut [&mut boost]);

    let mut versions: Vec<(String, String, Vec<f32>)> = vec![("original".into(), "Original (no ClearSpeak)".into(), pre.clone())];

    if allowed.nc {
        step(p, "Removing noise...");
        let t = Instant::now();
        let mut rn = noise_engine::build(noise_engine::NoiseEngineKind::RnNoise, None).map_err(|e| format!("{e:#}"))?;
        let mut agc = dsp_core::AutoGainStage::new();
        let mut lim = dsp_core::OutputLimiter::new();
        let x = run_frames(&pre, &mut [&mut agc, rn.as_mut(), &mut lim]);
        crate::log_info!("[voice-test] noise removal in {} ms", t.elapsed().as_millis());
        versions.push(("nc".into(), "Noise removed (NC)".into(), x));
    }

    let mut bvc_raw: Option<Vec<f32>> = None;
    if allowed.bvc {
        step(p, "Removing background voices...");
        let t = Instant::now();
        match load_bvc(settings) {
            Ok(mut stage) => {
                let raw = run_frames(&pre, &mut [stage.as_mut()]);
                crate::log_info!("[voice-test] background-voice removal in {} ms", t.elapsed().as_millis());
                versions.push(("bvc".into(), "Background voices removed (BVC)".into(), finish(&raw)));
                bvc_raw = Some(raw);
            }
            Err(e) => crate::log_error!("[voice-test] BVC could not load: {e}"),
        }
    }

    let mut accent_dir_and_dll = None;
    if allowed.accent {
        let dll = crate::setup::onnxruntime_dll_path(&crate::setup::assets_dir());
        match (accent_onnx::init_runtime(&dll), accent_onnx::find_models_dir()) {
            (Ok(()), Some(models)) => accent_dir_and_dll = Some(models),
            (Err(e), _) => crate::log_error!("[voice-test] accent runtime: {e}"),
            (_, None) => crate::log_error!("[voice-test] accent models not found"),
        }
    }
    if let Some(models) = &accent_dir_and_dll {
        let threads = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(2, 8) / 2;
        let voice = settings.accent_voice_index();
        let voice_label = accent_onnx::VOICE_CHOICES[(voice - 1) as usize];
        step(p, "Converting the accent...");
        let t = Instant::now();
        let acc = accent_onnx::pipeline::convert_offline(models, threads, voice, &pre)?;
        crate::log_info!("[voice-test] accent ({voice_label}) in {} ms", t.elapsed().as_millis());
        // Offline conversion uses about the threads the live accent does, so its speed predicts whether the
        // live accent will keep up on this PC.
        checks.push(crate::preshift::speed_check(t.elapsed().as_secs_f64(), pre.len() as f64 / SR as f64));
        versions.push(("accent".into(), format!("Accent ({voice_label})"), finish(&acc)));

        if allowed.nc || allowed.bvc {
            step(p, "Putting it all together...");
            let all = match &bvc_raw {
                // Live order with BVC on: BVC -> Accent -> AutoGain -> limiter.
                Some(raw) => finish(&accent_onnx::pipeline::convert_offline(models, threads, voice, raw)?),
                // Live order with BVC off: Accent -> AutoGain -> RNNoise -> limiter.
                None => {
                    let mut agc = dsp_core::AutoGainStage::new();
                    let mut rn = noise_engine::build(noise_engine::NoiseEngineKind::RnNoise, None).map_err(|e| format!("{e:#}"))?;
                    let mut lim = dsp_core::OutputLimiter::new();
                    run_frames(&acc, &mut [&mut agc, rn.as_mut(), &mut lim])
                }
            };
            versions.push(("all".into(), "All together".into(), all));
        }
    }

    step(p, "Writing what you said...");
    let mut asr = None;
    let mut transcript_note = String::new();
    let dll = crate::setup::onnxruntime_dll_path(&crate::setup::assets_dir());
    match (accent_onnx::init_runtime(&dll), asr_onnx::find_models_dir()) {
        (Ok(()), Some(d)) => match asr_onnx::AsrModel::load(&d, 2) {
            Ok(m) => asr = Some(m),
            Err(e) => transcript_note = format!("Speech model could not load: {e}"),
        },
        (Err(e), _) => transcript_note = format!("Speech runtime could not load: {e}"),
        (_, None) => transcript_note = "The speech model is not installed, so there is no text.".into(),
    }
    if !transcript_note.is_empty() {
        crate::log_error!("[voice-test] {transcript_note}");
    }

    let mut out = Vec::new();
    for (key, label, mut x) in versions {
        level_match(&mut x);
        let file = dir().join(format!("{key}.wav"));
        write_wav(&file, &x).map_err(|e| format!("Could not save the {label} version: {e}"))?;
        let transcript = match asr.as_mut() {
            Some(m) => {
                let t = Instant::now();
                let mut down = accent_onnx::causal_resampler::CausalDownsampler3x::new(97);
                let x16 = down.push(&x);
                let text = m.transcribe(&x16).unwrap_or_else(|e| {
                    crate::log_error!("[voice-test] transcript of {key} failed: {e}");
                    String::new()
                });
                crate::log_info!("[voice-test] transcript of {key} in {} ms ({} words)", t.elapsed().as_millis(), text.split_whitespace().count());
                text
            }
            None => String::new(),
        };
        out.push(Version { key, label, file, transcript });
    }
    Ok(Results { seconds: rec.len() as f32 / SR as f32, versions: out, transcript_note, ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_match_brings_quiet_and_loud_to_the_same_level() {
        let tone = |amp: f32| -> Vec<f32> { (0..48_000).map(|i| amp * (i as f32 * 0.05).sin()).collect() };
        let (mut a, mut b) = (tone(0.01), tone(0.5));
        level_match(&mut a);
        level_match(&mut b);
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        assert!((rms(&a) - rms(&b)).abs() < 1e-3, "{} vs {}", rms(&a), rms(&b));
        assert!(a.iter().all(|v| v.abs() <= 0.95 + 1e-4));
    }

    #[test]
    fn wav_header_is_valid() {
        let p = std::env::temp_dir().join(format!("clearnai-vt-{}.wav", std::process::id()));
        write_wav(&p, &[0.0, 0.5, -0.5]).unwrap();
        let b = std::fs::read(&p).unwrap();
        assert_eq!(&b[..4], b"RIFF");
        assert_eq!(&b[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes([b[24], b[25], b[26], b[27]]), 48_000);
        assert_eq!(b.len(), 44 + 6);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn run_frames_keeps_length() {
        let mut lim = dsp_core::OutputLimiter::new();
        assert_eq!(run_frames(&vec![0.1; 1234], &mut [&mut lim]).len(), 1234);
    }
}
