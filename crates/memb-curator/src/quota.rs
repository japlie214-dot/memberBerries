//! Recognises "the model's usage quota is used up" in what a command-line model wrote, and
//! reads when it says the quota comes back. Nothing here is sent anywhere or stored: the
//! text is only matched, and the result is a yes/no and a delay.
use std::time::Duration;

/// Longest wait memb will ever record for one quota hit.
pub const MAX_BACKOFF: Duration = Duration::from_secs(7 * 24 * 3600);
/// Shortest wait (avoids a tight loop when a message says "0 seconds").
pub const MIN_BACKOFF: Duration = Duration::from_secs(60);

const MARKERS: [&str; 9] = [
    "usage limit",
    "quota",
    "rate limit",
    "rate-limit",
    "resource_exhausted",
    "too many requests",
    "code 429",
    "http 429",
    "status 429",
];

/// True when `text` (a command's stderr or error output) reports an exhausted quota or a
/// rate limit. Matching is case-insensitive.
pub fn is_quota_error(text: &str) -> bool {
    let t = text.to_lowercase();
    MARKERS.iter().any(|m| t.contains(m))
}

/// How long the message says to wait, if it says. Understands
/// `Resets in 3h28m26s`, `try again in 5 minutes`, `retry after 30 seconds` and
/// `try again at Nov 28th, 2026 9:23 PM` (the last in the machine's local time, whose
/// offset from UTC is `utc_offset_min`). `now_ms` is the current UNIX time in milliseconds.
pub fn parse_reset(text: &str, now_ms: u64, utc_offset_min: i32) -> Option<Duration> {
    let t = text.to_lowercase();
    for marker in ["resets in ", "reset in ", "try again in ", "retry in ", "retry after ", "retry-after: "] {
        if let Some(i) = t.find(marker) {
            if let Some(d) = parse_duration(&t[i + marker.len()..]) {
                return Some(d);
            }
        }
    }
    let i = t.find("try again at ")?;
    let target_ms = parse_local_time(&t[i + "try again at ".len()..])? - i64::from(utc_offset_min) * 60_000;
    Some(Duration::from_millis(u64::try_from(target_ms - now_ms as i64).unwrap_or(0)))
}

/// `3h28m26s`, `5 minutes`, `1 hour 5 min`, `30` (seconds): digits followed by a unit.
fn parse_duration(s: &str) -> Option<Duration> {
    let mut total = 0u64;
    let mut seen = false;
    let mut rest = s.trim_start();
    loop {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() || digits.len() > 9 {
            break;
        }
        let n: u64 = digits.parse().ok()?;
        let after = rest[digits.len()..].trim_start();
        let unit: String = after.chars().take_while(char::is_ascii_alphabetic).collect();
        let secs = match unit.as_str() {
            u if u.starts_with("ms") => 0,
            u if u.starts_with('h') => 3600,
            u if u.starts_with('m') => 60,
            u if u.starts_with('s') => 1,
            "" if !seen => 1, // a bare number is seconds
            _ => break,
        };
        total = total.saturating_add(n.saturating_mul(secs));
        seen = true;
        rest = after[unit.len()..].trim_start_matches([' ', ',']);
    }
    seen.then(|| Duration::from_secs(total))
}

/// `nov 28th, 2026 9:23 pm` as milliseconds since the UNIX epoch, reading the fields as UTC.
fn parse_local_time(s: &str) -> Option<i64> {
    let mut words = s.split(|c: char| c == ' ' || c == ',').filter(|w| !w.is_empty());
    let month_word = words.next()?;
    let month = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
        .iter()
        .position(|m| month_word.starts_with(m))? as i64
        + 1;
    let day: i64 = words.next()?.trim_end_matches(|c: char| c.is_ascii_alphabetic()).parse().ok()?;
    let year: i64 = words.next()?.parse().ok()?;
    let clock = words.next()?;
    let (h, m) = clock.split_once(':')?;
    let (mut hour, minute): (i64, i64) = (h.parse().ok()?, m.trim_end_matches(|c: char| c.is_ascii_alphabetic()).parse().ok()?);
    let meridiem = words.next().unwrap_or(m.trim_start_matches(|c: char| c.is_ascii_digit()));
    if meridiem.starts_with("pm") && hour < 12 {
        hour += 12;
    } else if meridiem.starts_with("am") && hour == 12 {
        hour = 0;
    }
    if !(1..=31).contains(&day) || !(2000..=2200).contains(&year) || hour > 23 || minute > 59 {
        return None;
    }
    // Days since 1970-01-01 (proleptic Gregorian; Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hour) * 60 + minute) * 60_000)
}

/// The wait to record after quota hit number `strikes` (0 for the first) when the
/// message gave no time: 15 minutes, doubling, at most 6 hours.
pub fn default_backoff(strikes: u32) -> Duration {
    Duration::from_secs(15 * 60 * (1u64 << strikes.min(5))).min(Duration::from_secs(6 * 3600))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_messages_are_recognised() {
        assert!(is_quota_error("ERROR: You’ve hit your usage limit. Upgrade to Pro"));
        assert!(is_quota_error("RESOURCE_EXHAUSTED (code 429): Individual quota reached."));
        assert!(!is_quota_error("error: could not resolve host"));
        assert!(!is_quota_error("model not found"));
    }

    #[test]
    fn relative_waits_are_read() {
        let m = |t: &str| parse_reset(t, 0, 0).map(|d| d.as_secs());
        assert_eq!(m("Resets in 3h28m26s."), Some(3 * 3600 + 28 * 60 + 26));
        assert_eq!(m("please try again in 5 minutes"), Some(300));
        assert_eq!(m("retry after 30 seconds"), Some(30));
        assert_eq!(m("Retry-After: 120"), Some(120));
        assert_eq!(m("quota reached"), None);
    }

    #[test]
    fn the_codex_absolute_time_uses_the_local_offset() {
        // 2026-11-28 21:23 local at UTC+7 is 14:23 UTC = 1795875780 s.
        let msg = "try again at Nov 28th, 2026 9:23 PM.";
        let at = parse_reset(msg, 0, 7 * 60).unwrap().as_secs();
        assert_eq!(at, 1_795_875_780);
        // From one hour earlier the wait is one hour.
        let now = (1_795_875_780 - 3600) * 1000;
        assert_eq!(parse_reset(msg, now, 7 * 60).unwrap().as_secs(), 3600);
        // A time already past is a zero wait, not an error.
        assert_eq!(parse_reset(msg, 1_800_000_000_000, 7 * 60), Some(Duration::ZERO));
    }

    #[test]
    fn default_backoff_doubles_to_a_cap() {
        let m = |n| default_backoff(n).as_secs() / 60;
        assert_eq!((m(0), m(1), m(2), m(3), m(9)), (15, 30, 60, 120, 360));
    }
}
