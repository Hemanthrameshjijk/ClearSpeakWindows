//! Real-time placement of the accent engine: the 10 ms mic pipeline thread only touches two lock-free rings;
//! all model work happens on a dedicated worker thread.
//!
//! ```text
//!  mic thread (480 @ 48 kHz, no allocation)                worker thread (50 ms pushes)
//!  AccentStage::process --in ring--> 48k -> 16k (causal) -> leveler -> AccentStream (mel, converter,
//!        ^                                                              vocoder) -> 24k -> 48k (causal)
//!        +-- jitter buffer <--out ring-- 480 @ 48 kHz  <-----------------------------------+
//! ```
//!
//! Safety net: if the output ring runs dry, or the worker's real-time factor stays above 0.97 for 5 s, the stage
//! crossfades (20 ms) to the incoming (BVC-only) signal, keeps the engine running, and reports a "paused"
//! state; it returns to the accent output once the worker has caught up and the jitter buffer has refilled.

use crate::resample::CausalUpsampler2x;
use crate::{find_models_dir, init_runtime, AccentStream, VOICE_NAMES};
use dsp_core::{Consumer, Producer, RingBuffer, Stage, FRAME_SAMPLES};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use crate::causal_resampler::CausalDownsampler3x;

/// Worker push size: 100 ms of 48 kHz audio (-> 1600 samples at 16 kHz = 8 mel frames). 100 ms needs ~35% less CPU
/// than 50 ms (RTF 0.45 vs 0.69 at 4 threads) for +25 ms delay, which keeps it real-time when other apps load the PC.
const PUSH48: usize = 4800;
const PUSH16: usize = 1600;
const PUSH_MS: f32 = 100.0;
/// Input backlog (48 kHz samples) above which the worker skips ahead instead of falling further behind.
const MAX_BACKLOG48: usize = 4 * PUSH48;
const RING_SECONDS: usize = 1;
const RAMP_SAMPLES: f32 = 1920.0; // 40 ms crossfade
const REPRIME_HOLDOFF_FRAMES: u32 = 100; // 1 s of dry after a long stall before trusting the accent again
/// A short shortfall (the worker is a little late) is covered with silence in the converted voice instead of switching
/// to the plain voice and re-priming: the old way lost ~0.5 s of speech and then repeated ~0.5 s (heard as a skip).
/// Only a stall longer than this many 10 ms frames falls back to the plain voice.
const MAX_CONCEAL_FRAMES: u32 = 30;
/// Edge ramp (samples) around a concealed gap, so the silence does not click.
const GAP_RAMP: usize = 96;
/// The jitter buffer is trimmed back only after this many quiet 10 ms frames in a row (real silence, not a quiet
/// moment inside a word) and only if the audio about to be dropped is quiet too.
const QUIET_FRAMES_BEFORE_TRIM: u32 = 15;
/// Starting jitter buffer, ms. 100 ms was too tight: one slow inference push (another app, Transcript) drained it.
pub const DEFAULT_JITTER_MS: u32 = 160;
// RTF 1.0 is the real limit (above it the output falls behind). Pausing at 0.9 for 2 s flapped on PCs that run right at
// 0.90-0.95 (log: paused/resumed every few seconds, each a 40 ms crossfade the listener hears), so pause only when
// clearly over, for 5 s, and resume only once clearly under, for 3 s.
const SLOW_RTF: f32 = 0.97;
const SLOW_WINDOW: usize = 50; // pushes = 5 s
// Must sit clearly above the normal running RTF (0.55-0.8 on this PC) or a paused stage could never recover.
const RECOVER_RTF: f32 = 0.80;
const RECOVER_WINDOW: usize = 30; // pushes = 3 s
const LEVEL_TARGET_RMS: f32 = 0.06;
const RESAMPLER_TAPS: usize = 97;

/// 0 = off, 1..=6 = `VOICE_NAMES[voice - 1]`.
pub const ACCENT_OFF: u8 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccentState {
    Off,
    Loading,
    Starting,
    Active,
    /// Crossfaded back to the BVC-only signal (worker too slow / output ran dry).
    Paused,
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct AccentStatus {
    pub voice: u8,
    pub state: AccentState,
    /// Worker real-time factor (compute time / audio time), smoothed over ~2 s. 0 until measured.
    pub rtf: f32,
    /// Estimated total delay the accent stage adds, ms (model look-ahead + resamplers + push + jitter + compute).
    pub latency_ms: f32,
    pub underruns: u32,
    /// Current jitter buffer (grows after underruns, up to 300 ms).
    pub jitter_ms: u32,
    pub dropped_input: u32,
    pub catchups: u32,
    pub message: String,
}

impl AccentStatus {
    pub fn line(&self) -> String {
        match self.state {
            AccentState::Off => "Accent: off".into(),
            AccentState::Loading => "Accent: loading model...".into(),
            AccentState::Starting => "Accent: starting...".into(),
            AccentState::Active => {
                let mut l = format!("Accent: on | RTF {:.2} | +{:.0} ms", self.rtf, self.latency_ms);
                if self.underruns + self.catchups + self.dropped_input > 0 {
                    l += &format!(" | glitches: {} underruns, {} skips", self.underruns, self.catchups + self.dropped_input.min(1));
                }
                l
            }
            AccentState::Paused => format!("Accent paused - CPU too slow (RTF {:.2})", self.rtf),
            AccentState::Unavailable => format!("Accent unavailable: {}", self.message),
        }
    }
}

// worker states
const W_IDLE: u8 = 0;
const W_LOADING: u8 = 1;
const W_READY: u8 = 2;
const W_FAILED: u8 = 3;
// mic-side states
const M_IDLE: u8 = 0;
const M_STARTING: u8 = 1;
const M_ACTIVE: u8 = 2;
const M_PAUSED: u8 = 3;

struct Shared {
    voice: AtomicU8,
    worker: AtomicU8,
    mic: AtomicU8,
    generation: AtomicU32,
    slow: AtomicBool,
    stop: AtomicBool,
    rtf_milli: AtomicU32,
    rtf_conv_milli: AtomicU32,
    rtf_voc_milli: AtomicU32,
    latency_x10: AtomicU32,
    jitter_ms: AtomicU32,
    underruns: AtomicU32,
    dropped_input: AtomicU32,
    catchups: AtomicU32,
    error: Mutex<String>, // never locked on the mic thread
}

/// GUI-side handle: choose the voice, read the status.
#[derive(Clone)]
pub struct AccentHandle(Arc<Shared>);

impl AccentHandle {
    /// A handle for builds where accent conversion cannot run at all (reports `Unavailable` with `reason`).
    pub fn unavailable(reason: &str) -> Self {
        let (_stage, handle) = start_inner(None, reason);
        handle
    }
    pub fn set_voice(&self, voice: u8) {
        self.0.voice.store(voice.min(VOICE_NAMES.len() as u8), Relaxed);
    }
    pub fn voice(&self) -> u8 {
        self.0.voice.load(Relaxed)
    }
    pub fn status(&self) -> AccentStatus {
        let s = &self.0;
        let voice = s.voice.load(Relaxed);
        let w = s.worker.load(Relaxed);
        let state = if w == W_FAILED {
            AccentState::Unavailable
        } else if voice == ACCENT_OFF {
            AccentState::Off
        } else if w == W_LOADING || w == W_IDLE {
            AccentState::Loading
        } else {
            match s.mic.load(Relaxed) {
                M_ACTIVE => AccentState::Active,
                M_PAUSED => AccentState::Paused,
                _ => AccentState::Starting,
            }
        };
        AccentStatus {
            voice,
            state,
            rtf: s.rtf_milli.load(Relaxed) as f32 / 1000.0,
            latency_ms: s.latency_x10.load(Relaxed) as f32 / 10.0,
            underruns: s.underruns.load(Relaxed),
            jitter_ms: s.jitter_ms.load(Relaxed),
            dropped_input: s.dropped_input.load(Relaxed),
            catchups: s.catchups.load(Relaxed),
            message: s.error.lock().map(|m| m.clone()).unwrap_or_default(),
        }
    }
    /// For a handle that mirrors an engine running in another process: copies the engine's reported status into this
    /// handle's atomics so [`status`](Self::status) reads the same. The voice is not touched (the UI owns it).
    pub fn apply_remote(&self, st: &AccentStatus) {
        let s = &self.0;
        match st.state {
            // Off (voice switched off in the engine): clear the "unavailable" a freshly created mirror starts with.
            AccentState::Off => s.worker.store(W_IDLE, Relaxed),
            AccentState::Loading => s.worker.store(W_LOADING, Relaxed),
            AccentState::Unavailable => {
                s.worker.store(W_FAILED, Relaxed);
                if let Ok(mut m) = s.error.lock() {
                    *m = st.message.clone();
                }
            }
            AccentState::Starting | AccentState::Active | AccentState::Paused => {
                s.worker.store(W_READY, Relaxed);
                s.mic.store(
                    match st.state {
                        AccentState::Active => M_ACTIVE,
                        AccentState::Paused => M_PAUSED,
                        _ => M_STARTING,
                    },
                    Relaxed,
                );
            }
        }
        s.rtf_milli.store((st.rtf * 1000.0) as u32, Relaxed);
        s.latency_x10.store((st.latency_ms * 10.0) as u32, Relaxed);
        s.underruns.store(st.underruns, Relaxed);
        s.jitter_ms.store(st.jitter_ms, Relaxed);
        s.dropped_input.store(st.dropped_input, Relaxed);
        s.catchups.store(st.catchups, Relaxed);
    }
    /// (converter RTF, vocoder RTF) of the last measurement window.
    pub fn rtf_split(&self) -> (f32, f32) {
        (self.0.rtf_conv_milli.load(Relaxed) as f32 / 1000.0, self.0.rtf_voc_milli.load(Relaxed) as f32 / 1000.0)
    }
}

pub struct AccentConfig {
    /// `None`: search with [`find_models_dir`].
    pub models_dir: Option<PathBuf>,
    pub onnxruntime_dll: PathBuf,
    /// ORT intra-op threads for both sessions.
    pub threads: usize,
    /// Fixed jitter buffer on the output side.
    pub jitter_ms: u32,
    pub initial_voice: u8,
    /// Runs once on the worker thread right before the first model load (e.g. the app's "compare and install the
    /// bundle" step) and returns the directory to load from. Takes precedence over `models_dir`.
    pub prepare_models: Option<Box<dyn FnOnce() -> Result<PathBuf, String> + Send>>,
}

/// ORT intra-op threads for the accent sessions, chosen from the machine's logical core count so the model never
/// takes cores the audio threads need. Override: `CLEARSPEAK_ACCENT_THREADS`.
///
/// Above 8 cores this keeps the original rule, measured on a 12-thread PC with the full chain (Hush + Accent +
/// AutoGain, `accent_bench live`): 4 threads -> mic-thread p99 10.5 ms (over its 10 ms deadline), 2 underruns, +747 ms
/// added delay and an RTF close to the 0.9 pause limit; 8 threads -> p99 5.7 ms, 1 underrun, +477 ms. The smaller
/// tiers below are ESTIMATES (leave room for ~5 audio/worker threads), NOT measured on an i3 or any other small CPU.
pub fn accent_threads_for_cores(cores: usize) -> usize {
    match cores {
        0..=2 => 1,
        3..=4 => cores / 2,
        5..=8 => cores.saturating_sub(2).max(2),
        _ => cores.saturating_sub(4).clamp(4, 8),
    }
}

impl AccentConfig {
    pub fn new(onnxruntime_dll: PathBuf) -> Self {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let threads = std::env::var("CLEARSPEAK_ACCENT_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| accent_threads_for_cores(cores));
        Self { models_dir: None, onnxruntime_dll, threads, jitter_ms: DEFAULT_JITTER_MS, initial_voice: ACCENT_OFF, prepare_models: None }
    }
}

/// The mic-thread end. Implements [`Stage`]: `process` is allocation-free and never blocks.
pub struct AccentStage {
    sh: Arc<Shared>,
    in_prod: Producer<f32>,
    out_cons: Consumer<f32>,
    wet: [f32; FRAME_SAMPLES],
    jitter: usize,
    base_jitter: usize,
    /// Frames since the last underrun (drives the jitter-buffer decay).
    stable: u32,
    /// Consecutive frames covered with silence (a short shortfall); past `MAX_CONCEAL_FRAMES` it is a real stall.
    short_run: u32,
    /// Consecutive quiet converted frames (the jitter buffer is trimmed only in real silence).
    quiet_run: u32,
    /// Ramp the next real frame in after a concealed gap.
    fade_in: bool,
    primed: bool,
    mix: f32,
    was_on: bool,
    holdoff: u32,
}

/// Start the worker thread and return the mic-thread stage plus the GUI handle. The (large) model bundle is
/// loaded lazily the first time a voice is selected, so a user who leaves Accent off pays nothing.
pub fn start(cfg: AccentConfig) -> (AccentStage, AccentHandle) {
    start_inner(Some(cfg), "")
}

fn start_inner(cfg: Option<AccentConfig>, unavailable_reason: &str) -> (AccentStage, AccentHandle) {
    let initial_voice = cfg.as_ref().map_or(ACCENT_OFF, |c| c.initial_voice);
    let jitter_ms = cfg.as_ref().map_or(DEFAULT_JITTER_MS, |c| c.jitter_ms);
    let sh = Arc::new(Shared {
        voice: AtomicU8::new(initial_voice.min(VOICE_NAMES.len() as u8)),
        worker: AtomicU8::new(W_IDLE),
        mic: AtomicU8::new(M_IDLE),
        generation: AtomicU32::new(0),
        slow: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        rtf_milli: AtomicU32::new(0),
        rtf_conv_milli: AtomicU32::new(0),
        rtf_voc_milli: AtomicU32::new(0),
        latency_x10: AtomicU32::new(0),
        jitter_ms: AtomicU32::new(jitter_ms),
        underruns: AtomicU32::new(0),
        dropped_input: AtomicU32::new(0),
        catchups: AtomicU32::new(0),
        error: Mutex::new(String::new()),
    });
    let cap = 48_000 * RING_SECONDS;
    let (in_prod, in_cons) = RingBuffer::<f32>::new(cap);
    let (out_prod, out_cons) = RingBuffer::<f32>::new(cap);
    let jitter = (jitter_ms as usize * 48).max(FRAME_SAMPLES);
    match cfg {
        Some(cfg) => {
            let w_sh = sh.clone();
            let spawned = std::thread::Builder::new()
                .name("clearspeak-accent-worker".into())
                .spawn(move || worker(w_sh, in_cons, out_prod, cfg));
            if let Err(e) = spawned {
                sh.worker.store(W_FAILED, Relaxed);
                set_error(&sh, format!("could not start worker thread: {e}"));
            }
        }
        None => {
            sh.worker.store(W_FAILED, Relaxed);
            set_error(&sh, unavailable_reason.to_string());
        }
    }
    let stage = AccentStage {
        sh: sh.clone(),
        in_prod,
        out_cons,
        wet: [0.0; FRAME_SAMPLES],
        jitter,
        base_jitter: jitter,
        stable: 0,
        short_run: 0,
        quiet_run: 0,
        fade_in: false,
        primed: false,
        mix: 0.0,
        was_on: false,
        holdoff: 0,
    };
    (stage, AccentHandle(sh))
}

impl Drop for AccentStage {
    fn drop(&mut self) {
        self.sh.stop.store(true, Relaxed);
    }
}

/// True when the next `n` converted samples are all quiet (looked at without consuming them).
fn upcoming_is_quiet(cons: &mut Consumer<f32>, n: usize) -> bool {
    match cons.read_chunk(n) {
        // Dropped without `commit`: nothing is consumed.
        Ok(chunk) => {
            let (a, b) = chunk.as_slices();
            a.iter().chain(b.iter()).all(|v| v.abs() < 0.003)
        }
        Err(_) => false,
    }
}

impl Stage for AccentStage {
    fn name(&self) -> &'static str {
        "Accent"
    }

    fn process(&mut self, frame: &mut [f32]) {
        let sh = &*self.sh;
        if sh.voice.load(Relaxed) == ACCENT_OFF {
            if self.was_on {
                self.was_on = false;
                self.primed = false;
                self.mix = 0.0;
                sh.mic.store(M_IDLE, Relaxed);
            }
            return;
        }
        if !self.was_on {
            // Off -> on: drop stale audio, tell the worker to reset its streaming state.
            while self.out_cons.pop().is_ok() {}
            sh.generation.fetch_add(1, Relaxed);
            self.was_on = true;
            self.primed = false;
            self.mix = 0.0;
            self.holdoff = 0;
            sh.mic.store(M_STARTING, Relaxed);
        }
        if sh.worker.load(Relaxed) != W_READY || frame.len() != FRAME_SAMPLES {
            return; // loading or failed: pass the BVC signal through untouched
        }

        for &s in frame.iter() {
            if self.in_prod.push(s).is_err() {
                sh.dropped_input.fetch_add(1, Relaxed);
            }
        }

        let mut have = false;
        let avail = self.out_cons.slots();
        if !self.primed {
            // The worker delivers audio in bursts of one push (50 ms). Start with `jitter + one push` buffered so
            // the level then cycles between `jitter + push` (just after a burst) and `jitter` (just before the
            // next one): the jitter setting is the real safety margin, not a fraction of it.
            let target = self.jitter + PUSH48;
            if avail >= target + FRAME_SAMPLES {
                for _ in 0..avail - target - FRAME_SAMPLES {
                    let _ = self.out_cons.pop(); // skip audio that is older than the target level
                }
                self.primed = true;
            }
        }
        if self.primed {
            let n = self.out_cons.slots();
            if n >= FRAME_SAMPLES {
                for w in self.wet.iter_mut() {
                    *w = self.out_cons.pop().unwrap_or(0.0);
                }
                if std::mem::take(&mut self.fade_in) {
                    for (i, w) in self.wet.iter_mut().take(GAP_RAMP).enumerate() {
                        *w *= i as f32 / GAP_RAMP as f32;
                    }
                }
                self.short_run = 0;
                have = true;
            } else {
                if self.short_run == 0 {
                    // One count per gap. Adaptive jitter buffer: each gap asks for 40 ms more cushion (max 300 ms).
                    sh.underruns.fetch_add(1, Relaxed);
                    self.jitter = (self.jitter + 40 * 48).min(300 * 48);
                    sh.jitter_ms.store((self.jitter / 48) as u32, Relaxed);
                }
                self.short_run += 1;
                self.stable = 0;
                if self.short_run > MAX_CONCEAL_FRAMES {
                    // A real stall (the worker stopped for >300 ms): fall back to the plain voice and start over.
                    self.primed = false;
                    self.holdoff = REPRIME_HOLDOFF_FRAMES;
                    self.short_run = 0;
                    self.fade_in = false;
                } else {
                    // The worker is a little late: play what is there, then silence, and keep the timeline. The
                    // silence becomes extra cushion once the late push arrives (nothing is skipped or repeated).
                    self.wet = [0.0; FRAME_SAMPLES];
                    for w in self.wet.iter_mut().take(n) {
                        *w = self.out_cons.pop().unwrap_or(0.0);
                    }
                    let ramp = n.min(GAP_RAMP);
                    for (i, w) in self.wet[n - ramp..n].iter_mut().enumerate() {
                        *w *= 1.0 - (i + 1) as f32 / ramp.max(1) as f32;
                    }
                    self.fade_in = true;
                    have = true;
                }
            }
        }
        if !have {
            self.wet = [0.0; FRAME_SAMPLES];
            self.quiet_run = 0;
        } else {
            // The cushion only ever grows after a gap, so a rough patch would leave the delay permanently higher
            // (heard as "lag"). After 20 s without a gap, give 20 ms back - only in real silence (several quiet
            // frames in a row, and the 20 ms about to be dropped is quiet too), so nothing audible is cut.
            self.stable = self.stable.saturating_add(1);
            self.quiet_run = if self.wet.iter().all(|v| v.abs() < 0.003) { self.quiet_run.saturating_add(1) } else { 0 };
            if self.stable >= 2000
                && self.jitter > self.base_jitter
                && self.quiet_run >= QUIET_FRAMES_BEFORE_TRIM
                && self.out_cons.slots() >= 2 * FRAME_SAMPLES
                && upcoming_is_quiet(&mut self.out_cons, 20 * 48)
            {
                for _ in 0..20 * 48 {
                    let _ = self.out_cons.pop();
                }
                self.jitter = self.jitter.saturating_sub(20 * 48).max(self.base_jitter);
                sh.jitter_ms.store((self.jitter / 48) as u32, Relaxed);
                self.stable = 0;
            }
        }
        let slow = sh.slow.load(Relaxed);
        let want_wet = have && !slow && self.holdoff == 0;
        if self.holdoff > 0 {
            self.holdoff -= 1;
        }
        let target = if want_wet { 1.0 } else { 0.0 };
        let step = 1.0 / RAMP_SAMPLES;
        for (d, &w) in frame.iter_mut().zip(self.wet.iter()) {
            if self.mix < target {
                self.mix = (self.mix + step).min(target);
            } else if self.mix > target {
                self.mix = (self.mix - step).max(target);
            }
            *d = *d * (1.0 - self.mix) + w * self.mix;
        }
        sh.mic.store(
            if want_wet && self.mix > 0.5 {
                M_ACTIVE
            } else if slow {
                // Only a PC that really cannot keep up (RTF over the limit for 5 s) is "paused"; a single late push
                // is covered above and must not show "this PC cannot keep up".
                M_PAUSED
            } else {
                M_STARTING
            },
            Relaxed,
        );
    }
}

/// Input conditioning for the model, on the 16 kHz chunk the worker is about to convert:
///
/// * **Speech gate.** The model was trained on clean speech and will happily "pronounce" low-level noise or
///   background babble that BVC lets through, which sounds like BVC not working. Below `GATE_OPEN` RMS (and after
///   a 300 ms hangover) the chunk is replaced by digital silence, so the model outputs silence.
/// * **Slow leveler.** The converter carries input loudness through to the output and was trained on speech at
///   about 0.06 RMS (the reference app applies a per-mic gain for the same reason). The gain only follows speech
///   chunks, falls fast (a sudden loud passage must not clip) and rises slowly.
/// * **Soft clip** instead of a hard clamp, so no gain choice can produce harsh distortion.
struct Leveler {
    est: f32,
    gain: f32,
    open: bool,
    hang: u32,
}

const GATE_OPEN_RMS: f32 = 0.008;
const GATE_HANG_CHUNKS: u32 = 6;

fn soft_clip(v: f32) -> f32 {
    let a = v.abs();
    if a <= 0.8 {
        v
    } else {
        v.signum() * (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh())
    }
}

impl Leveler {
    fn new() -> Self {
        Self { est: LEVEL_TARGET_RMS, gain: 1.0, open: false, hang: 0 }
    }
    fn process(&mut self, x: &mut [f32]) {
        let rms = (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt();
        if rms > GATE_OPEN_RMS {
            self.open = true;
            self.hang = GATE_HANG_CHUNKS;
            self.est += 0.05 * (rms - self.est);
        } else if self.open {
            self.hang = self.hang.saturating_sub(1);
            if self.hang == 0 {
                self.open = false;
            }
        }
        if !self.open {
            x.iter_mut().for_each(|v| *v = 0.0);
            return;
        }
        let target = (LEVEL_TARGET_RMS / self.est.max(1e-4)).clamp(0.1, 6.0);
        // fast down (attack), slow up (release)
        let k = if target < self.gain { 0.7 } else { 0.1 };
        let next = self.gain + k * (target - self.gain);
        let n = x.len() as f32;
        for (i, v) in x.iter_mut().enumerate() {
            let g = self.gain + (next - self.gain) * (i as f32 + 1.0) / n;
            *v = soft_clip(*v * g);
        }
        self.gain = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mirror_handle_follows_the_engine_status() {
        let h = AccentHandle::unavailable("not started");
        assert_eq!(h.status().state, AccentState::Unavailable);
        let mut st = h.status();
        st.state = AccentState::Off;
        h.apply_remote(&st);
        assert_eq!(h.status().state, AccentState::Off, "engine says off: the starting 'unavailable' must clear");
        h.set_voice(1);
        st.state = AccentState::Paused;
        st.rtf = 0.93;
        h.apply_remote(&st);
        let got = h.status();
        assert_eq!(got.state, AccentState::Paused);
        assert!((got.rtf - 0.93).abs() < 0.01);
        st.state = AccentState::Active;
        h.apply_remote(&st);
        assert_eq!(h.status().state, AccentState::Active);
    }

    /// A stage whose worker is "ready" and whose converted output we feed by hand (no model needed).
    fn hand_fed_stage() -> (AccentStage, Producer<f32>) {
        let (mut stage, handle) = start_inner(None, "test");
        let (prod, cons) = RingBuffer::<f32>::new(48_000);
        stage.out_cons = cons;
        stage.sh.worker.store(W_READY, Relaxed);
        handle.set_voice(1);
        (stage, prod)
    }

    fn feed(p: &mut Producer<f32>, n: usize) {
        for _ in 0..n {
            p.push(0.5).unwrap();
        }
    }

    /// Runs one 10 ms frame (dry input 0.2) and returns its last sample.
    fn step(stage: &mut AccentStage) -> f32 {
        let mut f = [0.2f32; FRAME_SAMPLES];
        stage.process(&mut f);
        f[FRAME_SAMPLES - 1]
    }

    #[test]
    fn a_late_push_is_covered_with_silence_instead_of_falling_back_to_the_plain_voice() {
        let (mut stage, mut out) = hand_fed_stage();
        feed(&mut out, stage.jitter + PUSH48 + 2 * FRAME_SAMPLES);
        for _ in 0..60 {
            feed(&mut out, FRAME_SAMPLES);
            step(&mut stage);
        }
        assert!((step(&mut stage) - 0.5).abs() < 1e-3, "fully on the converted voice");
        // Drain the buffer, then the worker is late for a few frames.
        while stage.out_cons.slots() > 0 {
            let _ = stage.out_cons.pop();
        }
        for _ in 0..5 {
            let last = step(&mut stage);
            assert!(last.abs() < 1e-3, "a gap is silence in the converted voice, not the plain voice");
        }
        assert_eq!(stage.sh.underruns.load(Relaxed), 1, "one gap counts once");
        assert!(stage.primed && stage.holdoff == 0, "no re-prime, no 1 s fallback");
        assert_ne!(stage.sh.mic.load(Relaxed), M_PAUSED, "a late push is not 'PC cannot keep up'");
        // The late audio arrives: straight back on the converted voice.
        feed(&mut out, 4 * FRAME_SAMPLES);
        step(&mut stage);
        assert!((step(&mut stage) - 0.5).abs() < 1e-3);
    }

    #[test]
    fn a_long_stall_falls_back_to_the_plain_voice_without_saying_paused() {
        let (mut stage, mut out) = hand_fed_stage();
        feed(&mut out, stage.jitter + PUSH48 + 2 * FRAME_SAMPLES);
        for _ in 0..60 {
            feed(&mut out, FRAME_SAMPLES);
            step(&mut stage);
        }
        while stage.out_cons.slots() > 0 {
            let _ = stage.out_cons.pop();
        }
        for _ in 0..(MAX_CONCEAL_FRAMES + 10) {
            step(&mut stage);
        }
        assert!(!stage.primed && stage.holdoff > 0, "a real stall re-primes");
        assert!((step(&mut stage) - 0.2).abs() < 1e-3, "and plays the plain voice meanwhile");
        assert_ne!(stage.sh.mic.load(Relaxed), M_PAUSED);
    }

    #[test]
    fn the_buffer_is_trimmed_only_in_real_silence() {
        let mut quiet = RingBuffer::<f32>::new(4800);
        for _ in 0..1000 {
            quiet.0.push(0.0).unwrap();
        }
        assert!(upcoming_is_quiet(&mut quiet.1, 960));
        assert_eq!(quiet.1.slots(), 1000, "looking does not consume");
        let mut speech = RingBuffer::<f32>::new(4800);
        for i in 0..1000 {
            speech.0.push(if i == 500 { 0.3 } else { 0.0 }).unwrap();
        }
        assert!(!upcoming_is_quiet(&mut speech.1, 960), "speech in the next 20 ms blocks the trim");
    }

    #[test]
    fn accent_thread_tiers_by_core_count() {
        let expect = [(0, 1), (1, 1), (2, 1), (3, 1), (4, 2), (5, 3), (6, 4), (8, 6), (9, 5), (12, 8), (16, 8), (64, 8)];
        for (cores, threads) in expect {
            assert_eq!(accent_threads_for_cores(cores), threads, "cores = {cores}");
        }
    }

    fn tone(amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (i as f32 * 0.05).sin()).collect()
    }

    #[test]
    fn gate_silences_noise_floor_but_passes_speech_with_hangover() {
        let mut l = Leveler::new();
        let mut noise = tone(0.004, 800); // ~ -51 dB: BVC residue
        l.process(&mut noise);
        assert!(noise.iter().all(|&v| v == 0.0), "noise below the gate must become digital silence");
        let mut speech = tone(0.1, 800);
        l.process(&mut speech);
        assert!(speech.iter().any(|&v| v.abs() > 0.05));
        // pause: stays open for the hangover (word gaps), then closes
        for _ in 0..GATE_HANG_CHUNKS - 1 {
            let mut quiet = tone(0.004, 800);
            l.process(&mut quiet);
            assert!(quiet.iter().any(|&v| v != 0.0), "hangover must keep the gate open through short pauses");
        }
        let mut quiet = tone(0.004, 800);
        l.process(&mut quiet);
        assert!(quiet.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn loud_input_after_quiet_speech_never_hard_clips() {
        let mut l = Leveler::new();
        for _ in 0..40 {
            let mut quiet_speech = tone(0.02, 800);
            l.process(&mut quiet_speech); // gain climbs towards its ceiling
        }
        let mut loud = tone(0.9, 800);
        l.process(&mut loud);
        let peak = loud.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak < 1.0, "soft clip must stay below full scale, got {peak}");
        // gain must have dropped quickly, not stay at the quiet-speech ceiling
        let mut loud2 = tone(0.9, 800);
        l.process(&mut loud2);
        assert!(l.gain < 1.0, "gain must attack down fast, got {}", l.gain);
    }

    #[test]
    fn soft_clip_is_transparent_below_the_knee_and_bounded_above() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert!(soft_clip(5.0) <= 1.0 && soft_clip(-5.0) >= -1.0);
        assert!(soft_clip(0.9) > 0.8 && soft_clip(0.9) < 0.9);
    }

    #[test]
    fn measured_delay_finds_how_late_the_converted_voice_is() {
        // "Speech": bursts of different lengths, so only one lag matches.
        let mut input = vec![0.0f32; 48_000 * 4];
        for (start, len) in [(4_800usize, 9_600usize), (40_000, 4_800), (70_000, 19_200), (130_000, 7_200)] {
            for (i, s) in input[start..start + len].iter_mut().enumerate() {
                *s = 0.3 * (i as f32 * 0.07).sin();
            }
        }
        let lag = 9_600; // 200 ms
        let mut out = vec![0.0f32; lag];
        out.extend(input.iter().map(|v| v * 0.5)); // quieter and different, like a converted voice
        let d = measured_delay(&input, &out).expect("speech present");
        assert!((d as i64 - lag as i64).abs() <= 480, "measured {d}, want {lag}");
        assert_eq!(measured_delay(&vec![0.0; 48_000], &vec![0.0; 48_000]), None, "silence has nothing to match");
    }
}

fn set_error(sh: &Shared, msg: String) {
    if let Ok(mut m) = sh.error.lock() {
        *m = msg;
    }
}

/// Converts a whole 48 kHz recording at once (the "Test my voice" screen), with the same resamplers, leveler and push
/// size as the live worker, so it sounds like the live accent. `voice` is 1..=6. `init_runtime` must have been called.
/// The result is 48 kHz, the same length as the input (the model delay is removed so both line up).
pub fn convert_offline(models: &std::path::Path, threads: usize, voice: u8, x48: &[f32]) -> Result<Vec<f32>, String> {
    let name = VOICE_NAMES.get((voice.max(1) - 1) as usize).ok_or("unknown voice")?;
    let mut eng = AccentStream::load(models, threads, name)?;
    let mut down = CausalDownsampler3x::new(RESAMPLER_TAPS);
    let mut up = CausalUpsampler2x::new(RESAMPLER_TAPS);
    let mut leveler = Leveler::new();
    let mut x16: Vec<f32> = Vec::with_capacity(x48.len() / 3 + PUSH16);
    for c in x48.chunks(PUSH48) {
        x16.extend(down.push(c));
    }
    // Pad so the last words come out of the model look-ahead too.
    let pad = ((eng.algorithmic_latency_ms() + 200.0) * 16.0) as usize;
    x16.extend(std::iter::repeat_n(0.0, pad.div_ceil(PUSH16) * PUSH16));
    let mut out48: Vec<f32> = Vec::with_capacity(x48.len() + pad * 3);
    for c in x16.chunks(PUSH16) {
        let mut chunk = c.to_vec();
        leveler.process(&mut chunk);
        out48.extend(up.push(&eng.push(&chunk)?));
    }
    out48.extend(up.push(&eng.flush()?));
    // Remove the model + resampler delay so the converted voice lines up with the original. Measured from the speech
    // itself (the estimate from the model alone was 180 ms off); the estimate is used only when there is no speech.
    let estimate = (eng.algorithmic_latency_ms() * 48.0) as usize + RESAMPLER_TAPS;
    let delay = measured_delay(x48, &out48).unwrap_or(estimate).min(out48.len());
    out48.drain(..delay);
    out48.resize(x48.len(), 0.0);
    Ok(out48)
}

/// How many samples `out` lags `input`, from the best match of their 10 ms loudness envelopes over 0..1 s. `None` for
/// near-silence, where there is nothing to match.
fn measured_delay(input: &[f32], out: &[f32]) -> Option<usize> {
    const HOP: usize = 480;
    let env = |x: &[f32]| -> Vec<f32> { x.chunks(HOP).map(|c| c.iter().map(|v| v.abs()).sum::<f32>() / c.len() as f32).collect() };
    let (a, b) = (env(input), env(out));
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
    let (ma, mb) = (mean(&a), mean(&b));
    if ma < 1e-4 || mb < 1e-4 {
        return None;
    }
    let max_lag = 100.min(b.len().saturating_sub(1));
    let score = |lag: usize| -> f32 {
        a.iter().zip(b.iter().skip(lag)).map(|(x, y)| (x - ma) * (y - mb)).sum()
    };
    let best = (0..=max_lag).max_by(|&l, &m| score(l).partial_cmp(&score(m)).unwrap_or(std::cmp::Ordering::Equal))?;
    (score(best) > 0.0).then_some(best * HOP)
}

fn worker(sh: Arc<Shared>, mut in_cons: Consumer<f32>, mut out_prod: Producer<f32>, mut cfg: AccentConfig) {
    raise_thread_priority();
    let mut engine: Option<AccentStream> = None;
    let mut loaded_voice = 0u8;
    let mut seen_gen = sh.generation.load(Relaxed);
    let mut down = CausalDownsampler3x::new(RESAMPLER_TAPS);
    let mut up = CausalUpsampler2x::new(RESAMPLER_TAPS);
    let mut leveler = Leveler::new();
    let mut buf48 = vec![0.0f32; PUSH48];
    let mut x16: Vec<f32> = Vec::with_capacity(4 * PUSH16);
    let mut rtf_hist: VecDeque<f32> = VecDeque::with_capacity(SLOW_WINDOW + 1);
    let mut ok_streak = 0usize;
    let (mut conv_acc, mut voc_acc, mut total_acc, mut audio_acc) = (Duration::ZERO, Duration::ZERO, Duration::ZERO, 0.0f32);

    while !sh.stop.load(Relaxed) {
        let voice = sh.voice.load(Relaxed);
        if voice == ACCENT_OFF || sh.worker.load(Relaxed) == W_FAILED {
            while in_cons.pop().is_ok() {}
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if engine.is_none() {
            sh.worker.store(W_LOADING, Relaxed);
            let loaded = (|| -> Result<AccentStream, String> {
                init_runtime(&cfg.onnxruntime_dll)?;
                let prepared = match cfg.prepare_models.take() {
                    Some(f) => Some(f()?),
                    None => cfg.models_dir.clone(),
                };
                let dir = prepared.or_else(find_models_dir).ok_or("model bundle (app_models) not found")?;
                AccentStream::load(&dir, cfg.threads, VOICE_NAMES[voice as usize - 1])
            })();
            match loaded {
                Ok(e) => {
                    engine = Some(e);
                    loaded_voice = voice;
                    seen_gen = sh.generation.load(Relaxed);
                    sh.worker.store(W_READY, Relaxed);
                }
                Err(e) => {
                    set_error(&sh, e);
                    sh.worker.store(W_FAILED, Relaxed);
                }
            }
            continue;
        }
        let eng = engine.as_mut().unwrap();
        if voice != loaded_voice {
            if eng.set_voice(VOICE_NAMES[voice as usize - 1]).is_ok() {
                loaded_voice = voice;
            }
        }
        let g = sh.generation.load(Relaxed);
        if g != seen_gen {
            seen_gen = g;
            eng.reset();
            down.reset();
            up.reset();
            leveler = Leveler::new();
            x16.clear();
            rtf_hist.clear();
            sh.slow.store(false, Relaxed);
            ok_streak = 0;
        }

        let avail = in_cons.slots();
        if avail < PUSH48 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        if avail > MAX_BACKLOG48 {
            // Too far behind the mic: skip the oldest audio (a short gap beats an ever-growing delay).
            for _ in 0..avail - 2 * PUSH48 {
                let _ = in_cons.pop();
            }
            sh.catchups.fetch_add(1, Relaxed);
        }
        for s in buf48.iter_mut() {
            *s = in_cons.pop().unwrap_or(0.0);
        }

        let t0 = Instant::now();
        x16.extend(down.push(&buf48));
        while x16.len() >= PUSH16 {
            let mut chunk: Vec<f32> = x16.drain(..PUSH16).collect();
            leveler.process(&mut chunk);
            match eng.push(&chunk) {
                Ok(audio24) => {
                    let wet48 = up.push(&audio24);
                    for s in wet48 {
                        let _ = out_prod.push(s); // full ring: drop (the mic side will skip ahead anyway)
                    }
                }
                Err(e) => {
                    set_error(&sh, format!("inference failed: {e}"));
                    sh.worker.store(W_FAILED, Relaxed);
                    break;
                }
            }
            let (c, v) = eng.take_busy();
            conv_acc += c;
            voc_acc += v;
            audio_acc += PUSH_MS;
        }
        total_acc += t0.elapsed();

        // Real-time factor over this push.
        let rtf = t0.elapsed().as_secs_f32() * 1000.0 / PUSH_MS;
        if rtf_hist.len() == SLOW_WINDOW {
            rtf_hist.pop_front();
        }
        rtf_hist.push_back(rtf);
        let mean = rtf_hist.iter().sum::<f32>() / rtf_hist.len() as f32;
        sh.rtf_milli.store((mean * 1000.0) as u32, Relaxed);
        if audio_acc >= 1000.0 {
            sh.rtf_conv_milli.store((conv_acc.as_secs_f32() * 1000.0 / audio_acc * 1000.0) as u32, Relaxed);
            sh.rtf_voc_milli.store((voc_acc.as_secs_f32() * 1000.0 / audio_acc * 1000.0) as u32, Relaxed);
            conv_acc = Duration::ZERO;
            voc_acc = Duration::ZERO;
            total_acc = Duration::ZERO;
            audio_acc = 0.0;
        }
        let was_slow = sh.slow.load(Relaxed);
        if !was_slow && rtf_hist.len() == SLOW_WINDOW && mean > SLOW_RTF {
            sh.slow.store(true, Relaxed);
            ok_streak = 0;
        } else if was_slow {
            if mean < RECOVER_RTF {
                ok_streak += 1;
                if ok_streak >= RECOVER_WINDOW {
                    sh.slow.store(false, Relaxed);
                    ok_streak = 0;
                }
            } else {
                ok_streak = 0;
            }
        }
        // Added delay = model look-ahead + mel window reach + both resamplers + waiting for a full push
        // + compute + the fixed jitter buffer + one mic frame.
        let lat = eng.algorithmic_latency_ms()
            + 25.0
            + (down.group_delay_samples_48k + up.group_delay_samples_48k) as f32 / 48.0
            + PUSH_MS / 2.0
            + mean * PUSH_MS
            + sh.jitter_ms.load(Relaxed) as f32
            + PUSH_MS / 2.0 // average of the burst cycle on top of the jitter floor
            + 10.0;
        sh.latency_x10.store((lat * 10.0) as u32, Relaxed);
    }
}

/// Above-normal priority so the converter keeps its place when other apps load the CPU (no-op off Windows).
#[cfg(windows)]
fn raise_thread_priority() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> isize;
        fn SetThreadPriority(h: isize, p: i32) -> i32;
    }
    const THREAD_PRIORITY_ABOVE_NORMAL: i32 = 1;
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
}
#[cfg(not(windows))]
fn raise_thread_priority() {}
