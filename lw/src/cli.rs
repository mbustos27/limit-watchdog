//! Arguments in, JSON out. Errors go to stderr with a non-zero exit: 2 for a wrong command line, 1 for anything else.
use serde_json::{Value, json};

use crate::clock::{self, Ctx, Zone};
use crate::engine::{self, Fields, NewAgent, Usage};
use crate::ledger::{self, Kind, Ledger, Risk, State, Store, TimerKind, parse_word};
use crate::reset;
use crate::size;
use crate::toast::{self, Delivery};

pub enum Fail {
    Usage(String),
    Run(String),
}

impl From<String> for Fail {
    fn from(s: String) -> Fail {
        Fail::Run(s)
    }
}

impl From<&str> for Fail {
    fn from(s: &str) -> Fail {
        Fail::Run(s.to_string())
    }
}

pub enum Output {
    Json(Value),
    Text(String),
}

const BOOL_FLAGS: [&str; 6] = ["--unattended", "--json", "--remove", "--help", "-h", "--version"];
const GLOBAL: [&str; 3] = ["--ledger", "--session", "--now"];
const USAGE_OPTS: [&str; 4] = ["--five-pct", "--five-resets-at", "--weekly-pct", "--weekly-resets-at"];

pub const HELP: &str = "\
lw - the limit watchdog's ledger and timing decisions (prints JSON)

global:  --ledger PATH   --session ID (else CLAUDE_CODE_SESSION_ID)   --now ISO (tests)
         ledger default: %USERPROFILE%\\.claude\\work-ledger\\<session>.json

  init [--project TEXT] [--unattended]
  add --id ID --name NAME --agent-type TYPE --kind resumable|redispatch --risk read-only|local-writes|irreversible
      [--worktree PATH] [--handoff PATH] [--brief-file PATH] [--resume TEXT]
  add --id NEW --replaces OLD [any field above]   a re-dispatch: copies the rest and the resume count, sets OLD done
  edit ID [--name] [--agent-type] [--kind] [--risk] [--worktree] [--handoff] [--brief-file] [--resume]
  set ID STATE [--note TEXT]           states: running stopped-by-limit stopped-by-weekly needs-human paused-by-user done
  stopped ID --text \"<limit notice>\"   classify a limit failure (only a running agent changes state)
  unsent ID...                         batch items that were not sent go back to stopped-by-limit; then run tick
  reset-time \"<text or ISO>\"           {reset, wake_at, cron, backup_at}
  tick --five-pct N --five-resets-at ISO [--weekly-pct N --weekly-resets-at ISO]
  next --token T --five-pct N --five-resets-at ISO [--weekly-pct N --weekly-resets-at ISO]
  timer-add --kind cron-oneshot|desktop-task --id ID --fires ISO --token T
  wake-status --token T                read-only, for the backup task
  timers-clear
  size ID                              transcript size from file metadata only
  run-set KEY VALUE
  new-night                            zero wake cycles, herd guard and resume counts
  show [--json]
  log TEXT
  toast TITLE BODY
  toast-setup [--remove]               register (or remove) lw's toast app id under HKCU
  migrate-v1 PATH                      convert a version 1 ledger into a new version 2 ledger
";

struct Args {
    pos: Vec<String>,
    opts: Vec<(String, String)>,
    flags: Vec<String>,
}

impl Args {
    fn parse(argv: &[String]) -> Result<Args, Fail> {
        let mut a = Args { pos: Vec::new(), opts: Vec::new(), flags: Vec::new() };
        let mut only_pos = false;
        let mut i = 0;
        while i < argv.len() {
            let t = &argv[i];
            i += 1;
            if only_pos || !(t.starts_with("--") || t == "-h") {
                a.pos.push(t.clone());
                continue;
            }
            if t == "--" {
                only_pos = true;
                continue;
            }
            let (k, inline) = match t.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (t.clone(), None),
            };
            if BOOL_FLAGS.contains(&k.as_str()) {
                if inline.is_some() {
                    return Err(Fail::Usage(format!("{k} takes no value")));
                }
                a.flags.push(k);
                continue;
            }
            let v = match inline {
                Some(v) => v,
                None => {
                    let v = argv.get(i).ok_or_else(|| Fail::Usage(format!("{k} needs a value")))?.clone();
                    i += 1;
                    v
                }
            };
            if a.opts.iter().any(|(x, _)| *x == k) {
                return Err(Fail::Usage(format!("{k} is given twice")));
            }
            a.opts.push((k, v));
        }
        Ok(a)
    }

    fn opt(&self, k: &str) -> Option<&str> {
        self.opts.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str())
    }

    fn need(&self, k: &str) -> Result<&str, Fail> {
        self.opt(k).ok_or_else(|| Fail::Usage(format!("{k} is required")))
    }

    fn flag(&self, k: &str) -> bool {
        self.flags.iter().any(|f| f == k)
    }

    /// Refuse options and flags the command does not take, and a wrong number of positional arguments.
    fn check(&self, opts: &[&str], flags: &[&str], min: usize, max: Option<usize>) -> Result<(), Fail> {
        let cmd = self.pos.first().map(String::as_str).unwrap_or("");
        for (k, _) in &self.opts {
            if !GLOBAL.contains(&k.as_str()) && !opts.contains(&k.as_str()) {
                return Err(Fail::Usage(format!("{cmd} does not take {k}")));
            }
        }
        for f in &self.flags {
            if !flags.contains(&f.as_str()) {
                return Err(Fail::Usage(format!("{cmd} does not take {f}")));
            }
        }
        let n = self.pos.len().saturating_sub(1);
        if n < min || max.is_some_and(|m| n > m) {
            return Err(Fail::Usage(format!("{cmd}: wrong number of arguments ({n})")));
        }
        Ok(())
    }
}

fn word_arg<T: serde::de::DeserializeOwned>(v: &str, what: &str, choices: &str) -> Result<T, Fail> {
    parse_word(v, what, choices).map_err(Fail::Usage)
}

/// The agent fields `add` and `edit` take.
const FIELD_OPTS: [&str; 8] = ["--name", "--agent-type", "--kind", "--risk", "--worktree", "--handoff", "--brief-file", "--resume"];

fn fields(a: &Args) -> Result<Fields, Fail> {
    let s = |k: &str| a.opt(k).map(String::from);
    Ok(Fields {
        name: s("--name"),
        agent_type: s("--agent-type"),
        kind: a.opt("--kind").map(|v| word_arg::<Kind>(v, "--kind", ledger::KINDS)).transpose()?,
        risk: a.opt("--risk").map(|v| word_arg::<Risk>(v, "--risk", ledger::RISKS)).transpose()?,
        worktree: s("--worktree"),
        handoff: s("--handoff"),
        brief_file: s("--brief-file"),
        resume: s("--resume"),
    })
}

fn pct(k: &str, v: &str) -> Result<u32, Fail> {
    let n: f64 = v.trim().trim_end_matches('%').parse().map_err(|_| Fail::Usage(format!("{k}: {v:?} is not a number")))?;
    if !n.is_finite() || !(0.0..=1000.0).contains(&n) {
        return Err(Fail::Usage(format!("{k}: {v:?} is out of range")));
    }
    Ok(n.round() as u32)
}

fn usage(a: &Args, ctx: &Ctx) -> Result<Usage, Fail> {
    let five_pct = pct("--five-pct", a.need("--five-pct")?)?;
    let five_resets_at = reset::parse_reset(a.need("--five-resets-at")?, ctx).map_err(|e| Fail::Usage(format!("--five-resets-at: {e}")))?;
    let weekly = match (a.opt("--weekly-pct"), a.opt("--weekly-resets-at")) {
        (Some(p), Some(r)) => Some((pct("--weekly-pct", p)?, reset::parse_reset(r, ctx).map_err(|e| Fail::Usage(format!("--weekly-resets-at: {e}")))?)),
        (None, None) => None,
        _ => return Err(Fail::Usage("--weekly-pct and --weekly-resets-at go together".into())),
    };
    Ok(Usage { five_pct, five_resets_at, weekly })
}

/// Lock, change, stamp `updated`, write.
fn change(store: &Store, ctx: &Ctx, f: impl FnOnce(&mut Ledger) -> engine::Out) -> Result<Output, Fail> {
    let v = store.update(|l| {
        let (out, changed) = f(l)?;
        if changed {
            l.updated = ctx.now_str();
        }
        Ok((out, changed))
    })?;
    Ok(Output::Json(v))
}

pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args_os().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    match run(&argv) {
        Ok(Output::Json(v)) => {
            println!("{v}");
            0
        }
        Ok(Output::Text(s)) => {
            print!("{s}");
            0
        }
        Err(Fail::Usage(m)) => {
            eprintln!("lw: {m}\nRun `lw help` for the commands.");
            2
        }
        Err(Fail::Run(m)) => {
            eprintln!("lw: {m}");
            1
        }
    }
}

pub fn run(argv: &[String]) -> Result<Output, Fail> {
    let a = Args::parse(argv)?;
    if a.flag("--version") {
        return Ok(Output::Text(format!("lw {}\n", env!("CARGO_PKG_VERSION"))));
    }
    let cmd = a.pos.first().cloned().unwrap_or_else(|| "help".into());
    if a.flag("--help") || a.flag("-h") || cmd == "help" {
        return Ok(Output::Text(HELP.to_string()));
    }
    let zone = Zone::from_env()?;
    let ctx = Ctx { now: clock::now_from(a.opt("--now"), &zone)?, zone };
    let path = || ledger::resolve_path(a.opt("--ledger"), a.opt("--session")).map_err(Fail::Run);
    let store = || path().map(Store::new);

    match cmd.as_str() {
        "reset-time" => {
            a.check(&[], &[], 1, None)?;
            let r = reset::parse_reset(&a.pos[1..].join(" "), &ctx)?;
            Ok(Output::Json(reset::schedule(r, &ctx).json(&ctx)))
        }
        "init" => {
            a.check(&["--project"], &["--unattended"], 0, Some(0))?;
            let p = path()?;
            let sid = ledger::session_id(a.opt("--session")).unwrap_or_default();
            let host = std::env::var(ledger::HOST_SESSION_ENV).unwrap_or_default().trim().to_string();
            let unattended = a.flag("--unattended");
            let mut l = Ledger::new(&sid, &host, a.opt("--project").unwrap_or("").trim(), &ctx.now_str());
            l.run.unattended = unattended;
            l.push_log(&ctx.now_str(), if unattended { "init (unattended)" } else { "init" });
            Store::new(p.clone()).create(&l)?;
            let mut warnings: Vec<String> = Vec::new();
            if sid.is_empty() {
                warnings.push("no session id: lw cannot find transcripts, so every agent would be re-dispatched".into());
            }
            if host.is_empty() {
                warnings.push(format!(
                    "{} is empty: the Desktop backup task cannot send_message this session, so only the in-session one-shot can wake it",
                    ledger::HOST_SESSION_ENV
                ));
            }
            for w in &warnings {
                eprintln!("lw: warning: {w}");
            }
            Ok(Output::Json(json!({ "ok": true, "ledger": p.display().to_string(), "session_id": sid, "host_session_id": host, "warnings": warnings })))
        }
        "add" => {
            let mut opts = FIELD_OPTS.to_vec();
            opts.extend(["--id", "--replaces"]);
            a.check(&opts, &[], 0, Some(0))?;
            let replaces = a.opt("--replaces").map(|s| s.trim().to_string());
            if replaces.is_none() {
                for k in ["--name", "--agent-type", "--kind", "--risk"] {
                    a.need(k)?;
                }
            }
            let na = NewAgent { id: a.need("--id")?.trim().to_string(), replaces, fields: fields(&a)? };
            change(&store()?, &ctx, |l| engine::add(l, &ctx, na))
        }
        "edit" => {
            a.check(&FIELD_OPTS, &[], 1, Some(1))?;
            if !FIELD_OPTS.iter().any(|k| a.opt(k).is_some()) {
                return Err(Fail::Usage(format!("edit: give at least one of {}", FIELD_OPTS.join(" "))));
            }
            let f = fields(&a)?;
            change(&store()?, &ctx, |l| engine::edit(l, &ctx, a.pos[1].trim(), f))
        }
        "unsent" => {
            a.check(&[], &[], 1, None)?;
            change(&store()?, &ctx, |l| engine::unsent(l, &ctx, &a.pos[1..]))
        }
        "set" => {
            a.check(&["--note"], &[], 2, Some(2))?;
            let state = word_arg::<State>(&a.pos[2], "the state", ledger::STATES)?;
            change(&store()?, &ctx, |l| engine::set(l, &ctx, &a.pos[1], state, a.opt("--note")))
        }
        "stopped" => {
            a.check(&["--text"], &[], 1, Some(1))?;
            let text = a.need("--text")?;
            change(&store()?, &ctx, |l| engine::stopped(l, &ctx, &a.pos[1], text))
        }
        "tick" => {
            a.check(&USAGE_OPTS, &[], 0, Some(0))?;
            let u = usage(&a, &ctx)?;
            change(&store()?, &ctx, |l| engine::tick(l, &ctx, &u))
        }
        "next" => {
            let mut opts = USAGE_OPTS.to_vec();
            opts.push("--token");
            a.check(&opts, &[], 0, Some(0))?;
            let token = a.need("--token")?.trim().to_string();
            let u = usage(&a, &ctx)?;
            let root = size::projects_root();
            change(&store()?, &ctx, |l| {
                let sid = l.session_id.clone();
                let sizes = |ag: &ledger::Agent| root.as_deref().and_then(|r| size::find(r, &sid, &ag.id)).map(|(_, b)| b);
                engine::next(l, &ctx, &token, &u, &sizes)
            })
        }
        "timer-add" => {
            a.check(&["--kind", "--id", "--fires", "--token"], &[], 0, Some(0))?;
            let kind = word_arg::<TimerKind>(a.need("--kind")?, "--kind", ledger::TIMER_KINDS)?;
            let fires = clock::parse_instant(a.need("--fires")?, &ctx.zone).map_err(|e| Fail::Usage(format!("--fires: {e}")))?;
            let (id, token) = (a.need("--id")?, a.need("--token")?);
            change(&store()?, &ctx, |l| engine::timer_add(l, &ctx, kind, id, fires, token))
        }
        "wake-status" => {
            a.check(&["--token"], &[], 0, Some(0))?;
            let token = a.need("--token")?.trim().to_string();
            Ok(Output::Json(engine::wake_status(&store()?.read()?, &token)))
        }
        "timers-clear" => {
            a.check(&[], &[], 0, Some(0))?;
            change(&store()?, &ctx, |l| engine::timers_clear(l, &ctx))
        }
        "size" => {
            a.check(&[], &[], 1, Some(1))?;
            let id = a.pos[1].trim();
            if !ledger::safe_id(id) {
                return Err(Fail::Usage(format!("agent id {id:?}: use letters, digits, - and _ only")));
            }
            let l = store()?.read()?;
            let found = size::projects_root().and_then(|r| size::find(&r, &l.session_id, id));
            let agent = l.agent(id);
            let (mode, why) = size::mode(agent, found.as_ref().map(|f| f.1), l.run.redispatch_over_bytes);
            Ok(Output::Json(json!({
                "id": id, "path": found.as_ref().map(|f| f.0.display().to_string()), "bytes": found.as_ref().map(|f| f.1),
                "mode": mode, "why": why, "in_ledger": agent.is_some(),
            })))
        }
        "run-set" => {
            a.check(&[], &[], 2, Some(2))?;
            change(&store()?, &ctx, |l| engine::run_set(l, &ctx, &a.pos[1], &a.pos[2]))
        }
        "new-night" => {
            a.check(&[], &[], 0, Some(0))?;
            change(&store()?, &ctx, |l| engine::new_night(l, &ctx))
        }
        "show" => {
            a.check(&[], &["--json"], 0, Some(0))?;
            let s = store()?;
            let l = s.read()?;
            Ok(Output::Text(if a.flag("--json") { l.to_json() } else { engine::summary(&l, &s.path.display().to_string()) }))
        }
        "log" => {
            a.check(&[], &[], 1, None)?;
            let text = a.pos[1..].join(" ");
            if text.trim().is_empty() {
                return Err(Fail::Usage("log: the text is empty".into()));
            }
            change(&store()?, &ctx, |l| {
                l.push_log(&ctx.now_str(), text.trim());
                Ok((json!({ "ok": true }), true))
            })
        }
        "toast" => {
            a.check(&[], &[], 2, None)?;
            let title = a.pos[1].trim().to_string();
            let body = a.pos[2..].join(" ").trim().to_string();
            let mut warnings: Vec<String> = Vec::new();
            let (shown, how) = match toast::deliver(&title, &body) {
                Delivery::Sent(h) => (true, h),
                Delivery::NotSetUp => {
                    warnings.push("toasts are not set up on this PC: run `lw toast-setup` once".into());
                    (false, "not set up".to_string())
                }
                Delivery::Failed(e) => {
                    warnings.push(format!("the toast failed: {e}"));
                    (false, "failed".to_string())
                }
            };
            let msg = format!("toast{}: {title}: {body}", if shown { "" } else { " (not shown)" });
            let logged = match path() {
                Ok(p) if p.exists() => Store::new(p)
                    .update(|l| {
                        l.push_log(&ctx.now_str(), msg);
                        l.updated = ctx.now_str();
                        Ok(((), true))
                    })
                    .map_err(|e| format!("not logged: {e}")),
                Ok(p) => Err(format!("not logged: no ledger at {}", p.display())),
                Err(Fail::Run(e) | Fail::Usage(e)) => Err(format!("not logged: {e}")),
            };
            if let Err(e) = &logged {
                warnings.push(e.clone());
            }
            for w in &warnings {
                eprintln!("lw: warning: {w}");
            }
            Ok(Output::Json(json!({ "shown": shown, "delivery": how, "logged": logged.is_ok(), "warnings": warnings })))
        }
        "toast-setup" => {
            a.check(&[], &["--remove"], 0, Some(0))?;
            let did = if a.flag("--remove") { toast::unregister()? } else { toast::register()? };
            Ok(Output::Json(json!({ "ok": true, "did": did, "app_id": toast::APP_ID })))
        }
        "migrate-v1" => {
            a.check(&[], &[], 1, Some(1))?;
            let src = std::path::PathBuf::from(&a.pos[1]);
            let p = path()?;
            if std::fs::canonicalize(&src).ok().is_some_and(|s| std::fs::canonicalize(&p).ok().is_some_and(|d| d == s)) {
                return Err(Fail::Usage("the version 2 ledger must be a new file, not the version 1 file".into()));
            }
            let text = std::fs::read_to_string(&src).map_err(|e| format!("could not read {}: {e}", src.display()))?;
            let sid = ledger::session_id(a.opt("--session")).unwrap_or_default();
            let host = std::env::var(ledger::HOST_SESSION_ENV).unwrap_or_default().trim().to_string();
            let (mut l, notes) = engine::migrate_v1(&text, &ctx, &src.display().to_string(), &sid, &host)?;
            l.updated = ctx.now_str();
            Store::new(p.clone()).create(&l)?;
            Ok(Output::Json(json!({ "ok": true, "ledger": p.display().to_string(), "agents": l.agents.len(), "notes": notes })))
        }
        other => Err(Fail::Usage(format!("unknown command {other:?}"))),
    }
}
