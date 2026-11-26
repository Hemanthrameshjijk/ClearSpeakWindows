//! Call audio recording: one stereo WAV per call, the agent on the left (what the caller heard, after the whole mic
//! chain) and the caller on the right (what the agent heard).
//!
//! The audio threads only copy their frame into a lock-free ring ([`RecorderTap::push`], one atomic load when
//! recording is off). A low-priority writer thread downsamples to 16 kHz, keeps the two sides in step, and writes the
//! file. While armed (automatic call recording on) it always keeps the last [`PRE_ROLL_S`] seconds, so a call that is
//! only recognised a few seconds in still starts with its first words.

use accent_onnx::causal_resampler::CausalDownsampler3x;
use dsp_core::{Consumer, Producer, RingBuffer, FRAME_SAMPLES};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::wav::{to_i16, WavWriter};

/// Sample rate of the recordings (telephone audio has nothing above 8 kHz).
pub const RATE: u32 = 16_000;
/// Seconds kept from before the call was recognised.
pub const PRE_ROLL_S: usize = 10;
/// A side that delivered nothing for this long is silent (not routed, or stopped): the other side is not held back.
const DRY_MS: u64 = 300;
/// The two sides drifted this far apart: the leading side is caught up (1 sample in 100 skipped) until close again.
const SKEW_HIGH: usize = RATE as usize / 2;
const SKEW_LOW: usize = RATE as usize / 10;
/// The header is brought up to date this often (a file cut off by a crash plays up to here).
const SYNC_EVERY: usize = RATE as usize * 5;
/// Caller sound above this level counts as the caller being there (speech or hold music, not line hiss).
const CALLER_ACTIVE_DB: f32 = -45.0;
const RING_S: usize = 2;

const AGENT: usize = 0;
const CALLER: usize = 1;

enum Cmd {
    /// The file, and how many seconds from before the call was recognised go first.
    Start(PathBuf, usize),
    Pause,
    Resume,
    Stop,
}

struct Shared {
    armed: AtomicBool,
    /// Samples (48 kHz) each side lost before reaching the ring (the pipeline cut its backlog, or the ring was
    /// full): the writer puts the same length of silence in, so the two sides stay in step.
    gaps: [AtomicUsize; 2],
    cmds: Mutex<Vec<Cmd>>,
    /// Seconds written to the current file; `None` = not recording.
    recording: Mutex<Option<u32>>,
    /// Milliseconds since `started` when caller sound / any caller audio was last seen (0 = never).
    caller_heard_ms: AtomicU64,
    caller_flow_ms: AtomicU64,
    started: Instant,
}

fn new_shared() -> Shared {
    Shared {
        armed: AtomicBool::new(false),
        gaps: [AtomicUsize::new(0), AtomicUsize::new(0)],
        cmds: Mutex::new(Vec::new()),
        recording: Mutex::new(None),
        caller_heard_ms: AtomicU64::new(0),
        caller_flow_ms: AtomicU64::new(0),
        started: Instant::now(),
    }
}

/// Audio-thread end: one for the mic thread (agent), one for the call-audio thread (caller).
pub struct RecorderTap {
    sh: Arc<Shared>,
    prod: Producer<f32>,
    side: usize,
}

impl RecorderTap {
    /// Copies the frame (never changes it). Off = one atomic load.
    pub fn push(&mut self, frame: &[f32]) {
        if !self.sh.armed.load(Relaxed) {
            return;
        }
        for (i, &s) in frame.iter().enumerate() {
            if self.prod.push(s).is_err() {
                // Writer stalled: a gap in the recording (filled with silence), never a stall in the audio.
                self.gap(frame.len() - i);
                break;
            }
        }
    }

    /// `n` samples (48 kHz) of this side were dropped before reaching the tap (the pipeline cut its backlog).
    pub fn gap(&self, n: usize) {
        if n > 0 && self.sh.armed.load(Relaxed) {
            self.sh.gaps[self.side].fetch_add(n, Relaxed);
        }
    }
}

/// Control handle (cheap to clone).
#[derive(Clone)]
pub struct CallRecorder(Arc<Shared>);

impl Default for CallRecorder {
    /// Inert handle (no engine): commands are accepted and nothing is recorded.
    fn default() -> Self {
        Self(Arc::new(new_shared()))
    }
}

impl CallRecorder {
    /// On: the taps feed the writer and the pre-roll is kept. Off also stops a recording in progress.
    pub fn set_armed(&self, on: bool) {
        if !on {
            self.stop();
        }
        self.0.armed.store(on, Relaxed);
    }

    pub fn armed(&self) -> bool {
        self.0.armed.load(Relaxed)
    }

    /// Starts a new file at `path` (stopping any current one), beginning with the last `pre_roll_secs` seconds (at
    /// most [`PRE_ROLL_S`]): what was said before the call was recognised.
    pub fn start(&self, path: PathBuf, pre_roll_secs: usize) {
        self.0.cmds.lock().unwrap().push(Cmd::Start(path, pre_roll_secs.min(PRE_ROLL_S)));
    }

    /// On hold: nothing is written until [`Self::resume`].
    pub fn pause(&self) {
        self.0.cmds.lock().unwrap().push(Cmd::Pause);
    }

    pub fn resume(&self) {
        self.0.cmds.lock().unwrap().push(Cmd::Resume);
    }

    pub fn stop(&self) {
        self.0.cmds.lock().unwrap().push(Cmd::Stop);
    }

    /// Stops and waits (up to `max`) until the file is finished: for quitting, which ends the process at once.
    pub fn stop_and_wait(&self, max: Duration) {
        if self.recording_secs().is_none() {
            return;
        }
        self.stop();
        let t0 = Instant::now();
        while self.recording_secs().is_some() && t0.elapsed() < max {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Seconds recorded so far, while a file is open.
    pub fn recording_secs(&self) -> Option<u32> {
        *self.0.recording.lock().unwrap()
    }

    fn ago(&self, ms: &AtomicU64) -> Option<u32> {
        match ms.load(Relaxed) {
            0 => None,
            t => Some(((self.0.started.elapsed().as_millis() as u64).saturating_sub(t) / 1000) as u32),
        }
    }

    /// Seconds since the caller side last had sound (speech or hold music); `None` = never (while armed).
    pub fn caller_heard_secs_ago(&self) -> Option<u32> {
        self.ago(&self.0.caller_heard_ms)
    }

    /// Call audio is reaching ClearSpeak (the call app's speaker is routed through it).
    pub fn caller_audio_flowing(&self) -> bool {
        self.ago(&self.0.caller_flow_ms).is_some_and(|s| s < 3)
    }
}

/// Agent tap (mic thread), caller tap (call-audio thread) and the control handle; starts the writer thread.
pub fn start() -> (RecorderTap, RecorderTap, CallRecorder) {
    let sh = Arc::new(new_shared());
    let (pa, ca) = RingBuffer::<f32>::new(48_000 * RING_S);
    let (pc, cc) = RingBuffer::<f32>::new(48_000 * RING_S);
    let w = sh.clone();
    if let Err(e) = std::thread::Builder::new().name("clearspeak-call-recorder".into()).spawn(move || writer(w, [ca, cc])) {
        crate::log_info!("[call] recorder thread could not start: {e}");
    }
    (RecorderTap { sh: sh.clone(), prod: pa, side: AGENT }, RecorderTap { sh: sh.clone(), prod: pc, side: CALLER }, CallRecorder(sh))
}

fn writer(sh: Arc<Shared>, mut cons: [Consumer<f32>; 2]) {
    crate::sysutil::lower_current_thread_priority();
    let mut down = [CausalDownsampler3x::new(97), CausalDownsampler3x::new(97)];
    let mut core = Core::default();
    let mut chunk = vec![0.0f32; FRAME_SAMPLES];
    loop {
        std::thread::sleep(Duration::from_millis(20));
        let now = sh.started.elapsed().as_millis() as u64;
        for cmd in std::mem::take(&mut *sh.cmds.lock().unwrap()) {
            match cmd {
                Cmd::Start(path, pre_secs) => {
                    core.finish();
                    if let Err(e) = core.start(&path, pre_secs) {
                        crate::log_info!("[call] could not create the recording file: {e}");
                    }
                }
                Cmd::Pause => core.paused = true,
                Cmd::Resume => {
                    core.paused = false;
                    core.pre.clear(); // what played during the hold is not part of the call
                }
                Cmd::Stop => core.finish(),
            }
        }
        *sh.recording.lock().unwrap() = core.file.as_ref().map(WavWriter::seconds);
        if !sh.armed.load(Relaxed) {
            for c in cons.iter_mut() {
                while c.pop().is_ok() {}
            }
            core.reset_buffers();
            continue;
        }
        for k in [AGENT, CALLER] {
            let lost = sh.gaps[k].swap(0, Relaxed);
            if lost >= 3 {
                core.feed(k, &vec![0.0; lost / 3], now);
            }
            while cons[k].slots() >= FRAME_SAMPLES {
                for s in chunk.iter_mut() {
                    *s = cons[k].pop().unwrap_or(0.0);
                }
                let x16 = down[k].push(&chunk);
                if k == CALLER {
                    sh.caller_flow_ms.store(now.max(1), Relaxed);
                    if level_db(&x16) > CALLER_ACTIVE_DB {
                        sh.caller_heard_ms.store(now.max(1), Relaxed);
                    }
                }
                core.feed(k, &x16, now);
            }
        }
        if let Err(e) = core.drain(now) {
            crate::log_info!("[call] writing the recording failed, it stops here: {e}");
            core.file = None;
        }
        *sh.recording.lock().unwrap() = core.file.as_ref().map(WavWriter::seconds);
    }
}

fn level_db(x: &[f32]) -> f32 {
    if x.is_empty() {
        return -120.0;
    }
    let ms = x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32;
    10.0 * ms.max(1e-12).log10()
}

/// The part that needs no thread: lines the two sides up and writes them (tested directly).
#[derive(Default)]
struct Core {
    side: [VecDeque<i16>; 2],
    /// When each side last delivered samples (ms).
    last: [u64; 2],
    /// Side being caught up after drifting ahead.
    catching_up: Option<usize>,
    skip_count: usize,
    /// The last [`PRE_ROLL_S`] seconds, interleaved pairs.
    pre: VecDeque<[i16; 2]>,
    file: Option<WavWriter>,
    paused: bool,
    since_sync: usize,
    /// The first file of this recording and the number of the part being written (a very long call continues in
    /// `<name>_part2.wav` and so on, so no file passes [`ROLL_BYTES`]).
    path: Option<PathBuf>,
    part: u32,
}

/// A WAV file holds at most 4 GB, and many players stop at 2 GB: a recording continues in a new part before that
/// (about 8.7 hours each).
const ROLL_BYTES: u64 = 2_000_000_000;

/// `x.wav` -> `x_part2.wav`.
fn part_path(first: &std::path::Path, part: u32) -> PathBuf {
    let s = first.to_string_lossy();
    let base = s.strip_suffix(".wav").unwrap_or(&s);
    PathBuf::from(format!("{base}_part{part}.wav"))
}

impl Core {
    fn start(&mut self, path: &std::path::Path, pre_secs: usize) -> std::io::Result<()> {
        self.open_file(path, pre_secs)?;
        self.path = Some(path.to_path_buf());
        self.part = 1;
        Ok(())
    }

    fn open_file(&mut self, path: &std::path::Path, pre_secs: usize) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut w = WavWriter::create(path, RATE, 2)?;
        let skip = self.pre.len().saturating_sub(pre_secs * RATE as usize);
        let pre: Vec<i16> = self.pre.drain(..).skip(skip).flatten().collect();
        w.write(&pre)?;
        w.sync()?;
        self.file = Some(w);
        self.paused = false;
        self.since_sync = 0;
        Ok(())
    }

    fn finish(&mut self) {
        if let Some(w) = self.file.take() {
            let (secs, mb) = (w.seconds(), w.bytes() as f64 / 1e6);
            match w.finish() {
                Ok(()) => crate::log_info!("[call] audio saved: {} min {} s, {mb:.1} MB", secs / 60, secs % 60),
                Err(e) => crate::log_info!("[call] audio file could not be finished: {e}"),
            }
        }
        self.paused = false;
    }

    fn reset_buffers(&mut self) {
        self.side.iter_mut().for_each(VecDeque::clear);
        self.pre.clear();
        self.catching_up = None;
    }

    fn feed(&mut self, k: usize, x: &[f32], now: u64) {
        if x.is_empty() {
            return;
        }
        self.side[k].extend(x.iter().map(|&v| to_i16(v)));
        self.last[k] = now;
    }

    /// Moments both sides have delivered, as (agent, caller) pairs. A side that went quiet (not routed) is filled
    /// with silence so the other is not held back.
    fn pairs(&mut self, now: u64) -> Vec<[i16; 2]> {
        for k in [AGENT, CALLER] {
            let o = 1 - k;
            if self.side[k].len() > self.side[o].len() && now.saturating_sub(self.last[o]) > DRY_MS {
                let n = self.side[k].len();
                self.side[o].resize(n, 0);
            }
        }
        // Clock drift: the side that ran ahead is caught up slowly (1 % faster), never by cutting a piece out.
        let skew = self.side[AGENT].len().abs_diff(self.side[CALLER].len());
        if skew > SKEW_HIGH {
            self.catching_up = Some(if self.side[AGENT].len() > self.side[CALLER].len() { AGENT } else { CALLER });
        } else if skew < SKEW_LOW {
            self.catching_up = None;
        }
        let n = self.side[AGENT].len().min(self.side[CALLER].len());
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            if let Some(k) = self.catching_up {
                self.skip_count += 1;
                if self.skip_count % 100 == 0 {
                    self.side[k].pop_front();
                    if self.side[k].len() < self.side[1 - k].len() {
                        break;
                    }
                }
            }
            let (Some(a), Some(c)) = (self.side[AGENT].pop_front(), self.side[CALLER].pop_front()) else { break };
            out.push([a, c]);
        }
        out
    }

    fn drain(&mut self, now: u64) -> std::io::Result<()> {
        let pairs = self.pairs(now);
        match self.file.as_mut() {
            Some(w) if !self.paused => {
                let flat: Vec<i16> = pairs.iter().flatten().copied().collect();
                w.write(&flat)?;
                self.since_sync += pairs.len();
                if self.since_sync >= SYNC_EVERY {
                    self.since_sync = 0;
                    w.sync()?;
                }
                if w.bytes() >= ROLL_BYTES {
                    if let Some(first) = self.path.clone() {
                        let part = self.part + 1;
                        self.finish();
                        self.open_file(&part_path(&first, part), 0)?;
                        self.path = Some(first);
                        self.part = part;
                    }
                }
            }
            _ => {
                self.pre.extend(pairs);
                let over = self.pre.len().saturating_sub(PRE_ROLL_S * RATE as usize);
                self.pre.drain(..over);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_stereo(p: &std::path::Path) -> Vec<[i16; 2]> {
        let b = std::fs::read(p).unwrap();
        assert_eq!(u16::from_le_bytes([b[22], b[23]]), 2);
        b[44..].chunks_exact(4).map(|c| [i16::from_le_bytes([c[0], c[1]]), i16::from_le_bytes([c[2], c[3]])]).collect()
    }

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("clearspeak-rec-{tag}-{}.wav", std::process::id()))
    }

    #[test]
    fn agent_left_caller_right_with_the_pre_roll_first() {
        let p = tmp("lr");
        let mut c = Core::default();
        // 1 s before the call is recognised: kept as pre-roll.
        c.feed(AGENT, &vec![0.5; 16_000], 0);
        c.feed(CALLER, &vec![-0.5; 16_000], 0);
        c.drain(0).unwrap();
        c.start(&p, PRE_ROLL_S).unwrap();
        c.feed(AGENT, &vec![0.25; 8_000], 10);
        c.feed(CALLER, &vec![-0.25; 8_000], 10);
        c.drain(10).unwrap();
        c.finish();
        let s = read_stereo(&p);
        assert_eq!(s.len(), 24_000, "1 s pre-roll + 0.5 s");
        assert_eq!(s[0], [to_i16(0.5), to_i16(-0.5)], "agent left, caller right");
        assert_eq!(s[23_999], [to_i16(0.25), to_i16(-0.25)]);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_long_recording_continues_in_a_new_part() {
        assert_eq!(part_path(std::path::Path::new("d/x_call_eyeBeam.wav"), 2), PathBuf::from("d/x_call_eyeBeam_part2.wav"));
        assert!(crate::transcript_files::is_transcript_file("x_call_eyeBeam_part2.wav"), "cleaned up like the rest");
    }

    #[test]
    fn lost_samples_become_silence_so_the_sides_stay_in_step() {
        let mut c = Core::default();
        c.feed(AGENT, &vec![0.1; 1600], 0);
        c.feed(CALLER, &vec![0.0; 1600 - 160], 0); // 10 ms lost on the caller side...
        c.feed(CALLER, &vec![0.2; 160], 0); // ...reported as silence before what came after
        let p = c.pairs(10);
        assert_eq!(p.len(), 1600);
        assert_eq!(p[1599][1], to_i16(0.2), "the caller's next words line up with the agent's");
    }

    #[test]
    fn a_short_pre_roll_takes_only_the_newest_seconds() {
        let p = tmp("short");
        let mut c = Core::default();
        c.feed(AGENT, &vec![0.1; 16_000 * 4], 0);
        c.feed(CALLER, &vec![0.1; 16_000 * 4], 0);
        c.drain(0).unwrap();
        c.start(&p, 2).unwrap();
        c.finish();
        assert_eq!(read_stereo(&p).len(), 32_000, "2 of the 4 seconds before the call");
        assert!(c.pre.is_empty(), "the rest is dropped, never written later");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn the_pre_roll_keeps_only_the_last_seconds() {
        let mut c = Core::default();
        for i in 0..(PRE_ROLL_S as u64 + 5) {
            c.feed(AGENT, &vec![0.1; RATE as usize], i * 1000);
            c.feed(CALLER, &vec![0.1; RATE as usize], i * 1000);
            c.drain(i * 1000).unwrap();
        }
        assert_eq!(c.pre.len(), PRE_ROLL_S * RATE as usize);
    }

    #[test]
    fn no_call_audio_means_a_silent_right_channel_not_a_stuck_recording() {
        let p = tmp("dry");
        let mut c = Core::default();
        c.start(&p, PRE_ROLL_S).unwrap();
        c.last[CALLER] = 1_000; // call audio came moments ago
        c.feed(AGENT, &vec![0.5; 16_000], 1_000);
        assert!(c.pairs(1_100).is_empty(), "the caller side may just be late");
        c.feed(AGENT, &vec![0.5; 16_000], 2_000);
        c.drain(2_000 + DRY_MS + 1).unwrap();
        c.finish();
        let s = read_stereo(&p);
        assert_eq!(s.len(), 32_000, "everything the agent said is in");
        assert!(s.iter().all(|x| x[1] == 0));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_hold_writes_nothing_and_resuming_continues_the_same_file() {
        let p = tmp("hold");
        let mut c = Core::default();
        c.start(&p, PRE_ROLL_S).unwrap();
        let mut t = 0;
        let mut step = |c: &mut Core, v: f32| {
            t += 1000;
            c.feed(AGENT, &vec![v; 16_000], t);
            c.feed(CALLER, &vec![v; 16_000], t);
            c.drain(t).unwrap();
        };
        step(&mut c, 0.1);
        c.paused = true;
        step(&mut c, 0.9); // hold music
        c.paused = false;
        c.pre.clear();
        step(&mut c, 0.2);
        c.finish();
        let s = read_stereo(&p);
        assert_eq!(s.len(), 32_000);
        assert!(s.iter().all(|x| x[0] != to_i16(0.9)), "the hold is left out");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_side_that_runs_ahead_is_caught_up_without_losing_the_other() {
        let mut c = Core::default();
        c.feed(AGENT, &vec![0.1; RATE as usize], 0);
        c.feed(CALLER, &vec![0.1; RATE as usize * 4 / 10], 0); // 0.6 s behind, still flowing
        let written = c.pairs(10).len();
        assert_eq!(written, RATE as usize * 4 / 10, "the caller side was written in full");
        assert_eq!(c.side[AGENT].len(), RATE as usize - written - written / 100, "the agent side skipped 1 sample in 100");
    }
}
