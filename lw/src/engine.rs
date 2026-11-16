//! Every decision lw makes, as functions on a ledger. No files, no clock: the caller passes `Ctx` and transcript sizes.
//! Each returns its JSON output and whether the ledger changed.
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};

use crate::clock::{self, Ctx};
use crate::ledger::{Agent, Kind, Ledger, Reason, Risk, State, Timer, TimerKind, Wake, safe_id, word};
use crate::reset;
use crate::size;

pub type Out = Result<(Value, bool), String>;

/// Usage as `get_usage` reports it.
#[derive(Clone, Copy, Debug)]
pub struct Usage {
    pub five_pct: u32,
    pub five_resets_at: DateTime<Utc>,
    pub weekly: Option<(u32, DateTime<Utc>)>,
}

/// Two reset times this close are the same reset (`get_usage` milliseconds drift between calls).
const SAME_RESET_SECS: i64 = 120;

fn log(l: &mut Ledger, ctx: &Ctx, msg: impl Into<String>) {
    l.push_log(&ctx.now_str(), msg);
}

fn stored(ctx: &Ctx, s: &str) -> Option<DateTime<Utc>> {
    clock::parse_instant(s, &ctx.zone).ok()
}

pub fn default_resume(handoff: Option<&str>) -> String {
    format!(
        "Resume from {}. Skip steps marked done; re-check any in-flight operation before retrying it.",
        handoff.filter(|h| !h.trim().is_empty()).unwrap_or("HANDOFF.md")
    )
}

/// Explore and Plan agents cannot be resumed by id.
pub fn always_redispatch(agent_type: &str) -> bool {
    agent_type.trim().eq_ignore_ascii_case("explore") || agent_type.trim().eq_ignore_ascii_case("plan")
}

fn usage_text(u: &Usage) -> String {
    match u.weekly {
        Some((w, _)) => format!("5-hour {}%, weekly {w}%", u.five_pct),
        None => format!("5-hour {}%", u.five_pct),
    }
}

fn project_suffix(l: &Ledger) -> String {
    if l.project.trim().is_empty() { String::new() } else { format!(" in {}", l.project.trim()) }
}

fn take_timers(l: &mut Ledger) -> Vec<String> {
    l.timers.drain(..).map(|t| t.id).collect()
}

/// A new wake with a fresh token. Every timer of an older wake is now useless: their ids are returned for the caller to delete,
/// and lw drops them from the ledger.
fn new_wake(
    l: &mut Ledger,
    ctx: &Ctx,
    reason: Reason,
    for_reset: DateTime<Utc>,
    wake_at: DateTime<Utc>,
    backup_at: Option<DateTime<Utc>>,
) -> (Wake, Vec<String>) {
    l.run.wake_seq += 1;
    let w = Wake {
        token: format!("w{}", l.run.wake_seq),
        reason,
        for_reset: ctx.fmt(for_reset),
        wake_at: ctx.fmt(wake_at),
        backup_at: backup_at.map(|b| ctx.fmt(b)),
        handled_at: None,
    };
    let deleted = take_timers(l);
    l.wake = Some(w.clone());
    (w, deleted)
}

fn arm_json(action: &str, w: &Wake, ctx: &Ctx, deleted: Vec<String>) -> Value {
    let cron = stored(ctx, &w.wake_at).map(|t| ctx.cron(t));
    json!({
        "action": action,
        "stop_launching": true,
        "token": w.token,
        "reason": word(&w.reason),
        "cron": cron,
        "wake_at": w.wake_at,
        "backup_at": w.backup_at,
        "for_reset": w.for_reset,
        "delete_timers": deleted,
    })
}

// ---------------------------------------------------------------------------------------------------------------- agents

/// The fields of `add` and `edit`. `None` is "not given on the command line"; an empty path clears that field.
#[derive(Default)]
pub struct Fields {
    pub name: Option<String>,
    pub agent_type: Option<String>,
    pub kind: Option<Kind>,
    pub risk: Option<Risk>,
    pub worktree: Option<String>,
    pub handoff: Option<String>,
    pub brief_file: Option<String>,
    pub resume: Option<String>,
}

pub struct NewAgent {
    pub id: String,
    /// `--replaces OLD_ID`: a re-dispatch of that agent.
    pub replaces: Option<String>,
    pub fields: Fields,
}

fn nonempty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Explore and Plan are always `redispatch`; the note says so when that overrides the kind asked for.
fn forced_kind(agent_type: &str, kind: Kind) -> (Kind, Value) {
    if always_redispatch(agent_type) && kind != Kind::Redispatch {
        (Kind::Redispatch, json!(format!("{} agents cannot be resumed by id: kind set to redispatch", agent_type.trim())))
    } else {
        (kind, Value::Null)
    }
}

/// `base` with the given fields laid over it. A resume text that was the default for the old handoff follows a new handoff.
fn overlay(base: &Agent, f: Fields) -> Result<(Agent, Value), String> {
    let mut a = base.clone();
    if let Some(n) = f.name {
        a.name = n.trim().to_string();
    }
    if a.name.is_empty() {
        return Err("--name is empty".into());
    }
    if let Some(t) = f.agent_type {
        a.agent_type = t.trim().to_string();
    }
    if let Some(k) = f.kind {
        a.kind = k;
    }
    if let Some(r) = f.risk {
        a.risk = r;
    }
    if f.worktree.is_some() {
        a.worktree = nonempty(f.worktree);
    }
    if f.handoff.is_some() {
        a.handoff = nonempty(f.handoff);
    }
    if f.brief_file.is_some() {
        a.brief_file = nonempty(f.brief_file);
    }
    a.resume = match (f.resume, &base.resume) {
        (Some(r), _) if !r.trim().is_empty() => r.trim().to_string(),
        (Some(_), _) => default_resume(a.handoff.as_deref()),
        (None, old) if old.trim().is_empty() || *old == default_resume(base.handoff.as_deref()) => default_resume(a.handoff.as_deref()),
        (None, old) => old.clone(),
    };
    let (kind, note) = forced_kind(&a.agent_type, a.kind);
    a.kind = kind;
    Ok((a, note))
}

pub fn add(l: &mut Ledger, ctx: &Ctx, a: NewAgent) -> Out {
    if !safe_id(&a.id) {
        return Err(format!("agent id {:?}: use letters, digits, - and _ only", a.id));
    }
    if l.agent(&a.id).is_some() {
        return Err(format!("agent {} is already in the ledger", a.id));
    }
    let old = match a.replaces.as_deref().map(str::trim) {
        Some(r) => Some(l.agent(r).cloned().ok_or_else(|| format!("--replaces: no agent {r:?} in the ledger"))?),
        None => None,
    };
    let f = a.fields;
    // A new agent starts from nothing (every required field given); a re-dispatch starts from the old agent, keeping its
    // resume count and last resume so the per-agent cap and the herd guard still see them.
    let base = match &old {
        Some(o) => Agent { id: a.id.clone(), state: State::Running, last_failure: None, ..o.clone() },
        None => {
            let need = |v: &Option<String>, k: &str| v.as_ref().map(|_| ()).ok_or_else(|| format!("{k} is required"));
            need(&f.name, "--name")?;
            need(&f.agent_type, "--agent-type")?;
            Agent {
                id: a.id.clone(),
                name: String::new(),
                agent_type: String::new(),
                kind: f.kind.ok_or("--kind is required")?,
                risk: f.risk.ok_or("--risk is required")?,
                worktree: None,
                handoff: None,
                brief_file: None,
                state: State::Running,
                resumes_tonight: 0,
                last_resumed: None,
                prev_resumed: None,
                last_failure: None,
                resume: String::new(),
            }
        }
    };
    let (agent, note) = overlay(&base, f)?;
    match &old {
        Some(o) => log(
            l,
            ctx,
            format!(
                "added {} ({}): {} {} {}, replacing {} (resumed {} time(s) tonight)",
                agent.id,
                agent.name,
                agent.agent_type,
                word(&agent.kind),
                word(&agent.risk),
                o.id,
                agent.resumes_tonight
            ),
        ),
        None => log(l, ctx, format!("added {} ({}): {} {} {}", agent.id, agent.name, agent.agent_type, word(&agent.kind), word(&agent.risk))),
    }
    if let Some(o) = &old {
        let from = o.state;
        l.agent_mut(&o.id)?.state = State::Done;
        log(l, ctx, format!("set {} ({}): {} -> done: redispatched as {}", o.id, o.name, word(&from), agent.id));
    }
    let out = json!({ "ok": true, "agent": agent, "note": note, "replaced": old.as_ref().map(|o| o.id.clone()) });
    l.agents.push(agent);
    Ok((out, true))
}

/// Fill or change an agent's fields (not its state: that is `set`). Migrated v1 agents arrive with an empty agent type.
pub fn edit(l: &mut Ledger, ctx: &Ctx, id: &str, f: Fields) -> Out {
    let before = l.agent(id).cloned().ok_or_else(|| format!("no agent {id:?} in the ledger"))?;
    let (after, note) = overlay(&before, f)?;
    let (b, a) = (serde_json::to_value(&before).map_err(|e| e.to_string())?, serde_json::to_value(&after).map_err(|e| e.to_string())?);
    let changed: Vec<String> = a.as_object().into_iter().flatten().filter(|(k, v)| b.get(k.as_str()) != Some(*v)).map(|(k, _)| k.clone()).collect();
    if !changed.is_empty() {
        *l.agent_mut(id)? = after.clone();
        log(l, ctx, format!("edited {id} ({}): {}", after.name, changed.join(", ")));
    }
    let any = !changed.is_empty();
    Ok((json!({ "ok": true, "id": id, "changed": changed, "agent": after, "note": note }), any))
}

pub fn set(l: &mut Ledger, ctx: &Ctx, id: &str, state: State, note: Option<&str>) -> Out {
    let a = l.agent_mut(id)?;
    let from = a.state;
    a.state = state;
    let name = a.name.clone();
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    log(l, ctx, format!("set {id} ({name}): {} -> {}{}", word(&from), word(&state), note.map(|n| format!(": {n}")).unwrap_or_default()));
    Ok((json!({ "ok": true, "id": id, "name": name, "from": word(&from), "state": word(&state) }), true))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LimitKind {
    FiveHour,
    Weekly,
}

/// Which limit a failure notice is about, or `None` when it is not a usage-limit notice.
pub fn classify(text: &str) -> Option<LimitKind> {
    let t = text.to_lowercase();
    if ["weekly", "week limit", "7-day", "7 day"].iter().any(|k| t.contains(k)) {
        return Some(LimitKind::Weekly);
    }
    const FIVE: [&str; 10] = [
        "5-hour",
        "5 hour",
        "five-hour",
        "five hour",
        "session limit",
        "usage limit",
        "hit your limit",
        "reached your limit",
        "limit reached",
        "limit will reset",
    ];
    FIVE.iter().any(|k| t.contains(k)).then_some(LimitKind::FiveHour)
}

pub fn stopped(l: &mut Ledger, ctx: &Ctx, id: &str, text: &str) -> Out {
    let kind = classify(text).ok_or("not a recognised usage-limit message (record the state with lw set instead)")?;
    let reset = reset::parse_reset(text, ctx).ok();
    let refail = Duration::minutes(i64::from(l.run.herd_refail_min));
    let (state, label) = match kind {
        LimitKind::Weekly => (State::StoppedByWeekly, "weekly"),
        LimitKind::FiveHour => (State::StoppedByLimit, "5-hour"),
    };
    let failure = format!("{label} limit at {}{}", ctx.now_str(), reset.map(|r| format!(", resets {}", ctx.fmt(r))).unwrap_or_default());
    let reset_json = reset.map(|r| reset::schedule(r, ctx).json(ctx));
    let a = l.agent_mut(id)?;
    let name = a.name.clone();
    a.last_failure = Some(failure);
    // Only a running agent can have stopped. A late or repeated notice must not revive a finished, paused or parked agent.
    if a.state != State::Running {
        let current = a.state;
        log(l, ctx, format!("stopped notice for {id} ({name}): {label} limit; ignored, it is {}", word(&current)));
        return Ok((
            json!({
                "id": id, "name": name, "state": word(&current), "limit": label, "reset": reset_json,
                "ignored": true, "herd_tripped": false, "toast": Value::Null,
            }),
            true,
        ));
    }
    let since_resume = a.last_resumed.as_deref().and_then(|s| stored(ctx, s)).map(|t| ctx.now - t);
    let herd = since_resume.is_some_and(|d| d >= Duration::zero() && d < refail);
    a.state = state;
    log(l, ctx, format!("stopped {id} ({name}): {label} limit"));
    let mut toast = Value::Null;
    if herd {
        let mins = since_resume.map(|d| d.num_minutes()).unwrap_or(0);
        l.run.herd_tripped = true;
        log(l, ctx, format!("herd guard tripped: {id} failed on the limit {mins} min after it was resumed"));
        toast =
            json!(format!("Herd guard: {name} failed on the limit again {mins} min after it was resumed. Nothing more is resumed until you look (lw show)."));
    }
    Ok((
        json!({
            "id": id, "name": name, "state": word(&state), "limit": label, "reset": reset_json,
            "ignored": false, "herd_tripped": herd, "toast": toast,
        }),
        true,
    ))
}

// ---------------------------------------------------------------------------------------------------------------- tick

/// When the window is open again, an unhandled wake due within this long is left to do the resuming:
/// `batch_gap_min`, at least this many minutes. So a pending batch wake keeps its gap.
const DUE_SOON_MIN: i64 = 5;

fn due_soon(l: &Ledger) -> Duration {
    Duration::minutes(i64::from(l.run.batch_gap_min).max(DUE_SOON_MIN))
}

/// The current wake, if it has not been handled yet.
fn pending(l: &Ledger) -> Option<&Wake> {
    l.wake.as_ref().filter(|w| w.handled_at.is_none())
}

/// A wake is armed once its timer is recorded: a cron one-shot for a wall or batch wake, any timer for a weekly notice.
fn has_timer(l: &Ledger, w: &Wake) -> bool {
    l.timers.iter().any(|t| t.token == w.token && (w.reason == Reason::Weekly || t.kind == TimerKind::CronOneshot))
}

/// Keep the pending wake. It is `armed` once its one-shot is recorded with `timer-add`; until then `arm` again with the same
/// token (nothing new is created), moved to 2 minutes from now if its time has already gone, so the cron line still fires.
fn keep_wake(l: &mut Ledger, ctx: &Ctx, weekly_warn: bool) -> Out {
    let w = pending(l).cloned().ok_or("no pending wake")?;
    if has_timer(l, &w) {
        let mut out = arm_json("armed", &w, ctx, Vec::new());
        out["weekly_warn"] = json!(weekly_warn);
        return Ok((out, false));
    }
    let soonest = reset::from_now(ctx, reset::SOONEST_MIN);
    let mut changed = false;
    if stored(ctx, &w.wake_at).is_none_or(|t| t < soonest) {
        let (wake_at, backup_at) = match (w.reason, stored(ctx, &w.for_reset)) {
            (Reason::Wall, Some(r)) => {
                let s = reset::schedule(r, ctx);
                (s.wake_at, Some(s.backup_at))
            }
            _ => (soonest, w.backup_at.as_ref().map(|_| reset::later(soonest, reset::BACKUP_AFTER_MIN - reset::WAKE_AFTER_MIN, &ctx.zone))),
        };
        if let Some(cur) = l.wake.as_mut() {
            cur.wake_at = ctx.fmt(wake_at);
            cur.backup_at = backup_at.map(|b| ctx.fmt(b));
        }
        changed = true;
        log(l, ctx, format!("{} has no one-shot recorded and its time has gone: wake moved to {}", w.token, ctx.fmt(wake_at)));
    }
    let w = pending(l).cloned().ok_or("no pending wake")?;
    let mut out = arm_json("arm", &w, ctx, Vec::new());
    out["weekly_warn"] = json!(weekly_warn);
    out["reissued"] = json!(true);
    out["toast"] = Value::Null;
    Ok((out, changed))
}

pub fn tick(l: &mut Ledger, ctx: &Ctx, u: &Usage) -> Out {
    let over = u.five_pct >= l.run.arm_at_pct;
    let weekly_warn = u.weekly.is_some_and(|(p, _)| p >= l.run.weekly_stop_pct);
    if l.run.herd_tripped {
        return Ok((
            json!({ "action": "herd-stopped", "stop_launching": true, "weekly_warn": weekly_warn,
                    "reason": "the herd guard has tripped: nothing is armed until a person clears it (lw run-set herd_tripped false, or lw new-night)" }),
            false,
        ));
    }
    let running = l.count(State::Running);
    let limited = l.count(State::StoppedByLimit);
    let room = l.run.max_concurrent as usize > running;
    // An unhandled wake for a reset that has already come is due: its timer is about to fire. Never replace it.
    let due = pending(l).is_some_and(|w| stored(ctx, &w.for_reset).is_some_and(|r| r <= ctx.now));

    // The window is open again but agents are stopped by the limit (a late notice): resume them shortly, not at the next reset.
    if limited > 0 && !over && room {
        let soon = pending(l).is_some_and(|w| stored(ctx, &w.wake_at).is_some_and(|t| t <= ctx.now + due_soon(l)));
        if due || soon {
            return keep_wake(l, ctx, weekly_warn);
        }
        let wake_at = reset::from_now(ctx, reset::SOONEST_MIN);
        let backup_at = reset::later(wake_at, reset::BACKUP_AFTER_MIN - reset::WAKE_AFTER_MIN, &ctx.zone);
        let (w, deleted) = new_wake(l, ctx, Reason::Batch, u.five_resets_at, wake_at, Some(backup_at));
        log(l, ctx, format!("armed {} (batch) at {}: the window is open, {limited} stopped by the limit; wake {}", w.token, usage_text(u), w.wake_at));
        let mut out = arm_json("arm", &w, ctx, deleted);
        out["weekly_warn"] = json!(weekly_warn);
        out["reissued"] = json!(false);
        out["toast"] = json!(format!("The window is open again: {limited} agent(s) stopped by the limit resume at {}.", ctx.hm(wake_at)));
        return Ok((out, true));
    }

    if running + limited > 0 && (over || limited > 0) {
        let s = reset::schedule(u.five_resets_at, ctx);
        let same = pending(l).is_some_and(|w| stored(ctx, &w.for_reset).is_some_and(|r| (r - s.reset).num_seconds().abs() <= SAME_RESET_SECS));
        if due || same {
            return keep_wake(l, ctx, weekly_warn);
        }
        let (w, deleted) = new_wake(l, ctx, Reason::Wall, s.reset, s.wake_at, Some(s.backup_at));
        log(
            l,
            ctx,
            format!(
                "armed {} (wall) at {}: wake {}, backup {}, for the {} reset",
                w.token,
                usage_text(u),
                w.wake_at,
                w.backup_at.clone().unwrap_or_default(),
                w.for_reset
            ),
        );
        let mut out = arm_json("arm", &w, ctx, deleted);
        out["weekly_warn"] = json!(weekly_warn);
        out["reissued"] = json!(false);
        out["toast"] = json!(format!(
            "Armed: wake at {} for the {} reset ({running} running, {limited} stopped by the limit). Launching nothing more this window.",
            ctx.hm(s.wake_at),
            ctx.hm(s.reset)
        ));
        return Ok((out, true));
    }
    if let Some((p, r)) = u.weekly.filter(|_| weekly_warn) {
        return Ok((
            json!({ "action": "weekly-warn", "stop_launching": true, "weekly_pct": p, "weekly_resets_at": ctx.fmt(r),
                    "toast": format!("Weekly usage at {p}%: launch nothing more. It resets {}.", ctx.day_hm(r)) }),
            false,
        ));
    }
    Ok((json!({ "action": "ok", "stop_launching": over }), false))
}

// ---------------------------------------------------------------------------------------------------------------- timers, wake status

pub fn timer_add(l: &mut Ledger, ctx: &Ctx, kind: TimerKind, id: &str, fires: DateTime<Utc>, token: &str) -> Out {
    if id.trim().is_empty() || token.trim().is_empty() {
        return Err("--id and --token must not be empty".into());
    }
    let warning = match &l.wake {
        Some(w) if w.token == token => Value::Null,
        Some(w) => json!(format!("token {token} is not the current wake ({})", w.token)),
        None => json!("there is no current wake"),
    };
    let t = Timer { kind, id: id.trim().to_string(), fires: ctx.fmt(fires), token: token.trim().to_string() };
    log(l, ctx, format!("timer {} {} fires {} for {}", word(&t.kind), t.id, t.fires, t.token));
    match l.timers.iter_mut().find(|x| x.id == t.id) {
        Some(existing) => *existing = t,
        None => l.timers.push(t),
    }
    Ok((json!({ "ok": true, "timers": l.timers.len(), "warning": warning }), true))
}

pub fn timers_clear(l: &mut Ledger, ctx: &Ctx) -> Out {
    let deleted = take_timers(l);
    let changed = !deleted.is_empty();
    if changed {
        log(l, ctx, format!("timers cleared: {}", deleted.join(", ")));
    }
    Ok((json!({ "delete_timers": deleted }), changed))
}

/// Read-only, for the backup task: it must not consume the wake.
pub fn wake_status(l: &Ledger, token: &str) -> Value {
    let w = l.wake.as_ref().filter(|w| w.token == token);
    let current = w.is_some();
    let handled = w.is_none_or(|w| w.handled_at.is_some());
    let pending = match w {
        Some(w) if w.reason == Reason::Weekly => l.count(State::StoppedByWeekly),
        _ => l.count(State::StoppedByLimit),
    };
    json!({
        "token": token, "handled": handled, "current": current,
        "host_session_id": l.host_session_id, "session_id": l.session_id,
        "pending_agents": pending,
        "reason": w.map(|w| word(&w.reason)), "wake_at": w.map(|w| w.wake_at.clone()),
        "current_token": l.wake.as_ref().map(|w| w.token.clone()),
    })
}

// ---------------------------------------------------------------------------------------------------------------- next

fn herd_stop(l: &mut Ledger, ctx: &Ctx, reason: String) -> Value {
    l.run.herd_tripped = true;
    let deleted = take_timers(l);
    let waiting = l.count(State::StoppedByLimit);
    log(l, ctx, format!("herd stop: {reason}; {waiting} waiting"));
    json!({
        "action": "herd-stop", "reason": reason, "waiting": waiting, "delete_timers": deleted,
        "toast": format!("Stopped resuming: {reason}. {waiting} agent(s){} wait for you (lw show).", project_suffix(l)),
    })
}

/// What to do when a timer with `token` fires. See the README for the order of the checks.
pub fn next(l: &mut Ledger, ctx: &Ctx, token: &str, u: &Usage, sizes: &dyn Fn(&Agent) -> Option<u64>) -> Out {
    // 1. Not the current wake, or already handled by the other timer.
    let reason = match &l.wake {
        Some(w) if w.token == token && w.handled_at.is_none() => w.reason,
        other => {
            return Ok((json!({ "action": "already-handled", "token": token, "current_token": other.as_ref().map(|w| w.token.clone()) }), false));
        }
    };
    // 2. Stamp it.
    if let Some(w) = l.wake.as_mut() {
        w.handled_at = Some(ctx.now_str());
    }
    if reason == Reason::Wall {
        l.run.wake_cycles += 1;
        l.run.cycle_base_pct = Some(u.five_pct);
        l.run.cycle_started_at = Some(ctx.now_str());
    }
    log(l, ctx, format!("wake {token} ({}) handled at {}", word(&reason), usage_text(u)));

    // A weekly wake only tells the person, unless the run allows resuming after a weekly limit.
    if reason == Reason::Weekly {
        if !l.run.resume_after_weekly {
            let waiting = l.count(State::StoppedByWeekly);
            let deleted = take_timers(l);
            l.wake = None;
            log(l, ctx, format!("weekly reset notice: {waiting} waiting, nothing resumed"));
            return Ok((
                json!({ "action": "weekly-reset", "waiting": waiting, "delete_timers": deleted,
                        "toast": format!("The weekly limit has reset. {waiting} agent(s){} wait for you; nothing was resumed.", project_suffix(l)) }),
                true,
            ));
        }
        let ids: Vec<String> = l
            .agents
            .iter_mut()
            .filter(|a| a.state == State::StoppedByWeekly)
            .map(|a| {
                a.state = State::StoppedByLimit;
                a.id.clone()
            })
            .collect();
        if !ids.is_empty() {
            log(l, ctx, format!("resume_after_weekly: {} back to stopped-by-limit", ids.join(", ")));
        }
    }

    // 3. Still limited: try again at the next reset, a few times.
    if u.five_pct >= 100 {
        if l.run.wake_cycles >= l.run.max_wake_cycles {
            let r = format!("still limited after {} wake cycles (max {})", l.run.wake_cycles, l.run.max_wake_cycles);
            return Ok((herd_stop(l, ctx, r), true));
        }
        let s = reset::schedule(u.five_resets_at, ctx);
        let (w, deleted) = new_wake(l, ctx, Reason::Wall, s.reset, s.wake_at, Some(s.backup_at));
        log(l, ctx, format!("still limited: armed {} (wall) for the {} reset", w.token, w.for_reset));
        let waiting = l.count(State::StoppedByLimit);
        let mut out = arm_json("still-limited", &w, ctx, deleted);
        out["toast"] = json!(format!("Still limited at {}: trying again at {} ({waiting} waiting).", ctx.hm(ctx.now), ctx.hm(s.wake_at)));
        return Ok((out, true));
    }

    // 4. Weekly limit: stop and tell the person, never wait it out.
    if let Some((wp, wr)) = u.weekly
        && (wp >= 100 || (wp >= l.run.weekly_stop_pct && !l.run.resume_after_weekly))
    {
        let moved: Vec<String> = l
            .agents
            .iter_mut()
            .filter(|a| a.state == State::StoppedByLimit)
            .map(|a| {
                a.state = State::StoppedByWeekly;
                a.id.clone()
            })
            .collect();
        let notify_at = reset::later(wr, reset::WEEKLY_NOTIFY_AFTER_MIN, &ctx.zone).max(reset::from_now(ctx, reset::SOONEST_MIN));
        let (w, deleted) = new_wake(l, ctx, Reason::Weekly, wr, notify_at, None);
        log(l, ctx, format!("weekly stop at {wp}%: {} stopped-by-weekly; notice {} at {}", moved.len(), w.token, w.wake_at));
        return Ok((
            json!({
                "action": "weekly-stop", "weekly_pct": wp, "weekly_resets_at": ctx.fmt(wr), "notify_at": w.wake_at, "cron": ctx.cron(notify_at),
                "token": w.token, "agents": moved, "delete_timers": deleted,
                "toast": format!("Weekly usage at {wp}%: stopped. {} agent(s){} wait for you; the weekly limit resets {}.", moved.len(), project_suffix(l), ctx.day_hm(wr)),
            }),
            true,
        ));
    }

    // 5. Herd guard. The rise check is for a resume burst right after the wall wake, so it applies only within
    // `herd_window_min` of it. Later in a long cycle, ordinary work raises usage: a batch wake re-bases instead of comparing.
    // A ledger without `cycle_started_at` (written before it existed) counts as past the window.
    let in_window = l
        .run
        .cycle_started_at
        .as_deref()
        .and_then(|s| stored(ctx, s))
        .is_some_and(|started| ctx.now - started < Duration::minutes(i64::from(l.run.herd_window_min)));
    let herd = if l.run.herd_tripped {
        Some("a resumed agent failed on the limit again".to_string())
    } else if in_window
        && let Some(base) = l.run.cycle_base_pct
        && u.five_pct > base.saturating_add(l.run.herd_rise_pct)
    {
        Some(format!("5-hour usage rose from {base}% to {}% since the wake (more than {} points)", u.five_pct, l.run.herd_rise_pct))
    } else if l.run.wake_cycles > l.run.max_wake_cycles {
        Some(format!("{} wake cycles (max {})", l.run.wake_cycles, l.run.max_wake_cycles))
    } else {
        None
    };
    if let Some(r) = herd {
        return Ok((herd_stop(l, ctx, r), true));
    }
    if reason == Reason::Batch && !in_window && l.run.cycle_base_pct != Some(u.five_pct) {
        let from = l.run.cycle_base_pct.map(|b| format!("{b}%")).unwrap_or_else(|| "none".into());
        l.run.cycle_base_pct = Some(u.five_pct);
        log(l, ctx, format!("past the {} min herd window: cycle base {from} -> {}%", l.run.herd_window_min, u.five_pct));
    }

    // 6. Agents a person must decide on.
    let max_resumes = l.run.max_resumes_per_agent;
    let mut needs_human = Vec::new();
    for a in l.agents.iter_mut().filter(|a| a.state == State::StoppedByLimit) {
        let why = if a.risk == Risk::Irreversible {
            Some("irreversible: it pushes, deletes, sends, deploys or buys".to_string())
        } else if a.resumes_tonight >= max_resumes {
            Some(format!("already resumed {} time(s) tonight (max {max_resumes})", a.resumes_tonight))
        } else {
            None
        };
        if let Some(why) = why {
            a.state = State::NeedsHuman;
            needs_human.push(json!({ "id": a.id, "name": a.name, "why": why }));
        }
    }
    for n in &needs_human {
        log(l, ctx, format!("needs-human {} ({}): {}", n["id"].as_str().unwrap_or(""), n["name"].as_str().unwrap_or(""), n["why"].as_str().unwrap_or("")));
    }
    let names = |v: &[Value]| v.iter().filter_map(|n| n["name"].as_str()).collect::<Vec<_>>().join(", ");

    // 8. Nothing left: clean up.
    let mut cands: Vec<(usize, Option<u64>)> =
        l.agents.iter().enumerate().filter(|(_, a)| a.state == State::StoppedByLimit).map(|(i, a)| (i, sizes(a))).collect();
    if cands.is_empty() {
        let deleted = take_timers(l);
        l.wake = None;
        log(l, ctx, "done: nothing left to resume; timers cleared");
        let toast = if needs_human.is_empty() {
            "Nothing left to resume; timers cleared.".to_string()
        } else {
            format!("Nothing left to resume. Needs you: {}.", names(&needs_human))
        };
        return Ok((json!({ "action": "done", "delete_timers": deleted, "needs_human": needs_human, "toast": toast }), true));
    }

    // 7. One batch: read-only first, then the smallest transcript, then ledger order.
    cands.sort_by_key(|&(i, bytes)| (l.agents[i].risk != Risk::ReadOnly, bytes.unwrap_or(0), i));
    let running = u32::try_from(l.count(State::Running)).unwrap_or(u32::MAX);
    let take = l.run.batch_size.min(l.run.max_concurrent.saturating_sub(running)) as usize;
    let over = l.run.redispatch_over_bytes;
    let now = ctx.now_str();
    let mut batch = Vec::new();
    for &(i, bytes) in cands.iter().take(take) {
        let a = &mut l.agents[i];
        let (mode, why) = size::mode(Some(a), bytes, over);
        a.state = State::Running;
        a.resumes_tonight += 1;
        a.prev_resumed = a.last_resumed.replace(now.clone());
        batch.push(json!({
            "id": a.id, "name": a.name, "mode": mode, "why": why, "resume": a.resume, "agent_type": a.agent_type,
            "brief_file": a.brief_file, "handoff": a.handoff, "worktree": a.worktree, "transcript_bytes": bytes,
        }));
    }
    for b in &batch {
        log(l, ctx, format!("resumed {} ({}) as {}", b["id"].as_str().unwrap_or(""), b["name"].as_str().unwrap_or(""), b["mode"].as_str().unwrap_or("")));
    }
    let remaining = cands.len() - batch.len();
    // No room at all: no batch wake (it would only poll every batch_gap_min). The tick after the next completion arms one.
    let waiting_for_room = if take == 0 { remaining } else { 0 };
    let (arm, deleted) = if remaining > 0 && take > 0 {
        let wake_at = reset::from_now(ctx, i64::from(l.run.batch_gap_min));
        let backup_at = reset::later(wake_at, reset::BACKUP_AFTER_MIN - reset::WAKE_AFTER_MIN, &ctx.zone);
        let (w, deleted) = new_wake(l, ctx, Reason::Batch, u.five_resets_at, wake_at, Some(backup_at));
        log(l, ctx, format!("armed {} (batch) at {}: {remaining} waiting", w.token, w.wake_at));
        (json!({ "token": w.token, "cron": ctx.cron(wake_at), "wake_at": w.wake_at, "backup_at": w.backup_at }), deleted)
    } else {
        if waiting_for_room > 0 {
            log(l, ctx, format!("no room: {running} running (max {}); {waiting_for_room} wait for an agent to finish", l.run.max_concurrent));
        }
        (Value::Null, take_timers(l))
    };
    // An empty batch resumed nothing: no toast (it would only be noise). `waiting_for_room` and `needs_human` still say why.
    let toast = if batch.is_empty() {
        Value::Null
    } else {
        let mut t = format!("Resumed {}: {}.", batch.len(), names(&batch));
        if remaining > 0 {
            t.push_str(&format!(
                " {remaining} waiting; next batch at {}.",
                arm["wake_at"].as_str().and_then(|s| stored(ctx, s)).map(|t| ctx.hm(t)).unwrap_or_default()
            ));
        }
        if !needs_human.is_empty() {
            t.push_str(&format!(" Needs you: {}.", names(&needs_human)));
        }
        json!(t)
    };
    Ok((
        json!({
            "action": "resume", "batch": batch, "needs_human": needs_human, "remaining": remaining, "waiting_for_room": waiting_for_room,
            "arm": arm, "delete_timers": deleted, "toast": toast,
        }),
        true,
    ))
}

/// Batch items the caller could not send (a send or launch failed on the limit): back to `stopped-by-limit` as if `next` had never
/// resumed them. Their resume count and `last_resumed` are put back and the herd guard is not tripped, since they never ran.
/// The interrupted batch counts as a wake cycle, so a window that keeps refusing sends ends at the cycle cap instead of looping.
/// lw has no usage figures here: the caller runs `tick` next, which arms the right wake.
pub fn unsent(l: &mut Ledger, ctx: &Ctx, ids: &[String]) -> Out {
    let mut ids_once: Vec<&str> = Vec::new();
    for id in ids.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !ids_once.contains(&id) {
            ids_once.push(id);
        }
    }
    let ids = ids_once;
    if ids.is_empty() {
        return Err("give the ids of the batch items that were not sent".into());
    }
    let latest = l.agents.iter().filter_map(|a| a.last_resumed.as_deref().and_then(|s| stored(ctx, s))).max();
    for id in &ids {
        let a = l.agent(id).ok_or_else(|| format!("no agent {id:?} in the ledger"))?;
        let resumed = a.last_resumed.as_deref().and_then(|s| stored(ctx, s));
        if a.state != State::Running || resumed.is_none() || resumed != latest {
            return Err(format!(
                "{id} is not an item of the latest resume batch (it is {}, last resumed {})",
                word(&a.state),
                a.last_resumed.as_deref().unwrap_or("never")
            ));
        }
    }
    let mut out = Vec::new();
    for id in &ids {
        let a = l.agent_mut(id)?;
        a.state = State::StoppedByLimit;
        a.resumes_tonight = a.resumes_tonight.saturating_sub(1);
        a.last_resumed = a.prev_resumed.take();
        let (name, n) = (a.name.clone(), a.resumes_tonight);
        log(l, ctx, format!("unsent {id} ({name}): back to stopped-by-limit, resumed {n} time(s) tonight"));
        out.push(json!({ "id": id, "name": name }));
    }
    l.run.wake_cycles += 1;
    log(l, ctx, format!("interrupted batch: {} unsent; counts as wake cycle {}", out.len(), l.run.wake_cycles));
    Ok((json!({ "ok": true, "unsent": out, "wake_cycles": l.run.wake_cycles, "then": "tick" }), true))
}

// ---------------------------------------------------------------------------------------------------------------- run settings

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(format!("{v:?} is not true or false")),
    }
}

fn parse_num(v: &str, min: u64, max: u64) -> Result<u64, String> {
    let n: u64 = v.trim().parse().map_err(|_| format!("{v:?} is not a whole number"))?;
    if n < min || n > max {
        return Err(format!("{n} is outside {min}..={max}"));
    }
    Ok(n)
}

pub const RUN_KEYS: &str = "unattended, max_concurrent, batch_size, batch_gap_min, max_wake_cycles, wake_cycles, max_resumes_per_agent, arm_at_pct, herd_rise_pct, herd_refail_min, herd_window_min, weekly_stop_pct, redispatch_over_bytes, resume_after_weekly, cycle_base_pct, herd_tripped";

pub fn run_set(l: &mut Ledger, ctx: &Ctx, key: &str, value: &str) -> Out {
    let key = key.trim().replace('-', "_");
    let before = serde_json::to_value(&l.run).map_err(|e| e.to_string())?.get(&key).cloned();
    let r = &mut l.run;
    let n32 = |min: u64, max: u64| parse_num(value, min, max).map(|n| n as u32);
    let res: Result<(), String> = (|| {
        match key.as_str() {
            "unattended" => r.unattended = parse_bool(value)?,
            "resume_after_weekly" => r.resume_after_weekly = parse_bool(value)?,
            "herd_tripped" => r.herd_tripped = parse_bool(value)?,
            "max_concurrent" => r.max_concurrent = n32(1, 64)?,
            "batch_size" => r.batch_size = n32(1, 64)?,
            "batch_gap_min" => r.batch_gap_min = n32(1, 1440)?,
            "max_wake_cycles" => r.max_wake_cycles = n32(1, 100)?,
            "wake_cycles" => r.wake_cycles = n32(0, 1000)?,
            "max_resumes_per_agent" => r.max_resumes_per_agent = n32(0, 100)?,
            "arm_at_pct" => r.arm_at_pct = n32(1, 100)?,
            "herd_rise_pct" => r.herd_rise_pct = n32(1, 100)?,
            "herd_refail_min" => r.herd_refail_min = n32(0, 1440)?,
            "herd_window_min" => r.herd_window_min = n32(1, 1440)?,
            "weekly_stop_pct" => r.weekly_stop_pct = n32(1, 100)?,
            "redispatch_over_bytes" => r.redispatch_over_bytes = parse_num(value, 1, u64::MAX)?,
            "cycle_base_pct" => {
                r.cycle_base_pct = if matches!(value.trim().to_ascii_lowercase().as_str(), "null" | "none" | "") { None } else { Some(n32(0, 1000)?) }
            }
            "wake_seq" | "cycle_started_at" => return Err(format!("{key} is kept by lw itself")),
            _ => return Err(format!("unknown run key {key:?}; one of: {RUN_KEYS}")),
        }
        Ok(())
    })();
    res.map_err(|e| format!("run-set {key}: {e}"))?;
    let after = serde_json::to_value(&l.run).map_err(|e| e.to_string())?.get(&key).cloned();
    let changed = before != after;
    if changed {
        log(l, ctx, format!("run {key}: {} -> {}", before.clone().unwrap_or(Value::Null), after.clone().unwrap_or(Value::Null)));
    }
    Ok((json!({ "ok": true, "key": key, "from": before, "value": after }), changed))
}

/// Start a new night: wake cycles, the herd guard and every agent's resume count go back to zero.
pub fn new_night(l: &mut Ledger, ctx: &Ctx) -> Out {
    l.run.wake_cycles = 0;
    l.run.cycle_base_pct = None;
    l.run.cycle_started_at = None;
    l.run.herd_tripped = false;
    for a in &mut l.agents {
        a.resumes_tonight = 0;
    }
    log(l, ctx, "new night: wake cycles, herd guard and resume counts reset");
    Ok((json!({ "ok": true }), true))
}

// ---------------------------------------------------------------------------------------------------------------- migrate, show

/// Convert a version 1 ledger (`{"agents":[{id, stream, worktree, state, resume}]}` or a bare list) into version 2.
pub fn migrate_v1(text: &str, ctx: &Ctx, source: &str, session_id: &str, host_session_id: &str) -> Result<(Ledger, Vec<String>), String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("the v1 file is not valid JSON: {e}"))?;
    if v.get("version").and_then(Value::as_u64) == Some(2) {
        return Err("that file is already a version 2 ledger".into());
    }
    let list = if v.is_array() { v.as_array() } else { v.get("agents").or_else(|| v.get("streams")).and_then(Value::as_array) }
        .ok_or("no agent list in the v1 file (expected a list, or an object with \"agents\")")?;
    let project = v.get("project").and_then(Value::as_str).unwrap_or("");
    let mut l = Ledger::new(session_id, host_session_id, project, &ctx.now_str());
    let mut notes = Vec::new();
    let s = |item: &Value, k: &str| item.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(String::from);
    for (n, item) in list.iter().enumerate() {
        let Some(id) = s(item, "id") else {
            notes.push(format!("entry {n}: no id, skipped"));
            continue;
        };
        if !safe_id(&id) || l.agent(&id).is_some() {
            notes.push(format!("entry {n}: id {id:?} is unusable or repeated, skipped"));
            continue;
        }
        let state_word = s(item, "state").unwrap_or_default();
        let state = crate::ledger::parse_word::<State>(&state_word, "state", crate::ledger::STATES).unwrap_or_else(|_| {
            notes.push(format!("{id}: state {state_word:?} is not a v2 state; set to needs-human"));
            State::NeedsHuman
        });
        let handoff = s(item, "handoff");
        l.agents.push(Agent {
            name: s(item, "stream").or_else(|| s(item, "name")).unwrap_or_else(|| id.clone()),
            agent_type: String::new(),
            kind: Kind::Resumable,
            risk: Risk::LocalWrites,
            worktree: s(item, "worktree"),
            resume: s(item, "resume").unwrap_or_else(|| default_resume(handoff.as_deref())),
            handoff,
            brief_file: None,
            state,
            resumes_tonight: 0,
            last_resumed: None,
            prev_resumed: None,
            last_failure: None,
            id,
        });
    }
    let n = l.agents.len();
    log(&mut l, ctx, format!("migrated from v1 {source}: {n} agent(s)"));
    for note in &notes {
        log(&mut l, ctx, format!("migrate: {note}"));
    }
    Ok((l, notes))
}

pub fn summary(l: &Ledger, path: &str) -> String {
    let r = &l.run;
    let yes = |b: bool| if b { "yes" } else { "no" };
    let mut o = String::new();
    o.push_str(&format!("ledger   {path}\n"));
    o.push_str(&format!("session  {}  (host {})\n", or_dash(&l.session_id), or_dash(&l.host_session_id)));
    o.push_str(&format!("project  {}\n", or_dash(&l.project)));
    o.push_str(&format!("updated  {}\n", or_dash(&l.updated)));
    o.push_str(&format!(
        "run      {}; up to {} at once; batches of {} every {} min; arm at {}%; weekly stop at {}%{}\n",
        if r.unattended { "unattended" } else { "attended" },
        r.max_concurrent,
        r.batch_size,
        r.batch_gap_min,
        r.arm_at_pct,
        r.weekly_stop_pct,
        if r.resume_after_weekly { " (resumes after weekly)" } else { "" }
    ));
    o.push_str(&format!(
        "guard    wake cycles {}/{}; rise > {} pts{} within {} min of the wall wake; refail < {} min; {} resumes per agent; redispatch over {} bytes; tripped: {}\n",
        r.wake_cycles,
        r.max_wake_cycles,
        r.herd_rise_pct,
        r.cycle_base_pct.map(|b| format!(" from {b}%")).unwrap_or_default(),
        r.herd_window_min,
        r.herd_refail_min,
        r.max_resumes_per_agent,
        r.redispatch_over_bytes,
        yes(r.herd_tripped)
    ));
    match &l.wake {
        Some(w) => o.push_str(&format!(
            "wake     {} ({}) for the {} reset: wake {}, backup {}, handled {}\n",
            w.token,
            word(&w.reason),
            w.for_reset,
            w.wake_at,
            w.backup_at.as_deref().unwrap_or("-"),
            w.handled_at.as_deref().unwrap_or("no")
        )),
        None => o.push_str("wake     none\n"),
    }
    if l.timers.is_empty() {
        o.push_str("timers   none\n");
    }
    for t in &l.timers {
        o.push_str(&format!("timer    {} {} fires {} ({})\n", word(&t.kind), t.id, t.fires, t.token));
    }
    let mut counts: Vec<String> = Vec::new();
    for s in [State::Running, State::StoppedByLimit, State::StoppedByWeekly, State::NeedsHuman, State::PausedByUser, State::Done] {
        let n = l.count(s);
        if n > 0 {
            counts.push(format!("{n} {}", word(&s)));
        }
    }
    o.push_str(&format!("agents   {}{}\n", l.agents.len(), if counts.is_empty() { String::new() } else { format!(": {}", counts.join(", ")) }));
    for a in &l.agents {
        o.push_str(&format!(
            "  {:<17} {}  {}  [{} {} {}] resumed {}x{}\n",
            word(&a.state),
            a.id,
            a.name,
            or_dash(&a.agent_type),
            word(&a.kind),
            word(&a.risk),
            a.resumes_tonight,
            a.last_failure.as_deref().map(|f| format!("; last failure: {f}")).unwrap_or_default()
        ));
    }
    let tail = l.log.len().saturating_sub(8);
    o.push_str(&format!("log      {} line(s), newest last\n", l.log.len()));
    for e in &l.log[tail..] {
        o.push_str(&format!("  {}  {}\n", e.at, e.msg));
    }
    o
}

fn or_dash(s: &str) -> &str {
    if s.trim().is_empty() { "-" } else { s }
}

#[cfg(test)]
mod tests;
