//! The clock and the time zone every output is written in.
//!
//! "Local" is the system time zone, unless `LW_TZ` names an IANA zone (tests set it so results do not depend on the machine).
//! Instants are kept as UTC inside the program and written as RFC 3339 in the local zone.
use chrono::{DateTime, Duration, FixedOffset, Local, LocalResult, NaiveDateTime, SecondsFormat, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

pub const TZ_ENV: &str = "LW_TZ";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Zone {
    System,
    Named(Tz),
}

impl Zone {
    pub fn from_env() -> Result<Zone, String> {
        match std::env::var(TZ_ENV) {
            Ok(v) if !v.trim().is_empty() => named(v.trim()).ok_or_else(|| format!("{TZ_ENV}: unknown time zone {v:?}")),
            _ => Ok(Zone::System),
        }
    }

    pub fn fix(&self, t: DateTime<Utc>) -> DateTime<FixedOffset> {
        match self {
            Zone::System => t.with_timezone(&Local).fixed_offset(),
            Zone::Named(tz) => t.with_timezone(tz).fixed_offset(),
        }
    }

    pub fn naive(&self, t: DateTime<Utc>) -> NaiveDateTime {
        self.fix(t).naive_local()
    }

    /// A wall-clock time in this zone as an instant. A time that happens twice (clocks going back) is the first one;
    /// a time that never happens (clocks going forward) is taken an hour later.
    pub fn resolve(&self, n: NaiveDateTime) -> Option<DateTime<Utc>> {
        fn pick<T: TimeZone>(r: LocalResult<DateTime<T>>) -> Option<DateTime<Utc>> {
            match r {
                LocalResult::Single(d) => Some(d.with_timezone(&Utc)),
                LocalResult::Ambiguous(a, _) => Some(a.with_timezone(&Utc)),
                LocalResult::None => None,
            }
        }
        let at = |n: NaiveDateTime| match self {
            Zone::System => pick(Local.from_local_datetime(&n)),
            Zone::Named(tz) => pick(tz.from_local_datetime(&n)),
        };
        at(n).or_else(|| at(n + Duration::hours(1)))
    }

    /// True when the local wall time of `t` happens twice (clocks going back), so a cron line for it names two instants.
    pub fn ambiguous(&self, t: DateTime<Utc>) -> bool {
        let n = self.naive(t);
        match self {
            Zone::System => matches!(Local.from_local_datetime(&n), LocalResult::Ambiguous(..)),
            Zone::Named(tz) => matches!(tz.from_local_datetime(&n), LocalResult::Ambiguous(..)),
        }
    }
}

/// An IANA zone name ("America/Los_Angeles"); case does not matter.
pub fn named(name: &str) -> Option<Zone> {
    let name = name.trim();
    if let Ok(tz) = name.parse::<Tz>() {
        return Some(Zone::Named(tz));
    }
    chrono_tz::TZ_VARIANTS.iter().find(|tz| tz.name().eq_ignore_ascii_case(name)).map(|tz| Zone::Named(*tz))
}

/// "Now" (the real clock or `--now`) and the zone outputs are written in.
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    pub now: DateTime<Utc>,
    pub zone: Zone,
}

impl Ctx {
    pub fn fmt(&self, t: DateTime<Utc>) -> String {
        self.zone.fix(t).to_rfc3339_opts(SecondsFormat::Secs, false)
    }
    pub fn now_str(&self) -> String {
        self.fmt(self.now)
    }
    /// "15:34" in the local zone, for toasts.
    pub fn hm(&self, t: DateTime<Utc>) -> String {
        self.zone.fix(t).format("%H:%M").to_string()
    }
    /// "Mon 15:34" in the local zone, for toasts about a day or more away.
    pub fn day_hm(&self, t: DateTime<Utc>) -> String {
        self.zone.fix(t).format("%a %H:%M").to_string()
    }
    /// The five-field cron line ("M H DoM Mon *") for a one-shot at `t`, local time.
    pub fn cron(&self, t: DateTime<Utc>) -> String {
        let l = self.zone.fix(t);
        format!("{} {} {} {} *", l.minute(), l.hour(), chrono::Datelike::day(&l), chrono::Datelike::month(&l))
    }
}

/// An ISO 8601 / RFC 3339 instant: `2026-11-15T23:30:00.194Z`, `2026-11-16T03:40:00-08:00`, `2026-11-16T03:40-08:00`.
/// Without an offset (`2026-11-16T03:40`) it is a wall-clock time in `zone`.
pub fn parse_instant(s: &str, zone: &Zone) -> Result<DateTime<Utc>, String> {
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Ok(d.with_timezone(&Utc));
    }
    for f in ["%Y-%m-%dT%H:%M%:z", "%Y-%m-%d %H:%M%:z"] {
        if let Ok(d) = DateTime::parse_from_str(s, f) {
            return Ok(d.with_timezone(&Utc));
        }
    }
    if let Some(utc) = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')) {
        for f in ["%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
            if let Ok(n) = NaiveDateTime::parse_from_str(utc, f) {
                return Ok(n.and_utc());
            }
        }
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, f) {
            return zone.resolve(n).ok_or_else(|| format!("{s:?} is not a time that exists in the local zone"));
        }
    }
    Err(format!("{s:?} is not an ISO 8601 time (like 2026-11-15T23:30:00.194Z or 2026-11-16T03:40:00-08:00)"))
}

/// The real clock, or `--now`.
pub fn now_from(arg: Option<&str>, zone: &Zone) -> Result<DateTime<Utc>, String> {
    match arg {
        Some(s) => parse_instant(s, zone).map_err(|e| format!("--now: {e}")),
        None => Ok(Utc::now()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn la() -> Zone {
        named("America/Los_Angeles").unwrap()
    }

    #[test]
    fn iso_with_millis_and_z() {
        let t = parse_instant("2026-11-15T23:30:00.194Z", &la()).unwrap();
        assert_eq!(t.to_rfc3339_opts(SecondsFormat::Millis, true), "2026-11-15T23:30:00.194Z");
    }

    #[test]
    fn iso_without_seconds_and_with_offset() {
        let t = parse_instant("2026-11-16T03:40-08:00", &la()).unwrap();
        assert_eq!(t.to_rfc3339_opts(SecondsFormat::Secs, true), "2026-11-16T11:40:00Z");
    }

    #[test]
    fn iso_without_offset_is_local() {
        let t = parse_instant("2026-11-16T03:40", &la()).unwrap();
        assert_eq!(t.to_rfc3339_opts(SecondsFormat::Secs, true), "2026-11-16T11:40:00Z");
    }

    #[test]
    fn output_is_rfc3339_in_the_local_zone() {
        let ctx = Ctx { now: parse_instant("2026-11-15T23:30:00.194Z", &la()).unwrap(), zone: la() };
        assert_eq!(ctx.now_str(), "2026-11-15T15:30:00-08:00");
        assert_eq!(ctx.cron(ctx.now), "30 15 15 11 *");
    }

    #[test]
    fn zone_names_ignore_case() {
        assert_eq!(named("america/los_angeles"), Some(la()));
        assert!(named("Mars/Olympus").is_none());
    }

    #[test]
    fn a_time_skipped_by_daylight_saving_moves_an_hour_on() {
        // 2026-03-08 02:30 does not exist in Los Angeles
        let n = NaiveDateTime::parse_from_str("2026-03-08T02:30", "%Y-%m-%dT%H:%M").unwrap();
        let t = la().resolve(n).unwrap();
        assert_eq!(la().fix(t).to_rfc3339_opts(SecondsFormat::Secs, false), "2026-03-08T03:30:00-07:00");
    }
}
