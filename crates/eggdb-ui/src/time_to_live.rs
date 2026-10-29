use std::time::Duration;

const UNITS: [(&str, u64); 5] = [
    ("d", 86_400_000),
    ("h", 3_600_000),
    ("m", 60_000),
    ("s", 1_000),
    ("ms", 1),
];
const SECONDS_PER_DAY: u64 = 86_400;
const SECONDS_PER_HOUR: u64 = 3_600;
const SECONDS_PER_MINUTE: u64 = 60;
const MOST_DAYS_SHOWN: u64 = 9_999;

/// Why a text is not a time to live, from [`parse_time_to_live`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeToLiveError {
    /// The text is not a time such as `30s`, `5m`, `1h30m`, `7d` or `1500ms`.
    NotATime,
    /// The time is zero, which would remove the key at once.
    Zero,
}

/// Reads a time to live as people type it and as [`format_time_left`] prints it: counts with a
/// unit, `ms`, `s`, `m`, `h` or `d`, joined from the largest unit down, each unit at most once,
/// with no spaces, decimals or bare numbers: `1500ms`, `30s`, `5m`, `1h30m`, `7d`, `1d12h`.
pub fn parse_time_to_live(text: &str) -> Result<Duration, TimeToLiveError> {
    let bytes = text.as_bytes();
    if bytes.is_empty() {
        return Err(TimeToLiveError::NotATime);
    }
    let mut position = 0;
    let mut smallest_used = None;
    let mut millis = 0u64;
    while position < bytes.len() {
        let digits_start = position;
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        let digits = &text[digits_start..position];
        let rest = &text[position..];
        let Some(rank) = UNITS.iter().enumerate().rev().find_map(|(rank, (unit, _))| {
            rest.starts_with(unit)
                .then_some(rank)
                .filter(|_| !(*unit == "m" && rest.starts_with("ms")))
        }) else {
            return Err(TimeToLiveError::NotATime);
        };
        if digits.is_empty() || smallest_used.is_some_and(|used| rank <= used) {
            return Err(TimeToLiveError::NotATime);
        }
        smallest_used = Some(rank);
        let (unit, unit_millis) = UNITS[rank];
        position += unit.len();
        let count: u64 = digits.parse().map_err(|_| TimeToLiveError::NotATime)?;
        millis = count
            .checked_mul(unit_millis)
            .and_then(|part| millis.checked_add(part))
            .ok_or(TimeToLiveError::NotATime)?;
    }
    if millis == 0 {
        return Err(TimeToLiveError::Zero);
    }
    Ok(Duration::from_millis(millis))
}

/// A remaining time in full, in the grammar [`parse_time_to_live`] reads: milliseconds under one
/// second (`412ms`, and `1ms` for less), otherwise whole seconds rounded down, split into days,
/// hours, minutes and seconds with zero parts left out (`29s`, `1h4m12s`, `7d`, `2d5s`).
pub fn format_time_left(left: Duration) -> String {
    if left < Duration::from_secs(1) {
        return format!("{}ms", left.as_millis().max(1));
    }
    let seconds = left.as_secs();
    let parts = [
        (seconds / SECONDS_PER_DAY, "d"),
        (seconds % SECONDS_PER_DAY / SECONDS_PER_HOUR, "h"),
        (seconds % SECONDS_PER_HOUR / SECONDS_PER_MINUTE, "m"),
        (seconds % SECONDS_PER_MINUTE, "s"),
    ];
    parts
        .iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, unit)| format!("{count}{unit}"))
        .collect()
}

/// A remaining time in its largest unit alone, rounded down, for narrow columns: `29s`, `12m`,
/// `1h`, `7d`, `412ms`, and `>27y` past 9,999 days.
pub fn format_time_left_short(left: Duration) -> String {
    let seconds = left.as_secs();
    if seconds / SECONDS_PER_DAY > MOST_DAYS_SHOWN {
        ">27y".to_owned()
    } else if seconds >= SECONDS_PER_DAY {
        format!("{}d", seconds / SECONDS_PER_DAY)
    } else if seconds >= SECONDS_PER_HOUR {
        format!("{}h", seconds / SECONDS_PER_HOUR)
    } else if seconds >= SECONDS_PER_MINUTE {
        format!("{}m", seconds / SECONDS_PER_MINUTE)
    } else if seconds >= 1 {
        format!("{seconds}s")
    } else {
        format!("{}ms", left.as_millis().max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::{TimeToLiveError, format_time_left, format_time_left_short, parse_time_to_live};
    use std::time::Duration;

    fn seconds(count: u64) -> Duration {
        Duration::from_secs(count)
    }

    #[test]
    fn times_join_units_from_the_largest_down() {
        for (text, expected) in [
            ("1500ms", Duration::from_millis(1500)),
            ("30s", seconds(30)),
            ("5m", seconds(300)),
            ("2h", seconds(7200)),
            ("7d", seconds(7 * 86_400)),
            ("1h30m", seconds(5400)),
            ("1d12h", seconds(129_600)),
            ("1m1s1ms", Duration::from_millis(61_001)),
            ("0h5m", seconds(300)),
        ] {
            assert_eq!(parse_time_to_live(text), Ok(expected), "{text}");
        }
    }

    #[test]
    fn bare_numbers_spaces_decimals_and_repeats_are_not_times() {
        for text in [
            "", "30", "s", "1.5s", "1 s", " 1s", "1s ", "30m5h", "1s1s", "1w", "1y", "5M", "-1s",
            "1sms", "99999999999999999999d", "1msm",
        ] {
            assert_eq!(parse_time_to_live(text), Err(TimeToLiveError::NotATime), "{text}");
        }
        assert_eq!(parse_time_to_live("0s"), Err(TimeToLiveError::Zero));
        assert_eq!(parse_time_to_live("0h0m"), Err(TimeToLiveError::Zero));
    }

    #[test]
    fn a_remaining_time_prints_in_the_grammar_it_is_read_in() {
        for (left, expected) in [
            (Duration::from_millis(412), "412ms"),
            (Duration::from_micros(3), "1ms"),
            (Duration::from_millis(29_999), "29s"),
            (seconds(3852), "1h4m12s"),
            (seconds(7 * 86_400), "7d"),
            (seconds(2 * 86_400 + 5), "2d5s"),
        ] {
            assert_eq!(format_time_left(left), expected);
            assert!(parse_time_to_live(expected).is_ok(), "{expected}");
        }
    }

    #[test]
    fn a_short_remaining_time_is_its_largest_unit() {
        for (left, expected) in [
            (Duration::from_millis(412), "412ms"),
            (seconds(29), "29s"),
            (seconds(12 * 60 + 59), "12m"),
            (seconds(3852), "1h"),
            (seconds(7 * 86_400 + 3600), "7d"),
            (seconds(10_000 * 86_400), ">27y"),
        ] {
            assert_eq!(format_time_left_short(left), expected);
        }
    }
}
