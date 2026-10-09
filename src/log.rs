//! One JSON object per line on stdout, with an RFC 3339 timestamp first.

use serde_json::Value;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub type Logger = Arc<dyn Fn(Value) + Send + Sync>;

pub fn stdout_logger() -> Logger {
    Arc::new(|entry: Value| println!("{}", format_line(&entry, SystemTime::now())))
}

pub fn null_logger() -> Logger {
    Arc::new(|_| {})
}

fn format_line(entry: &Value, now: SystemTime) -> String {
    let mut entry = entry.clone();
    if let Value::Object(map) = &mut entry {
        map.retain(|_, v| !v.is_null());
    }
    let body = entry.to_string();
    let ts = timestamp(now);
    match body.strip_prefix('{') {
        Some("}") | None => format!("{{\"ts\":\"{ts}\"}}"),
        Some(rest) => format!("{{\"ts\":\"{ts}\",{rest}"),
    }
}

fn timestamp(now: SystemTime) -> String {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        since.subsec_millis()
    )
}

/// Days since 1970-01-01 to (year, month, day), from Howard Hinnant's date algorithms.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn formats_timestamps() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let t = UNIX_EPOCH + Duration::from_millis(1_791_573_799_123);
        assert_eq!(timestamp(t), "2026-10-09T19:23:19.123Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(&timestamp(leap)[..10], "2000-02-29");
    }

    #[test]
    fn timestamp_comes_first() {
        let line = format_line(&serde_json::json!({"event": "x", "unset": null}), UNIX_EPOCH);
        assert_eq!(line, r#"{"ts":"1970-01-01T00:00:00.000Z","event":"x"}"#);
    }
}
