//! Runs the real binary against ledgers in a temp folder. Every run points USERPROFILE, the projects folder and the toast hook
//! into that folder too, so nothing here can touch the real `.claude` folder or show a real toast.
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

struct Env {
    dir: tempfile::TempDir,
}

struct Ran {
    code: i32,
    out: String,
    err: String,
}

impl Ran {
    fn json(&self) -> Value {
        assert_eq!(self.code, 0, "exit {}: {}{}", self.code, self.out, self.err);
        serde_json::from_str(self.out.trim()).unwrap_or_else(|e| panic!("{e}: {}", self.out))
    }
}

impl Env {
    fn new() -> Env {
        let e = Env { dir: tempfile::tempdir().unwrap() };
        std::fs::create_dir_all(e.home()).unwrap();
        std::fs::create_dir_all(e.projects()).unwrap();
        e
    }
    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }
    fn projects(&self) -> PathBuf {
        self.dir.path().join("projects")
    }
    fn toasts(&self) -> PathBuf {
        self.dir.path().join("toasts.jsonl")
    }
    fn ledger(&self) -> PathBuf {
        self.dir.path().join("ledgers").join("l.json")
    }
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_lw"));
        c.env("USERPROFILE", self.home())
            .env("HOME", self.home())
            .env("LW_TZ", "America/Los_Angeles")
            .env("LW_PROJECTS_DIR", self.projects())
            .env("LW_TOAST_FILE", self.toasts())
            .env_remove("LW_TOAST_FAIL")
            .env("CLAUDE_CODE_SESSION_ID", "sess-1")
            .env("CLAUDE_CODE_HOST_SESSION_ID", "local_test");
        c
    }
    fn raw(&self, c: &mut Command, args: &[&str]) -> Ran {
        let o = c.args(args).output().unwrap();
        Ran { code: o.status.code().unwrap_or(-1), out: String::from_utf8_lossy(&o.stdout).into(), err: String::from_utf8_lossy(&o.stderr).into() }
    }
    /// Run against the temp ledger at `now`.
    fn at(&self, now: &str, args: &[&str]) -> Ran {
        let l = self.ledger();
        let mut full = vec!["--ledger", l.to_str().unwrap(), "--now", now];
        full.extend_from_slice(args);
        self.raw(&mut self.cmd(), &full)
    }
    fn ok(&self, now: &str, args: &[&str]) -> Value {
        self.at(now, args).json()
    }
    fn transcript(&self, agent: &str, bytes: usize) {
        let d = self.projects().join("C--proj").join("sess-1").join("subagents");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("agent-{agent}.jsonl")), vec![b'.'; bytes]).unwrap();
    }
    fn read_ledger(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.ledger()).unwrap()).unwrap()
    }
}

const T0: &str = "2026-11-15T14:00:00-08:00";
const RESET: &str = "2026-11-15T23:30:00.194Z"; // 15:30 local, as get_usage writes it
const RESET2: &str = "2026-11-16T04:30:00.000Z"; // 20:30 local

fn add(e: &Env, id: &str, ty: &str, risk: &str) -> Value {
    e.ok(
        T0,
        &[
            "add",
            "--id",
            id,
            "--name",
            &format!("{id} stream"),
            "--agent-type",
            ty,
            "--kind",
            "resumable",
            "--risk",
            risk,
            "--worktree",
            &format!("C:\\wt\\{id}"),
            "--handoff",
            &format!("C:\\wt\\{id}\\HANDOFF.md"),
            "--brief-file",
            &format!("C:\\wt\\{id}\\BRIEF.md"),
        ],
    )
}

#[test]
fn a_night_from_arming_to_done() {
    let e = Env::new();
    let init = e.ok(T0, &["init", "--project", "demo (C:\\Dev\\demo)", "--unattended"]);
    assert_eq!(init["session_id"], "sess-1");
    assert_eq!(init["host_session_id"], "local_test");
    add(&e, "a1", "general-purpose", "local-writes");
    add(&e, "a2", "general-purpose", "read-only");
    let explore = add(&e, "a3", "Explore", "read-only");
    assert_eq!(explore["agent"]["kind"], "redispatch");
    e.transcript("a1", 4_000);
    e.transcript("a2", 9_000);

    assert_eq!(e.ok(T0, &["tick", "--five-pct", "40", "--five-resets-at", RESET])["action"], "ok");
    let arm = e.ok(
        "2026-11-15T14:30:00-08:00",
        &["tick", "--five-pct", "81", "--five-resets-at", RESET, "--weekly-pct", "30", "--weekly-resets-at", "2026-11-19T17:00:00Z"],
    );
    assert_eq!(arm["action"], "arm");
    assert_eq!(arm["token"], "w1");
    assert_eq!(arm["cron"], "34 15 15 11 *");
    assert_eq!(arm["wake_at"], "2026-11-15T15:34:00-08:00");
    assert_eq!(arm["backup_at"], "2026-11-15T15:40:00-08:00");
    let again = e.ok("2026-11-15T14:40:00-08:00", &["tick", "--five-pct", "88", "--five-resets-at", "2026-11-15T23:30:00.201Z"]);
    assert_eq!((again["action"].as_str(), again["token"].as_str()), (Some("armed"), Some("w1")));

    e.ok("2026-11-15T14:41:00-08:00", &["timer-add", "--kind", "cron-oneshot", "--id", "cron-abc", "--fires", "2026-11-15T15:34:00-08:00", "--token", "w1"]);
    e.ok(
        "2026-11-15T14:41:00-08:00",
        &["timer-add", "--kind", "desktop-task", "--id", "limit-watchdog-w1", "--fires", "2026-11-15T15:40:00-08:00", "--token", "w1"],
    );
    for id in ["a1", "a2", "a3"] {
        let s = e.ok("2026-11-15T15:20:00-08:00", &["stopped", id, "--text", "You've hit your 5-hour limit \u{b7} resets 3:30pm (America/Los_Angeles)"]);
        assert_eq!(s["state"], "stopped-by-limit");
        assert_eq!(s["reset"]["reset"], "2026-11-15T15:30:00-08:00");
    }

    // the backup task's read-only check changes nothing
    let before = std::fs::read(e.ledger()).unwrap();
    let ws = e.ok("2026-11-15T15:33:00-08:00", &["wake-status", "--token", "w1"]);
    assert_eq!((ws["handled"].as_bool(), ws["current"].as_bool(), ws["pending_agents"].as_u64()), (Some(false), Some(true), Some(3)));
    assert_eq!(ws["host_session_id"], "local_test");
    assert_eq!(std::fs::read(e.ledger()).unwrap(), before);

    // the one-shot fires: first batch is the two read-only agents, smallest transcript first (a3 has none: 0)
    let n = e.ok("2026-11-15T15:34:02-08:00", &["next", "--token", "w1", "--five-pct", "2", "--five-resets-at", RESET2]);
    assert_eq!(n["action"], "resume");
    let batch: Vec<(&str, &str)> = n["batch"].as_array().unwrap().iter().map(|b| (b["id"].as_str().unwrap(), b["mode"].as_str().unwrap())).collect();
    assert_eq!(batch, [("a3", "redispatch"), ("a2", "resume")]);
    assert_eq!(n["batch"][1]["transcript_bytes"], 9_000);
    assert_eq!(n["remaining"], 1);
    assert_eq!(n["arm"]["token"], "w2");
    assert_eq!(n["arm"]["wake_at"], "2026-11-15T15:47:00-08:00");
    assert_eq!(n["delete_timers"], serde_json::json!(["cron-abc", "limit-watchdog-w1"]));
    // the backup task fires later with the same token
    assert_eq!(e.ok("2026-11-15T15:40:00-08:00", &["next", "--token", "w1", "--five-pct", "5", "--five-resets-at", RESET2])["action"], "already-handled");
    assert_eq!(e.ok("2026-11-15T15:40:00-08:00", &["wake-status", "--token", "w1"])["handled"], true);

    // second batch
    let n = e.ok("2026-11-15T15:47:00-08:00", &["next", "--token", "w2", "--five-pct", "9", "--five-resets-at", RESET2]);
    assert_eq!(n["batch"][0]["id"], "a1");
    assert_eq!(n["batch"][0]["mode"], "resume");
    assert_eq!(n["arm"], Value::Null);
    assert_eq!(n["remaining"], 0);

    for id in ["a1", "a2", "a3"] {
        e.ok("2026-11-15T16:30:00-08:00", &["set", id, "done", "--note", "finished"]);
    }
    let l = e.read_ledger();
    assert_eq!(l["version"], 2);
    assert_eq!(l["updated"], "2026-11-15T16:30:00-08:00");
    assert_eq!(l["run"]["wake_cycles"], 1);
    assert_eq!(l["agents"][0]["resumes_tonight"], 1);
    assert!(l["timers"].as_array().unwrap().is_empty());
    let show = e.at("2026-11-15T16:31:00-08:00", &["show"]);
    assert_eq!(show.code, 0);
    assert!(show.out.contains("3 done"), "{}", show.out);
    let js: Value = serde_json::from_str(&e.at("2026-11-15T16:31:00-08:00", &["show", "--json"]).out).unwrap();
    assert_eq!(js["project"], "demo (C:\\Dev\\demo)");
    assert!(!PathBuf::from(format!("{}.lock", e.ledger().display())).exists());
}

#[test]
fn done_when_everything_finished_before_the_wall() {
    let e = Env::new();
    e.ok(T0, &["init"]);
    add(&e, "a1", "general-purpose", "local-writes");
    e.ok(T0, &["tick", "--five-pct", "85", "--five-resets-at", RESET]);
    e.ok(T0, &["timer-add", "--kind", "cron-oneshot", "--id", "c1", "--fires", "2026-11-15T15:34:00-08:00", "--token", "w1"]);
    e.ok(T0, &["set", "a1", "done"]);
    let d = e.ok("2026-11-15T15:34:00-08:00", &["next", "--token", "w1", "--five-pct", "1", "--five-resets-at", RESET2]);
    assert_eq!(d["action"], "done");
    assert_eq!(d["delete_timers"], serde_json::json!(["c1"]));
    let l = e.read_ledger();
    assert_eq!(l["wake"], Value::Null);
    assert!(l["timers"].as_array().unwrap().is_empty());
    assert_eq!(e.ok(T0, &["timers-clear"])["delete_timers"], serde_json::json!([]));
}

#[test]
fn still_limited_and_weekly_paths_through_the_binary() {
    let e = Env::new();
    e.ok(T0, &["init"]);
    add(&e, "a1", "general-purpose", "local-writes");
    e.ok(T0, &["stopped", "a1", "--text", "5-hour limit reached \u{2219} resets 3:30pm"]);
    assert_eq!(e.ok(T0, &["tick", "--five-pct", "100", "--five-resets-at", RESET])["action"], "arm");
    let s = e.ok("2026-11-15T15:34:00-08:00", &["next", "--token", "w1", "--five-pct", "100", "--five-resets-at", RESET2]);
    assert_eq!((s["action"].as_str(), s["token"].as_str(), s["cron"].as_str()), (Some("still-limited"), Some("w2"), Some("34 20 15 11 *")));
    let w = e.ok(
        "2026-11-15T20:34:00-08:00",
        &[
            "next",
            "--token",
            "w2",
            "--five-pct",
            "0",
            "--five-resets-at",
            "2026-11-16T01:34:00-08:00",
            "--weekly-pct",
            "100",
            "--weekly-resets-at",
            "2026-11-17T09:00:00-08:00",
        ],
    );
    assert_eq!(w["action"], "weekly-stop");
    assert_eq!(w["notify_at"], "2026-11-17T09:15:00-08:00");
    assert_eq!(e.read_ledger()["agents"][0]["state"], "stopped-by-weekly");
}

#[test]
fn a_refail_trips_the_herd_guard() {
    let e = Env::new();
    e.ok(T0, &["init"]);
    add(&e, "a1", "general-purpose", "local-writes");
    add(&e, "a2", "general-purpose", "local-writes");
    add(&e, "a3", "general-purpose", "local-writes");
    for id in ["a1", "a2", "a3"] {
        e.ok(T0, &["stopped", id, "--text", "usage limit reached"]);
    }
    e.ok(T0, &["tick", "--five-pct", "100", "--five-resets-at", RESET]);
    let n = e.ok("2026-11-15T15:34:00-08:00", &["next", "--token", "w1", "--five-pct", "3", "--five-resets-at", RESET2]);
    assert_eq!(n["remaining"], 1);
    let s = e.ok("2026-11-15T15:50:00-08:00", &["stopped", "a1", "--text", "You've hit your limit \u{b7} resets 8:30pm"]);
    assert_eq!(s["herd_tripped"], true);
    let h = e.ok("2026-11-15T15:52:00-08:00", &["next", "--token", "w2", "--five-pct", "12", "--five-resets-at", RESET2]);
    assert_eq!(h["action"], "herd-stop");
    assert_eq!(e.ok("2026-11-15T15:53:00-08:00", &["tick", "--five-pct", "90", "--five-resets-at", RESET2])["action"], "herd-stopped");
    e.ok("2026-11-16T09:00:00-08:00", &["new-night"]);
    assert_eq!(e.read_ledger()["run"]["herd_tripped"], false);
}

#[test]
fn refusals_and_command_line_errors() {
    let e = Env::new();
    e.ok(T0, &["init"]);
    let again = e.at(T0, &["init"]);
    assert_eq!(again.code, 1);
    assert!(again.err.contains("already exists"), "{}", again.err);
    add(&e, "a1", "general-purpose", "local-writes");
    let dup = e.at(T0, &["add", "--id", "a1", "--name", "x", "--agent-type", "general-purpose", "--kind", "resumable", "--risk", "read-only"]);
    assert_eq!(dup.code, 1);
    assert!(dup.err.contains("already"));
    for (args, code) in [
        (vec!["set", "a1", "sleeping"], 2),
        (vec!["set", "zz", "done"], 1),
        (vec!["frobnicate"], 2),
        (vec!["tick", "--five-resets-at", RESET], 2),
        (vec!["tick", "--five-pct", "50", "--five-resets-at", RESET, "--weekly-pct", "50"], 2),
        (vec!["tick", "--five-pct", "lots", "--five-resets-at", RESET], 2),
        (vec!["add", "--id", "b1", "--name", "x", "--agent-type", "t", "--kind", "resumable", "--risk", "dangerous"], 2),
        (vec!["stopped", "a1", "--text", "Error: connection reset by peer"], 1),
        (vec!["timer-add", "--kind", "alarm", "--id", "x", "--fires", T0, "--token", "w1"], 2),
        (vec!["run-set", "arm_at_pct", "150"], 1),
        (vec!["show", "--remove"], 2),
        (vec!["reset-time", "resets whenever"], 1),
        (vec!["size", "../a1"], 2),
    ] {
        let r = e.at(T0, &args);
        assert_eq!(r.code, code, "{args:?}: {}{}", r.out, r.err);
        assert!(r.err.starts_with("lw: "), "{args:?}: {}", r.err);
        assert!(r.out.is_empty(), "{args:?}: {}", r.out);
    }
    let bad_now = e.raw(&mut e.cmd(), &["--ledger", e.ledger().to_str().unwrap(), "--now", "yesterday", "show"]);
    assert_eq!(bad_now.code, 1);
    // the ledger is untouched by all of that
    assert_eq!(e.read_ledger()["agents"].as_array().unwrap().len(), 1);
}

#[test]
fn reset_time_is_a_pure_parser() {
    let e = Env::new();
    let r = e.ok("2026-11-15T22:00:00-08:00", &["reset-time", "You've hit your 5-hour limit \u{b7} resets 3:40am (America/Los_Angeles)"]);
    assert_eq!(r["reset"], "2026-11-16T03:40:00-08:00");
    assert_eq!(r["wake_at"], "2026-11-16T03:44:00-08:00");
    assert_eq!(r["cron"], "44 3 16 11 *");
    assert_eq!(r["backup_at"], "2026-11-16T03:50:00-08:00");
    let r = e.ok(T0, &["reset-time", RESET]);
    assert_eq!(r["wake_at"], "2026-11-15T15:34:00-08:00");
    assert!(!e.ledger().exists());
}

#[test]
fn the_default_ledger_path_comes_from_the_session() {
    let e = Env::new();
    let r = e.raw(&mut e.cmd(), &["--now", T0, "init"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let by_env = e.home().join(".claude").join("work-ledger").join("sess-1.json");
    assert!(by_env.exists());
    let r = e.raw(&mut e.cmd(), &["--session", "other-2", "--now", T0, "init"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(e.home().join(".claude").join("work-ledger").join("other-2.json").exists());
    let r = e.raw(e.cmd().env_remove("CLAUDE_CODE_SESSION_ID"), &["show"]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("--ledger"), "{}", r.err);
    let r = e.raw(&mut e.cmd(), &["--session", "..\\escape", "show"]);
    assert_eq!(r.code, 1);
}

#[test]
fn size_reads_metadata_only() {
    let e = Env::new();
    e.ok(T0, &["init"]);
    add(&e, "a1", "general-purpose", "local-writes");
    e.transcript("a1", 3_500_000);
    let s = e.ok(T0, &["size", "a1"]);
    assert_eq!((s["bytes"].as_u64(), s["mode"].as_str()), (Some(3_500_000), Some("redispatch")));
    assert!(s["path"].as_str().unwrap().ends_with("agent-a1.jsonl"));
    e.transcript("a1", 10);
    assert_eq!(e.ok(T0, &["size", "a1"])["mode"], "resume");
    let missing = e.ok(T0, &["size", "a9"]);
    assert_eq!((missing["bytes"].clone(), missing["mode"].as_str()), (Value::Null, Some("redispatch")));
}

#[test]
fn toast_logs_and_never_fails_the_caller() {
    let e = Env::new();
    e.ok(T0, &["init"]);
    let t = e.ok(T0, &["toast", "Limit watchdog", "Resumed 2: a, b."]);
    assert_eq!((t["shown"].as_bool(), t["logged"].as_bool()), (Some(true), Some(true)));
    let line: Value = serde_json::from_str(std::fs::read_to_string(e.toasts()).unwrap().lines().next().unwrap()).unwrap();
    assert_eq!((line["title"].as_str(), line["body"].as_str()), (Some("Limit watchdog"), Some("Resumed 2: a, b.")));
    let r = e.raw(e.cmd().env("LW_TOAST_FAIL", "1"), &["--ledger", e.ledger().to_str().unwrap(), "--now", T0, "toast", "T", "B"]);
    assert_eq!(r.code, 0);
    assert!(r.err.contains("warning"), "{}", r.err);
    let v: Value = serde_json::from_str(r.out.trim()).unwrap();
    assert_eq!((v["shown"].as_bool(), v["logged"].as_bool()), (Some(false), Some(true)));
    let log = e.read_ledger()["log"].clone();
    assert!(log.as_array().unwrap().iter().any(|l| l["msg"] == "toast (not shown): T: B"));
    // no ledger: still shown, says it was not logged
    let r = e.raw(e.cmd().env_remove("CLAUDE_CODE_SESSION_ID"), &["toast", "T", "B"]);
    assert_eq!(r.code, 0);
    assert!(r.err.contains("not logged"));
}

#[test]
fn migrate_v1_reads_a_copy_and_writes_a_new_ledger() {
    let e = Env::new();
    let v1 = e.dir.path().join("work-ledger-v1-copy.json");
    let text = r#"{"agents":[{"id":"a1","stream":"bridge","worktree":"C:\\wt","state":"stopped-by-limit","resume":"Go on."},{"id":"a2","stream":"docs","state":"done"}]}"#;
    std::fs::write(&v1, text).unwrap();
    let m = e.ok(T0, &["migrate-v1", v1.to_str().unwrap()]);
    assert_eq!(m["agents"], 2);
    let l = e.read_ledger();
    assert_eq!(l["version"], 2);
    assert_eq!(l["session_id"], "sess-1");
    assert_eq!(l["agents"][0]["name"], "bridge");
    assert_eq!(l["agents"][0]["risk"], "local-writes");
    assert_eq!(l["agents"][0]["kind"], "resumable");
    assert_eq!(std::fs::read_to_string(&v1).unwrap(), text);
    assert_eq!(e.at(T0, &["migrate-v1", v1.to_str().unwrap()]).code, 1, "refuses to overwrite the new ledger");
    // migrating onto the v1 file itself is refused
    let r = e.raw(&mut e.cmd(), &["--ledger", v1.to_str().unwrap(), "migrate-v1", v1.to_str().unwrap()]);
    assert_eq!(r.code, 2);
    assert_eq!(std::fs::read_to_string(&v1).unwrap(), text);
}

#[test]
fn a_stale_lock_is_taken_over_and_removed() {
    let e = Env::new();
    e.ok(T0, &["init"]);
    let lock = PathBuf::from(format!("{}.lock", e.ledger().display()));
    let f = std::fs::File::create(&lock).unwrap();
    f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(120)).unwrap();
    drop(f);
    e.ok(T0, &["log", "after a crash"]);
    assert!(!lock.exists());
    assert!(no_temp_files(e.ledger().parent().unwrap()));
}

fn no_temp_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).unwrap().all(|f| !f.unwrap().file_name().to_string_lossy().contains(".tmp-"))
}

#[test]
fn run_set_and_log_go_through() {
    let e = Env::new();
    e.ok(T0, &["init"]);
    let r = e.ok(T0, &["run-set", "max-concurrent", "3"]);
    assert_eq!((r["from"].as_u64(), r["value"].as_u64()), (Some(4), Some(3)));
    e.ok(T0, &["log", "hello", "there"]);
    let l = e.read_ledger();
    assert_eq!(l["run"]["max_concurrent"], 3);
    assert_eq!(l["log"].as_array().unwrap().last().unwrap()["msg"], "hello there");
}
