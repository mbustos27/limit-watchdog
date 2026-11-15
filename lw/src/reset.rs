//! Reading a reset time, and the wake times that follow from it.
//!
//! Accepted: `get_usage`'s ISO UTC (`2026-11-15T23:30:00.194Z`), any RFC 3339 time, and limit text such as
//! `You've hit your 5-hour limit · resets 3:40am (America/Los_Angeles)`, `resets 3:45pm`, `resets Mon 12:00am`,
//! `resets Nov 17, 10am`, `resets in 4h 25m`. Without a zone in brackets a clock time is local.
//! A clock time earlier than now is tomorrow; a weekday is its next occurrence; a month and day earlier than now is next year.
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, Timelike, Utc, Weekday};
use serde_json::{Value, json};

use crate::clock::{self, Ctx, Zone};

/// The one-shot wake fires this long after the reset.
pub const WAKE_AFTER_MIN: i64 = 4;
/// The Desktop backup task fires this long after the reset.
pub const BACKUP_AFTER_MIN: i64 = 10;
/// A weekly stop notifies this long after the weekly reset.
pub const WEEKLY_NOTIFY_AFTER_MIN: i64 = 15;
/// A wake is never set sooner than this many minutes from now (a reset already in the past still gets a wake that can fire).
pub const SOONEST_MIN: i64 = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Schedule {
    pub reset: DateTime<Utc>,
    pub wake_at: DateTime<Utc>,
    pub backup_at: DateTime<Utc>,
}

impl Schedule {
    pub fn json(&self, ctx: &Ctx) -> Value {
        json!({ "reset": ctx.fmt(self.reset), "wake_at": ctx.fmt(self.wake_at), "cron": ctx.cron(self.wake_at), "backup_at": ctx.fmt(self.backup_at) })
    }
}

pub fn whole_seconds(t: DateTime<Utc>) -> DateTime<Utc> {
    t - Duration::nanoseconds(i64::from(t.timestamp_subsec_nanos()))
}

/// `t` plus `minutes`, rounded up to a whole minute, then one minute on if it lands on :00 or :30 local time
/// (one-shots on those minutes can fire up to 90 seconds early).
pub fn later(t: DateTime<Utc>, minutes: i64, zone: &Zone) -> DateTime<Utc> {
    let mut x = whole_seconds(t) + Duration::minutes(minutes);
    if x.second() != 0 {
        x = x - Duration::seconds(i64::from(x.second())) + Duration::minutes(1);
    }
    let m = zone.fix(x).minute();
    if m == 0 || m == 30 {
        x += Duration::minutes(1);
    }
    x
}

/// Wake at reset + 4 min and back up at reset + 10 min, each off :00/:30. Neither is ever in the past.
pub fn schedule(reset: DateTime<Utc>, ctx: &Ctx) -> Schedule {
    let reset = whole_seconds(reset);
    let mut wake_at = later(reset, WAKE_AFTER_MIN, &ctx.zone);
    let soonest = later(ctx.now, SOONEST_MIN, &ctx.zone);
    if wake_at < soonest {
        wake_at = soonest;
    }
    let mut backup_at = later(reset, BACKUP_AFTER_MIN, &ctx.zone);
    let after_wake = later(wake_at, BACKUP_AFTER_MIN - WAKE_AFTER_MIN - 1, &ctx.zone);
    if backup_at < after_wake {
        backup_at = after_wake;
    }
    Schedule { reset, wake_at, backup_at }
}

/// A time `minutes` from now (off :00/:30), for batch wakes.
pub fn from_now(ctx: &Ctx, minutes: i64) -> DateTime<Utc> {
    later(ctx.now, minutes.max(SOONEST_MIN), &ctx.zone)
}

/// Read a reset time from ISO text or a limit message.
pub fn parse_reset(text: &str, ctx: &Ctx) -> Result<DateTime<Utc>, String> {
    let original = text.trim();
    if original.is_empty() {
        return Err("no reset time given".into());
    }
    if let Ok(t) = clock::parse_instant(original, &ctx.zone) {
        return Ok(t);
    }
    // ASCII lowercasing keeps byte offsets, so positions found in `lower` are valid in `original`.
    let lower = original.to_ascii_lowercase();
    let clause = match lower.rfind("reset") {
        Some(i) => {
            let mut j = i + "reset".len();
            if lower[j..].starts_with('s') {
                j += 1;
            }
            &original[j..]
        }
        None => original,
    };
    // The reset clause ends at the next separator ("resets 3pm · /upgrade for more").
    let clause = match clause.find(['\u{b7}', '\u{2219}', '\u{2022}', '|', '\n']) {
        Some(end) => &clause[..end],
        None => clause,
    };
    let clause = clause.trim().trim_start_matches(':').trim();
    let bare = clause.trim_end_matches(['.', ';', '!', ')']).trim();
    if let Ok(t) = clock::parse_instant(bare, &ctx.zone) {
        return Ok(t);
    }
    let (body, zone) = match (clause.find('('), clause.rfind(')')) {
        (Some(a), Some(b)) if b > a => {
            let name = clause[a + 1..b].trim();
            let z = clock::named(name).ok_or_else(|| format!("unknown time zone {name:?} in {original:?}"))?;
            (format!("{} {}", &clause[..a], &clause[b + 1..]), z)
        }
        _ => (clause.to_string(), ctx.zone),
    };
    let cleaned: String = body.chars().map(|c| if c == ',' || c == '\u{b7}' || c == '\u{2219}' || c == '\u{2022}' { ' ' } else { c }).collect();
    let toks: Vec<String> = cleaned
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase().replace('.', "").trim_end_matches([';', '!', '?']).to_string())
        .filter(|t| !t.is_empty())
        .collect();

    let mut weekday: Option<Weekday> = None;
    let mut month_day: Option<(u32, u32)> = None;
    let mut time: Option<NaiveTime> = None;
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i].as_str();
        if matches!(t, "at" | "on" | "the") {
            i += 1;
            continue;
        }
        if t == "in" && weekday.is_none() && month_day.is_none() && time.is_none() {
            let d = relative(&toks[i + 1..]).ok_or_else(|| format!("could not read the duration in {original:?}"))?;
            return Ok(whole_seconds(ctx.now) + d);
        }
        if let Some(w) = weekday_of(t) {
            if weekday.replace(w).is_some() {
                return Err(format!("two weekdays in {original:?}"));
            }
            i += 1;
            continue;
        }
        if let Some(m) = month_of(t) {
            let d = toks.get(i + 1).and_then(|d| day_of(d)).ok_or_else(|| format!("a month without a day in {original:?}"))?;
            month_day = Some((m, d));
            i += 2;
            continue;
        }
        if let Some((tm, used)) = time_of(t, toks.get(i + 1).map(String::as_str)) {
            if time.replace(tm).is_some() {
                return Err(format!("two clock times in {original:?}"));
            }
            i += used;
            continue;
        }
        if time.is_some() {
            break; // trailing words after the time ("resets 3pm. Upgrade for more")
        }
        return Err(format!("could not read {t:?} in {original:?}"));
    }
    let time = time.ok_or_else(|| format!("no clock time in {original:?}"))?;
    let today = zone.naive(ctx.now).date();
    let at = |date: NaiveDate| zone.resolve(date.and_time(time)).ok_or_else(|| format!("{date} {time} does not exist in that zone"));
    if let Some((m, d)) = month_day {
        let date = NaiveDate::from_ymd_opt(today.year(), m, d).ok_or_else(|| format!("no such date in {original:?}"))?;
        let t = at(date)?;
        if t >= ctx.now {
            return Ok(t);
        }
        let next = NaiveDate::from_ymd_opt(today.year() + 1, m, d).ok_or_else(|| format!("no such date in {original:?}"))?;
        return at(next);
    }
    if let Some(w) = weekday {
        let ahead = (7 + i64::from(w.num_days_from_monday()) - i64::from(today.weekday().num_days_from_monday())) % 7;
        let t = at(today + Duration::days(ahead))?;
        return if t >= ctx.now { Ok(t) } else { at(today + Duration::days(ahead + 7)) };
    }
    let t = at(today)?;
    if t >= ctx.now { Ok(t) } else { at(today + Duration::days(1)) }
}

const WEEKDAYS: [(&str, Weekday); 7] = [
    ("monday", Weekday::Mon),
    ("tuesday", Weekday::Tue),
    ("wednesday", Weekday::Wed),
    ("thursday", Weekday::Thu),
    ("friday", Weekday::Fri),
    ("saturday", Weekday::Sat),
    ("sunday", Weekday::Sun),
];
const MONTHS: [&str; 12] = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];

fn weekday_of(t: &str) -> Option<Weekday> {
    if t.len() < 3 {
        return None;
    }
    WEEKDAYS.iter().find(|(n, _)| n.starts_with(t)).map(|(_, w)| *w)
}

fn month_of(t: &str) -> Option<u32> {
    if t.len() < 3 {
        return None;
    }
    MONTHS.iter().position(|n| n.starts_with(t)).map(|i| i as u32 + 1)
}

fn day_of(t: &str) -> Option<u32> {
    let digits = t.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &t[digits.len()..];
    if !matches!(suffix, "" | "st" | "nd" | "rd" | "th") || digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|d| (1..=31).contains(d))
}

/// `3:40am`, `3am`, `3:40 am`, `15:40`, `noon`, `midnight`. Returns the time and how many tokens it used.
fn time_of(t: &str, next: Option<&str>) -> Option<(NaiveTime, usize)> {
    match t {
        "noon" => return Some((NaiveTime::from_hms_opt(12, 0, 0)?, 1)),
        "midnight" => return Some((NaiveTime::from_hms_opt(0, 0, 0)?, 1)),
        _ => {}
    }
    let (clock, pm, used) = if let Some(c) = t.strip_suffix("am") {
        (c, Some(false), 1)
    } else if let Some(c) = t.strip_suffix("pm") {
        (c, Some(true), 1)
    } else if next == Some("am") {
        (t, Some(false), 2)
    } else if next == Some("pm") {
        (t, Some(true), 2)
    } else {
        (t, None, 1)
    };
    let (h, m) = match clock.split_once(':') {
        Some((h, m)) => (h, Some(m)),
        None => (clock, None),
    };
    let num = |s: &str, max_len: usize| -> Option<u32> {
        if s.is_empty() || s.len() > max_len || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    let h = num(h, 2)?;
    let m = match m {
        Some(m) if m.len() == 2 => num(m, 2)?,
        Some(_) => return None,
        None if pm.is_some() => 0,
        None => return None, // a bare number is not a time
    };
    if m > 59 {
        return None;
    }
    let h = match pm {
        Some(pm) => {
            if !(1..=12).contains(&h) {
                return None;
            }
            h % 12 + if pm { 12 } else { 0 }
        }
        None if h < 24 => h,
        None => return None,
    };
    Some((NaiveTime::from_hms_opt(h, m, 0)?, used))
}

/// `4h 25m`, `25m`, `2d 3h`, `4 hours 25 minutes`.
fn relative(toks: &[String]) -> Option<Duration> {
    let mut total = Duration::zero();
    let mut any = false;
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i].as_str();
        let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return None;
        }
        let n: i64 = digits.parse().ok()?;
        let mut unit = t[digits.len()..].to_string();
        if unit.is_empty() {
            unit = toks.get(i + 1)?.clone();
            i += 1;
        }
        let d = match unit.as_str() {
            "d" | "day" | "days" => Duration::days(n),
            "h" | "hr" | "hrs" | "hour" | "hours" => Duration::hours(n),
            "m" | "min" | "mins" | "minute" | "minutes" => Duration::minutes(n),
            _ => return None,
        };
        total += d;
        any = true;
        i += 1;
    }
    any.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::SecondsFormat;

    fn la() -> Zone {
        clock::named("America/Los_Angeles").unwrap()
    }
    fn ctx_at(now: &str) -> Ctx {
        Ctx { now: clock::parse_instant(now, &la()).unwrap(), zone: la() }
    }
    fn local(ctx: &Ctx, t: DateTime<Utc>) -> String {
        ctx.zone.fix(t).to_rfc3339_opts(SecondsFormat::Secs, false)
    }
    fn parse(text: &str, now: &str) -> String {
        let ctx = ctx_at(now);
        local(&ctx, parse_reset(text, &ctx).unwrap_or_else(|e| panic!("{text}: {e}")))
    }

    #[test]
    fn iso_utc_with_millis_and_z() {
        assert_eq!(parse("2026-11-15T23:30:00.194Z", "2026-11-15T12:00:00-08:00"), "2026-11-15T15:30:00-08:00");
    }

    #[test]
    fn the_millis_do_not_push_the_wake_a_minute_later() {
        let ctx = ctx_at("2026-11-15T12:00:00-08:00");
        let s = schedule(parse_reset("2026-11-15T23:30:00.194Z", &ctx).unwrap(), &ctx);
        assert_eq!(local(&ctx, s.wake_at), "2026-11-15T15:34:00-08:00");
        assert_eq!(local(&ctx, s.backup_at), "2026-11-15T15:40:00-08:00");
        assert_eq!(ctx.cron(s.wake_at), "34 15 15 11 *");
    }

    #[test]
    fn clock_time_with_zone_suffix_and_leading_text() {
        assert_eq!(parse("You've hit your 5-hour limit \u{b7} resets 3:40am (America/Los_Angeles)", "2026-11-15T22:00:00-08:00"), "2026-11-16T03:40:00-08:00");
    }

    #[test]
    fn the_zone_suffix_is_honoured_when_local_is_elsewhere() {
        // 3:40am New York is 0:40am Los Angeles
        assert_eq!(parse("resets 3:40am (America/New_York)", "2026-11-15T22:00:00-08:00"), "2026-11-16T00:40:00-08:00");
        // and "today" is New York's today: at 22:00 LA it is already 01:00 on the 16th in New York, so 3:40am is still ahead that day
        assert_eq!(parse("resets 3:40am (America/New_York)", "2026-11-15T22:30:00-08:00"), "2026-11-16T00:40:00-08:00");
    }

    #[test]
    fn no_zone_means_local() {
        assert_eq!(parse("resets 3:45pm", "2026-11-15T12:00:00-08:00"), "2026-11-15T15:45:00-08:00");
    }

    #[test]
    fn a_clock_time_earlier_than_now_is_tomorrow_across_midnight() {
        assert_eq!(parse("resets 12:10am", "2026-11-15T23:50:00-08:00"), "2026-11-16T00:10:00-08:00");
        assert_eq!(parse("resets 3:45pm", "2026-11-15T16:00:00-08:00"), "2026-11-16T15:45:00-08:00");
        // month and year roll over too
        assert_eq!(parse("resets 12:05am", "2026-12-31T23:59:00-08:00"), "2027-01-01T00:05:00-08:00");
    }

    #[test]
    fn the_same_minute_as_now_is_today() {
        assert_eq!(parse("resets 3:45pm", "2026-11-15T15:45:00-08:00"), "2026-11-15T15:45:00-08:00");
    }

    #[test]
    fn weekday_midnight_is_its_next_occurrence() {
        // 2026-11-15 is a Sunday
        assert_eq!(parse("resets Mon 12:00am", "2026-11-15T10:00:00-08:00"), "2026-11-16T00:00:00-08:00");
        // on Monday morning, Monday 12:00am has passed: a week later
        assert_eq!(parse("resets Mon 12:00am", "2026-11-16T10:00:00-08:00"), "2026-11-23T00:00:00-08:00");
        // later the same weekday is today
        assert_eq!(parse("resets Sunday 3pm", "2026-11-15T10:00:00-08:00"), "2026-11-15T15:00:00-08:00");
        assert_eq!(
            parse("You've hit your weekly limit \u{b7} resets Thu 9:00am (America/Los_Angeles)", "2026-11-15T10:00:00-08:00"),
            "2026-11-19T09:00:00-08:00"
        );
    }

    #[test]
    fn twelve_am_and_twelve_pm() {
        assert_eq!(parse("resets 12am", "2026-11-15T10:00:00-08:00"), "2026-11-16T00:00:00-08:00");
        assert_eq!(parse("resets 12pm", "2026-11-15T10:00:00-08:00"), "2026-11-15T12:00:00-08:00");
        assert_eq!(parse("resets 3:40 PM", "2026-11-15T10:00:00-08:00"), "2026-11-15T15:40:00-08:00");
        assert_eq!(parse("resets 3:40 p.m.", "2026-11-15T10:00:00-08:00"), "2026-11-15T15:40:00-08:00");
        assert_eq!(parse("resets 15:40", "2026-11-15T10:00:00-08:00"), "2026-11-15T15:40:00-08:00");
    }

    #[test]
    fn older_wordings_month_day_and_relative() {
        assert_eq!(parse("Your limit will reset at 3pm (America/Los_Angeles).", "2026-11-15T10:00:00-08:00"), "2026-11-15T15:00:00-08:00");
        assert_eq!(parse("Weekly limit reached \u{2219} resets Nov 17, 10am", "2026-11-15T10:00:00-08:00"), "2026-11-17T10:00:00-08:00");
        assert_eq!(parse("resets Jan 2 at 10am", "2026-11-15T10:00:00-08:00"), "2027-01-02T10:00:00-08:00");
        assert_eq!(parse("resets in 4h 25m", "2026-11-15T10:00:00-08:00"), "2026-11-15T14:25:00-08:00");
        assert_eq!(parse("resets 2026-11-16T03:40-08:00", "2026-11-15T10:00:00-08:00"), "2026-11-16T03:40:00-08:00");
        assert_eq!(parse("resets 3pm (America/Los_Angeles) \u{b7} /upgrade to Max (more)", "2026-11-15T10:00:00-08:00"), "2026-11-15T15:00:00-08:00");
        assert_eq!(parse("resets 3pm. Upgrade for more", "2026-11-15T10:00:00-08:00"), "2026-11-15T15:00:00-08:00");
    }

    #[test]
    fn unreadable_text_is_an_error_not_a_guess() {
        let ctx = ctx_at("2026-11-15T10:00:00-08:00");
        for bad in ["", "resets soon", "resets 3", "resets 13pm", "resets 3:4pm", "resets 3:40am (Mars/Base)", "resets Mon", "resets Feb 30 1pm", "hello"] {
            assert!(parse_reset(bad, &ctx).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn wake_and_backup_move_off_the_hour_and_half_hour() {
        let ctx = ctx_at("2026-11-15T10:00:00-08:00");
        // reset + 4 lands on :30 -> :31; reset + 10 is :36
        let s = schedule(clock::parse_instant("2026-11-15T15:26:00-08:00", &la()).unwrap(), &ctx);
        assert_eq!(local(&ctx, s.wake_at), "2026-11-15T15:31:00-08:00");
        assert_eq!(local(&ctx, s.backup_at), "2026-11-15T15:36:00-08:00");
        // reset + 4 lands on :00 -> :01 (next hour, next day)
        let s = schedule(clock::parse_instant("2026-11-15T23:56:00-08:00", &la()).unwrap(), &ctx);
        assert_eq!(local(&ctx, s.wake_at), "2026-11-16T00:01:00-08:00");
        assert_eq!(ctx.cron(s.wake_at), "1 0 16 11 *");
        // reset + 10 lands on :00 -> :01
        let s = schedule(clock::parse_instant("2026-11-15T15:50:00-08:00", &la()).unwrap(), &ctx);
        assert_eq!(local(&ctx, s.wake_at), "2026-11-15T15:54:00-08:00");
        assert_eq!(local(&ctx, s.backup_at), "2026-11-15T16:01:00-08:00");
        // a reset with seconds rounds the wake up, never down
        let s = schedule(clock::parse_instant("2026-11-15T15:39:59-08:00", &la()).unwrap(), &ctx);
        assert_eq!(local(&ctx, s.wake_at), "2026-11-15T15:44:00-08:00");
    }

    #[test]
    fn a_reset_already_past_still_gets_a_wake_in_the_future() {
        let ctx = ctx_at("2026-11-15T15:50:20-08:00");
        let s = schedule(clock::parse_instant("2026-11-15T15:30:00-08:00", &la()).unwrap(), &ctx);
        assert_eq!(local(&ctx, s.wake_at), "2026-11-15T15:53:00-08:00");
        assert_eq!(local(&ctx, s.backup_at), "2026-11-15T15:58:00-08:00");
    }

    #[test]
    fn later_is_judged_in_local_minutes() {
        // India is UTC+5:30: 10:00 UTC is 15:30 local, so it must move
        let ist = clock::named("Asia/Kolkata").unwrap();
        let t = later(clock::parse_instant("2026-11-15T09:56:00Z", &ist).unwrap(), 4, &ist);
        assert_eq!(t.to_rfc3339_opts(SecondsFormat::Secs, true), "2026-11-15T10:01:00Z");
    }
}
