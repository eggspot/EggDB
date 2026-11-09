use std::time::Duration;

const MICROS_PER_SECOND: u64 = 1_000_000;
const SECONDS_PER_DAY: u64 = 86_400;

/// `unix_micros`, microseconds since 1970-01-01 UTC, as an RFC 3339 time in UTC with
/// microseconds, such as `2026-01-01T00:00:00.000000Z`.
pub fn utc_rfc3339(unix_micros: u64) -> String {
    format!(
        "{}.{:06}Z",
        date_and_time(unix_micros / MICROS_PER_SECOND),
        unix_micros % MICROS_PER_SECOND
    )
}

/// When a key with `time_left` to live expires, seen at `now_unix_micros`: an RFC 3339 time in
/// UTC to the millisecond, such as `2026-10-30T09:44:07.123Z`. It is the `expires_at` of
/// `eggdb put --json`, `eggdb ttl --json`, `eggdb expire --json` and the MCP tools `put` and
/// `expire` (D346).
pub fn expires_at(now_unix_micros: u64, time_left: Duration) -> String {
    let left = u64::try_from(time_left.as_micros()).unwrap_or(u64::MAX);
    let at = now_unix_micros.saturating_add(left);
    format!(
        "{}.{:03}Z",
        date_and_time(at / MICROS_PER_SECOND),
        at % MICROS_PER_SECOND / 1000
    )
}

fn date_and_time(unix_seconds: u64) -> String {
    let (year, month, day) = civil_from_days(unix_seconds / SECONDS_PER_DAY);
    let of_day = unix_seconds % SECONDS_PER_DAY;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60
    )
}

fn civil_from_days(days_since_epoch: u64) -> (u64, u64, u64) {
    let shifted = days_since_epoch + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::{expires_at, utc_rfc3339};
    use std::time::Duration;

    #[test]
    fn times_are_utc_with_microseconds() {
        assert_eq!(utc_rfc3339(0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(
            utc_rfc3339(1_767_225_600_000_000),
            "2026-01-01T00:00:00.000000Z"
        );
        assert_eq!(
            utc_rfc3339(951_782_400_000_001),
            "2000-02-29T00:00:00.000001Z"
        );
        assert_eq!(
            utc_rfc3339(1_761_470_047_123_456),
            "2025-10-26T09:14:07.123456Z"
        );
    }

    #[test]
    fn an_expiry_time_is_now_plus_the_time_left_to_the_millisecond() {
        let now = 1_761_470_047_123_456;
        assert_eq!(
            expires_at(now, Duration::from_secs(60)),
            "2025-10-26T09:15:07.123Z"
        );
        assert_eq!(
            expires_at(now, Duration::from_millis(877)),
            "2025-10-26T09:14:08.000Z"
        );
        assert_eq!(expires_at(0, Duration::ZERO), "1970-01-01T00:00:00.000Z");
        let far = expires_at(now, Duration::MAX);
        assert!(far.ends_with('Z') && far.contains('T'), "{far}");
    }
}
