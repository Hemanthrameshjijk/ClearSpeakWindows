//! Mic-chain tap + worker thread. The mic thread only copies the frame into a ring (never blocks, never changes the
//! audio); the worker, at below-normal priority, does 48 -> 16 kHz, the voice detector, and one model run per 3 s of
//! speech (one per ~12 s once this session has decided; an answer saved
//! from an earlier session does not count, every chunk is checked until the voice confirms or overturns it). Nothing runs, and nothing is loaded, while detection is off.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU8};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use accent_onnx::causal_resampler::CausalDownsampler3x;
use dsp_core::{Consumer, Producer, RingBuffer, Stage, FRAME_SAMPLES};

use crate::analysis::{VoiceGate, SR};
use crate::detector::Detector;
use crate::ecapa::{Model, ECAPA_ONNX, GENDER_NPZ};
use crate::Gender;

const RING_SECONDS: usize = 4;
const RESAMPLER_TAPS: usize = 97;
/// Once an answer is known, only every n-th chunk is checked (re-checking is for a shift change, not urgent).
const RECHECK_EVERY: u32 = 4;

pub const S_OFF: u8 = 0;
pub const S_LOADING: u8 = 1;
pub const S_LISTENING: u8 = 2;
pub const S_UNAVAILABLE: u8 = 3;

pub struct GenderConfig {
    /// Folder with `gender.npz` + `ecapa.onnx`; `None` = look next to the program, then in the app data folder.
    pub models_dir: Option<PathBuf>,
    pub onnxruntime_dll: PathBuf,
    /// The answer saved for this person earlier, used until the voice says otherwise.
    pub known: Option<Gender>,
    pub enabled: bool,
}

struct Shared {
    enabled: AtomicBool,
    state: AtomicU8,
    /// 0 = not known, else `Gender::code`.
    gender: AtomicU8,
    /// The answer was decided from this session's speech (`false` while it is only the saved guess).
    confirmed: AtomicBool,
    /// Bumped on every new answer.
    changes: AtomicU32,
    /// Set by the app when the person changes (`known` for the new person, 0 = unknown, 255 = nothing pending).
    reset_to: AtomicU8,
    voiced_ms: AtomicU32,
    last_ecapa_x100: AtomicI32,
    last_f0: AtomicU32,
    stop: AtomicBool,
    message: Mutex<String>,
}

/// The app's end: switch detection on/off, read the answer.
#[derive(Clone)]
pub struct GenderHandle(Arc<Shared>);

impl GenderHandle {
    pub fn set_enabled(&self, on: bool) {
        self.0.enabled.store(on, Relaxed);
    }
    pub fn enabled(&self) -> bool {
        self.0.enabled.load(Relaxed)
    }
    pub fn state(&self) -> u8 {
        self.0.state.load(Relaxed)
    }
    pub fn gender(&self) -> Option<Gender> {
        Gender::from_code(self.0.gender.load(Relaxed))
    }
    /// True once this session's speech decided the answer; `false` while [`gender`](Self::gender) is only the saved
    /// guess (or nothing).
    pub fn confirmed(&self) -> bool {
        self.0.confirmed.load(Relaxed)
    }
    /// Changes every time a new answer is published (compare with the last value seen).
    pub fn changes(&self) -> u32 {
        self.0.changes.load(Relaxed)
    }
    /// Voiced speech heard so far (ms), last ECAPA score and pitch: for the log.
    pub fn stats(&self) -> (u32, f32, u32) {
        (self.0.voiced_ms.load(Relaxed), self.0.last_ecapa_x100.load(Relaxed) as f32 / 100.0, self.0.last_f0.load(Relaxed))
    }
    pub fn message(&self) -> String {
        self.0.message.lock().map(|m| m.clone()).unwrap_or_default()
    }
    /// A different person signed in: start again from what is saved for them (`None` = listen from scratch).
    pub fn reset(&self, known: Option<Gender>) {
        self.0.gender.store(known.map_or(0, Gender::code), Relaxed);
        self.0.confirmed.store(false, Relaxed);
        self.0.reset_to.store(known.map_or(0, Gender::code), Relaxed);
    }
}

/// The mic-thread end. A pure tap.
pub struct GenderStage {
    sh: Arc<Shared>,
    prod: Producer<f32>,
}

impl Drop for GenderStage {
    fn drop(&mut self) {
        self.sh.stop.store(true, Relaxed);
    }
}

impl Stage for GenderStage {
    fn name(&self) -> &'static str {
        "Gender"
    }

    fn process(&mut self, frame: &mut [f32]) {
        if !self.sh.enabled.load(Relaxed) || frame.len() != FRAME_SAMPLES || self.sh.state.load(Relaxed) != S_LISTENING {
            return;
        }
        for &s in frame.iter() {
            let _ = self.prod.push(s); // ring full = worker busy; losing a little audio is harmless here
        }
    }
}

pub fn start(cfg: GenderConfig) -> (GenderStage, GenderHandle) {
    let sh = Arc::new(Shared {
        enabled: AtomicBool::new(cfg.enabled),
        state: AtomicU8::new(S_OFF),
        gender: AtomicU8::new(cfg.known.map_or(0, Gender::code)),
        confirmed: AtomicBool::new(false),
        changes: AtomicU32::new(0),
        reset_to: AtomicU8::new(255),
        voiced_ms: AtomicU32::new(0),
        last_ecapa_x100: AtomicI32::new(0),
        last_f0: AtomicU32::new(0),
        stop: AtomicBool::new(false),
        message: Mutex::new(String::new()),
    });
    let (prod, cons) = RingBuffer::<f32>::new(48_000 * RING_SECONDS);
    let w = sh.clone();
    if let Err(e) = std::thread::Builder::new().name("clearspeak-gender-worker".into()).spawn(move || worker(w, cons, cfg)) {
        fail(&sh, format!("could not start worker thread: {e}"));
    }
    (GenderStage { sh: sh.clone(), prod }, GenderHandle(sh))
}

fn fail(sh: &Shared, msg: String) {
    if let Ok(mut m) = sh.message.lock() {
        *m = msg;
    }
    sh.state.store(S_UNAVAILABLE, Relaxed);
}

/// Where the model files are: the configured folder, next to the program (`app_models`), or the app data folder.
pub fn find_models_dir(configured: Option<&Path>) -> Option<PathBuf> {
    let has = |d: &Path| d.join(GENDER_NPZ).is_file() && d.join(ECAPA_ONNX).is_file();
    if let Some(d) = configured {
        return has(d).then(|| d.to_path_buf());
    }
    let mut c = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            c.push(d.join("app_models"));
        }
    }
    if let Some(l) = std::env::var_os("LOCALAPPDATA") {
        c.push(PathBuf::from(l).join("ClearSpeak").join("app_models"));
    }
    c.into_iter().find(|d| has(d))
}

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

fn worker(sh: Arc<Shared>, mut cons: Consumer<f32>, cfg: GenderConfig) {
    lower_thread_priority();
    let mut model: Option<Model> = None;
    let mut detector: Option<Detector> = None;
    let mut known = cfg.known;
    let mut down = CausalDownsampler3x::new(RESAMPLER_TAPS);
    let mut gate = VoiceGate::default();
    let mut chunk_in = vec![0.0f32; FRAME_SAMPLES];
    let mut voiced: Vec<f32> = Vec::new();
    let mut chunks_since_answer = 0u32;

    while !sh.stop.load(Relaxed) {
        let r = sh.reset_to.swap(255, Relaxed);
        if r != 255 {
            known = Gender::from_code(r);
            detector = model.as_ref().map(|m| Detector::new(m.sure, known));
            voiced.clear();
            gate.reset();
            chunks_since_answer = 0;
            sh.voiced_ms.store(0, Relaxed);
        }
        if !sh.enabled.load(Relaxed) {
            if sh.state.load(Relaxed) != S_UNAVAILABLE {
                sh.state.store(S_OFF, Relaxed);
            }
            while cons.pop().is_ok() {}
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        if sh.state.load(Relaxed) == S_UNAVAILABLE {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        if model.is_none() {
            sh.state.store(S_LOADING, Relaxed);
            let Some(dir) = find_models_dir(cfg.models_dir.as_deref()) else {
                fail(&sh, format!("voice detection files ({GENDER_NPZ}, {ECAPA_ONNX}) not found"));
                continue;
            };
            if let Err(e) = accent_onnx::ort_util::init_ort(&cfg.onnxruntime_dll) {
                fail(&sh, format!("onnxruntime: {e}"));
                continue;
            }
            match Model::load(&dir, 1) {
                Ok(m) => {
                    detector = Some(Detector::new(m.sure, known));
                    model = Some(m);
                }
                Err(e) => {
                    fail(&sh, e);
                    continue;
                }
            }
            while cons.pop().is_ok() {}
            down.reset();
            sh.state.store(S_LISTENING, Relaxed);
        }
        if cons.slots() < FRAME_SAMPLES {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        for v in chunk_in.iter_mut() {
            *v = cons.pop().unwrap_or(0.0);
        }
        let x16 = down.push(&chunk_in);
        let before = voiced.len();
        gate.push(&x16, &mut voiced);
        let added_ms = ((voiced.len() - before) * 1000 / SR) as u32;
        sh.voiced_ms.fetch_add(added_ms, Relaxed);

        let (Some(m), Some(d)) = (model.as_mut(), detector.as_mut()) else { continue };
        let need = (m.chunk_s * SR as f32) as usize;
        if voiced.len() < need {
            continue;
        }
        let chunk: Vec<f32> = voiced.drain(..need).collect();
        // Only re-checks are thinned out: while the answer is just the saved guess, every chunk counts.
        if d.confirmed() {
            chunks_since_answer += 1;
            if chunks_since_answer % RECHECK_EVERY != 0 {
                continue;
            }
        }
        match m.score_chunk(&chunk) {
            Ok(s) => {
                sh.last_ecapa_x100.store((s.ecapa * 100.0) as i32, Relaxed);
                sh.last_f0.store(s.f0.unwrap_or(0.0) as u32, Relaxed);
                if let Some(c) = d.push(&s) {
                    sh.gender.store(c.gender.code(), Relaxed);
                    sh.confirmed.store(true, Relaxed);
                    sh.changes.fetch_add(1, Relaxed);
                    chunks_since_answer = 0;
                }
            }
            Err(e) => {
                if let Ok(mut msg) = sh.message.lock() {
                    *msg = format!("chunk skipped: {e}");
                }
            }
        }
    }
}
