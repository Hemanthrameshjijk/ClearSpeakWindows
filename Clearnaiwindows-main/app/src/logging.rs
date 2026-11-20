//! Minimal file-backed logger for this app crate only.
//!
//! With `#![windows_subsystem = "windows"]` (see `main.rs`) the process has
//! no console, so `eprintln!`/`println!` output is simply never seen by
//! anyone. This module replaces those calls (in `main.rs`, `gui.rs`,
//! `engine.rs`, `setup.rs`, `settings.rs` only - other crates' own
//! `eprintln!`s are a known, unfixed gap, see the top-level report) with a
//! tiny appender that writes timestamped lines to
//! `<app_data_dir>/clearspeak.log`.
//!
//! Deliberately not the `log` crate + a custom `Log` impl: a couple of thin
//! wrapper functions plus two macros is less code than wiring up a facade
//! crate for a single-binary app with no library consumers of the log
//! output.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

static LOG_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Past this size the file is truncated back to empty before the next line
/// is written, so it never grows past roughly this bound. (The writer
/// thread reopens the file with `truncate(true)`: `set_len(0)` on the old
/// append-mode handle fails with "Access is denied" on Windows, which is why
/// a previous version of this cap never worked - real logs reached 200+ MB.)
const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

/// Lines waiting for the writer thread. When it is full (disk stalled) new
/// lines are dropped: a log line is never worth stalling an audio thread.
const QUEUE_LINES: usize = 1024;

/// The same message is written at most once per this window (see `repeat_count`).
const REPEAT_WINDOW: Duration = Duration::from_secs(1);

/// Bound on the number of distinct messages tracked for de-duplication.
const MAX_TRACKED_MESSAGES: usize = 128;

/// Real-hardware finding: some of this app's realtime audio threads (mic
/// capture, hardware render) call `log_error!` on *every* dropped/underrun
/// frame - if a stall starts, that's once every ~10ms, forever, with no
/// automatic recovery. Two earlier versions of this module made that
/// genuinely dangerous:
///
/// 1. Reopening the log file on every call, then (after the first fix)
///    writing to it synchronously under one global mutex. Disk I/O on a
///    realtime audio thread can take longer than the 10ms frame budget once
///    the file is large, which causes *more* drops, which logs more - a
///    runaway feedback loop that was measured to cause audible glitching
///    (the mic capture ring was ~95% full for two hours).
/// 2. De-duplicating only *consecutive* identical messages. Mic capture,
///    mic pipeline and speaker pipeline log different messages in
///    alternation, so nothing was ever "consecutive" and every line was
///    written.
///
/// Now `write_line` does only a short in-memory per-message de-duplication
/// and a non-blocking hand-off to a background writer thread, which does all
/// the file I/O. A realtime thread never touches the disk.
struct Logger {
    tx: SyncSender<String>,
    seen: Mutex<HashMap<String, (Instant, u64)>>,
}

static LOGGER: OnceLock<Option<Logger>> = OnceLock::new();

/// Must be called once, as the very first thing in `main()`, before any
/// other code in this crate might log. `dir` is `setup::app_data_dir()`.
pub fn init(dir: &Path) {
    init_named(dir, "clearspeak.log");
}

/// Same as [`init`] with a chosen file name: the window and the engine are separate processes and each writes its own
/// log (`clearspeak-window.log` / `clearspeak.log`) so their lines never interleave in one file.
pub fn init_named(dir: &Path, file_name: &str) {
    let _ = LOG_PATH.set(Some(dir.join(file_name)));
}

/// Local wall-clock time, readable: `2026-11-16 18:20:31.547`.
#[cfg(windows)]
fn timestamp() -> String {
    // SAFETY: GetLocalTime only fills the struct it is given.
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

#[cfg(not(windows))]
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

fn logger() -> Option<&'static Logger> {
    LOGGER
        .get_or_init(|| {
            let path = LOG_PATH.get().and_then(|p| p.clone())?;
            let (tx, rx) = sync_channel(QUEUE_LINES);
            std::thread::Builder::new()
                .name("clearspeak-log-writer".into())
                .spawn(move || writer(&path, rx, MAX_LOG_BYTES))
                .ok()?;
            Some(Logger { tx, seen: Mutex::new(HashMap::new()) })
        })
        .as_ref()
}

/// `None`: this message was already written less than `REPEAT_WINDOW` ago
/// (counted, not written). `Some(n)`: write it now; `n` identical messages
/// were suppressed since the last time it was written.
fn repeat_count(seen: &mut HashMap<String, (Instant, u64)>, msg: &str, now: Instant) -> Option<u64> {
    match seen.get_mut(msg) {
        Some((last, suppressed)) if now.duration_since(*last) < REPEAT_WINDOW => {
            *suppressed += 1;
            None
        }
        Some((last, suppressed)) => {
            *last = now;
            Some(std::mem::take(suppressed))
        }
        None => {
            if seen.len() >= MAX_TRACKED_MESSAGES {
                seen.clear();
            }
            seen.insert(msg.to_string(), (now, 0));
            Some(0)
        }
    }
}

/// Shared network folders the PCs write their logs to, so support can read them without asking the agent. They are
/// tried in this order and the first one that can be reached is used (the sites are on different networks); logs go
/// to `%LOCALAPPDATA%\ClearSpeak` only while none of them can be reached. Each PC gets its own sub-folder,
/// `<Windows user name>_<PC name>_system<N>` (see [`folder_name`]). `CLEARSPEAK_LOG_SHARE` overrides the list (several
/// folders separated by `;`); set it to an empty value to keep logs local only.
const DEFAULT_LOG_SHARES: &[&str] = &[r"\\10.11.12.4\Clearspeak log\FileLogs", r"\\192.168.168.4\Clear Voice\Logfiles"];

/// `system<N>` numbers are claimed as empty folders `<share>\_systems\system<N>` (creating a folder that exists fails,
/// so two PCs can never get the same number), and the number is kept in `<app data>\system_id.txt` for good.
const SYSTEMS_DIR: &str = "_systems";
const SYSTEM_ID_FILE: &str = "system_id.txt";
static SYSTEM_NO: Mutex<Option<u32>> = Mutex::new(None);

/// The license holder's name: `None` = not told yet, `Some(None)` = nobody signed in. No longer part of the folder
/// name; a change still makes the writer re-open its share file, which is harmless (same folder).
static PERSON: Mutex<Option<Option<String>>> = Mutex::new(None);
/// Bumped on every change of `PERSON`, so the writer re-opens the share file.
static PERSON_GEN: AtomicU64 = AtomicU64::new(0);

/// Tells the logger whose license this copy runs under (`None`: not signed in). Call it at start and whenever the
/// license may have changed; repeats are cheap.
pub fn set_person(name: Option<&str>) {
    let name = name.map(clean_part).filter(|n| !n.is_empty());
    let mut p = PERSON.lock().unwrap_or_else(|e| e.into_inner());
    if p.as_ref() != Some(&name) {
        *p = Some(name);
        PERSON_GEN.fetch_add(1, Ordering::SeqCst);
    }
}

/// Text that is safe as one Windows folder-name part.
fn clean_part(s: &str) -> String {
    let v: String = s.chars().filter(|c| !matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') && !c.is_control()).collect();
    v.trim().trim_end_matches('.').trim().to_string()
}

fn pc_name() -> String {
    let pc = clean_part(&std::env::var("COMPUTERNAME").unwrap_or_default());
    if pc.is_empty() { "PC".into() } else { pc }
}

fn user_name() -> String {
    let u = clean_part(&std::env::var("USERNAME").unwrap_or_default());
    if u.is_empty() { "user".into() } else { u }
}

/// `<user>_<PC>_system<N>`; just `<user>_<PC>` while no number could be claimed.
fn folder_name(user: &str, pc: &str, system: Option<u32>) -> String {
    match system {
        Some(n) => format!("{user}_{pc}_system{n}"),
        None => format!("{user}_{pc}"),
    }
}

fn system_id_file() -> Option<PathBuf> {
    LOG_PATH.get()?.as_ref()?.parent().map(|d| d.join(SYSTEM_ID_FILE))
}

/// This PC's number if it already has one (from memory or `system_id.txt`). Touches no network.
fn saved_system_no() -> Option<u32> {
    let mut cached = SYSTEM_NO.lock().unwrap_or_else(|e| e.into_inner());
    if cached.is_none() {
        *cached = std::fs::read_to_string(system_id_file()?).ok()?.trim().parse().ok();
    }
    *cached
}

/// Claims the next free number under `<root>\_systems`, writing `owner.txt` (`<user>_<PC>`) into it for support.
fn claim_system_no(root: &Path, owner: &str) -> std::io::Result<u32> {
    let dir = root.join(SYSTEMS_DIR);
    std::fs::create_dir_all(&dir)?;
    let taken = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok()?.file_name().to_str()?.strip_prefix("system")?.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    for n in taken + 1..taken + 1000 {
        let claim = dir.join(format!("system{n}"));
        match std::fs::create_dir(&claim) {
            Ok(()) => {
                let _ = std::fs::write(claim.join("owner.txt"), owner);
                return Ok(n);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue, // another PC was quicker
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free system number"))
}

/// This PC's number: the saved one, or a new one claimed on the first share that works (and saved locally).
fn system_no(roots: &[PathBuf], owner: &str) -> Option<u32> {
    if let Some(n) = saved_system_no() {
        return Some(n);
    }
    let n = roots.iter().find_map(|r| claim_system_no(r, owner).ok())?;
    *SYSTEM_NO.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
    if let Some(f) = system_id_file() {
        let _ = std::fs::write(f, n.to_string());
    }
    Some(n)
}

/// The share folders, in the order they are tried (empty: logs stay local).
fn share_roots() -> Vec<PathBuf> {
    match std::env::var("CLEARSPEAK_LOG_SHARE") {
        Ok(list) => list.split(';').map(str::trim).filter(|s| !s.is_empty()).map(PathBuf::from).collect(),
        Err(_) => DEFAULT_LOG_SHARES.iter().map(PathBuf::from).collect(),
    }
}

/// This agent's folder on each share as of now, in the order they are tried (empty when the share is switched off).
/// Touches no network.
pub fn share_dirs() -> Vec<PathBuf> {
    let folder = folder_name(&user_name(), &pc_name(), saved_system_no());
    share_roots().into_iter().map(|r| r.join(&folder)).collect()
}

/// Works out the share folders for one connection attempt, first choice first (in the real app: claims the system
/// number if needed, so it runs on the share thread; in tests: fixed).
type ShareDir = Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>;

fn real_share() -> Option<ShareDir> {
    let roots = share_roots();
    if roots.is_empty() {
        return None;
    }
    Some(Arc::new(move || {
        let (user, pc) = (user_name(), pc_name());
        let n = system_no(&roots, &format!("{user}_{pc}"));
        let folder = folder_name(&user, &pc, n);
        roots.iter().map(|r| r.join(&folder)).collect()
    }))
}

/// After the shared folder could not be reached (or a write to it failed), try again this often.
const SHARE_RETRY: Duration = Duration::from_secs(60);

/// At start, lines are held in memory (up to this much) until it is known whether the share can be used, so they all
/// end up in one place. If deciding takes longer than `DECIDE_LIMIT` they go to the local log instead.
const HOLD_BYTES: usize = 4 * 1024 * 1024;
const DECIDE_LIMIT: Duration = Duration::from_secs(30);

/// One log file with the size cap. The writer keeps one on the local disk and, when reachable, one on the share.
struct Sink {
    path: PathBuf,
    file: Option<std::fs::File>,
    size: u64,
}

impl Sink {
    fn open_file(path: &Path, truncate: bool) -> std::io::Result<std::fs::File> {
        let mut o = OpenOptions::new();
        o.create(true);
        if truncate {
            o.write(true).truncate(true);
        } else {
            o.append(true);
        }
        o.open(path)
    }

    fn open(path: PathBuf) -> std::io::Result<Sink> {
        let file = Self::open_file(&path, false)?;
        let size = file.metadata().map_or(0, |m| m.len());
        Ok(Sink { path, file: Some(file), size })
    }

    /// Writes `batch` (rolling the file over at `max_bytes` first). `Err` only when the write itself failed.
    fn write(&mut self, batch: &str, max_bytes: u64) -> std::io::Result<()> {
        let mut note = String::new();
        if self.size >= max_bytes {
            // Keep the previous file as `<name>.old` (replacing an older one) so the history before an issue is not
            // lost; fall back to truncating if the rename fails.
            drop(self.file.take());
            let old = self.path.with_extension("log.old");
            let _ = std::fs::remove_file(&old);
            let kept = std::fs::rename(&self.path, &old).is_ok();
            self.file = Self::open_file(&self.path, true).ok();
            self.size = 0;
            note = format!(
                "[{}] (log file reached {max_bytes} bytes; {})\n",
                timestamp(),
                if kept { "previous part kept as .log.old" } else { "truncated" }
            );
        }
        let Some(f) = self.file.as_mut() else { return Ok(()) };
        f.write_all(note.as_bytes())?;
        f.write_all(batch.as_bytes())?;
        self.size += (note.len() + batch.len()) as u64;
        Ok(())
    }
}

/// What a share connection attempt reports: the opened file (or, once, why it failed) and the `PERSON_GEN` its folder
/// was worked out for.
type ShareResult = (Sink, Option<String>, u64);

/// Opens `<dir>\<file_name>` in the first share folder that works, on its own thread (an unreachable server can take
/// 20+ s to fail, and the local log must not wait for that), retrying all of them every `SHARE_RETRY` until one works.
/// The folders are worked out again on every attempt, so a retry after a sign-in goes to the new folder.
fn connect_share(dirs: ShareDir, file_name: String) -> Receiver<ShareResult> {
    let (tx, rx) = sync_channel(1);
    let _ = std::thread::Builder::new().name("clearspeak-log-share".into()).spawn(move || {
        let mut first_error = true;
        loop {
            let gen = PERSON_GEN.load(Ordering::SeqCst);
            let mut errors = Vec::new();
            let mut opened = None;
            for dir in dirs() {
                match std::fs::create_dir_all(&dir).and_then(|_| Sink::open(dir.join(&file_name))) {
                    Ok(sink) => {
                        opened = Some(sink);
                        break;
                    }
                    Err(e) => errors.push(format!("{} ({e})", dir.display())),
                }
            }
            match opened {
                Some(sink) => {
                    let _ = tx.send((sink, None, gen));
                    return;
                }
                None => {
                    if first_error {
                        first_error = false;
                        // Report the failure once (through the same channel, with no sink) so the local log says why.
                        let msg = format!(
                            "shared log folders not reachable: {}; logging locally only, retrying every {} s",
                            errors.join(", "),
                            SHARE_RETRY.as_secs()
                        );
                        if tx.send((Sink { path: PathBuf::new(), file: None, size: 0 }, Some(msg), gen)).is_err() {
                            return; // the writer has gone
                        }
                    }
                    std::thread::sleep(SHARE_RETRY);
                }
            }
        }
    });
    rx
}

/// Owns the files. Lines go to this user's folder on the share; only while the share cannot be used do they go to
/// the local log (`path`), which then also says why.
fn writer(path: &Path, rx: Receiver<String>, max_bytes: u64) {
    writer_with_share(path, rx, max_bytes, real_share())
}

fn writer_with_share(path: &Path, rx: Receiver<String>, max_bytes: u64, share: Option<ShareDir>) {
    use std::sync::mpsc::RecvTimeoutError;
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "clearspeak.log".into());
    // Opened only when something has to go there, so a PC on the share leaves no local log behind.
    let mut local: Option<Sink> = None;
    let write_local = |local: &mut Option<Sink>, text: &str| {
        let sink = local.get_or_insert_with(|| {
            Sink::open(path.to_path_buf()).unwrap_or(Sink { path: path.to_path_buf(), file: None, size: 0 })
        });
        let _ = sink.write(text, max_bytes);
    };
    let mut remote: Option<Sink> = None;
    // The `PERSON_GEN` the open share file was named for; a newer one means "move to the new folder".
    let mut remote_gen = 0u64;
    let mut pending = share.clone().map(|d| connect_share(d, file_name.clone()));
    // `Some(start)`: still waiting for the first answer from the share; lines are held in `held` meanwhile.
    let mut deciding = pending.as_ref().map(|_| Instant::now());
    let mut held = String::new();
    let mut retry_at: Option<Instant> = None;
    let mut batch = String::new();
    loop {
        batch.clear();
        let closed = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(first) => {
                batch.push_str(&first);
                batch.push('\n');
                while let Ok(more) = rx.try_recv() {
                    batch.push_str(&more);
                    batch.push('\n');
                }
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        // The share's thread reports once if it failed, and again when (if ever) it opens the file.
        let mut note = String::new();
        if let Some(rx_share) = &pending {
            while let Ok((sink, failure, gen)) = rx_share.try_recv() {
                deciding = None;
                match failure {
                    Some(msg) => note.push_str(&format!("[{}] [INFO] [log] {msg}\n", timestamp())),
                    None => {
                        let same = remote.as_ref().is_some_and(|r| r.path == sink.path);
                        if !same {
                            let line = format!("[{}] [INFO] [log] writing this log to {}\n", timestamp(), sink.path.display());
                            // Whoever opens the previous file (local, or the old share folder) sees where the rest went.
                            match remote.as_mut() {
                                Some(old) => {
                                    let _ = old.write(&line, max_bytes);
                                }
                                None if local.is_some() => write_local(&mut local, &line),
                                None => {}
                            }
                            note.push_str(&line);
                            remote = Some(sink);
                        }
                        remote_gen = gen;
                        pending = None;
                        break;
                    }
                }
            }
        }
        // Signed in or out since the share file was opened: open the one in the new folder (the old one is used until
        // then, so nothing waits on the network).
        if let (Some(dir), true, None) = (&share, remote.is_some(), &pending) {
            if PERSON_GEN.load(Ordering::SeqCst) != remote_gen {
                pending = Some(connect_share(dir.clone(), file_name.clone()));
            }
        }
        if deciding.is_some_and(|t| t.elapsed() >= DECIDE_LIMIT) {
            deciding = None;
            note.push_str(&format!(
                "[{}] [INFO] [log] shared log folder still not answering after {} s; logging locally for now\n",
                timestamp(),
                DECIDE_LIMIT.as_secs()
            ));
        }
        if retry_at.is_some_and(|t| Instant::now() >= t) {
            retry_at = None;
            pending = share.clone().map(|d| connect_share(d, file_name.clone()));
        }
        if deciding.is_some() && !closed {
            if held.len() + batch.len() <= HOLD_BYTES {
                held.push_str(&batch);
            }
            continue;
        }
        let mut out = note;
        out.push_str(&std::mem::take(&mut held));
        out.push_str(&batch);
        if !out.is_empty() {
            let failed = match remote.as_mut() {
                Some(sink) => sink.write(&out, max_bytes).err().map(|e| (e, sink.path.clone())),
                None => {
                    write_local(&mut local, &out);
                    None
                }
            };
            if let Some((e, p)) = failed {
                remote = None;
                retry_at = Some(Instant::now() + SHARE_RETRY);
                out.push_str(&format!(
                    "[{}] [ERROR] [log] writing to {} failed ({e}); logging locally, retrying in {} s\n",
                    timestamp(),
                    p.display(),
                    SHARE_RETRY.as_secs()
                ));
                write_local(&mut local, &out);
            }
        }
        if closed {
            return;
        }
    }
}

/// Queues one line for the log file, unless `init` was never called (then
/// this is a no-op). Never blocks on disk I/O and never waits for a full
/// queue - logging must never itself become a source of realtime-audio-thread
/// stalls. Repeats of the same message within a second are collapsed.
pub fn write_line(level: &str, msg: &str) {
    let Some(logger) = logger() else { return };
    let repeated = {
        let mut seen = logger.seen.lock().unwrap_or_else(|e| e.into_inner());
        match repeat_count(&mut seen, msg, Instant::now()) {
            Some(n) => n,
            None => return,
        }
    };
    let line = if repeated > 0 {
        format!("[{}] [{level}] {msg} (+{repeated} identical since the previous line)", timestamp())
    } else {
        format!("[{}] [{level}] {msg}", timestamp())
    };
    let _ = logger.tx.try_send(line);
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logging::write_line("INFO", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logging::write_line("ERROR", &format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(p: PathBuf) -> ShareDir {
        Arc::new(move || vec![p.clone()])
    }

    #[test]
    fn folder_is_user_pc_and_system_and_safe() {
        assert_eq!(folder_name("mudasir", "LP2026-07", Some(1)), "mudasir_LP2026-07_system1");
        assert_eq!(folder_name("mudasir", "LP2026-07", None), "mudasir_LP2026-07", "no number claimed yet");
        assert_eq!(clean_part(r#" A/B\C:D*E?F"G<H>I|J. "#), "ABCDEFGHIJ", "no characters Windows refuses in a folder name");
        assert_eq!(clean_part("Developer (test build)"), "Developer (test build)");
    }

    #[test]
    fn writer_moves_to_the_new_folder_after_sign_in() {
        let base = std::env::temp_dir().join(format!("clearspeak-move-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let folder = Arc::new(Mutex::new(base.join("PC-07")));
        let f = folder.clone();
        let dir: ShareDir = Arc::new(move || vec![f.lock().unwrap().clone()]);
        let local = base.join("local.log");
        let (tx, rx) = sync_channel(QUEUE_LINES);
        let p = local.clone();
        let h = std::thread::spawn(move || writer_with_share(&p, rx, 1_000_000, Some(dir)));
        let wait_for = |file: PathBuf, tx: &SyncSender<String>, tag: &str| {
            for i in 0..300 {
                tx.send(format!("{tag} {i}")).unwrap();
                std::thread::sleep(Duration::from_millis(10));
                if std::fs::read_to_string(&file).is_ok_and(|t| t.contains(&format!("{tag} {i}"))) {
                    return;
                }
            }
            panic!("{} never got lines", file.display());
        };
        wait_for(base.join("PC-07").join("local.log"), &tx, "before");
        *folder.lock().unwrap() = base.join("Asha_PC-07");
        PERSON_GEN.fetch_add(1, Ordering::SeqCst); // what set_person does on sign-in
        wait_for(base.join("Asha_PC-07").join("local.log"), &tx, "after");
        drop(tx);
        h.join().unwrap();
        let old = std::fs::read_to_string(base.join("PC-07").join("local.log")).unwrap();
        assert!(old.contains("Asha_PC-07"), "the old file says where the log went");
        assert!(!local.exists(), "nothing local while the share works");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn interleaved_messages_are_each_collapsed() {
        // The failure that defeated the old logger: A, B, A, B, ... from different threads.
        let mut seen = HashMap::new();
        let t0 = Instant::now();
        let mut written = 0;
        for i in 0..1000u32 {
            let msg = if i % 2 == 0 { "mic capture ring buffer full" } else { "output ring buffer full" };
            if repeat_count(&mut seen, msg, t0 + Duration::from_millis(i as u64)).is_some() {
                written += 1;
            }
        }
        assert_eq!(written, 2, "1 s of interleaved repeats must produce one line per message");
        // After the window the next occurrence is written with the suppressed count.
        assert_eq!(repeat_count(&mut seen, "output ring buffer full", t0 + Duration::from_millis(1500)), Some(499));
    }

    #[test]
    fn writer_uses_the_share_and_falls_back_to_local_when_it_is_missing() {
        let base = std::env::temp_dir().join(format!("clearspeak-share-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("local")).unwrap();
        let local = base.join("local").join("clearspeak.log");
        let share = base.join("share").join("someuser");
        let (tx, rx) = sync_channel(QUEUE_LINES);
        let (p, s) = (local.clone(), share.clone());
        let h = std::thread::spawn(move || writer_with_share(&p, rx, 1_000_000, Some(fixed(s))));
        tx.send("first".into()).unwrap();
        // The share is opened on its own thread; wait for it like a running app would.
        let shared = share.join("clearspeak.log");
        for i in 0..200 {
            tx.send(format!("line {i}")).unwrap();
            std::thread::sleep(Duration::from_millis(10));
            if std::fs::read_to_string(&shared).is_ok_and(|t| t.contains(&format!("line {i}"))) {
                break;
            }
        }
        tx.send("last".into()).unwrap();
        drop(tx);
        h.join().unwrap();
        let s = std::fs::read_to_string(&shared).unwrap();
        assert!(s.contains("first") && s.contains("last"), "share has every line, including those from before it opened");
        assert!(s.contains("writing this log to"), "the log says where it is");
        assert!(!local.exists(), "nothing is written locally while the share works");

        // An unreachable share: the local log is used and says why.
        let local2 = base.join("local").join("b.log");
        let blocker = base.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let bad = blocker.join("not-a-dir"); // parent is a file -> cannot create
        let (tx, rx) = sync_channel(QUEUE_LINES);
        let p = local2.clone();
        let h = std::thread::spawn(move || writer_with_share(&p, rx, 1_000_000, Some(fixed(bad))));
        for i in 0..50 {
            tx.send(format!("x {i}")).unwrap();
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(tx);
        h.join().unwrap();
        let l2 = std::fs::read_to_string(&local2).unwrap();
        assert!(l2.contains("x 49"), "local log kept going");
        assert!(l2.contains("not reachable"), "local log says the share failed");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn writer_uses_the_second_share_when_the_first_is_missing() {
        let base = std::env::temp_dir().join(format!("clearspeak-second-share-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("local")).unwrap();
        let blocker = base.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        let first = blocker.join("Asha"); // parent is a file -> cannot create, like an unreachable server
        let second = base.join("second").join("Asha");
        let local = base.join("local").join("clearspeak.log");
        let dirs: ShareDir = {
            let (a, b) = (first.clone(), second.clone());
            Arc::new(move || vec![a.clone(), b.clone()])
        };
        let (tx, rx) = sync_channel(QUEUE_LINES);
        let p = local.clone();
        let h = std::thread::spawn(move || writer_with_share(&p, rx, 1_000_000, Some(dirs)));
        let shared = second.join("clearspeak.log");
        for i in 0..200 {
            tx.send(format!("line {i}")).unwrap();
            std::thread::sleep(Duration::from_millis(10));
            if std::fs::read_to_string(&shared).is_ok_and(|t| t.contains(&format!("line {i}"))) {
                break;
            }
        }
        drop(tx);
        h.join().unwrap();
        let s = std::fs::read_to_string(&shared).unwrap();
        assert!(s.contains("line 0"), "second share has the lines");
        assert!(!local.exists(), "nothing local while the second share works");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn system_numbers_are_claimed_once_each() {
        let root = std::env::temp_dir().join(format!("clearspeak-systems-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(claim_system_no(&root, "a_PC1").unwrap(), 1, "first PC");
        assert_eq!(claim_system_no(&root, "b_PC2").unwrap(), 2, "second PC");
        std::fs::create_dir(root.join(SYSTEMS_DIR).join("system7")).unwrap();
        assert_eq!(claim_system_no(&root, "c_PC3").unwrap(), 8, "after the highest number taken");
        let owner = std::fs::read_to_string(root.join(SYSTEMS_DIR).join("system2").join("owner.txt")).unwrap();
        assert_eq!(owner, "b_PC2", "support can see which PC has which number");
        // Many PCs at once: every number handed out exactly once.
        let got: Vec<u32> = (0..8)
            .map(|i| {
                let r = root.clone();
                std::thread::spawn(move || claim_system_no(&r, &format!("x_PC{i}")).unwrap())
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), got.len(), "no number given twice: {got:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_shares_are_tried_in_order() {
        let d: Vec<PathBuf> = DEFAULT_LOG_SHARES.iter().map(PathBuf::from).collect();
        assert_eq!(d[0], PathBuf::from(r"\\10.11.12.4\Clearspeak log\FileLogs"));
        assert_eq!(d[1], PathBuf::from(r"\\192.168.168.4\Clear Voice\Logfiles"));
    }

    /// Against the real shared folders (needs the network): `cargo test --lib real_share -- --ignored --nocapture`.
    /// Writes `<first reachable share>\Share Check_<PC>\clearspeak-share-check.log` and removes it (and the folder)
    /// again. Claims no system number (that would use one up on the real share).
    #[test]
    #[ignore]
    fn real_share() {
        let folder = folder_name("Share Check", &pc_name(), None);
        let shares: Vec<PathBuf> = share_roots().into_iter().map(|r| r.join(&folder)).collect();
        assert!(!shares.is_empty(), "share switched off");
        let local = std::env::temp_dir().join("clearspeak-share-check.log");
        let _ = std::fs::remove_file(&local);
        let (tx, rx) = sync_channel(QUEUE_LINES);
        let p = local.clone();
        let list = shares.clone();
        let started = Instant::now();
        let h = std::thread::spawn(move || writer_with_share(&p, rx, 1_000_000, Some(Arc::new(move || list.clone()))));
        let reached = |shares: &[PathBuf], i: usize| {
            shares.iter().map(|s| s.join("clearspeak-share-check.log")).find(|t| {
                std::fs::read_to_string(t).is_ok_and(|x| x.contains(&format!("check line {i}")))
            })
        };
        let mut target = None;
        for i in 0..1200 {
            tx.send(format!("check line {i}")).unwrap();
            std::thread::sleep(Duration::from_millis(50));
            if let Some(t) = reached(&shares, i) {
                target = Some(t);
                break;
            }
        }
        drop(tx);
        h.join().unwrap();
        let target = target.expect("no share reachable");
        let text = std::fs::read_to_string(&target).unwrap_or_default();
        println!("share file {} after {:?}:\n{text}", target.display(), started.elapsed());
        println!("local fallback file exists: {}", local.exists());
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_dir(target.parent().unwrap());
        assert!(target.parent().unwrap().ends_with(&folder), "folder is <user>_<PC>");
        assert!(text.contains("check line 0"), "lines reached the share");
        assert!(!local.exists(), "nothing written locally");
    }

    #[test]
    fn writer_truncates_at_the_cap_and_keeps_going() {
        let dir = std::env::temp_dir().join(format!("clearspeak-log-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.log");
        std::fs::write(&path, vec![b'@'; 5000]).unwrap(); // already over the cap, like the 200 MB real log
        let (tx, rx) = sync_channel(QUEUE_LINES);
        let p = path.clone();
        let h = std::thread::spawn(move || writer_with_share(&p, rx, 1000, None));
        for i in 0..200 {
            tx.send(format!("line {i}")).unwrap();
        }
        drop(tx);
        h.join().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.len() < 2500, "file stayed bounded, got {} bytes", text.len());
        assert!(!text.contains('@'), "the old oversized content is gone");
        assert!(text.contains("line 199"), "the newest line is present");
        let old = std::fs::read_to_string(path.with_extension("log.old")).unwrap();
        assert!(old.len() < 6000, "only the last part is kept as .old, got {} bytes", old.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
