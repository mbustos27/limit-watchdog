//! The ledger (schema version 2) and the only code that reads or writes it.
//!
//! Every change happens under `<ledger>.lock` (exclusive create holding a random nonce; waits up to 10 s; a lock older than 60 s
//! is stale and taken over by an atomic rename; released only while it still holds the holder's nonce) and is written to a temp
//! file in the same folder, then renamed over the ledger, so a reader never sees half a file.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 2;
/// The log keeps this many newest lines.
pub const LOG_KEEP: usize = 500;
pub const SESSION_ENV: &str = "CLAUDE_CODE_SESSION_ID";
pub const HOST_SESSION_ENV: &str = "CLAUDE_CODE_HOST_SESSION_ID";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Running,
    StoppedByLimit,
    StoppedByWeekly,
    NeedsHuman,
    PausedByUser,
    Done,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Resumable,
    Redispatch,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Risk {
    ReadOnly,
    LocalWrites,
    Irreversible,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Reason {
    Wall,
    Batch,
    Weekly,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TimerKind {
    CronOneshot,
    DesktopTask,
}

/// The word a value is written as in the ledger ("stopped-by-limit").
pub fn word<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        _ => String::new(),
    }
}

/// Parse a ledger word; `what` and `choices` make the error message.
pub fn parse_word<T: DeserializeOwned>(s: &str, what: &str, choices: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(s.trim().to_string())).map_err(|_| format!("{what} must be one of {choices}, not {s:?}"))
}

pub const STATES: &str = "running, stopped-by-limit, stopped-by-weekly, needs-human, paused-by-user, done";
pub const KINDS: &str = "resumable, redispatch";
pub const RISKS: &str = "read-only, local-writes, irreversible";
pub const TIMER_KINDS: &str = "cron-oneshot, desktop-task";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Run {
    pub unattended: bool,
    pub max_concurrent: u32,
    pub batch_size: u32,
    pub batch_gap_min: u32,
    pub max_wake_cycles: u32,
    pub wake_cycles: u32,
    pub max_resumes_per_agent: u32,
    pub arm_at_pct: u32,
    pub herd_rise_pct: u32,
    pub herd_refail_min: u32,
    /// The rise check (5-hour % minus `cycle_base_pct` over `herd_rise_pct`) applies only this many minutes after the wall
    /// wake was handled: it is for a resume burst, not for ordinary work later in the cycle.
    pub herd_window_min: u32,
    pub weekly_stop_pct: u32,
    pub redispatch_over_bytes: u64,
    pub resume_after_weekly: bool,
    pub cycle_base_pct: Option<u32>,
    /// When the current cycle's wall wake was handled; the herd window starts here. Kept by lw itself.
    pub cycle_started_at: Option<String>,
    pub herd_tripped: bool,
    /// How many wakes this ledger has created; the next token is `w<wake_seq + 1>`, so a token is never reused.
    pub wake_seq: u32,
}

impl Default for Run {
    fn default() -> Self {
        Run {
            unattended: false,
            max_concurrent: 4,
            batch_size: 2,
            batch_gap_min: 12,
            max_wake_cycles: 3,
            wake_cycles: 0,
            max_resumes_per_agent: 2,
            arm_at_pct: 80,
            herd_rise_pct: 20,
            herd_refail_min: 30,
            herd_window_min: 30,
            weekly_stop_pct: 90,
            redispatch_over_bytes: 3_000_000,
            resume_after_weekly: false,
            cycle_base_pct: None,
            cycle_started_at: None,
            herd_tripped: false,
            wake_seq: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Agent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub agent_type: String,
    pub kind: Kind,
    pub risk: Risk,
    #[serde(default)]
    pub worktree: Option<String>,
    #[serde(default)]
    pub handoff: Option<String>,
    #[serde(default)]
    pub brief_file: Option<String>,
    pub state: State,
    #[serde(default)]
    pub resumes_tonight: u32,
    #[serde(default)]
    pub last_resumed: Option<String>,
    /// `last_resumed` before the latest resume, so `unsent` can put it back.
    #[serde(default)]
    pub prev_resumed: Option<String>,
    #[serde(default)]
    pub last_failure: Option<String>,
    #[serde(default)]
    pub resume: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Wake {
    pub token: String,
    pub reason: Reason,
    pub for_reset: String,
    pub wake_at: String,
    #[serde(default)]
    pub backup_at: Option<String>,
    #[serde(default)]
    pub handled_at: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Timer {
    pub kind: TimerKind,
    pub id: String,
    pub fires: String,
    pub token: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct LogEntry {
    pub at: String,
    pub msg: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Ledger {
    pub version: u32,
    pub session_id: String,
    #[serde(default)]
    pub host_session_id: String,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub run: Run,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub wake: Option<Wake>,
    #[serde(default)]
    pub timers: Vec<Timer>,
    #[serde(default)]
    pub log: Vec<LogEntry>,
}

impl Ledger {
    pub fn new(session_id: &str, host_session_id: &str, project: &str, updated: &str) -> Ledger {
        Ledger {
            version: VERSION,
            session_id: session_id.to_string(),
            host_session_id: host_session_id.to_string(),
            project: project.to_string(),
            updated: updated.to_string(),
            run: Run::default(),
            agents: Vec::new(),
            wake: None,
            timers: Vec::new(),
            log: Vec::new(),
        }
    }

    pub fn agent(&self, id: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id == id)
    }

    pub fn agent_mut(&mut self, id: &str) -> Result<&mut Agent, String> {
        self.agents.iter_mut().find(|a| a.id == id).ok_or_else(|| format!("no agent {id:?} in the ledger"))
    }

    pub fn count(&self, s: State) -> usize {
        self.agents.iter().filter(|a| a.state == s).count()
    }

    pub fn push_log(&mut self, at: &str, msg: impl Into<String>) {
        self.log.push(LogEntry { at: at.to_string(), msg: msg.into() });
        if self.log.len() > LOG_KEEP {
            let extra = self.log.len() - LOG_KEEP;
            self.log.drain(..extra);
        }
    }

    pub fn to_json(&self) -> String {
        let mut s = serde_json::to_string_pretty(self).unwrap_or_default();
        s.push('\n');
        s
    }

    pub fn from_json(text: &str) -> Result<Ledger, String> {
        let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("the ledger is not valid JSON: {e}"))?;
        match v.get("version").and_then(|x| x.as_u64()) {
            Some(2) => {}
            Some(n) => return Err(format!("ledger version {n}: lw reads version 2 (convert a version 1 file with migrate-v1)")),
            None => return Err("the file has no \"version\": it is not a version 2 ledger (convert a version 1 file with migrate-v1)".into()),
        }
        serde_json::from_value(v).map_err(|e| format!("the ledger does not match schema version 2: {e}"))
    }
}

/// An id that is safe inside a file name: letters, digits, `-` and `_`.
pub fn safe_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `--ledger PATH`, or `%USERPROFILE%\.claude\work-ledger\<session>.json` for `--session ID` / `CLAUDE_CODE_SESSION_ID`.
pub fn resolve_path(ledger: Option<&str>, session: Option<&str>) -> Result<PathBuf, String> {
    if let Some(p) = ledger.filter(|p| !p.trim().is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let session =
        session_id(session).ok_or_else(|| format!("no ledger: pass --ledger PATH or --session ID, or run inside a session that sets {SESSION_ENV}"))?;
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).ok_or("USERPROFILE is not set")?;
    Ok(PathBuf::from(home).join(".claude").join("work-ledger").join(format!("{session}.json")))
}

/// `--session ID`, else `CLAUDE_CODE_SESSION_ID`. Only ids that are safe as a file name.
pub fn session_id(arg: Option<&str>) -> Option<String> {
    let s = match arg {
        Some(s) => s.trim().to_string(),
        None => std::env::var(SESSION_ENV).ok()?.trim().to_string(),
    };
    safe_id(&s).then_some(s)
}

pub struct Store {
    pub path: PathBuf,
    pub lock_wait: Duration,
    pub lock_stale: Duration,
}

impl Store {
    pub fn new(path: PathBuf) -> Store {
        Store { path, lock_wait: Duration::from_secs(10), lock_stale: Duration::from_secs(60) }
    }

    pub fn lock_path(&self) -> PathBuf {
        let mut s = self.path.as_os_str().to_os_string();
        s.push(".lock");
        PathBuf::from(s)
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Read without the lock: writes are atomic renames, so the file is always whole.
    pub fn read(&self) -> Result<Ledger, String> {
        let text = fs::read_to_string(&self.path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("no ledger at {} (run lw init first)", self.path.display())
            } else {
                format!("could not read {}: {e}", self.path.display())
            }
        })?;
        Ledger::from_json(&text).map_err(|e| format!("{}: {e}", self.path.display()))
    }

    /// Write a new ledger; refuses if one is already there.
    pub fn create(&self, l: &Ledger) -> Result<(), String> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        }
        let _lock = acquire_lock(&self.lock_path(), self.lock_wait, self.lock_stale)?;
        if self.exists() {
            return Err(format!("a ledger already exists at {}", self.path.display()));
        }
        write_atomic(&self.path, l.to_json().as_bytes())
    }

    /// Lock, read, change, write. `f` returns its output and whether it changed anything; nothing is written when it did not.
    pub fn update<T>(&self, f: impl FnOnce(&mut Ledger) -> Result<(T, bool), String>) -> Result<T, String> {
        let _lock = acquire_lock(&self.lock_path(), self.lock_wait, self.lock_stale)?;
        let mut l = self.read()?;
        let (out, changed) = f(&mut l)?;
        if changed {
            write_atomic(&self.path, l.to_json().as_bytes())?;
        }
        Ok(out)
    }
}

/// Holds `<ledger>.lock`, which contains this holder's random nonce. Dropping it removes the file only if it still holds that nonce:
/// if the lock was taken over as stale meanwhile, the new holder's lock stays.
#[derive(Debug)]
pub struct LockGuard {
    path: PathBuf,
    content: String,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let tag = format!("released-{}", nonce());
        remove_if(&self.path, &self.content, &tag);
    }
}

/// 128 random bits as hex, from the standard library's randomly keyed hasher (no extra crate), mixed with the pid, the time and a counter.
pub fn nonce() -> String {
    use std::hash::{BuildHasher, RandomState};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let a = RandomState::new().hash_one((std::process::id(), nanos, c));
    let b = RandomState::new().hash_one((a, c, nanos));
    format!("{a:016x}{b:016x}")
}

fn busy(e: &std::io::Error) -> bool {
    // Windows answers "access denied" (or a sharing violation) for a lock file that is being deleted: same as "taken", try again.
    matches!(e.kind(), std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied) || e.raw_os_error() == Some(32)
}

/// The lock's content if the lock is older than `stale`. Content and age come from one open handle, so they belong to the same file.
fn stale_content(path: &Path, stale: Duration) -> Option<String> {
    use std::io::Read;
    let mut f = fs::File::open(path).ok()?;
    let age = f.metadata().ok()?.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok())?;
    if age <= stale {
        return None;
    }
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

/// Move the lock aside under a name of our own, then delete it if it is the lock we meant (`expect`), else put it back.
/// The rename is atomic: of two processes that both mean to remove the same lock, only one moves it, and a lock that was
/// replaced in the meantime is recognised by its nonce and restored. True when the expected lock was removed.
fn remove_if(path: &Path, expect: &str, tag: &str) -> bool {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{tag}"));
    let aside = PathBuf::from(name);
    if fs::rename(path, &aside).is_err() {
        return false;
    }
    let ours = fs::read_to_string(&aside).is_ok_and(|c| c == expect);
    if !ours {
        // Someone else's lock: put it back. A hard link never replaces a lock that appeared meanwhile.
        if fs::hard_link(&aside, path).is_err_and(|e| e.kind() != std::io::ErrorKind::AlreadyExists) && !path.exists() {
            let _ = fs::rename(&aside, path);
        }
    }
    let _ = fs::remove_file(&aside);
    ours
}

pub fn acquire_lock(path: &Path, wait: Duration, stale: Duration) -> Result<LockGuard, String> {
    let start = Instant::now();
    let me = nonce();
    let content = format!("lw pid {} nonce {me}\n", std::process::id());
    loop {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut f) => {
                let written = f.write_all(content.as_bytes()).and_then(|()| f.flush());
                drop(f);
                if let Err(e) = written {
                    // Created a moment ago, so it cannot look stale to anyone yet: it is still ours to remove.
                    let _ = fs::remove_file(path);
                    return Err(format!("could not write the lock {}: {e}", path.display()));
                }
                return Ok(LockGuard { path: path.to_path_buf(), content });
            }
            Err(e) if busy(&e) => {
                if let Some(old) = stale_content(path, stale)
                    && remove_if(path, &old, &format!("stale-{me}"))
                {
                    continue;
                }
                if start.elapsed() >= wait {
                    return Err(format!("the ledger is locked ({} exists); waited {} s", path.display(), wait.as_secs_f32()));
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(e) => return Err(format!("could not create the lock {}: {e}", path.display())),
        }
    }
}

/// Write `bytes` to a temp file next to `path`, flush it to disk, then rename it over `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "ledger".into());
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let tmp = dir.join(format!(".{name}.tmp-{}-{nanos}", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(format!("could not write {}: {e}", tmp.display()));
    }
    // A reader that opened the ledger without delete sharing makes the rename fail for a moment: retry for about a second.
    let mut last = None;
    for _ in 0..40 {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    let _ = fs::remove_file(&tmp);
    Err(format!("could not replace {}: {}", path.display(), last.map(|e| e.to_string()).unwrap_or_default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sample() -> Ledger {
        Ledger::new("s1", "local_1", "p", "2026-11-15T10:00:00-08:00")
    }

    #[test]
    fn words_round_trip() {
        assert_eq!(word(&State::StoppedByLimit), "stopped-by-limit");
        assert_eq!(word(&Risk::ReadOnly), "read-only");
        assert_eq!(word(&TimerKind::CronOneshot), "cron-oneshot");
        assert_eq!(parse_word::<State>("paused-by-user", "state", STATES).unwrap(), State::PausedByUser);
        assert!(parse_word::<State>("asleep", "state", STATES).unwrap_err().contains("must be one of"));
    }

    #[test]
    fn the_ledger_round_trips_and_has_the_v2_fields() {
        let l = sample();
        let v: serde_json::Value = serde_json::from_str(&l.to_json()).unwrap();
        for k in ["version", "session_id", "host_session_id", "project", "updated", "run", "agents", "wake", "timers", "log"] {
            assert!(v.get(k).is_some(), "{k}");
        }
        assert_eq!(v["run"]["arm_at_pct"], 80);
        assert_eq!(v["run"]["cycle_base_pct"], serde_json::Value::Null);
        assert_eq!(Ledger::from_json(&l.to_json()).unwrap(), l);
    }

    #[test]
    fn other_versions_are_refused() {
        assert!(Ledger::from_json(r#"{"version":1,"agents":[]}"#).unwrap_err().contains("migrate-v1"));
        assert!(Ledger::from_json(r#"[{"id":"a"}]"#).unwrap_err().contains("version"));
    }

    #[test]
    fn the_log_keeps_the_newest_lines() {
        let mut l = sample();
        for i in 0..(LOG_KEEP + 7) {
            l.push_log("t", format!("m{i}"));
        }
        assert_eq!(l.log.len(), LOG_KEEP);
        assert_eq!(l.log[0].msg, "m7");
    }

    #[test]
    fn ids_that_could_leave_the_folder_are_refused() {
        assert!(safe_id("a1b2c3_d-e"));
        for bad in ["", "..", "a/b", "a\\b", "a.json", "a b"] {
            assert!(!safe_id(bad), "{bad}");
        }
        assert_eq!(session_id(Some("../x")), None);
    }

    #[test]
    fn an_atomic_write_replaces_the_file_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("l.json");
        fs::write(&p, "old").unwrap();
        write_atomic(&p, b"new").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "new");
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
    }

    #[test]
    fn a_held_lock_makes_others_wait_and_then_give_up() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("l.json.lock");
        let held = acquire_lock(&lock, Duration::from_secs(1), Duration::from_secs(60)).unwrap();
        let t = Instant::now();
        let err = acquire_lock(&lock, Duration::from_millis(300), Duration::from_secs(60)).unwrap_err();
        assert!(err.contains("locked"), "{err}");
        assert!(t.elapsed() >= Duration::from_millis(300));
        drop(held);
        assert!(!lock.exists());
        acquire_lock(&lock, Duration::from_millis(300), Duration::from_secs(60)).unwrap();
    }

    #[test]
    fn a_stale_lock_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("l.json.lock");
        fs::write(&lock, "left behind by a dead process").unwrap();
        std::thread::sleep(Duration::from_millis(250));
        // stale after 100 ms for the test (60 s in use)
        let g = acquire_lock(&lock, Duration::from_secs(2), Duration::from_millis(100)).unwrap();
        assert!(fs::read_to_string(&lock).unwrap().starts_with("lw pid"));
        drop(g);
        assert!(!lock.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0, "nothing left beside the lock");
    }

    #[test]
    fn each_lock_holds_its_own_nonce() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("l.json.lock");
        let held = || {
            let g = acquire_lock(&lock, Duration::from_secs(1), Duration::from_secs(60)).unwrap();
            let c = fs::read_to_string(&lock).unwrap();
            drop(g);
            c
        };
        let (a, b) = (held(), held());
        assert_ne!(a, b);
        assert!(b.contains(" nonce ") && b.trim_end().rsplit(' ').next().unwrap().len() == 32, "{b}");
    }

    #[test]
    fn release_leaves_a_lock_that_is_no_longer_ours() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("l.json.lock");
        let g = acquire_lock(&lock, Duration::from_secs(1), Duration::from_secs(60)).unwrap();
        // this holder stalled past the stale age, and another process took the lock over
        fs::write(&lock, "lw pid 999 nonce 0123456789abcdef0123456789abcdef\n").unwrap();
        drop(g);
        assert_eq!(fs::read_to_string(&lock).unwrap(), "lw pid 999 nonce 0123456789abcdef0123456789abcdef\n");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1, "only the other holder's lock");
    }

    #[test]
    fn a_stale_takeover_removes_only_the_lock_it_judged_stale() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("l.json.lock");
        // a waiter read the stale lock (old content); before its takeover, another waiter already replaced it with a fresh one
        let fresh = "lw pid 2 nonce ffffffffffffffffffffffffffffffff\n";
        fs::write(&lock, fresh).unwrap();
        assert!(!remove_if(&lock, "lw pid 1 nonce 00000000000000000000000000000000\n", "stale-test"));
        assert_eq!(fs::read_to_string(&lock).unwrap(), fresh, "the fresh lock is put back");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        // the lock it did judge stale goes
        assert!(remove_if(&lock, fresh, "stale-test"));
        assert!(!lock.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn concurrent_updates_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("l.json");
        Store::new(p.clone()).create(&sample()).unwrap();
        let p = Arc::new(p);
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let p = Arc::clone(&p);
                std::thread::spawn(move || {
                    for j in 0..5 {
                        Store::new((*p).clone())
                            .update(|l| {
                                l.push_log("t", format!("{i}-{j}"));
                                Ok(((), true))
                            })
                            .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let l = Store::new((*p).clone()).read().unwrap();
        assert_eq!(l.log.len(), 40);
        assert!(!Store::new((*p).clone()).lock_path().exists());
    }

    #[test]
    fn create_refuses_an_existing_ledger_and_update_without_change_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("l.json");
        let s = Store::new(p.clone());
        s.create(&sample()).unwrap();
        assert!(s.create(&sample()).unwrap_err().contains("already exists"));
        let before = fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(30));
        s.update(|_| Ok(((), false))).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().modified().unwrap(), before);
    }
}
