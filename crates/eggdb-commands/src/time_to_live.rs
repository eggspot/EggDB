use std::time::Duration;

const UNITS: [(&str, u64); 5] = [
    ("d", 86_400_000),
    ("h", 3_600_000),
    ("m", 60_000),
    ("s", 1_000),
    ("ms", 1),
];

/// Why a text is not a time to live, from [`parse_time_to_live`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeToLiveError {
    /// The text is not a time such as `30s`, `5m`, `1h30m`, `7d` or `1500ms`.
    NotATime,
    /// The time is zero, which would remove the key at once.
    Zero,
}

/// Reads a time to live as people type it (`eggdb put --ttl`, the MCP tool `put`, `eggdb ui`):
/// counts with a unit, `ms`, `s`, `m`, `h` or `d`, joined from the largest unit down, each unit
/// at most once, with no spaces, decimals or bare numbers: `1500ms`, `30s`, `5m`, `1h30m`, `7d`,
/// `1d12h`.
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
        let Some(rank) = UNITS
            .iter()
            .enumerate()
            .rev()
            .find_map(|(rank, (unit, _))| {
                rest.starts_with(unit)
                    .then_some(rank)
                    .filter(|_| !(*unit == "m" && rest.starts_with("ms")))
            })
        else {
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

#[cfg(test)]
mod tests {
    use super::{TimeToLiveError, parse_time_to_live};
    use std::time::Duration;

    #[test]
    fn times_join_units_from_the_largest_down_and_nothing_else_is_a_time() {
        assert_eq!(
            parse_time_to_live("1500ms"),
            Ok(Duration::from_millis(1500))
        );
        assert_eq!(parse_time_to_live("1h30m"), Ok(Duration::from_secs(5400)));
        assert_eq!(
            parse_time_to_live("7d"),
            Ok(Duration::from_secs(7 * 86_400))
        );
        for text in [
            "",
            "30",
            "1.5s",
            "1 s",
            "30m5h",
            "1w",
            "-1s",
            "99999999999999999999d",
        ] {
            assert_eq!(
                parse_time_to_live(text),
                Err(TimeToLiveError::NotATime),
                "{text}"
            );
        }
        assert_eq!(parse_time_to_live("0s"), Err(TimeToLiveError::Zero));
    }
}
