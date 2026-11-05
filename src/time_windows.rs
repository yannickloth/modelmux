//! Time-window evaluation.
//!
//! Windows carry `HH:MM` bounds plus an optional day-of-week list. Each
//! window is evaluated in its own IANA timezone (window `tz`, falling back
//! to `server.window_timezone`). This matters for DST zones: a fixed
//! local-time window slides one hour twice a year, so schedules defined
//! elsewhere (e.g. DeepSeek's UTC peak windows) pin `tz = "UTC"`.

use std::str::FromStr;

use chrono::{Datelike, Local, Timelike, Utc};

use crate::config::WindowCfg;

/// "HH:MM" -> minutes since midnight.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// Resolve a timezone name: "utc", "local", or any IANA zone
/// (e.g. "Europe/Brussels"). Unknown names fall back to UTC with a warning.
pub fn parse_tz(name: &str) -> chrono_tz::Tz {
    match name {
        "utc" | "UTC" => chrono_tz::Tz::UTC,
        _ => chrono_tz::Tz::from_str(name).unwrap_or_else(|e| {
            tracing::warn!(tz = %name, "unknown timezone, falling back to UTC: {e}");
            chrono_tz::Tz::UTC
        }),
    }
}

/// True when the name is "utc", "local", or a known IANA zone.
pub fn is_valid_tz(name: &str) -> bool {
    matches!(name, "utc" | "local") || chrono_tz::Tz::from_str(name).is_ok()
}

/// Current time as minutes since midnight in the named timezone.
pub fn now_minutes_in(tz_name: &str) -> u32 {
    match tz_name {
        "local" => Local::now().time().num_seconds_from_midnight() / 60,
        _ => {
            Utc::now()
                .with_timezone(&parse_tz(tz_name))
                .time()
                .num_seconds_from_midnight()
                / 60
        }
    }
}

/// Current ISO weekday in the named timezone: 1 = Monday .. 7 = Sunday.
pub fn now_weekday_in(tz_name: &str) -> u32 {
    match tz_name {
        "local" => Local::now().weekday().num_days_from_monday() + 1,
        _ => {
            Utc::now()
                .with_timezone(&parse_tz(tz_name))
                .weekday()
                .num_days_from_monday()
                + 1
        }
    }
}

fn weekday_prefix(wd: u32) -> &'static str {
    match wd {
        1 => "mon",
        2 => "tue",
        3 => "wed",
        4 => "thu",
        5 => "fri",
        6 => "sat",
        _ => "sun",
    }
}

fn day_matches(entry: &str, wd: u32) -> bool {
    let l = entry.to_ascii_lowercase();
    match l.get(..3) {
        Some(p) => p == weekday_prefix(wd),
        None => false,
    }
}

/// True when `now_min` (minutes since midnight) falls inside the window
/// range, honoring midnight wraparound (start > end). A start equal to end
/// means the full day (all 1440 minutes).
pub fn range_contains(start: u32, end: u32, now_min: u32) -> bool {
    if start == end {
        return true;
    }
    if start < end {
        now_min >= start && now_min < end
    } else {
        now_min >= start || now_min < end
    }
}

/// True when the window is active at the given minute (and weekday).
/// Pure function over explicit values; used by tests.
pub fn window_active_at(w: &WindowCfg, tz: &str, now_min: u32) -> bool {
    let (Some(start), Some(end)) = (parse_hhmm(&w.start), parse_hhmm(&w.end)) else {
        return false;
    };
    if !range_contains(start, end, now_min) {
        return false;
    }
    if w.days.is_empty() {
        return true;
    }
    let wd = now_weekday_in(tz);
    w.days.iter().any(|d| day_matches(d, wd))
}

/// The timezone this window is evaluated in: its own `tz`, else the default.
fn window_tz<'a>(w: &'a WindowCfg, default_tz: &'a str) -> &'a str {
    w.tz.as_deref().unwrap_or(default_tz)
}

/// True when the window is active right now, evaluated in the window's own
/// timezone (window `tz`, else `default_tz`).
pub fn window_active(w: &WindowCfg, default_tz: &str) -> bool {
    let tz_name = window_tz(w, default_tz);
    window_active_at(w, tz_name, now_minutes_in(tz_name))
}

pub fn is_day_name(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    matches!(
        l.as_str(),
        "mon"
            | "tue"
            | "wed"
            | "thu"
            | "fri"
            | "sat"
            | "sun"
            | "monday"
            | "tuesday"
            | "wednesday"
            | "thursday"
            | "friday"
            | "saturday"
            | "sunday"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(start: &str, end: &str, days: Vec<&str>) -> WindowCfg {
        WindowCfg {
            start: start.into(),
            end: end.into(),
            mult: 1.0,
            days: days.into_iter().map(|s| s.to_string()).collect(),
            tz: None,
        }
    }

    #[test]
    fn parse_hhmm_values() {
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("08:30"), Some(510));
        assert_eq!(parse_hhmm("23:59"), Some(1439));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("123"), None);
    }

    #[test]
    fn normal_range() {
        assert!(range_contains(480, 720, 480)); // start inclusive
        assert!(range_contains(480, 720, 719));
        assert!(!range_contains(480, 720, 720)); // end exclusive
        assert!(!range_contains(480, 720, 479));
    }

    #[test]
    fn full_day_range() {
        // start == end means all 1440 minutes
        assert!(range_contains(480, 480, 0));
        assert!(range_contains(480, 480, 720));
        assert!(range_contains(480, 480, 1439));
        assert!(range_contains(0, 0, 500));
    }

    #[test]
    fn wrapping_range() {
        // 23:00 -> 01:00 wraps midnight
        assert!(range_contains(1380, 60, 23 * 60 + 30));
        assert!(range_contains(1380, 60, 0));
        assert!(range_contains(1380, 60, 59));
        assert!(!range_contains(1380, 60, 60));
        assert!(!range_contains(1380, 60, 720));
    }

    #[test]
    fn day_matching() {
        assert!(day_matches("Monday", 1));
        assert!(day_matches("mon", 1));
        assert!(day_matches("sat", 6));
        assert!(day_matches("Sun", 7));
        assert!(!day_matches("Sat", 1));
        assert!(!day_matches("Mon", 2));
        let monday = w("00:00", "23:59", vec!["Mon"]);
        assert!(range_contains(0, 1439, 720));
        assert!(!monday.days.is_empty());
    }

    #[test]
    fn day_name_validation() {
        assert!(is_day_name("Mon"));
        assert!(is_day_name("friday"));
        assert!(!is_day_name("monde"));
        assert!(!is_day_name("M"));
    }

    #[test]
    fn tz_parsing() {
        assert!(is_valid_tz("utc"));
        assert!(is_valid_tz("local"));
        assert!(is_valid_tz("Europe/Brussels"));
        assert!(!is_valid_tz("Mars/Olympus_Mons"));
        // unknown names fall back to UTC instead of panicking
        let m_bad = now_minutes_in("Mars/Olympus_Mons");
        let m_utc = now_minutes_in("utc");
        assert!((m_bad as i32 - m_utc as i32).abs() <= 1);
    }

    #[test]
    fn brussels_offset_is_one_or_two_hours_ahead_of_utc() {
        // Brussels is UTC+1 (CET) or UTC+2 (CEST); the offset flips twice a
        // year, so the test asserts the DST-aware invariant instead of a
        // fixed value.
        let diff = now_minutes_in("Europe/Brussels") as i32 - now_minutes_in("utc") as i32;
        let normalized = ((diff % 1440) + 1440) % 1440;
        assert!(
            normalized == 60 || normalized == 120,
            "diff was {normalized}"
        );
    }

    #[test]
    fn window_in_own_timezone() {
        // A UTC-pinned window 01:00-04:00 is active at 02:30 UTC regardless
        // of the default tz, and the tz picker resolves window.tz first.
        let mut win = w("01:00", "04:00", vec![]);
        win.tz = Some("UTC".into());
        assert_eq!(window_tz(&win, "Europe/Brussels"), "UTC");
        assert!(window_active_at(&win, "utc", 150)); // 02:30 UTC is inside
        assert!(!window_active_at(&win, "utc", 300)); // 05:00 UTC is outside
                                                      // A window without tz inherits the default.
        let plain = w("01:00", "04:00", vec![]);
        assert_eq!(window_tz(&plain, "Europe/Brussels"), "Europe/Brussels");
        assert_eq!(window_tz(&plain, "utc"), "utc");
    }
}
