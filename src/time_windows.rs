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

fn now_in(tz: &str) -> chrono::DateTime<chrono::FixedOffset> {
    if tz.eq_ignore_ascii_case("local") {
        Local::now().into()
    } else {
        Utc::now().into()
    }
}

/// Current time as minutes since midnight in the configured timezone.
pub fn now_minutes(tz: &str) -> u32 {
    now_in(tz).time().num_seconds_from_midnight() / 60
}

/// Current ISO weekday: 1 = Monday .. 7 = Sunday.
pub fn now_weekday_iso(tz: &str) -> u32 {
    now_in(tz).weekday().num_days_from_monday() + 1
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
/// range, honoring midnight wraparound (start > end).
pub fn range_contains(start: u32, end: u32, now_min: u32) -> bool {
    if start <= end {
        now_min >= start && now_min < end
    } else {
        now_min >= start || now_min < end
    }
}

/// True when the window is active at the given minute (and weekday).
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
    let wd = now_weekday_iso(tz);
    w.days.iter().any(|d| day_matches(d, wd))
}

/// True when the window is active right now.
pub fn window_active(w: &WindowCfg, tz: &str) -> bool {
    window_active_at(w, tz, now_minutes(tz))
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
    fn wrapping_range() {
        // 23:00 -> 01:00 wraps midnight
        assert!(range_contains(1380, 60, 23 * 60 + 30));
        assert!(range_contains(1380, 60, 0));
        assert!(range_contains(1380, 60, 59));
        assert!(!range_contains(1380, 60, 60));
        assert!(!range_contains(1380, 60, 720));
    }

    #[test]
    fn weekday_filter() {
        // day_matches is deterministic; window_active_at with a day filter
        // depends on the real clock, so it is covered via the range + day
        // primitives here.
        assert!(day_matches("Monday", 1));
        assert!(day_matches("mon", 1));
        assert!(day_matches("sat", 6));
        assert!(day_matches("Sun", 7));
        assert!(!day_matches("Sat", 1));
        assert!(!day_matches("Mon", 2));
        // range check for a full-day window at 12:00
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
}
