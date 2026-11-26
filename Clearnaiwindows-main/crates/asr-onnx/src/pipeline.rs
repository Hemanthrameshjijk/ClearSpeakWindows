//! Live transcript: the 10 ms mic thread only copies frames into a lock-free ring (never modifies the audio,
//! never allocates); a worker thread downsamples to 16 kHz, cuts utterances, runs the speech model and publishes
//! text lines to the GUI (and appends them to a transcript file).
//!
//! ```text
//!  mic thread: TranscriptStage::process --ring--> worker: 48k -> 16k -> Segmenter -> backlog -> AsrModel -> lines
//! ```
//!
//! Two sides ([`start_two_sided`]): the agent's mic and the call audio (the caller) each feed their own ring and
//! sentence cutter; one worker and one speech model serve both, so every line is labelled by where the sound came
//! from, not by guessing voices (that is exact; telling voices apart on one mixed recording gets ~93% at best).
//! An echo guard drops an Agent sentence that is only the caller leaking into the mic.
//!
//! When the PC is busy ([`TranscriptStage::set_busy`], from the low-end PC mode) no audio is dropped: the worker keeps
//! cutting sentences (cheap) into a backlog and only stops running the speech model; once the PC has room again it
//! works through the backlog, each line keeping the time it was said. Before, the engine stopped feeding the
//! transcript instead, and everything said meanwhile was lost (29.9% word errors on a test with a third of the audio
//! skipped, against 3.5% live).

use crate::model::{find_models_dir, AsrModel};
use crate::segmenter::{Event, Segmenter};
use dsp_core::{Consumer, Producer, RingBuffer, Stage, FRAME_SAMPLES};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use accent_onnx::causal_resampler::CausalDownsampler3x;

const RESAMPLER_TAPS: usize = 97;
const MAX_LINES: usize = 500;
const RING_SECONDS: usize = 30;
/// Most speech kept waiting while the PC is busy (seconds of audio); past this the oldest sentences are dropped.
const MAX_BACKLOG_S: f32 = 600.0;

const S_OFF: u8 = 0;
const S_LOADING: u8 = 1;
const S_LISTENING: u8 = 2;
const S_TRANSCRIBING: u8 = 3;
const S_UNAVAILABLE: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptState {
    Off,
    Loading,
    Listening,
    Transcribing,
    Unavailable,
}

/// Who said a line: known from the stream it came from (the mic = the agent, the call audio = the caller).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Side {
    #[default]
    Agent,
    Caller,
}

impl Side {
    pub fn label(self) -> &'static str {
        match self {
            Side::Agent => "Agent",
            Side::Caller => "Caller",
        }
    }
    /// For the wire between engine and window: 0 agent, 1 caller.
    pub fn to_u8(self) -> u8 {
        self as u8
    }
    pub fn from_u8(v: u8) -> Self {
        if v == 1 {
            Side::Caller
        } else {
            Side::Agent
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptLine {
    /// Seconds since the app started (when the sentence ended).
    pub at_secs: u32,
    pub side: Side,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct TranscriptStatus {
    pub state: TranscriptState,
    pub message: String,
    pub rtf: f32,
    pub line_count: usize,
}

impl TranscriptStatus {
    pub fn line(&self) -> String {
        match self.state {
            TranscriptState::Off => "Transcript: off".into(),
            TranscriptState::Loading => "Transcript: loading speech model...".into(),
            TranscriptState::Listening => "Transcript: listening".into(),
            TranscriptState::Transcribing => format!("Transcript: transcribing (RTF {:.2})", self.rtf),
            TranscriptState::Unavailable => format!("Transcript unavailable - {}", self.message),
        }
    }
}

pub struct TranscriptConfig {
    pub dll: PathBuf,
    pub models_dir: Option<PathBuf>,
    pub threads: usize,
    /// Directory for the call files (`YYYY-MM-DD_HH-MM-SS_call.txt`, one per call); `None` = do not save.
    pub save_dir: Option<PathBuf>,
    pub initial_enabled: bool,
}

impl TranscriptConfig {
    pub fn new(dll: PathBuf) -> Self {
        let threads = std::env::var("CLEARSPEAK_ASR_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
        Self { dll, models_dir: None, threads, save_dir: None, initial_enabled: false }
    }
}

#[derive(Default)]
struct Data {
    lines: Vec<TranscriptLine>,
    /// Lines added since the last clear. Keeps counting after `lines` is capped at `MAX_LINES`, so whoever sends new
    /// lines on (the engine to the window) can still tell how many are new.
    total: usize,
    partial: String,
    message: String,
    save_path: Option<PathBuf>,
    /// When the last saved line was said (seconds since start): a long quiet gap starts a new call file.
    last_saved_at: Option<u32>,
    /// Calls are detected by the app (automatic call recording): only lines said during a call are saved, each to
    /// its call's file. Off: the quiet-gap rule above splits the files.
    call_mode: bool,
    /// The current (or last) detected call.
    call: Option<CallFile>,
    /// The call before it: back to back, its last sentences are still being transcribed when the next one starts.
    prev_call: Option<CallFile>,
}

/// One detected call: its lines go to `<stem>.txt` and `<stem>.jsonl`, timed like its WAV (from the start of the
/// recording, holds left out).
struct CallFile {
    stem: PathBuf,
    /// Seconds since start when the call began and ended (`None` while it goes on).
    start: u32,
    end: Option<u32>,
    /// Holds as (from, to) seconds since start, `to` = `None` while on hold. What is said on hold is not saved.
    holds: Vec<(u32, Option<u32>)>,
}

impl CallFile {
    /// A sentence that ended at `at` belongs to this call.
    fn covers(&self, at: u32) -> bool {
        at + 1 >= self.start && self.end.is_none_or(|e| at <= e + CALL_TAIL_S)
    }

    fn on_hold_at(&self, at: u32) -> bool {
        self.holds.iter().any(|&(from, to)| at >= from && to.is_none_or(|t| at < t))
    }

    /// Time in the file: from the start, minus the time on hold before `at` (the WAV leaves holds out).
    fn file_time(&self, at: u32) -> u32 {
        let held: u32 = self.holds.iter().map(|&(from, to)| to.unwrap_or(at).min(at).saturating_sub(from)).sum();
        at.saturating_sub(self.start).saturating_sub(held)
    }
}

/// `<stem>.<ext>`; `Path::with_extension` would cut a stem that has a dot in it.
pub fn stem_with(stem: &Path, ext: &str) -> PathBuf {
    let mut s = stem.as_os_str().to_os_string();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// A line said up to this long after the call ended still belongs to it (the sentence was finishing).
const CALL_TAIL_S: u32 = 2;

struct Shared {
    enabled: AtomicBool,
    state: AtomicU8,
    stop: AtomicBool,
    rtf_milli: AtomicU32,
    version: AtomicU32,
    clear_requested: AtomicBool,
    /// Counts clears, so whoever mirrors the transcript (the window) can tell a clear happened.
    clear_gen: AtomicU32,
    /// Counts clears that also drop the work in progress: a line being transcribed meanwhile is dropped. Not during a
    /// recorded call, whose sentences still belong in its file.
    drop_gen: AtomicU32,
    /// The PC is short of CPU: queue sentences, do not run the speech model.
    busy: AtomicBool,
    /// Agent sentences dropped by the echo guard (the caller leaking into the mic), for the log and tests.
    echo_dropped: AtomicU32,
    /// Load the speech model even while switched off, so the first words of a call are not lost to loading.
    preload: AtomicBool,
    /// Finished sentences waiting to be transcribed (lets the app switch off only after the last one is written).
    pending: AtomicU32,
    started: Instant,
    data: Mutex<Data>,
}

/// GUI-side handle (cheap to clone).
#[derive(Clone)]
pub struct TranscriptHandle(Arc<Shared>);

impl TranscriptHandle {
    /// Inert handle for builds/paths where the engine is not running.
    pub fn unavailable(reason: &str) -> Self {
        let sh = new_shared(false);
        sh.state.store(S_UNAVAILABLE, Relaxed);
        sh.data.lock().unwrap().message = reason.to_string();
        Self(Arc::new(sh))
    }

    pub fn set_enabled(&self, on: bool) {
        self.0.enabled.store(on, Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.0.enabled.load(Relaxed)
    }

    pub fn status(&self) -> TranscriptStatus {
        let d = self.0.data.lock().unwrap();
        let state = match self.0.state.load(Relaxed) {
            S_LOADING => TranscriptState::Loading,
            S_LISTENING => TranscriptState::Listening,
            S_TRANSCRIBING => TranscriptState::Transcribing,
            S_UNAVAILABLE => TranscriptState::Unavailable,
            _ => TranscriptState::Off,
        };
        TranscriptStatus { state, message: d.message.clone(), rtf: self.0.rtf_milli.load(Relaxed) as f32 / 1000.0, line_count: d.total }
    }

    /// Last `max` finished lines plus the in-progress (partial) text.
    pub fn snapshot(&self, max: usize) -> (Vec<TranscriptLine>, String) {
        let d = self.0.data.lock().unwrap();
        let from = d.lines.len().saturating_sub(max);
        (d.lines[from..].to_vec(), d.partial.clone())
    }

    /// Lines added since `sent` of them were taken (`sent` from the previous call; 0 at the start), the new total, and
    /// the partial text, all in one lock: taking the count and the lines separately could skip a line and repeat
    /// another when one was added in between. A total below `sent` means the transcript was cleared.
    pub fn since(&self, sent: usize) -> (usize, Vec<TranscriptLine>, String) {
        let d = self.0.data.lock().unwrap();
        let new = d.total.saturating_sub(sent).min(d.lines.len());
        (d.total, d.lines[d.lines.len() - new..].to_vec(), d.partial.clone())
    }

    /// For a handle that mirrors an engine running in another process: applies the engine's state plus the lines that
    /// are new since the last call (`reset` = the engine cleared its transcript, so start over). `enabled` is not
    /// touched (the UI owns it).
    pub fn apply_remote(&self, state: TranscriptState, message: &str, rtf: f32, reset: bool, new_lines: Vec<TranscriptLine>, partial: &str) {
        self.0.state.store(
            match state {
                TranscriptState::Off => S_OFF,
                TranscriptState::Loading => S_LOADING,
                TranscriptState::Listening => S_LISTENING,
                TranscriptState::Transcribing => S_TRANSCRIBING,
                TranscriptState::Unavailable => S_UNAVAILABLE,
            },
            Relaxed,
        );
        self.0.rtf_milli.store((rtf * 1000.0) as u32, Relaxed);
        let mut d = self.0.data.lock().unwrap();
        let changed = reset || !new_lines.is_empty() || d.partial != partial;
        if reset {
            d.lines.clear();
            d.total = 0;
        }
        d.total += new_lines.len();
        d.lines.extend(new_lines);
        let over = d.lines.len().saturating_sub(MAX_LINES);
        d.lines.drain(..over);
        d.partial = partial.to_string();
        d.message = message.to_string();
        if changed {
            self.0.version.fetch_add(1, Relaxed);
        }
    }

    /// Changes on every [`Self::clear`].
    pub fn clear_gen(&self) -> u32 {
        self.0.clear_gen.load(Relaxed)
    }

    /// Empties the transcript box. During a recorded call only the box is emptied: the sentences still being
    /// transcribed are kept for the call's file.
    pub fn clear(&self) {
        let mut d = self.0.data.lock().unwrap();
        self.0.clear_gen.fetch_add(1, Relaxed);
        if !d.call.as_ref().is_some_and(|c| c.end.is_none()) {
            self.0.drop_gen.fetch_add(1, Relaxed);
            self.0.clear_requested.store(true, Relaxed);
        }
        d.lines.clear();
        d.total = 0;
        d.partial.clear();
        self.0.version.fetch_add(1, Relaxed);
    }

    /// Changes whenever lines or the partial text change (lets the GUI skip redundant work).
    pub fn version(&self) -> u32 {
        self.0.version.load(Relaxed)
    }

    pub fn save_path(&self) -> Option<PathBuf> {
        self.0.data.lock().unwrap().save_path.clone()
    }

    /// Agent sentences dropped so far because they were only the caller's voice leaking into the mic.
    pub fn echo_dropped(&self) -> u32 {
        self.0.echo_dropped.load(Relaxed)
    }

    /// Load the speech model now even while switched off (automatic call recording is on).
    pub fn set_preload(&self, on: bool) {
        self.0.preload.store(on, Relaxed);
    }

    /// Sentences heard but not transcribed yet.
    pub fn pending(&self) -> u32 {
        self.0.pending.load(Relaxed)
    }

    /// On: the app tells when calls start and end, and only lines said during a call are saved (to that call's
    /// file). Off: lines are saved whenever the transcript is on, split into files by quiet gaps.
    pub fn set_call_mode(&self, on: bool) {
        let mut d = self.0.data.lock().unwrap();
        d.call_mode = on;
        // Either way the quiet-gap files start afresh: never append to the last call's file.
        d.save_path = None;
        d.last_saved_at = None;
        if !on {
            d.call = None;
            d.prev_call = None;
        }
    }

    /// A call started: lines go to `<stem>.txt` and `<stem>.jsonl`, timed like the WAV, which begins `pre_roll_secs`
    /// before now (never before the previous call ended). Both files are created at once, starting with `header`
    /// (one line, e.g. the caller's number; empty = none).
    pub fn begin_call(&self, stem: PathBuf, header: &str, pre_roll_secs: u32) {
        let now = self.0.started.elapsed().as_secs() as u32;
        if let Some(dir) = stem.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(stem_with(&stem, "txt")) {
            if !header.is_empty() {
                let _ = writeln!(f, "{header}");
            }
        }
        if let Ok(mut j) = std::fs::OpenOptions::new().create(true).append(true).open(stem_with(&stem, "jsonl")) {
            if !header.is_empty() {
                let _ = writeln!(j, "{{\"meta\":\"{}\"}}", json_escape(header));
            }
        }
        let mut d = self.0.data.lock().unwrap();
        d.call_mode = true;
        d.save_path = Some(stem_with(&stem, "txt"));
        d.last_saved_at = None;
        if let Some(mut prev) = d.call.take() {
            prev.end.get_or_insert(now);
            d.prev_call = Some(prev);
        }
        let prev_end = d.prev_call.as_ref().and_then(|p| p.end).unwrap_or(0);
        let start = now.saturating_sub(pre_roll_secs).max(prev_end).min(now);
        d.call = Some(CallFile { stem, start, end: None, holds: Vec::new() });
    }

    /// The call ended: lines said after now (plus a short tail) are no longer saved to it.
    pub fn end_call(&self) {
        let now = self.0.started.elapsed().as_secs() as u32;
        if let Some(c) = self.0.data.lock().unwrap().call.as_mut() {
            c.end.get_or_insert(now);
            if let Some(h) = c.holds.last_mut().filter(|h| h.1.is_none()) {
                h.1 = Some(now);
            }
        }
    }

    /// The call is on hold: nothing said from now on is saved to it, and the hold is left out of its times.
    pub fn pause_call(&self) {
        let now = self.0.started.elapsed().as_secs() as u32;
        if let Some(c) = self.0.data.lock().unwrap().call.as_mut() {
            if !c.holds.last().is_some_and(|h| h.1.is_none()) {
                c.holds.push((now, None));
            }
        }
    }

    pub fn resume_call(&self) {
        let now = self.0.started.elapsed().as_secs() as u32;
        if let Some(c) = self.0.data.lock().unwrap().call.as_mut() {
            if let Some(h) = c.holds.last_mut().filter(|h| h.1.is_none()) {
                h.1 = Some(now);
            }
        }
    }
}

pub fn fmt_time(secs: u32) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn new_shared(enabled: bool) -> Shared {
    Shared {
        enabled: AtomicBool::new(enabled),
        state: AtomicU8::new(S_OFF),
        stop: AtomicBool::new(false),
        rtf_milli: AtomicU32::new(0),
        version: AtomicU32::new(0),
        clear_requested: AtomicBool::new(false),
        clear_gen: AtomicU32::new(0),
        drop_gen: AtomicU32::new(0),
        busy: AtomicBool::new(false),
        echo_dropped: AtomicU32::new(0),
        preload: AtomicBool::new(false),
        pending: AtomicU32::new(0),
        started: Instant::now(),
        data: Mutex::new(Data::default()),
    }
}

/// Audio-thread end (the mic thread for the agent, the call-audio thread for the caller). A pure tap: the frame is
/// left exactly as it is.
pub struct TranscriptStage {
    sh: Arc<Shared>,
    prod: Producer<f32>,
    /// The agent stage owns the worker: dropping it stops the worker. The caller stage does not.
    owner: bool,
}

impl Drop for TranscriptStage {
    fn drop(&mut self) {
        if self.owner {
            self.sh.stop.store(true, Relaxed);
        }
    }
}

impl TranscriptStage {
    /// The PC cannot keep up: keep listening but hold the speech model back until it can (no audio is lost).
    /// Cheap enough to call every frame.
    pub fn set_busy(&self, busy: bool) {
        self.sh.busy.store(busy, Relaxed);
    }
}

impl Stage for TranscriptStage {
    fn name(&self) -> &'static str {
        "Transcript"
    }

    fn process(&mut self, frame: &mut [f32]) {
        if !self.sh.enabled.load(Relaxed) || frame.len() != FRAME_SAMPLES {
            return;
        }
        let st = self.sh.state.load(Relaxed);
        if st == S_LISTENING || st == S_TRANSCRIBING {
            for &s in frame.iter() {
                let _ = self.prod.push(s); // ring full = worker is busy; dropping is fine for a transcript
            }
        }
    }
}

/// Agent (mic) only: every line is labelled Agent.
pub fn start(cfg: TranscriptConfig) -> (TranscriptStage, TranscriptHandle) {
    let (agent, _caller, h) = start_two_sided(cfg);
    (agent, h)
}

/// Agent stage (for the mic thread), caller stage (for the call-audio thread), and the handle. Both stages take
/// 48 kHz frames of [`FRAME_SAMPLES`]. A side that is never fed simply produces no lines.
pub fn start_two_sided(cfg: TranscriptConfig) -> (TranscriptStage, TranscriptStage, TranscriptHandle) {
    let sh = Arc::new(new_shared(cfg.initial_enabled));
    let (prod_a, cons_a) = RingBuffer::<f32>::new(48_000 * RING_SECONDS);
    let (prod_c, cons_c) = RingBuffer::<f32>::new(48_000 * RING_SECONDS);
    let w_sh = sh.clone();
    if let Err(e) = std::thread::Builder::new().name("clearspeak-transcript-worker".into()).spawn(move || worker(w_sh, [cons_a, cons_c], cfg)) {
        sh.state.store(S_UNAVAILABLE, Relaxed);
        sh.data.lock().unwrap().message = format!("could not start worker thread: {e}");
    }
    (
        TranscriptStage { sh: sh.clone(), prod: prod_a, owner: true },
        TranscriptStage { sh: sh.clone(), prod: prod_c, owner: false },
        TranscriptHandle(sh),
    )
}

fn fail(sh: &Shared, msg: String) {
    sh.data.lock().unwrap().message = msg;
    sh.state.store(S_UNAVAILABLE, Relaxed);
}

/// Nobody spoke for this long: the next line starts a new call file.
const CALL_GAP_S: u32 = 120;

/// Local date and time as `YYYY-MM-DD_HH-MM-SS` (for call file names).
#[cfg(windows)]
pub fn local_stamp() -> String {
    #[repr(C)]
    #[derive(Default)]
    struct SystemTime {
        year: u16,
        month: u16,
        _dow: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        _ms: u16,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLocalTime(t: *mut SystemTime);
    }
    let mut t = SystemTime::default();
    unsafe { GetLocalTime(&mut t) };
    format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second)
}
#[cfg(not(windows))]
pub fn local_stamp() -> String {
    let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    format!("{s}")
}

/// `{"at":"00:05","secs":5,"side":"Agent","text":"..."}`
fn json_line(at: u32, side: Side, text: &str) -> String {
    format!("{{\"at\":\"{}\",\"secs\":{at},\"side\":\"{}\",\"text\":\"{}\"}}", fmt_time(at), side.label(), json_escape(text))
}

fn json_escape(text: &str) -> String {
    let mut esc = String::with_capacity(text.len() + 2);
    for c in text.chars() {
        match c {
            '"' => esc.push_str("\\\""),
            '\\' => esc.push_str("\\\\"),
            c if (c as u32) < 0x20 => esc.push_str(&format!("\\u{:04x}", c as u32)),
            c => esc.push(c),
        }
    }
    esc
}

/// `at`: seconds since start when the sentence ended (a line from the backlog keeps its own time). Saved lines go to
/// one file per call: a new file after [`CALL_GAP_S`] of quiet, or after the transcript was switched off and on.
fn publish_line(sh: &Shared, at: u32, side: Side, text: String, save_dir: &Option<PathBuf>) {
    // Decide the file under the lock, write it after: a slow disk must not stall the window's status calls.
    let (path, file_at) = {
        let mut d = sh.data.lock().unwrap();
        // Calls detected by the app: a line said during a call goes to that call's file, timed like its WAV. The
        // previous call is checked first (back to back, its last sentence ends after the next call began). Not saved:
        // a line said outside every call, during a hold, or after the call's files were deleted ("Delete all"), so
        // nothing reappears under a deleted name.
        let path = if d.call_mode {
            let target = [d.prev_call.as_ref(), d.call.as_ref()].into_iter().flatten().find(|c| c.covers(at));
            save_dir
                .as_ref()
                .and(target)
                .filter(|c| !c.on_hold_at(at) && stem_with(&c.stem, "txt").exists())
                .map(|c| (stem_with(&c.stem, "txt"), c.file_time(at)))
        } else {
            save_dir.as_ref().map(|dir| {
            // A new call after a long quiet gap; also when the file was deleted meanwhile ("Delete all"), so the rest
            // of the call does not reappear under the deleted name.
            if d.last_saved_at.is_none_or(|last| at.saturating_sub(last) >= CALL_GAP_S) || d.save_path.as_ref().is_some_and(|p| !p.exists()) {
                d.save_path = None;
            }
            let path = d.save_path.clone().unwrap_or_else(|| {
                let stamp = local_stamp();
                let mut p = dir.join(format!("{stamp}_call.txt"));
                for n in 2.. {
                    if !p.exists() {
                        break;
                    }
                    p = dir.join(format!("{stamp}_call-{n}.txt"));
                }
                p
            });
            d.save_path = Some(path.clone());
            d.last_saved_at = Some(at);
            (path, at)
            })
        };
        d.lines.push(TranscriptLine { at_secs: at, side, text: text.clone() });
        d.total += 1;
        if d.lines.len() > MAX_LINES {
            d.lines.remove(0);
        }
        d.partial.clear();
        sh.version.fetch_add(1, Relaxed);
        path.unzip()
    };
    if let (Some(path), Some(dir), Some(at)) = (path, save_dir, file_at) {
        let _ = std::fs::create_dir_all(dir);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "[{}] {}: {}", fmt_time(at), side.label(), text);
        }
        // The same line for export tools: JSON Lines next to the text file (one object per line, so a call cut off
        // mid-way still leaves a readable file).
        if let Ok(mut j) = std::fs::OpenOptions::new().create(true).append(true).open(path.with_extension("jsonl")) {
            let _ = writeln!(j, "{}", json_line(at, side, &text));
        }
    }
}

/// Below-normal priority: the transcript must never compete with the mic/BVC/Accent threads for a core.
#[cfg(windows)]
fn lower_thread_priority() {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThread() -> isize;
        fn SetThreadPriority(h: isize, p: i32) -> i32;
    }
    const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
}
#[cfg(not(windows))]
fn lower_thread_priority() {}

/// A finished sentence waiting for the speech model. Times are seconds since start, when it was said (from the audio
/// itself, not from when the worker got to it).
struct Sentence {
    id: u64,
    side: Side,
    start: f32,
    end: f32,
    level_db: f32,
    audio: Vec<f32>,
}

/// The same voice on both streams: the other end hears the agent (a phone on loudspeaker, a second device in the same
/// room) and sends it back as "caller" audio a network delay later, or the caller leaks into the agent's mic. A
/// sentence is such a copy when a sentence of the other side began before it (by at most [`COPY_MAX_DELAY_S`]), and
/// it repeats at least [`COPY_WORDS_MATCH`] of that one's words, and its loudness rises and falls the same way
/// ([`COPY_ENV_CORR`]): the words alone could match a real "thank you" / "thank you", the sound of another person
/// saying it does not. The copy is dropped, the original kept. Seen on a live test call: almost every line came out
/// twice ("what's your name" as Agent, "what's her name" as Caller a second later).
const COPY_MAX_DELAY_S: f32 = 3.0;
/// A copy sent back over the call comes at least a network delay later. Two sentences starting closer than this are
/// the same moment (the caller leaking into the mic): which began first is then chance, so the leak rule
/// ([`echo_suspects`]) decides instead.
const COPY_MIN_DELAY_S: f32 = 0.25;
const COPY_WORDS_MATCH: f32 = 0.5;
const COPY_ENV_CORR: f32 = 0.6;
/// Transcribed sentences remembered for the copy check (seconds).
const COPY_MEMORY_S: f32 = 15.0;

/// A transcribed sentence, for the copy check.
struct Heard {
    side: Side,
    start: f32,
    env: Vec<f32>,
    text: String,
}

/// Loudness over time: dB of each 10 ms of a 16 kHz sentence (floored at -70 dB).
fn envelope(a: &[f32]) -> Vec<f32> {
    a.chunks(160).map(|c| 10.0 * (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).max(1e-7).log10()).collect()
}

/// How alike two loudness curves are: the best correlation over all shifts that overlap at least 60% of the shorter
/// one (and 0.3 s). 1 = the same shape.
fn env_corr(a: &[f32], b: &[f32]) -> f32 {
    let min_len = ((a.len().min(b.len()) as f32 * 0.6) as usize).max(30) as isize;
    let mut best = -1.0f32;
    let (n, m) = (a.len() as isize, b.len() as isize);
    for shift in -(m - 1)..n {
        // a[i] pairs with b[i - shift]
        let lo = shift.max(0);
        let hi = n.min(m + shift);
        if hi - lo < min_len {
            continue;
        }
        let k = (hi - lo) as f32;
        let (mut sa, mut sb) = (0.0f32, 0.0f32);
        for i in lo..hi {
            sa += a[i as usize];
            sb += b[(i - shift) as usize];
        }
        let (ma, mb) = (sa / k, sb / k);
        let (mut num, mut da, mut db) = (0.0f32, 0.0f32, 0.0f32);
        for i in lo..hi {
            let x = a[i as usize] - ma;
            let y = b[(i - shift) as usize] - mb;
            num += x * y;
            da += x * x;
            db += y * y;
        }
        if da > 1e-6 && db > 1e-6 {
            best = best.max(num / (da * db).sqrt());
        }
    }
    best
}

/// Is a sentence of `side` that began at `start` a copy of one already transcribed on the other side?
fn is_copy(side: Side, start: f32, env: &[f32], text: &str, heard: &std::collections::VecDeque<Heard>) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() < ECHO_MIN_WORDS {
        return false;
    }
    heard.iter().any(|h| {
        if h.side == side || start - h.start < COPY_MIN_DELAY_S || start - h.start > COPY_MAX_DELAY_S {
            return false;
        }
        let theirs: std::collections::HashSet<&str> = h.text.split_whitespace().collect();
        let hits = words.iter().filter(|w| theirs.contains(*w)).count();
        hits as f32 >= COPY_WORDS_MATCH * words.len() as f32 && env_corr(&h.env, env) >= COPY_ENV_CORR
    })
}

/// A recent caller sentence, for the echo guard; `text` once it has been transcribed.
struct CallerSpan {
    id: u64,
    start: f32,
    end: f32,
    level_db: f32,
    text: Option<String>,
}

/// Echo guard. An Agent sentence is only the caller leaking into the mic when all three hold:
/// - at least [`ECHO_OVERLAP`] of it overlaps caller speech,
/// - that caller speech is more than [`ECHO_MARGIN_DB`] louder (a leak left after echo removal and Hush is quiet),
/// - at least [`ECHO_WORDS_MATCH`] of its words (and at least [`ECHO_MIN_WORDS`]) are in that caller text: a leak
///   repeats the caller's words, a real reply said over the caller does not. The mic and the call audio come
///   through different chains with unrelated gain, so the level alone could take a quiet mic's "okay, got it" for a
///   leak; the words cannot.
const ECHO_OVERLAP: f32 = 0.5;
const ECHO_MARGIN_DB: f32 = 6.0;
const ECHO_WORDS_MATCH: f32 = 0.6;
const ECHO_MIN_WORDS: usize = 2;
/// An Agent sentence overlapping a caller sentence that is still being said waits at most this long for it to end
/// (the sentence cutter ends any sentence at 10 s).
const ECHO_WAIT_S: f32 = 11.0;
/// Caller sentences remembered for the echo guard (seconds; longer while older Agent sentences still wait).
const ECHO_MEMORY_S: f32 = 60.0;

/// Speech level of a 16 kHz sentence: the 90th percentile of its 20 ms frame levels, in dB (the sentence's own
/// silence before and after does not pull it down).
fn level_db(a: &[f32]) -> f32 {
    let mut v: Vec<f32> = a.chunks(320).map(|c| c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).collect();
    if v.is_empty() {
        return -120.0;
    }
    v.sort_by(|x, y| x.total_cmp(y));
    10.0 * v[(v.len() * 9 / 10).min(v.len() - 1)].max(1e-12).log10()
}

fn overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

/// The caller sentences an Agent sentence `(start, end, level)` could be a leak of: louder by the margin and, together,
/// covering at least [`ECHO_OVERLAP`] of it. Empty = certainly not echo.
fn echo_suspects<'a>(agent: (f32, f32, f32), callers: impl IntoIterator<Item = &'a CallerSpan>) -> Vec<&'a CallerSpan> {
    let dur = (agent.1 - agent.0).max(1e-3);
    let loud: Vec<&CallerSpan> = callers
        .into_iter()
        // the leak's source began first (a caller sentence starting later is rather a copy of the agent: see is_copy)
        .filter(|c| c.level_db - agent.2 > ECHO_MARGIN_DB && c.start <= agent.0 + 0.2 && overlap(agent.0, agent.1, c.start, c.end) > 0.0)
        .collect();
    // Covered time (caller sentences do not overlap each other, but count each moment once anyway).
    let mut spans: Vec<(f32, f32)> = loud.iter().map(|c| (c.start.max(agent.0), c.end.min(agent.1))).collect();
    spans.sort_by(|x, y| x.0.total_cmp(&y.0));
    let (mut covered, mut reach) = (0.0f32, agent.0);
    for (s, e) in spans {
        let s = s.max(reach);
        if e > s {
            covered += e - s;
            reach = e;
        }
    }
    if covered >= ECHO_OVERLAP * dur {
        loud
    } else {
        Vec::new()
    }
}

/// Do the Agent sentence's words repeat the caller's? (See [`ECHO_WORDS_MATCH`].)
fn words_repeat(agent_text: &str, caller_texts: &[&str]) -> bool {
    let words: Vec<&str> = agent_text.split_whitespace().collect();
    if words.len() < ECHO_MIN_WORDS {
        return false;
    }
    let heard: std::collections::HashSet<&str> = caller_texts.iter().flat_map(|t| t.split_whitespace()).collect();
    let hits = words.iter().filter(|w| heard.contains(*w)).count();
    hits as f32 >= ECHO_WORDS_MATCH * words.len() as f32
}

/// Where the next sentence to transcribe is in `backlog`: normally the oldest. An Agent sentence that may be a leak
/// first needs the caller's words: then the caller sentence it overlaps goes first. `None` = wait (the Agent sentence
/// overlaps a caller sentence that is still being said).
fn next_job(backlog: &std::collections::VecDeque<Sentence>, callers: &std::collections::VecDeque<CallerSpan>, caller_speaking_since: Option<f32>, now: f32) -> Option<usize> {
    let i = next_job_inner(backlog, callers, caller_speaking_since, now)?;
    // The original before its possible copy: a sentence of the other side that began shortly before this one goes
    // first, so the copy check (made on the later one) keeps the right line.
    let c = &backlog[i];
    let original = backlog
        .iter()
        .enumerate()
        .filter(|(_, o)| o.side != c.side && c.start - o.start >= COPY_MIN_DELAY_S && c.start - o.start <= COPY_MAX_DELAY_S)
        .min_by(|x, y| x.1.start.total_cmp(&y.1.start))
        .map(|(j, _)| j);
    Some(original.unwrap_or(i))
}

fn next_job_inner(backlog: &std::collections::VecDeque<Sentence>, callers: &std::collections::VecDeque<CallerSpan>, caller_speaking_since: Option<f32>, now: f32) -> Option<usize> {
    let head = backlog.front()?;
    if head.side != Side::Agent {
        return Some(0);
    }
    let suspects = echo_suspects((head.start, head.end, head.level_db), callers.iter());
    // A caller sentence it overlaps is still waiting: transcribe that first (any of them; one may have been dropped
    // from the backlog without text).
    if let Some(i) = suspects.iter().filter(|c| c.text.is_none()).find_map(|c| backlog.iter().position(|s| s.id == c.id)) {
        return Some(i);
    }
    if let Some(since) = caller_speaking_since {
        // The caller is mid-sentence and it overlaps this one: wait for it to end (not forever).
        if since < head.end && now - head.end < ECHO_WAIT_S {
            // Meanwhile the caller's own waiting sentences can go (their words are needed for the check anyway).
            return backlog.iter().position(|s| s.side == Side::Caller);
        }
    }
    Some(0)
}

/// Loads the speech model; on failure marks the transcript unavailable (with the reason) and returns `None`.
fn load_model(sh: &Shared, cfg: &TranscriptConfig) -> Option<AsrModel> {
    let Some(dir) = cfg.models_dir.clone().or_else(find_models_dir) else {
        fail(sh, "speech model not found (put asr_models next to the app, or set ASR_MODELS_DIR)".into());
        return None;
    };
    if let Err(e) = accent_onnx::ort_util::init_ort(&cfg.dll) {
        fail(sh, format!("onnxruntime: {e}"));
        return None;
    }
    AsrModel::load(&dir, cfg.threads).map_err(|e| fail(sh, e)).ok()
}

fn worker(sh: Arc<Shared>, mut cons: [Consumer<f32>; 2], cfg: TranscriptConfig) {
    lower_thread_priority();
    let mut model: Option<AsrModel> = None;
    let mut down = [CausalDownsampler3x::new(RESAMPLER_TAPS), CausalDownsampler3x::new(RESAMPLER_TAPS)];
    let mut seg = [Segmenter::new(), Segmenter::new()];
    const SIDES: [Side; 2] = [Side::Agent, Side::Caller];
    let mut chunk = vec![0.0f32; FRAME_SAMPLES];
    let mut was_enabled = false;
    let mut next_id = 0u64;
    // Finished sentences of both sides not yet transcribed, ordered by when they ended.
    let mut backlog: std::collections::VecDeque<Sentence> = std::collections::VecDeque::new();
    let mut backlog_samples = 0usize;
    let mut callers: std::collections::VecDeque<CallerSpan> = std::collections::VecDeque::new();
    let mut heard: std::collections::VecDeque<Heard> = std::collections::VecDeque::new();
    // When the caller's current sentence began (seconds), while they are speaking.
    let mut caller_speaking_since: Option<f32> = None;
    // When the call audio last delivered a frame: a source that stops mid-sentence must not look like endless talk.
    let mut caller_last_frame = 0.0f32;
    let mut partial: Option<(Side, Vec<f32>)> = None; // the newest in-progress sentence

    let drain = |cons: &mut [Consumer<f32>; 2]| {
        for c in cons.iter_mut() {
            while c.pop().is_ok() {}
        }
    };

    while !sh.stop.load(Relaxed) {
        if !sh.enabled.load(Relaxed) {
            if was_enabled {
                was_enabled = false;
                seg.iter_mut().for_each(Segmenter::reset);
                backlog.clear();
                backlog_samples = 0;
                callers.clear();
                heard.clear();
                caller_speaking_since = None;
                partial = None;
                {
                    let mut d = sh.data.lock().unwrap();
                    d.partial.clear();
                    d.last_saved_at = None; // switching off ends the call: the next line starts a new file
                }
                sh.version.fetch_add(1, Relaxed);
            }
            if sh.state.load(Relaxed) != S_UNAVAILABLE {
                // Automatic call recording: have the model ready before the first call.
                if model.is_none() && sh.preload.load(Relaxed) {
                    model = load_model(&sh, &cfg);
                }
                if sh.state.load(Relaxed) != S_UNAVAILABLE {
                    sh.state.store(S_OFF, Relaxed);
                }
            }
            sh.pending.store(0, Relaxed);
            drain(&mut cons);
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }
        if sh.state.load(Relaxed) == S_UNAVAILABLE {
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }
        if model.is_none() {
            sh.state.store(S_LOADING, Relaxed);
            model = load_model(&sh, &cfg);
            if model.is_none() {
                continue;
            }
            drain(&mut cons);
        }
        if !was_enabled {
            was_enabled = true;
            seg.iter_mut().for_each(Segmenter::reset);
            down = [CausalDownsampler3x::new(RESAMPLER_TAPS), CausalDownsampler3x::new(RESAMPLER_TAPS)];
        }
        if sh.clear_requested.swap(false, Relaxed) {
            seg.iter_mut().for_each(Segmenter::reset);
            backlog.clear();
            backlog_samples = 0;
            callers.clear();
            heard.clear();
            caller_speaking_since = None;
            partial = None;
        }
        sh.state.store(if seg.iter().any(Segmenter::in_speech) { S_TRANSCRIBING } else { S_LISTENING }, Relaxed);

        // 1. Listen: everything waiting in both rings goes through that side's segmenter (cheap, so no ring
        //    overflows). Each frame's time is now minus the audio still queued behind it in the ring, so a sentence
        //    keeps the time it was said even when the worker was busy for seconds.
        let mut fresh: Vec<Sentence> = Vec::new();
        for k in 0..2 {
            while cons[k].slots() >= FRAME_SAMPLES {
                for s in chunk.iter_mut() {
                    *s = cons[k].pop().unwrap_or(0.0);
                }
                let t_frame_end = sh.started.elapsed().as_secs_f32() - cons[k].slots() as f32 / 48_000.0;
                let x16 = down[k].push(&chunk);
                let was_speaking = seg[k].in_speech();
                let mut final_end: Option<f32> = None;
                for ev in seg[k].push(&x16) {
                    match ev {
                        Event::Final(a) => {
                            partial = None;
                            let end = t_frame_end - (seg[k].fed() - seg[k].last_final_end()) as f32 / 16_000.0;
                            final_end = Some(end);
                            next_id += 1;
                            fresh.push(Sentence { id: next_id, side: SIDES[k], start: end - a.len() as f32 / 16_000.0, end, level_db: level_db(&a), audio: a });
                        }
                        // A newer partial of the same sentence makes an older one pointless.
                        Event::Partial(a) => partial = Some((SIDES[k], a)),
                    }
                }
                if k == 1 {
                    caller_last_frame = t_frame_end;
                    caller_speaking_since = match (was_speaking, seg[1].in_speech(), final_end) {
                        (_, false, _) => None,
                        // A long sentence was cut (10 s) and goes on: the open rest starts where the cut piece ended.
                        (_, true, Some(end)) => Some(end),
                        (false, true, None) => Some(t_frame_end - 0.3), // the cutter keeps 300 ms before speech
                        (true, true, None) => caller_speaking_since,
                    };
                }
            }
        }
        for s in fresh {
            if s.side == Side::Caller {
                callers.push_back(CallerSpan { id: s.id, start: s.start, end: s.end, level_db: s.level_db, text: None });
            }
            backlog_samples += s.audio.len();
            // Keep the backlog in the order said: the two sides are read one after the other.
            let at = backlog.iter().rposition(|b| b.end <= s.end).map_or(0, |i| i + 1);
            backlog.insert(at, s);
        }
        while backlog_samples as f32 > MAX_BACKLOG_S * 16_000.0 && backlog.len() > 1 {
            if let Some(old) = backlog.pop_front() {
                backlog_samples -= old.audio.len();
            }
            sh.data.lock().unwrap().message = "the PC was busy for a long time; some older speech was not transcribed".into();
        }
        // Forget caller sentences no waiting Agent sentence can overlap any more.
        let now = sh.started.elapsed().as_secs_f32();
        let keep_from = backlog.iter().filter(|s| s.side == Side::Agent).map(|s| s.start).fold(now - ECHO_MEMORY_S, f32::min);
        while callers.front().is_some_and(|c| c.end < keep_from) {
            callers.pop_front();
        }
        while heard.front().is_some_and(|h| h.start < now - COPY_MEMORY_S) {
            heard.pop_front();
        }

        sh.pending.store(backlog.len() as u32, Relaxed);

        // 2. Transcribe: oldest finished sentence first; the in-progress text only when nothing is waiting.
        if sh.busy.load(Relaxed) {
            partial = None; // a busy PC skips the live preview; the finished line still comes
            std::thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        // No call audio for a second (the source stopped or was switched): the caller is not "still talking".
        let speaking = caller_speaking_since.filter(|_| now - caller_last_frame < 1.0);
        let job = match next_job(&backlog, &callers, speaking, now) {
            Some(i) => backlog.remove(i).map(|s| {
                backlog_samples -= s.audio.len();
                s
            }),
            None if !backlog.is_empty() => {
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            None => None,
        };
        let (side, at, audio, sentence) = match job {
            Some(s) => {
                let meta = Some((s.id, s.start, s.end, s.level_db, envelope(&s.audio)));
                (s.side, Some(s.end.max(0.0) as u32), s.audio, meta)
            }
            None => match partial.take() {
                Some((side, a)) => (side, None, a, None),
                None => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
            },
        };
        let Some(m) = model.as_mut() else { continue };
        let gen = sh.drop_gen.load(Relaxed);
        let t0 = Instant::now();
        match m.transcribe(&audio) {
            // Cleared (or every file deleted) while this was being transcribed: it belongs to what was cleared.
            Ok(_) if sh.drop_gen.load(Relaxed) != gen => {}
            Ok(text) => {
                let rtf = t0.elapsed().as_secs_f32() / (audio.len() as f32 / 16_000.0);
                sh.rtf_milli.store((rtf * 1000.0) as u32, Relaxed);
                if let Some((id, start, end, level, env)) = sentence {
                    // The same voice on both streams: keep the original only.
                    let copy = is_copy(side, start, &env, &text, &heard);
                    if !text.is_empty() {
                        heard.push_back(Heard { side, start, env, text: text.clone() });
                    }
                    if copy {
                        sh.echo_dropped.fetch_add(1, Relaxed);
                        sh.data.lock().unwrap().partial.clear();
                        sh.version.fetch_add(1, Relaxed);
                        continue;
                    }
                    match side {
                        Side::Caller => {
                            if let Some(c) = callers.iter_mut().find(|c| c.id == id) {
                                c.text = Some(text.clone());
                            }
                        }
                        Side::Agent => {
                            let suspects = echo_suspects((start, end, level), callers.iter());
                            let heard: Vec<&str> = suspects.iter().filter_map(|c| c.text.as_deref()).collect();
                            if !suspects.is_empty() && words_repeat(&text, &heard) {
                                sh.echo_dropped.fetch_add(1, Relaxed);
                                sh.data.lock().unwrap().partial.clear();
                                sh.version.fetch_add(1, Relaxed);
                                continue;
                            }
                        }
                    }
                }
                match at {
                    Some(at) if !text.is_empty() => publish_line(&sh, at, side, text, &cfg.save_dir),
                    Some(_) => {
                        sh.data.lock().unwrap().partial.clear();
                        sh.version.fetch_add(1, Relaxed);
                    }
                    None => {
                        // The live preview says whose words they are.
                        sh.data.lock().unwrap().partial = if text.is_empty() { text } else { format!("{}: {text}", side.label()) };
                        sh.version.fetch_add(1, Relaxed);
                    }
                }
            }
            Err(e) => sh.data.lock().unwrap().message = e,
        }
    }
}

#[cfg(test)]
mod echo_guard_tests {
    use super::*;

    fn caller(id: u64, start: f32, end: f32, level_db: f32, text: Option<&str>) -> CallerSpan {
        CallerSpan { id, start, end, level_db, text: text.map(String::from) }
    }

    #[test]
    fn a_quiet_agent_sentence_under_loud_caller_speech_is_a_suspect() {
        let c = [caller(1, 10.0, 14.0, -20.0, None)];
        assert_eq!(echo_suspects((10.5, 13.5, -45.0), &c).len(), 1);
    }

    #[test]
    fn the_agent_as_loud_as_the_caller_or_not_overlapping_is_no_suspect() {
        let c = [caller(1, 10.0, 14.0, -20.0, None)];
        assert!(echo_suspects((10.5, 13.5, -24.0), &c).is_empty(), "about as loud as the caller: real speech");
        assert!(echo_suspects((20.0, 23.0, -45.0), &c).is_empty(), "no overlap");
        assert!(echo_suspects((13.0, 17.0, -45.0), &c).is_empty(), "only a quarter overlaps");
    }

    #[test]
    fn overlapping_caller_sentences_count_each_moment_once() {
        // two caller sentences that both cover 10..11 s: the agent sentence 10..12 s is only half... covered once
        let c = [caller(1, 9.0, 11.0, -20.0, None), caller(2, 10.0, 11.0, -20.0, None)];
        assert_eq!(echo_suspects((10.0, 12.0, -45.0), &c).len(), 2, "exactly half covered: still a suspect");
        assert!(echo_suspects((10.0, 12.5, -45.0), &c).is_empty(), "40% covered (counted once), not 80%");
    }

    #[test]
    fn only_repeated_words_make_a_leak() {
        let heard = ["thank you for calling safe road insurance my name is rio"];
        assert!(words_repeat("safe road insurance my name is rio", &heard));
        assert!(!words_repeat("okay let me check that for you", &heard), "a real reply over the caller is kept");
        assert!(!words_repeat("insurance", &heard), "one word is too little to call it a leak");
        assert!(!words_repeat("safe road insurance", &[]), "no caller text, no drop");
    }

    #[test]
    fn a_suspect_agent_sentence_waits_for_the_callers_words() {
        let s = |id, side, start, end, level_db| Sentence { id, side, start, end, level_db, audio: vec![] };
        let mut backlog = std::collections::VecDeque::from([s(1, Side::Agent, 10.5, 13.5, -45.0), s(2, Side::Caller, 10.0, 14.0, -20.0)]);
        let mut callers = std::collections::VecDeque::from([caller(2, 10.0, 14.0, -20.0, None)]);
        assert_eq!(next_job(&backlog, &callers, None, 15.0), Some(1), "the caller sentence goes first");
        callers[0].text = Some("x".into());
        backlog.remove(1);
        assert_eq!(next_job(&backlog, &callers, None, 15.0), Some(0));
        // caller still mid-sentence since 12 s: wait, unless the agent sentence is old
        let none = std::collections::VecDeque::new();
        assert_eq!(next_job(&backlog, &none, Some(12.0), 14.0), None);
        assert_eq!(next_job(&backlog, &none, Some(12.0), 30.0), Some(0), "waited long enough");
        assert_eq!(next_job(&backlog, &none, Some(14.0), 14.5), Some(0), "the caller started after it ended");
    }

    #[test]
    fn any_waiting_caller_sentence_it_overlaps_goes_first() {
        let s = |id, side, start, end, level_db| Sentence { id, side, start, end, level_db, audio: vec![] };
        // caller sentence 2 was dropped from the backlog without text; caller sentence 3 still waits
        let backlog = std::collections::VecDeque::from([s(1, Side::Agent, 10.0, 14.0, -45.0), s(3, Side::Caller, 9.9, 14.5, -20.0)]);
        let callers = std::collections::VecDeque::from([caller(2, 8.0, 9.9, -20.0, None), caller(3, 9.9, 14.5, -20.0, None)]);
        assert_eq!(next_job(&backlog, &callers, None, 16.0), Some(1));
    }

    #[test]
    fn a_caller_sentence_starting_later_is_no_leak_source() {
        // the far end sending the agent back: the caller copy is louder but starts after the agent's original
        let c = [caller(1, 11.0, 14.0, -15.0, None)];
        assert!(echo_suspects((10.0, 13.0, -30.0), &c).is_empty(), "the original must not be dropped as a leak");
    }

    #[test]
    fn the_original_is_transcribed_before_its_possible_copy() {
        let s = |id, side, start, end| Sentence { id, side, start, end, level_db: -20.0, audio: vec![] };
        // the caller copy happened to end first
        let backlog = std::collections::VecDeque::from([s(2, Side::Caller, 11.0, 12.8), s(1, Side::Agent, 10.0, 13.0)]);
        assert_eq!(next_job(&backlog, &std::collections::VecDeque::new(), None, 20.0), Some(1));
    }

    fn speechlike(seed: u32, secs: f32) -> Vec<f32> {
        // syllable-like bursts whose rhythm depends on the seed
        let n = (secs * 16_000.0) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                let rate = 3.0 + seed as f32 * 1.7;
                let env = (0.5 + 0.5 * (std::f32::consts::TAU * rate * t + seed as f32).sin()).powi(3);
                env * 0.2 * (std::f32::consts::TAU * 180.0 * t).sin()
            })
            .collect()
    }

    #[test]
    fn a_copy_has_the_same_loudness_shape_another_voice_does_not() {
        let a = speechlike(1, 2.0);
        let mut copy = vec![0.0f32; 4_000]; // 0.25 s later, quieter
        copy.extend(a.iter().map(|x| x * 0.3));
        let other = speechlike(4, 2.0);
        let (ea, ec, eo) = (envelope(&a), envelope(&copy), envelope(&other));
        assert!(env_corr(&ea, &ec) > 0.9, "copy {}", env_corr(&ea, &ec));
        assert!(env_corr(&ea, &eo) < COPY_ENV_CORR, "other {}", env_corr(&ea, &eo));
    }

    #[test]
    fn only_a_later_same_sounding_repeat_on_the_other_side_is_a_copy() {
        let a = speechlike(1, 2.0);
        let heard = std::collections::VecDeque::from([Heard { side: Side::Agent, start: 10.0, env: envelope(&a), text: "what's your name".into() }]);
        let same = envelope(&a);
        let other = envelope(&speechlike(4, 2.0));
        assert!(is_copy(Side::Caller, 11.0, &same, "what's her name", &heard), "the far end sending the agent back");
        assert!(!is_copy(Side::Caller, 11.0, &other, "what's her name", &heard), "another voice, same words: a real reply");
        assert!(!is_copy(Side::Caller, 11.0, &same, "i live in texas", &heard), "other words");
        assert!(!is_copy(Side::Caller, 9.5, &same, "what's her name", &heard), "it started first: it is the original");
        assert!(!is_copy(Side::Caller, 14.0, &same, "what's her name", &heard), "too long after");
        assert!(!is_copy(Side::Agent, 11.0, &same, "what's your name", &heard), "same side");
    }

    #[test]
    fn a_line_transcribed_during_a_clear_is_dropped() {
        let h = TranscriptHandle(Arc::new(new_shared(true)));
        let g = h.clear_gen();
        h.clear();
        assert_ne!(h.clear_gen(), g, "the worker compares this before publishing");
    }

    #[test]
    fn speech_level_ignores_the_silence_around_it() {
        let mut a = vec![0.0f32; 16_000];
        a.extend((0..16_000).map(|i| 0.1 * (i as f32 * 0.05).sin()));
        let db = level_db(&a);
        assert!((db - (-23.0)).abs() < 1.0, "a 0.1 sine is about -23 dB, got {db}");
        assert_eq!(level_db(&[]), -120.0);
    }
}

#[cfg(test)]
mod line_count_tests {
    use super::*;

    #[test]
    fn new_lines_are_still_seen_after_the_500_line_cap() {
        let h = TranscriptHandle(Arc::new(new_shared(false)));
        for i in 0..MAX_LINES + 100 {
            publish_line(&h.0, 0, Side::Agent, format!("line {i}"), &None);
        }
        assert_eq!(h.status().line_count, MAX_LINES + 100, "the count keeps going past the cap");
        let sent = MAX_LINES + 97;
        let (new, _) = h.snapshot(h.status().line_count - sent);
        let texts: Vec<_> = new.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["line 597", "line 598", "line 599"], "only the 3 new lines");
    }
}

#[cfg(test)]
mod call_file_tests {
    use super::*;

    #[test]
    fn each_call_gets_its_own_file_with_labelled_lines() {
        let dir = std::env::temp_dir().join(format!("clearspeak-call-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let h = TranscriptHandle(Arc::new(new_shared(true)));
        let save = Some(dir.clone());
        publish_line(&h.0, 5, Side::Agent, "how can i help".into(), &save);
        publish_line(&h.0, 9, Side::Caller, "my internet is down".into(), &save);
        let first = h.save_path().unwrap();
        publish_line(&h.0, 9 + CALL_GAP_S, Side::Agent, "next caller please".into(), &save);
        let second = h.save_path().unwrap();
        assert_ne!(first, second, "a long quiet gap starts a new call file");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "[00:05] Agent: how can i help\n[00:09] Caller: my internet is down\n");
        assert!(std::fs::read_to_string(&second).unwrap().ends_with("Agent: next caller please\n"));
        assert!(second.file_name().unwrap().to_string_lossy().contains("_call"));
        let json = std::fs::read_to_string(first.with_extension("jsonl")).unwrap();
        assert_eq!(json.lines().count(), 2, "the JSON copy has the same lines");
        assert_eq!(json.lines().nth(1).unwrap(), r#"{"at":"00:09","secs":9,"side":"Caller","text":"my internet is down"}"#);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn after_delete_all_the_rest_of_the_call_goes_to_a_new_file() {
        let dir = std::env::temp_dir().join(format!("clearspeak-call-deleted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let h = TranscriptHandle(Arc::new(new_shared(true)));
        let save = Some(dir.clone());
        publish_line(&h.0, 5, Side::Agent, "first".into(), &save);
        let first = h.save_path().unwrap();
        std::fs::remove_file(&first).unwrap();
        std::fs::remove_file(first.with_extension("jsonl")).unwrap();
        publish_line(&h.0, 8, Side::Caller, "second".into(), &save);
        let second = h.save_path().unwrap();
        assert!(second.exists());
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "[00:08] Caller: second\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sets the current call's start (seconds since start) as if `begin_call` ran then.
    fn call_started_at(h: &TranscriptHandle, start: u32) {
        h.0.data.lock().unwrap().call.as_mut().unwrap().start = start;
    }

    fn call_ended_at(h: &TranscriptHandle, end: u32) {
        h.0.data.lock().unwrap().call.as_mut().unwrap().end = Some(end);
    }

    #[test]
    fn detected_calls_get_one_file_each_and_nothing_outside_a_call_is_saved() {
        let dir = std::env::temp_dir().join(format!("clearspeak-call-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let h = TranscriptHandle(Arc::new(new_shared(true)));
        let save = Some(dir.clone());
        h.set_call_mode(true);
        publish_line(&h.0, 3, Side::Agent, "before the call".into(), &save);
        assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none(), "nothing saved outside a call");

        let first = dir.join("2026-11-26_10-00-00_call_eyeBeam_+919876543210");
        h.begin_call(first.clone(), "Caller: +919876543210  App: eyeBeam", 0);
        call_started_at(&h, 100);
        publish_line(&h.0, 105, Side::Agent, "thank you for calling".into(), &save);
        // A long hold inside the call does not split it.
        publish_line(&h.0, 105 + CALL_GAP_S + 60, Side::Caller, "i am back".into(), &save);
        call_ended_at(&h, 400);
        publish_line(&h.0, 401, Side::Agent, "bye".into(), &save);
        publish_line(&h.0, 420, Side::Agent, "after the call".into(), &save);

        let second = dir.join("2026-11-26_10-10-00_call_eyeBeam");
        h.begin_call(second.clone(), "", 0);
        call_started_at(&h, 500);
        publish_line(&h.0, 510, Side::Caller, "hello".into(), &save);

        let txt = std::fs::read_to_string(stem_with(&first, "txt")).unwrap();
        assert_eq!(
            txt,
            "Caller: +919876543210  App: eyeBeam\n[00:05] Agent: thank you for calling\n[03:05] Caller: i am back\n[05:01] Agent: bye\n",
            "header, times from the call's start, the hold kept in, the line after the end left out"
        );
        let json = std::fs::read_to_string(stem_with(&first, "jsonl")).unwrap();
        assert!(json.starts_with(r#"{"meta":"Caller: +919876543210  App: eyeBeam"}"#), "{json}");
        assert_eq!(json.lines().count(), 4);
        assert_eq!(std::fs::read_to_string(stem_with(&second, "txt")).unwrap(), "[00:10] Caller: hello\n");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 4, "two calls, a .txt and a .jsonl each");
        assert_eq!(h.snapshot(10).0.len(), 6, "every line is still shown in the window");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn call_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clearspeak-callmode-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn back_to_back_the_last_sentence_stays_with_its_own_call() {
        let dir = call_dir("b2b");
        let h = TranscriptHandle(Arc::new(new_shared(true)));
        let save = Some(dir.clone());
        let a = dir.join("a_call_VICIdial_111");
        let b = dir.join("b_call_VICIdial_222");
        h.begin_call(a.clone(), "", 0);
        call_started_at(&h, 100);
        call_ended_at(&h, 200);
        h.begin_call(b.clone(), "", 0);
        call_started_at(&h, 200);
        // A's last sentence ended as B began and is transcribed only now.
        publish_line(&h.0, 201, Side::Caller, "thanks bye".into(), &save);
        publish_line(&h.0, 205, Side::Caller, "hello".into(), &save);
        assert_eq!(std::fs::read_to_string(stem_with(&a, "txt")).unwrap(), "[01:41] Caller: thanks bye\n");
        assert_eq!(std::fs::read_to_string(stem_with(&b, "txt")).unwrap(), "[00:05] Caller: hello\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hold_is_left_out_like_in_the_wav() {
        let dir = call_dir("hold");
        let h = TranscriptHandle(Arc::new(new_shared(true)));
        let save = Some(dir.clone());
        let c = dir.join("c_call_eyeBeam");
        h.begin_call(c.clone(), "", 0);
        call_started_at(&h, 100);
        h.0.data.lock().unwrap().call.as_mut().unwrap().holds = vec![(130, Some(190))];
        publish_line(&h.0, 120, Side::Agent, "one moment please".into(), &save);
        publish_line(&h.0, 150, Side::Agent, "talking to a colleague".into(), &save);
        publish_line(&h.0, 200, Side::Agent, "thanks for waiting".into(), &save);
        assert_eq!(
            std::fs::read_to_string(stem_with(&c, "txt")).unwrap(),
            "[00:20] Agent: one moment please\n[00:40] Agent: thanks for waiting\n",
            "nothing from the hold, and its minute left out of the times"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deleted_call_file_is_not_recreated() {
        let dir = call_dir("mode-deleted");
        let h = TranscriptHandle(Arc::new(new_shared(true)));
        let save = Some(dir.clone());
        let c = dir.join("c_call_eyeBeam");
        h.begin_call(c.clone(), "Call x", 0);
        std::fs::remove_file(stem_with(&c, "txt")).unwrap();
        publish_line(&h.0, 1, Side::Agent, "after delete all".into(), &save);
        assert!(!stem_with(&c, "txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_the_box_during_a_call_keeps_the_sentences_in_progress() {
        let h = TranscriptHandle(Arc::new(new_shared(true)));
        h.begin_call(call_dir("clear").join("c"), "", 0);
        let g = h.0.drop_gen.load(Relaxed);
        h.clear();
        assert_eq!(h.0.drop_gen.load(Relaxed), g, "in a call: nothing in progress is dropped");
        assert!(!h.0.clear_requested.load(Relaxed));
        h.end_call();
        h.clear();
        assert_ne!(h.0.drop_gen.load(Relaxed), g, "outside a call it is");
    }

    #[test]
    fn a_stem_with_a_dot_keeps_it() {
        assert_eq!(stem_with(Path::new("a/x_call_eyeBeam.exe"), "wav"), PathBuf::from("a/x_call_eyeBeam.exe.wav"));
    }
}

#[cfg(test)]
mod json_tests {
    use super::*;

    #[test]
    fn quotes_backslashes_and_control_characters_are_escaped() {
        assert_eq!(
            json_line(65, Side::Agent, "say \"hi\" \\ ok\n"),
            r#"{"at":"01:05","secs":65,"side":"Agent","text":"say \"hi\" \\ ok\u000a"}"#
        );
    }
}
