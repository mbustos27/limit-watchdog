use super::*;
use crate::clock::{Zone, named, parse_instant};
use crate::ledger::{Agent, Kind, Ledger, Reason, Risk, State, TimerKind};

fn la() -> Zone {
    named("America/Los_Angeles").unwrap()
}
fn ctx(now: &str) -> Ctx {
    Ctx { now: parse_instant(now, &la()).unwrap(), zone: la() }
}
fn t(s: &str) -> DateTime<Utc> {
    parse_instant(s, &la()).unwrap()
}
fn agent(id: &str, risk: Risk, state: State) -> Agent {
    Agent {
        id: id.into(),
        name: format!("{id}-stream"),
        agent_type: "general-purpose".into(),
        kind: Kind::Resumable,
        risk,
        worktree: Some(format!("C:\\wt\\{id}")),
        handoff: Some(format!("C:\\wt\\{id}\\HANDOFF.md")),
        brief_file: Some(format!("C:\\wt\\{id}\\BRIEF.md")),
        state,
        resumes_tonight: 0,
        last_resumed: None,
        prev_resumed: None,
        last_failure: None,
        resume: default_resume(Some("HANDOFF.md")),
    }
}
fn cron(l: &mut Ledger, at: &str, id: &str, token: &str) {
    timer_add(l, &ctx(at), TimerKind::CronOneshot, id, t("2026-11-15T23:59:00-08:00"), token).unwrap();
}
fn ledger(agents: Vec<Agent>) -> Ledger {
    let mut l = Ledger::new("sess", "local_host", "proj", "2026-11-15T10:00:00-08:00");
    l.run.unattended = true;
    l.agents = agents;
    l
}
fn usage(five: u32, reset: &str) -> Usage {
    Usage { five_pct: five, five_resets_at: t(reset), weekly: None }
}
fn weekly(five: u32, reset: &str, w: u32, wreset: &str) -> Usage {
    Usage { five_pct: five, five_resets_at: t(reset), weekly: Some((w, t(wreset))) }
}
fn small(_: &Agent) -> Option<u64> {
    Some(1_000)
}
const R1: &str = "2026-11-15T15:30:00.194-08:00";
const R2: &str = "2026-11-15T20:30:00-08:00";

/// Arm a wall wake for R1 from a running agent at 85 % and record its two timers; returns the token.
fn armed(l: &mut Ledger) -> String {
    let (out, _) = tick(l, &ctx("2026-11-15T14:00:00-08:00"), &usage(85, R1)).unwrap();
    assert_eq!(out["action"], "arm", "{out}");
    let token = out["token"].as_str().unwrap().to_string();
    let c = ctx("2026-11-15T14:00:10-08:00");
    timer_add(l, &c, TimerKind::CronOneshot, "cron-1", t("2026-11-15T15:34:00-08:00"), &token).unwrap();
    timer_add(l, &c, TimerKind::DesktopTask, "task-1", t("2026-11-15T15:40:00-08:00"), &token).unwrap();
    token
}

// ------------------------------------------------------------------------------------------------ tick

#[test]
fn tick_arms_once_then_is_idempotent() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    let c = ctx("2026-11-15T14:00:00-08:00");
    let (out, changed) = tick(&mut l, &c, &usage(85, R1)).unwrap();
    assert!(changed);
    assert_eq!(out["action"], "arm");
    assert_eq!(out["stop_launching"], true);
    assert_eq!(out["token"], "w1");
    assert_eq!(out["cron"], "34 15 15 11 *");
    assert_eq!(out["wake_at"], "2026-11-15T15:34:00-08:00");
    assert_eq!(out["backup_at"], "2026-11-15T15:40:00-08:00");
    assert_eq!(out["for_reset"], "2026-11-15T15:30:00-08:00");
    assert!(out["toast"].as_str().unwrap().contains("15:34"));
    assert_eq!(out["reissued"], false);
    let snapshot = l.clone();
    // same reset (milliseconds drift), higher usage, but no one-shot recorded yet: arm again with the same token, nothing new
    let (again, changed) = tick(&mut l, &ctx("2026-11-15T14:20:00-08:00"), &usage(93, "2026-11-15T23:30:00.201Z")).unwrap();
    assert!(!changed);
    assert_eq!((again["action"].as_str(), again["token"].as_str(), again["reissued"].as_bool()), (Some("arm"), Some("w1"), Some(true)));
    assert_eq!(again["cron"], "34 15 15 11 *");
    assert_eq!(l, snapshot);
    // a Desktop task alone does not count
    timer_add(&mut l, &ctx("2026-11-15T14:21:00-08:00"), TimerKind::DesktopTask, "task-1", t("2026-11-15T15:40:00-08:00"), "w1").unwrap();
    assert_eq!(tick(&mut l, &ctx("2026-11-15T14:22:00-08:00"), &usage(93, R1)).unwrap().0["action"], "arm");
    // once the one-shot is recorded: armed, and nothing changes
    cron(&mut l, "2026-11-15T14:23:00-08:00", "cron-1", "w1");
    let snapshot = l.clone();
    let (again, changed) = tick(&mut l, &ctx("2026-11-15T14:30:00-08:00"), &usage(95, "2026-11-15T23:30:00.201Z")).unwrap();
    assert!(!changed);
    assert_eq!((again["action"].as_str(), again["token"].as_str(), again["cron"].as_str()), (Some("armed"), Some("w1"), Some("34 15 15 11 *")));
    assert_eq!(l, snapshot);
}

#[test]
fn a_reissued_arm_whose_time_has_gone_moves_to_two_minutes_from_now() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    tick(&mut l, &ctx("2026-11-15T15:00:00-08:00"), &usage(85, R1)).unwrap();
    // no timer recorded; 15:40 is past the 15:34 wake (and the reset): same token, new time
    let (out, changed) = tick(&mut l, &ctx("2026-11-15T15:40:00-08:00"), &usage(100, R1)).unwrap();
    assert!(changed);
    assert_eq!((out["action"].as_str(), out["token"].as_str()), (Some("arm"), Some("w1")));
    assert_eq!(out["wake_at"], "2026-11-15T15:42:00-08:00");
    assert_eq!(out["cron"], "42 15 15 11 *");
    assert_eq!(l.run.wake_seq, 1);
}

#[test]
fn tick_is_ok_below_the_threshold_and_never_arms_without_agents() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    let (out, changed) = tick(&mut l, &ctx("2026-11-15T14:00:00-08:00"), &usage(50, R1)).unwrap();
    assert_eq!((out["action"].as_str(), out["stop_launching"].as_bool(), changed), (Some("ok"), Some(false), false));
    let mut idle = ledger(vec![agent("a1", Risk::LocalWrites, State::Done)]);
    let (out, changed) = tick(&mut idle, &ctx("2026-11-15T14:00:00-08:00"), &usage(95, R1)).unwrap();
    assert_eq!((out["action"].as_str(), out["stop_launching"].as_bool(), changed), (Some("ok"), Some(true), false));
    assert!(idle.wake.is_none());
}

#[test]
fn a_limit_failure_with_the_window_open_arms_a_batch_two_minutes_out() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    let (out, changed) = tick(&mut l, &ctx("2026-11-15T14:00:00-08:00"), &usage(40, R1)).unwrap();
    assert!(changed);
    assert_eq!((out["action"].as_str(), out["reason"].as_str()), (Some("arm"), Some("batch")));
    assert_eq!(out["wake_at"], "2026-11-15T14:02:00-08:00");
    assert_eq!(out["cron"], "2 14 15 11 *");
    // with no room it waits for the reset instead (and the tick after a completion arms the batch)
    let mut full = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    full.run.max_concurrent = 1;
    full.agents.push(agent("r1", Risk::LocalWrites, State::Running));
    let (out, _) = tick(&mut full, &ctx("2026-11-15T14:00:00-08:00"), &usage(40, R1)).unwrap();
    assert_eq!((out["action"].as_str(), out["reason"].as_str()), (Some("arm"), Some("wall")));
}

#[test]
fn a_late_notice_after_the_reset_resumes_now_not_at_the_next_reset() {
    // armed for R1 (15:30) and handled at 15:34 with nothing to resume; then a notice from before the reset arrives at 15:50
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running), agent("a2", Risk::LocalWrites, State::Running)]);
    let w1 = armed(&mut l);
    let (_, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(1, R2), &small).unwrap();
    stopped(&mut l, &ctx("2026-11-15T15:50:00-08:00"), "a1", "You've hit your 5-hour limit \u{b7} resets 3:30pm").unwrap();
    let (out, _) = tick(&mut l, &ctx("2026-11-15T15:50:00-08:00"), &usage(3, R2)).unwrap();
    assert_eq!((out["action"].as_str(), out["reason"].as_str(), out["wake_at"].as_str()), (Some("arm"), Some("batch"), Some("2026-11-15T15:52:00-08:00")));
    // a pending wall wake for the next reset is replaced by the batch
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    let (wall, _) = tick(&mut l, &ctx("2026-11-15T16:00:00-08:00"), &usage(85, R2)).unwrap();
    cron(&mut l, "2026-11-15T16:00:00-08:00", "cron-9", wall["token"].as_str().unwrap());
    stopped(&mut l, &ctx("2026-11-15T16:10:00-08:00"), "a1", "usage limit reached").unwrap();
    let (out, _) = tick(&mut l, &ctx("2026-11-15T16:10:00-08:00"), &usage(10, R2)).unwrap();
    assert_eq!((out["action"].as_str(), out["reason"].as_str()), (Some("arm"), Some("batch")));
    assert_eq!(out["delete_timers"], json!(["cron-9"]));
}

#[test]
fn a_pending_wake_due_soon_is_kept_when_the_window_is_open() {
    let mut l = ledger(vec![
        agent("a1", Risk::LocalWrites, State::StoppedByLimit),
        agent("a2", Risk::LocalWrites, State::StoppedByLimit),
        agent("a3", Risk::LocalWrites, State::StoppedByLimit),
    ]);
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    let w2 = out["arm"]["token"].as_str().unwrap().to_string();
    cron(&mut l, "2026-11-15T15:34:30-08:00", "cron-2", &w2);
    // a1 finishes: the batch wake at 15:46 keeps its gap
    set(&mut l, &ctx("2026-11-15T15:36:00-08:00"), "a1", State::Done, None).unwrap();
    let snapshot = l.clone();
    let (out, changed) = tick(&mut l, &ctx("2026-11-15T15:36:00-08:00"), &usage(5, R2)).unwrap();
    assert_eq!((out["action"].as_str(), out["token"].as_str(), changed), (Some("armed"), Some(w2.as_str()), false));
    assert_eq!(l, snapshot);
}

#[test]
fn tick_never_replaces_a_wake_whose_reset_has_come() {
    // w1 is for 15:30; at 15:32 a late notice comes in with every slot taken, and usage already shows the next reset
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    l.run.max_concurrent = 1;
    let w1 = armed(&mut l);
    l.agents.push(agent("a2", Risk::LocalWrites, State::StoppedByLimit));
    let snapshot = l.clone();
    let (out, changed) = tick(&mut l, &ctx("2026-11-15T15:32:00-08:00"), &usage(90, R2)).unwrap();
    assert_eq!((out["action"].as_str(), out["token"].as_str(), changed), (Some("armed"), Some(w1.as_str()), false));
    assert_eq!(l, snapshot);
    // with the window open and room, too
    l.run.max_concurrent = 4;
    let (out, _) = tick(&mut l, &ctx("2026-11-15T15:32:00-08:00"), &usage(1, R2)).unwrap();
    assert_eq!((out["action"].as_str(), out["token"].as_str()), (Some("armed"), Some(w1.as_str())));
    // the same time before the reset, a different reset replaces it
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    armed(&mut l);
    let (out, _) = tick(&mut l, &ctx("2026-11-15T15:00:00-08:00"), &usage(90, R2)).unwrap();
    assert_eq!((out["action"].as_str(), out["token"].as_str()), (Some("arm"), Some("w2")));
}

#[test]
fn tick_warns_on_weekly_usage() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    let (out, changed) = tick(&mut l, &ctx("2026-11-15T14:00:00-08:00"), &weekly(50, R1, 91, "2026-11-17T00:00:00-08:00")).unwrap();
    assert!(!changed);
    assert_eq!(out["action"], "weekly-warn");
    assert_eq!(out["stop_launching"], true);
    assert_eq!(out["weekly_pct"], 91);
    // when it also arms, the arm wins and carries the warning
    let (out, _) = tick(&mut l, &ctx("2026-11-15T14:00:00-08:00"), &weekly(85, R1, 91, "2026-11-17T00:00:00-08:00")).unwrap();
    assert_eq!(out["action"], "arm");
    assert_eq!(out["weekly_warn"], true);
}

#[test]
fn tick_arms_again_for_a_different_reset_and_drops_the_old_timers() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    armed(&mut l);
    // before R1 has come (after it, see tick_never_replaces_a_wake_whose_reset_has_come)
    let (out, changed) = tick(&mut l, &ctx("2026-11-15T15:10:00-08:00"), &usage(85, R2)).unwrap();
    assert!(changed);
    assert_eq!(out["action"], "arm");
    assert_eq!(out["token"], "w2");
    assert_eq!(out["delete_timers"], json!(["cron-1", "task-1"]));
    assert!(l.timers.is_empty());
}

// ------------------------------------------------------------------------------------------------ next: dedup, still limited

#[test]
fn next_dedups_the_second_timer_and_unknown_tokens() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    let token = armed(&mut l);
    let c = ctx("2026-11-15T15:34:05-08:00");
    let (first, changed) = next(&mut l, &c, &token, &usage(3, R2), &small).unwrap();
    assert!(changed);
    assert_eq!(first["action"], "resume");
    let snapshot = l.clone();
    // the Desktop backup with the same token, a little later
    let (second, changed) = next(&mut l, &ctx("2026-11-15T15:40:05-08:00"), &token, &usage(5, R2), &small).unwrap();
    assert!(!changed);
    assert_eq!(second["action"], "already-handled");
    let (other, changed) = next(&mut l, &c, "w99", &usage(5, R2), &small).unwrap();
    assert!(!changed);
    assert_eq!(other["action"], "already-handled");
    assert_eq!(l, snapshot);
}

#[test]
fn next_still_limited_rearms_until_the_cycle_cap_then_stops() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(100, R2), &small).unwrap();
    assert_eq!(out["action"], "still-limited");
    assert_eq!(out["token"], "w2");
    assert_eq!(out["for_reset"], "2026-11-15T20:30:00-08:00");
    assert_eq!(out["wake_at"], "2026-11-15T20:34:00-08:00");
    assert_eq!(out["delete_timers"], json!(["cron-1", "task-1"]));
    assert_eq!(l.run.wake_cycles, 1);
    assert_eq!(l.wake.as_ref().unwrap().reason, Reason::Wall);
    let (out, _) = next(&mut l, &ctx("2026-11-15T20:34:00-08:00"), "w2", &usage(100, "2026-11-16T01:30:00-08:00"), &small).unwrap();
    assert_eq!((out["action"].as_str(), out["token"].as_str()), (Some("still-limited"), Some("w3")));
    let (out, _) = next(&mut l, &ctx("2026-11-16T01:34:00-08:00"), "w3", &usage(100, "2026-11-16T06:30:00-08:00"), &small).unwrap();
    assert_eq!(out["action"], "herd-stop");
    assert!(out["reason"].as_str().unwrap().contains("3 wake cycles"), "{out}");
    assert!(l.run.herd_tripped);
    assert_eq!(l.agents[0].state, State::StoppedByLimit);
}

// ------------------------------------------------------------------------------------------------ next: weekly

#[test]
fn next_weekly_stop_marks_agents_and_sets_a_notice_wake() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit), agent("a2", Risk::ReadOnly, State::StoppedByLimit)]);
    let w1 = armed(&mut l);
    let (out, changed) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &weekly(2, R2, 93, "2026-11-16T23:45:00-08:00"), &small).unwrap();
    assert!(changed);
    assert_eq!(out["action"], "weekly-stop");
    assert_eq!(out["weekly_resets_at"], "2026-11-16T23:45:00-08:00");
    // reset + 15 lands on :00 -> :01
    assert_eq!(out["notify_at"], "2026-11-17T00:01:00-08:00");
    assert_eq!(out["agents"], json!(["a1", "a2"]));
    assert!(l.agents.iter().all(|a| a.state == State::StoppedByWeekly));
    let w = l.wake.as_ref().unwrap();
    assert_eq!((w.reason, w.token.as_str()), (Reason::Weekly, "w2"));
    // the notice wake only tells the person
    let (out, _) =
        next(&mut l, &ctx("2026-11-17T00:01:00-08:00"), "w2", &weekly(0, "2026-11-17T05:00:00-08:00", 0, "2026-11-23T23:45:00-08:00"), &small).unwrap();
    assert_eq!(out["action"], "weekly-reset");
    assert_eq!(out["waiting"], 2);
    assert!(l.wake.is_none());
    assert!(l.agents.iter().all(|a| a.state == State::StoppedByWeekly));
}

#[test]
fn weekly_at_100_stops_even_when_resuming_after_weekly_is_allowed() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    l.run.resume_after_weekly = true;
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &weekly(2, R2, 100, "2026-11-16T12:00:00-08:00"), &small).unwrap();
    assert_eq!(out["action"], "weekly-stop");
    // at 93 % with resume_after_weekly it carries on
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    l.run.resume_after_weekly = true;
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &weekly(2, R2, 93, "2026-11-16T12:00:00-08:00"), &small).unwrap();
    assert_eq!(out["action"], "resume");
}

// ------------------------------------------------------------------------------------------------ next: herd guard

#[test]
fn herd_stop_when_a_resumed_agent_fails_again() {
    let mut l = ledger(vec![
        agent("a1", Risk::LocalWrites, State::StoppedByLimit),
        agent("a2", Risk::LocalWrites, State::StoppedByLimit),
        agent("a3", Risk::LocalWrites, State::StoppedByLimit),
    ]);
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    assert_eq!(out["action"], "resume");
    let batch_token = out["arm"]["token"].as_str().unwrap().to_string();
    // a1 dies on the limit again 10 minutes after its resume
    let (st, _) = stopped(&mut l, &ctx("2026-11-15T15:44:00-08:00"), "a1", "You've hit your 5-hour limit \u{b7} resets 8:30pm").unwrap();
    assert_eq!(st["herd_tripped"], true);
    assert!(st["toast"].as_str().unwrap().contains("Herd guard"));
    assert!(l.run.herd_tripped);
    // the pending batch wake stops instead of resuming more
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:46:00-08:00"), &batch_token, &usage(10, R2), &small).unwrap();
    assert_eq!(out["action"], "herd-stop");
    assert!(out["reason"].as_str().unwrap().contains("failed on the limit again"));
    // and tick does not arm while the guard is tripped
    let (out, changed) = tick(&mut l, &ctx("2026-11-15T15:50:00-08:00"), &usage(85, R2)).unwrap();
    assert_eq!((out["action"].as_str(), changed), (Some("herd-stopped"), false));
}

#[test]
fn a_failure_long_after_a_resume_does_not_trip_the_guard() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    l.agents[0].last_resumed = Some("2026-11-15T14:00:00-08:00".into());
    let (st, _) = stopped(&mut l, &ctx("2026-11-15T14:45:00-08:00"), "a1", "5-hour limit reached").unwrap();
    assert_eq!(st["herd_tripped"], false);
    assert!(!l.run.herd_tripped);
}

#[test]
fn herd_stop_when_usage_rises_too_fast_since_the_wake() {
    let agents = (1..=5).map(|i| agent(&format!("a{i}"), Risk::LocalWrites, State::StoppedByLimit)).collect();
    let mut l = ledger(agents);
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(10, R2), &small).unwrap();
    assert_eq!(l.run.cycle_base_pct, Some(10));
    let w2 = out["arm"]["token"].as_str().unwrap().to_string();
    // 30 is exactly 20 points up: allowed
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:47:00-08:00"), &w2, &usage(30, R2), &small).unwrap();
    assert_eq!(out["action"], "resume");
    let w3 = out["arm"]["token"].as_str().unwrap().to_string();
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:59:00-08:00"), &w3, &usage(31, R2), &small).unwrap();
    assert_eq!(out["action"], "herd-stop");
    assert!(out["reason"].as_str().unwrap().contains("rose from 10% to 31%"), "{out}");
}

#[test]
fn the_rise_check_covers_only_the_herd_window_after_the_wall_wake() {
    let agents = (1..=6).map(|i| agent(&format!("a{i}"), Risk::LocalWrites, State::StoppedByLimit)).collect();
    let mut l = ledger(agents);
    l.run.max_concurrent = 8;
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(10, R2), &small).unwrap();
    assert_eq!((l.run.cycle_base_pct, l.run.cycle_started_at.as_deref()), (Some(10), Some("2026-11-15T15:34:00-08:00")));
    let w2 = out["arm"]["token"].as_str().unwrap().to_string();
    // just inside the 30-minute window, 31 % is still a burst
    let mut inside = l.clone();
    let (out, _) = next(&mut inside, &ctx("2026-11-15T16:03:59-08:00"), &w2, &usage(31, R2), &small).unwrap();
    assert_eq!(out["action"], "herd-stop", "{out}");
    // a longer window set by the person still covers 16:04
    let mut longer = l.clone();
    run_set(&mut longer, &ctx("2026-11-15T15:35:00-08:00"), "herd-window-min", "60").unwrap();
    let (out, _) = next(&mut longer, &ctx("2026-11-15T16:04:00-08:00"), &w2, &usage(45, R2), &small).unwrap();
    assert_eq!(out["action"], "herd-stop", "{out}");
    // 30 minutes after the wall wake, ordinary work has raised usage: no herd stop; the base moves to the current figure
    let (out, _) = next(&mut l, &ctx("2026-11-15T16:04:00-08:00"), &w2, &usage(45, R2), &small).unwrap();
    assert_eq!(out["action"], "resume", "{out}");
    assert_eq!((l.run.cycle_base_pct, l.run.herd_tripped), (Some(45), false));
    assert_eq!(l.run.cycle_started_at.as_deref(), Some("2026-11-15T15:34:00-08:00"));
    assert!(l.log.iter().any(|e| e.msg == "past the 30 min herd window: cycle base 10% -> 45%"));
    let w3 = out["arm"]["token"].as_str().unwrap().to_string();
    let (out, _) = next(&mut l, &ctx("2026-11-15T16:16:00-08:00"), &w3, &usage(70, R2), &small).unwrap();
    assert_eq!((out["action"].as_str(), l.run.cycle_base_pct), (Some("resume"), Some(70)));
    // so the run still meets the next limit: tick arms the wall wake instead of answering herd-stopped
    let (out, _) = tick(&mut l, &ctx("2026-11-15T16:30:00-08:00"), &usage(85, R2)).unwrap();
    assert_eq!((out["action"].as_str(), out["reason"].as_str()), (Some("arm"), Some("wall")));
    // a ledger written before cycle_started_at existed counts as past the window
    let mut old = ledger((1..=3).map(|i| agent(&format!("a{i}"), Risk::LocalWrites, State::StoppedByLimit)).collect());
    let w1 = armed(&mut old);
    let (out, _) = next(&mut old, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(10, R2), &small).unwrap();
    old.run.cycle_started_at = None;
    let (out, _) = next(&mut old, &ctx("2026-11-15T15:46:00-08:00"), out["arm"]["token"].as_str().unwrap(), &usage(45, R2), &small).unwrap();
    assert_eq!((out["action"].as_str(), old.run.cycle_base_pct), (Some("resume"), Some(45)));
}

#[test]
fn herd_stop_past_the_wake_cycle_cap() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    l.run.wake_cycles = 3;
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    assert_eq!(out["action"], "herd-stop");
    assert!(out["reason"].as_str().unwrap().contains("4 wake cycles"), "{out}");
    assert_eq!(out["delete_timers"], json!(["cron-1", "task-1"]));
    assert!(l.timers.is_empty());
    assert!(l.wake.as_ref().unwrap().handled_at.is_some());
}

// ------------------------------------------------------------------------------------------------ next: needs-human, batches

#[test]
fn irreversible_agents_and_the_resume_cap_go_to_a_person() {
    let mut capped = agent("a2", Risk::LocalWrites, State::StoppedByLimit);
    capped.resumes_tonight = 2;
    let mut l = ledger(vec![agent("a1", Risk::Irreversible, State::StoppedByLimit), capped, agent("a3", Risk::LocalWrites, State::StoppedByLimit)]);
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    assert_eq!(out["action"], "resume");
    let nh: Vec<&str> = out["needs_human"].as_array().unwrap().iter().map(|n| n["id"].as_str().unwrap()).collect();
    assert_eq!(nh, ["a1", "a2"]);
    assert_eq!(out["batch"].as_array().unwrap().len(), 1);
    assert_eq!(out["batch"][0]["id"], "a3");
    assert_eq!(l.agents[0].state, State::NeedsHuman);
    assert_eq!(l.agents[1].state, State::NeedsHuman);
    assert!(out["toast"].as_str().unwrap().contains("Needs you"));
}

#[test]
fn batches_go_read_only_first_then_smallest_and_respect_size_and_concurrency() {
    let mut explore = agent("e1", Risk::ReadOnly, State::StoppedByLimit);
    explore.agent_type = "Explore".into();
    explore.kind = Kind::Redispatch;
    let mut l = ledger(vec![
        agent("lw-small", Risk::LocalWrites, State::StoppedByLimit),
        agent("ro-big", Risk::ReadOnly, State::StoppedByLimit),
        agent("lw-big", Risk::LocalWrites, State::StoppedByLimit),
        agent("ro-small", Risk::ReadOnly, State::StoppedByLimit),
        explore,
        agent("busy1", Risk::LocalWrites, State::Running),
        agent("busy2", Risk::LocalWrites, State::Running),
        agent("busy3", Risk::LocalWrites, State::Running),
    ]);
    let sizes = |a: &Agent| -> Option<u64> {
        match a.id.as_str() {
            "lw-small" => Some(500),
            "ro-big" => Some(5_000_000),
            "lw-big" => Some(2_000_000),
            "ro-small" => Some(800),
            _ => Some(100),
        }
    };
    let w1 = armed(&mut l);
    // 3 running of max 4: one slot
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &sizes).unwrap();
    let ids: Vec<&str> = out["batch"].as_array().unwrap().iter().map(|b| b["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["e1"]);
    assert_eq!(out["batch"][0]["mode"], "redispatch");
    assert_eq!(out["remaining"], 4);
    assert_eq!(out["arm"]["token"], "w2");
    // gap 12 min: 15:46
    assert_eq!(out["arm"]["wake_at"], "2026-11-15T15:46:00-08:00");
    assert_eq!(out["arm"]["cron"], "46 15 15 11 *");
    assert_eq!(out["delete_timers"], json!(["cron-1", "task-1"]));
    assert_eq!(l.wake.as_ref().unwrap().reason, Reason::Batch);
    // the busy ones finish; the next batch takes two: read-only by size, then local writes
    for b in ["busy1", "busy2", "busy3", "e1"] {
        set(&mut l, &ctx("2026-11-15T15:40:00-08:00"), b, State::Done, None).unwrap();
    }
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:46:00-08:00"), "w2", &usage(8, R2), &sizes).unwrap();
    let modes: Vec<(&str, &str)> = out["batch"].as_array().unwrap().iter().map(|b| (b["id"].as_str().unwrap(), b["mode"].as_str().unwrap())).collect();
    assert_eq!(modes, [("ro-small", "resume"), ("ro-big", "redispatch")]);
    assert_eq!(out["remaining"], 2);
    assert_eq!(out["batch"][0]["resume"], default_resume(Some("HANDOFF.md")));
    assert_eq!(out["batch"][0]["brief_file"], "C:\\wt\\ro-small\\BRIEF.md");
    let ro_small = l.agent("ro-small").unwrap();
    assert_eq!((ro_small.state, ro_small.resumes_tonight), (State::Running, 1));
    assert_eq!(ro_small.last_resumed.as_deref(), Some("2026-11-15T15:46:00-08:00"));
    // a batch wake does not count as a wake cycle
    assert_eq!(l.run.wake_cycles, 1);
}

#[test]
fn a_full_house_resumes_nobody_and_waits_for_a_completion_instead_of_polling() {
    let mut l = ledger(vec![
        agent("a1", Risk::LocalWrites, State::StoppedByLimit),
        agent("r1", Risk::LocalWrites, State::Running),
        agent("r2", Risk::LocalWrites, State::Running),
        agent("r3", Risk::LocalWrites, State::Running),
        agent("r4", Risk::LocalWrites, State::Running),
    ]);
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    assert_eq!(out["action"], "resume");
    assert_eq!(out["batch"], json!([]));
    assert_eq!((out["remaining"].as_u64(), out["waiting_for_room"].as_u64()), (Some(1), Some(1)));
    assert_eq!(out["arm"], Value::Null);
    assert_eq!(out["delete_timers"], json!(["cron-1", "task-1"]));
    assert_eq!(l.run.wake_seq, 1, "no batch wake");
    // while all four still run, tick does not arm (the window is open, but there is no room)
    let (out, _) = tick(&mut l, &ctx("2026-11-15T15:40:00-08:00"), &usage(5, R2)).unwrap();
    assert_ne!(out["reason"], "batch");
    // an agent finishes: the tick after it arms the batch
    set(&mut l, &ctx("2026-11-15T15:50:00-08:00"), "r1", State::Done, None).unwrap();
    let (out, _) = tick(&mut l, &ctx("2026-11-15T15:50:00-08:00"), &usage(5, R2)).unwrap();
    assert_eq!((out["action"].as_str(), out["reason"].as_str(), out["wake_at"].as_str()), (Some("arm"), Some("batch"), Some("2026-11-15T15:52:00-08:00")));
}

#[test]
fn an_empty_batch_has_no_toast() {
    let mut l = ledger(vec![
        agent("a1", Risk::LocalWrites, State::StoppedByLimit),
        agent("x1", Risk::Irreversible, State::StoppedByLimit),
        agent("r1", Risk::LocalWrites, State::Running),
    ]);
    l.run.max_concurrent = 1;
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    assert_eq!((out["action"].as_str(), out["batch"].clone()), (Some("resume"), json!([])));
    assert_eq!(out["waiting_for_room"], 1);
    assert_eq!(out["needs_human"][0]["id"], "x1");
    assert_eq!(out["toast"], Value::Null, "{out}");
    // a batch that resumed someone still has one
    set(&mut l, &ctx("2026-11-15T15:40:00-08:00"), "r1", State::Done, None).unwrap();
    let w = tick(&mut l, &ctx("2026-11-15T15:40:00-08:00"), &usage(5, R2)).unwrap().0["token"].as_str().unwrap().to_string();
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:42:00-08:00"), &w, &usage(5, R2), &small).unwrap();
    assert_eq!(out["toast"], "Resumed 1: a1-stream.");
}

#[test]
fn a_batch_with_room_left_still_waits_the_batch_gap() {
    let mut l = ledger((1..=3).map(|i| agent(&format!("a{i}"), Risk::LocalWrites, State::StoppedByLimit)).collect());
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    assert_eq!((out["remaining"].as_u64(), out["waiting_for_room"].as_u64()), (Some(1), Some(0)));
    assert_eq!(out["arm"]["wake_at"], "2026-11-15T15:46:00-08:00");
}

#[test]
fn a_missing_transcript_means_redispatch() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &|_| None).unwrap();
    assert_eq!(out["batch"][0]["mode"], "redispatch");
    assert_eq!(out["batch"][0]["transcript_bytes"], Value::Null);
}

#[test]
fn done_clears_timers_and_the_wake() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    let w1 = armed(&mut l);
    set(&mut l, &ctx("2026-11-15T15:00:00-08:00"), "a1", State::Done, Some("finished before the wall")).unwrap();
    let (out, changed) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    assert!(changed);
    assert_eq!(out["action"], "done");
    assert_eq!(out["delete_timers"], json!(["cron-1", "task-1"]));
    assert!(l.timers.is_empty());
    assert!(l.wake.is_none());
    // the backup timer then finds nothing
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:40:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    assert_eq!(out["action"], "already-handled");
}

#[test]
fn the_last_batch_drops_the_handled_wakes_timers() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    assert_eq!(out["remaining"], 0);
    assert_eq!(out["arm"], Value::Null);
    assert_eq!(out["delete_timers"], json!(["cron-1", "task-1"]));
    assert!(l.timers.is_empty());
}

// ------------------------------------------------------------------------------------------------ wake-status, stopped, add, set, run-set, migrate

#[test]
fn wake_status_reads_without_consuming() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    let w1 = armed(&mut l);
    let s = wake_status(&l, &w1);
    assert_eq!((s["handled"].as_bool(), s["current"].as_bool(), s["pending_agents"].as_u64()), (Some(false), Some(true), Some(1)));
    assert_eq!(s["host_session_id"], "local_host");
    let s = wake_status(&l, "w42");
    assert_eq!((s["handled"].as_bool(), s["current"].as_bool()), (Some(true), Some(false)));
    next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    assert_eq!(wake_status(&l, &w1)["handled"], true);
}

#[test]
fn stopped_classifies_the_notice() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running), agent("a2", Risk::LocalWrites, State::Running)]);
    let c = ctx("2026-11-15T14:00:00-08:00");
    let (out, _) = stopped(&mut l, &c, "a1", "You've hit your 5-hour limit \u{b7} resets 3:30pm (America/Los_Angeles)").unwrap();
    assert_eq!(out["state"], "stopped-by-limit");
    assert_eq!(out["reset"]["reset"], "2026-11-15T15:30:00-08:00");
    assert_eq!(out["reset"]["wake_at"], "2026-11-15T15:34:00-08:00");
    assert!(l.agents[0].last_failure.as_deref().unwrap().contains("resets 2026-11-15T15:30:00-08:00"));
    let (out, _) = stopped(&mut l, &c, "a2", "You've hit your weekly limit \u{b7} resets Mon 12:00am").unwrap();
    assert_eq!(out["state"], "stopped-by-weekly");
    assert_eq!(out["reset"]["reset"], "2026-11-16T00:00:00-08:00");
    assert!(stopped(&mut l, &c, "a1", "API Error: 500 internal server error").is_err());
    assert!(stopped(&mut l, &c, "nobody", "5-hour limit reached").is_err());
    // a notice without a readable reset still records the stop
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    let (out, _) = stopped(&mut l, &c, "a1", "Agent was stopped by a usage limit").unwrap();
    assert_eq!((out["state"].as_str(), out["reset"].is_null(), out["ignored"].as_bool()), (Some("stopped-by-limit"), true, Some(false)));
}

#[test]
fn a_notice_for_an_agent_that_is_not_running_changes_only_its_last_failure() {
    let c = ctx("2026-11-15T14:00:00-08:00");
    for st in [State::Done, State::NeedsHuman, State::PausedByUser, State::StoppedByWeekly, State::StoppedByLimit] {
        let mut a = agent("a1", Risk::LocalWrites, st);
        a.last_resumed = Some("2026-11-15T13:55:00-08:00".into()); // would trip the herd guard if it counted
        let mut l = ledger(vec![a]);
        let (out, changed) = stopped(&mut l, &c, "a1", "You've hit your 5-hour limit \u{b7} resets 3:30pm").unwrap();
        assert!(changed);
        assert_eq!(out["ignored"], true, "{st:?}");
        assert_eq!(out["state"], word(&st));
        assert_eq!(out["herd_tripped"], false);
        assert_eq!(l.agents[0].state, st);
        assert!(!l.run.herd_tripped);
        assert!(l.agents[0].last_failure.as_deref().unwrap().starts_with("5-hour limit at"));
        assert!(l.log.last().unwrap().msg.contains("ignored"));
    }
}

#[test]
fn add_validates_and_forces_redispatch_for_explore_and_plan() {
    let mut l = ledger(vec![]);
    let c = ctx("2026-11-15T14:00:00-08:00");
    let na = |id: &str, ty: &str| NewAgent {
        id: id.into(),
        replaces: None,
        fields: Fields {
            name: Some("stream".into()),
            agent_type: Some(ty.into()),
            kind: Some(Kind::Resumable),
            risk: Some(Risk::ReadOnly),
            handoff: Some("C:\\wt\\HANDOFF.md".into()),
            ..Fields::default()
        },
    };
    let (out, _) = add(&mut l, &c, na("x1", "Explore")).unwrap();
    assert_eq!(out["agent"]["kind"], "redispatch");
    assert!(out["note"].is_string());
    assert_eq!(out["agent"]["state"], "running");
    assert_eq!(out["agent"]["resume"], "Resume from C:\\wt\\HANDOFF.md. Skip steps marked done; re-check any in-flight operation before retrying it.");
    let (out, _) = add(&mut l, &c, na("x2", "plan")).unwrap();
    assert_eq!(out["agent"]["kind"], "redispatch");
    let (out, _) = add(&mut l, &c, na("x3", "general-purpose")).unwrap();
    assert_eq!(out["agent"]["kind"], "resumable");
    assert!(add(&mut l, &c, na("x3", "general-purpose")).unwrap_err().contains("already"));
    assert!(add(&mut l, &c, na("../x", "general-purpose")).is_err());
    // without --replaces every field is required
    let mut bare = na("x4", "general-purpose");
    bare.fields.risk = None;
    assert!(add(&mut l, &c, bare).unwrap_err().contains("--risk"));
}

#[test]
fn a_redispatch_keeps_the_resume_count_and_the_fields_not_given() {
    // o1 was resumed twice tonight; the second time as a redispatch, so the skill launches a fresh agent n1 for it
    let mut old = agent("o1", Risk::LocalWrites, State::Running);
    old.resumes_tonight = 2;
    old.last_resumed = Some("2026-11-15T15:34:00-08:00".into());
    old.prev_resumed = Some("2026-11-15T10:34:00-08:00".into());
    old.resume = default_resume(old.handoff.as_deref());
    let mut l = ledger(vec![old]);
    let c = ctx("2026-11-15T15:35:00-08:00");
    let (out, changed) =
        add(&mut l, &c, NewAgent { id: "n1".into(), replaces: Some("o1".into()), fields: Fields { name: Some("renamed".into()), ..Fields::default() } })
            .unwrap();
    assert!(changed);
    assert_eq!(out["replaced"], "o1");
    let n = l.agent("n1").unwrap().clone();
    assert_eq!(
        (n.name.as_str(), n.agent_type.as_str(), n.kind, n.risk, n.state),
        ("renamed", "general-purpose", Kind::Resumable, Risk::LocalWrites, State::Running)
    );
    assert_eq!(
        (n.worktree.as_deref(), n.handoff.as_deref(), n.brief_file.as_deref()),
        (Some("C:\\wt\\o1"), Some("C:\\wt\\o1\\HANDOFF.md"), Some("C:\\wt\\o1\\BRIEF.md"))
    );
    assert_eq!((n.resumes_tonight, n.last_resumed.as_deref()), (2, Some("2026-11-15T15:34:00-08:00")));
    assert_eq!(n.resume, default_resume(Some("C:\\wt\\o1\\HANDOFF.md")));
    let o = l.agent("o1").unwrap();
    assert_eq!(o.state, State::Done);
    let log: Vec<&str> = l.log.iter().map(|e| e.msg.as_str()).collect();
    assert!(log.iter().any(|m| m.starts_with("added n1 (renamed)") && m.contains("replacing o1")), "{log:?}");
    assert!(log.contains(&"set o1 (o1-stream): running -> done: redispatched as n1"), "{log:?}");
    // the cap still holds for the new agent: if it stops on the limit it goes to a person, not back into a batch
    stopped(&mut l, &ctx("2026-11-15T16:30:00-08:00"), "n1", "usage limit reached").unwrap();
    let w = tick(&mut l, &ctx("2026-11-15T16:30:00-08:00"), &usage(100, R2)).unwrap().0["token"].as_str().unwrap().to_string();
    let (out, _) = next(&mut l, &ctx("2026-11-15T20:34:00-08:00"), &w, &usage(1, "2026-11-16T01:30:00-08:00"), &small).unwrap();
    assert_eq!(out["needs_human"][0]["id"], "n1");
    assert!(out["needs_human"][0]["why"].as_str().unwrap().contains("already resumed 2"), "{out}");
    // a new handoff carries a default resume text along with it; a missing old agent is an error
    let (out, _) = add(
        &mut l,
        &c,
        NewAgent { id: "n2".into(), replaces: Some("n1".into()), fields: Fields { handoff: Some("C:\\wt\\n2\\HANDOFF.md".into()), ..Fields::default() } },
    )
    .unwrap();
    assert_eq!(out["agent"]["resume"], default_resume(Some("C:\\wt\\n2\\HANDOFF.md")));
    assert!(add(&mut l, &c, NewAgent { id: "n3".into(), replaces: Some("zz".into()), fields: Fields::default() }).unwrap_err().contains("--replaces"));
}

#[test]
fn edit_fills_fields_on_an_existing_agent() {
    let v1 = r#"[{"id":"m1","stream":"migrated","state":"stopped-by-limit"}]"#;
    let (mut l, _) = migrate_v1(v1, &ctx("2026-11-15T14:00:00-08:00"), "x", "s", "h").unwrap();
    let c = ctx("2026-11-15T14:05:00-08:00");
    assert_eq!(l.agents[0].agent_type, "");
    let f =
        Fields { agent_type: Some("general-purpose".into()), risk: Some(Risk::ReadOnly), handoff: Some("C:\\wt\\m1\\HANDOFF.md".into()), ..Fields::default() };
    let (out, changed) = edit(&mut l, &c, "m1", f).unwrap();
    assert!(changed);
    assert_eq!(out["changed"], json!(["agent_type", "handoff", "resume", "risk"]));
    let a = &l.agents[0];
    assert_eq!((a.agent_type.as_str(), a.risk, a.state, a.name.as_str()), ("general-purpose", Risk::ReadOnly, State::StoppedByLimit, "migrated"));
    assert_eq!(a.resume, default_resume(Some("C:\\wt\\m1\\HANDOFF.md")));
    assert!(l.log.last().unwrap().msg.starts_with("edited m1 (migrated): agent_type"));
    // Explore is forced to redispatch; an empty path clears; nothing new changes nothing
    let (out, _) = edit(&mut l, &c, "m1", Fields { agent_type: Some("Explore".into()), worktree: Some(String::new()), ..Fields::default() }).unwrap();
    assert!(out["note"].is_string());
    assert_eq!(l.agents[0].kind, Kind::Redispatch);
    let snapshot = l.clone();
    let (out, changed) = edit(&mut l, &c, "m1", Fields { agent_type: Some("Explore".into()), ..Fields::default() }).unwrap();
    assert_eq!((changed, out["changed"].clone()), (false, json!([])));
    assert_eq!(l, snapshot);
    assert!(edit(&mut l, &c, "m1", Fields { name: Some(" ".into()), ..Fields::default() }).is_err());
    assert!(edit(&mut l, &c, "zz", Fields { name: Some("x".into()), ..Fields::default() }).is_err());
}

#[test]
fn unsent_puts_an_interrupted_batch_back_without_the_herd_guard() {
    let mut a1 = agent("a1", Risk::LocalWrites, State::StoppedByLimit);
    a1.resumes_tonight = 1;
    a1.last_resumed = Some("2026-11-15T10:34:00-08:00".into());
    let mut l = ledger(vec![a1, agent("a2", Risk::LocalWrites, State::StoppedByLimit), agent("a3", Risk::LocalWrites, State::StoppedByLimit)]);
    let w1 = armed(&mut l);
    let (out, _) = next(&mut l, &ctx("2026-11-15T15:34:00-08:00"), &w1, &usage(2, R2), &small).unwrap();
    let ids: Vec<&str> = out["batch"].as_array().unwrap().iter().map(|b| b["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["a1", "a2"]);
    let cycles = l.run.wake_cycles;
    // sending to a1 failed on the limit, so neither was sent
    let (out, changed) = unsent(&mut l, &ctx("2026-11-15T15:35:00-08:00"), &["a1".into(), "a2".into(), "a1".into()]).unwrap();
    assert!(changed);
    assert_eq!(out["then"], "tick");
    assert_eq!(out["unsent"].as_array().unwrap().len(), 2);
    let a1 = l.agent("a1").unwrap();
    assert_eq!((a1.state, a1.resumes_tonight, a1.last_resumed.as_deref()), (State::StoppedByLimit, 1, Some("2026-11-15T10:34:00-08:00")));
    let a2 = l.agent("a2").unwrap();
    assert_eq!((a2.state, a2.resumes_tonight, a2.last_resumed.as_deref()), (State::StoppedByLimit, 0, None));
    assert!(!l.run.herd_tripped);
    assert_eq!(l.run.wake_cycles, cycles + 1);
    assert!(l.log.iter().any(|e| e.msg.starts_with("unsent a1 (a1-stream): back to stopped-by-limit")));
    // a later notice for them is not a refail: they are not running
    let (st, _) = stopped(&mut l, &ctx("2026-11-15T15:36:00-08:00"), "a2", "usage limit reached").unwrap();
    assert_eq!((st["ignored"].as_bool(), st["herd_tripped"].as_bool()), (Some(true), Some(false)));
    // only running items of the latest batch are accepted, and a refusal changes nothing
    let snapshot = l.clone();
    assert!(unsent(&mut l, &ctx("2026-11-15T15:37:00-08:00"), &["a1".into()]).unwrap_err().contains("latest resume batch"));
    assert!(unsent(&mut l, &ctx("2026-11-15T15:37:00-08:00"), &["a3".into()]).is_err());
    assert!(unsent(&mut l, &ctx("2026-11-15T15:37:00-08:00"), &["zz".into()]).is_err());
    assert_eq!(l, snapshot);
}

#[test]
fn interrupted_batches_end_at_the_cycle_cap() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    l.run.max_resumes_per_agent = 100;
    let mut token = armed(&mut l);
    let mut at = t("2026-11-15T15:34:00-08:00");
    for round in 0..10 {
        let (out, _) = next(&mut l, &Ctx { now: at, zone: la() }, &token, &usage(2, R2), &small).unwrap();
        if out["action"] == "herd-stop" {
            assert!(round >= 2, "stopped too early: round {round}");
            return;
        }
        assert_eq!(out["action"], "resume", "{out}");
        unsent(&mut l, &Ctx { now: at + Duration::minutes(1), zone: la() }, &["a1".into()]).unwrap();
        let (out, _) = tick(&mut l, &Ctx { now: at + Duration::minutes(1), zone: la() }, &usage(3, R2)).unwrap();
        token = out["token"].as_str().unwrap().to_string();
        at += Duration::minutes(3);
    }
    panic!("an endless loop of interrupted batches");
}

#[test]
fn set_logs_the_change() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::Running)]);
    let (out, _) = set(&mut l, &ctx("2026-11-15T14:00:00-08:00"), "a1", State::PausedByUser, Some("user asked")).unwrap();
    assert_eq!((out["from"].as_str(), out["state"].as_str()), (Some("running"), Some("paused-by-user")));
    assert!(l.log.last().unwrap().msg.ends_with("running -> paused-by-user: user asked"));
    assert!(set(&mut l, &ctx("2026-11-15T14:00:00-08:00"), "zz", State::Done, None).is_err());
}

#[test]
fn run_set_validates_keys_and_values() {
    let mut l = ledger(vec![]);
    let c = ctx("2026-11-15T14:00:00-08:00");
    let (out, changed) = run_set(&mut l, &c, "batch-size", "3").unwrap();
    assert!(changed);
    assert_eq!((out["from"].as_u64(), out["value"].as_u64()), (Some(2), Some(3)));
    assert_eq!(l.run.batch_size, 3);
    run_set(&mut l, &c, "herd_tripped", "true").unwrap();
    assert!(l.run.herd_tripped);
    run_set(&mut l, &c, "cycle_base_pct", "40").unwrap();
    assert_eq!(l.run.cycle_base_pct, Some(40));
    run_set(&mut l, &c, "cycle_base_pct", "null").unwrap();
    assert_eq!(l.run.cycle_base_pct, None);
    let (_, changed) = run_set(&mut l, &c, "batch_size", "3").unwrap();
    assert!(!changed);
    assert_eq!(l.run.herd_window_min, 30);
    run_set(&mut l, &c, "herd-window-min", "45").unwrap();
    assert_eq!(l.run.herd_window_min, 45);
    for (k, v) in [
        ("herd_window_min", "0"),
        ("herd_window_min", "1441"),
        ("cycle_started_at", "2026-11-15T14:00:00-08:00"),
        ("arm_at_pct", "0"),
        ("arm_at_pct", "101"),
        ("max_concurrent", "0"),
        ("unattended", "maybe"),
        ("nope", "1"),
        ("wake_seq", "1"),
        ("batch_gap_min", "-1"),
    ] {
        assert!(run_set(&mut l, &c, k, v).is_err(), "{k}={v}");
    }
}

#[test]
fn new_night_resets_the_counters() {
    let mut a = agent("a1", Risk::LocalWrites, State::Running);
    a.resumes_tonight = 2;
    let mut l = ledger(vec![a]);
    l.run.wake_cycles = 3;
    l.run.herd_tripped = true;
    l.run.cycle_base_pct = Some(50);
    l.run.cycle_started_at = Some("2026-11-16T03:34:00-08:00".into());
    new_night(&mut l, &ctx("2026-11-16T09:00:00-08:00")).unwrap();
    assert_eq!((l.run.wake_cycles, l.run.herd_tripped, l.run.cycle_base_pct, l.agents[0].resumes_tonight), (0, false, None, 0));
    assert_eq!(l.run.cycle_started_at, None);
}

#[test]
fn migrate_v1_converts_the_old_fields() {
    let v1 = r#"{"agents":[
        {"id":"a1","stream":"bridge tests","worktree":"C:\\wt\\a1","state":"stopped-by-limit","resume":"Carry on."},
        {"id":"a2","stream":"docs","state":"sleeping"},
        {"stream":"no id"}
    ]}"#;
    let (l, notes) = migrate_v1(v1, &ctx("2026-11-15T14:00:00-08:00"), "old.json", "sess", "local_h").unwrap();
    assert_eq!(l.version, 2);
    assert_eq!(l.agents.len(), 2);
    let a1 = &l.agents[0];
    assert_eq!(
        (a1.name.as_str(), a1.kind, a1.risk, a1.agent_type.as_str(), a1.state),
        ("bridge tests", Kind::Resumable, Risk::LocalWrites, "", State::StoppedByLimit)
    );
    assert_eq!(a1.resume, "Carry on.");
    assert_eq!(a1.worktree.as_deref(), Some("C:\\wt\\a1"));
    assert_eq!(l.agents[1].state, State::NeedsHuman);
    assert_eq!(l.agents[1].resume, default_resume(None));
    assert_eq!(notes.len(), 2);
    // a bare list works too; a v2 file is refused
    assert_eq!(migrate_v1(r#"[{"id":"b1","stream":"s","state":"running"}]"#, &ctx("2026-11-15T14:00:00-08:00"), "x", "s", "h").unwrap().0.agents.len(), 1);
    assert!(migrate_v1(r#"{"version":2,"agents":[]}"#, &ctx("2026-11-15T14:00:00-08:00"), "x", "s", "h").is_err());
}

#[test]
fn summary_mentions_the_essentials() {
    let mut l = ledger(vec![agent("a1", Risk::LocalWrites, State::StoppedByLimit)]);
    armed(&mut l);
    let s = summary(&l, "C:\\x.json");
    for needle in ["w1 (wall)", "cron-oneshot cron-1", "1 stopped-by-limit", "a1-stream", "unattended"] {
        assert!(s.contains(needle), "{needle}\n{s}");
    }
}
