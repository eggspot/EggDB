use crate::der::tag;

const SECONDS_PER_DAY: i64 = 86_400;
const SECONDS_PER_HOUR: i64 = 3_600;
const SECONDS_PER_MINUTE: i64 = 60;
const FIRST_UTC_TIME_YEAR: i64 = 1950;
const FIRST_GENERALIZED_TIME_YEAR: i64 = 2050;
const LAST_GENERALIZED_TIME_YEAR: i64 = 9999;
const UTC_TIME_DIGITS: usize = 12;
const GENERALIZED_TIME_DIGITS: usize = 14;
const UNIX_EPOCH_DAYS_FROM_MARCH_ZERO: i64 = 719_468;
const DAYS_PER_400_YEARS: i64 = 146_097;

pub(crate) fn parse_utc_time(text: &[u8]) -> Option<i64> {
    let [digits @ .., b'Z'] = text else {
        return None;
    };
    if digits.len() != UTC_TIME_DIGITS {
        return None;
    }
    let two_digit_year = decimal(&digits[..2])?;
    let century = if two_digit_year >= FIRST_UTC_TIME_YEAR % 100 {
        1900
    } else {
        2000
    };
    unix_seconds_from_fields(century + two_digit_year, &digits[2..])
}

pub(crate) fn parse_generalized_time(text: &[u8]) -> Option<i64> {
    let [digits @ .., b'Z'] = text else {
        return None;
    };
    if digits.len() != GENERALIZED_TIME_DIGITS {
        return None;
    }
    let year = decimal(&digits[..4])?;
    unix_seconds_from_fields(year, &digits[4..])
}

pub(crate) fn encode_certificate_time(unix_seconds: i64) -> Option<(u8, String)> {
    let (year, month, day) = civil_from_days(unix_seconds.div_euclid(SECONDS_PER_DAY));
    let second_of_day = unix_seconds.rem_euclid(SECONDS_PER_DAY);
    let hour = second_of_day / SECONDS_PER_HOUR;
    let minute = second_of_day % SECONDS_PER_HOUR / SECONDS_PER_MINUTE;
    let second = second_of_day % SECONDS_PER_MINUTE;
    let time_of_day = format!("{month:02}{day:02}{hour:02}{minute:02}{second:02}Z");
    if (FIRST_UTC_TIME_YEAR..FIRST_GENERALIZED_TIME_YEAR).contains(&year) {
        Some((tag::UTC_TIME, format!("{:02}{time_of_day}", year % 100)))
    } else if (0..=LAST_GENERALIZED_TIME_YEAR).contains(&year) {
        Some((tag::GENERALIZED_TIME, format!("{year:04}{time_of_day}")))
    } else {
        None
    }
}

fn unix_seconds_from_fields(year: i64, month_to_second: &[u8]) -> Option<i64> {
    let field = |start: usize| decimal(month_to_second.get(start..start + 2)?);
    let (month, day, hour, minute, second) =
        (field(0)?, field(2)?, field(4)?, field(6)?, field(8)?);
    let valid = (1..=12).contains(&month)
        && (1..=days_in_month(year, month)).contains(&day)
        && hour < 24
        && minute < 60
        && second < 60;
    valid.then(|| {
        days_from_civil(year, month, day) * SECONDS_PER_DAY
            + hour * SECONDS_PER_HOUR
            + minute * SECONDS_PER_MINUTE
            + second
    })
}

fn decimal(digits: &[u8]) -> Option<i64> {
    digits.iter().try_fold(0i64, |value, &digit| {
        digit
            .is_ascii_digit()
            .then(|| value * 10 + i64::from(digit - b'0'))
    })
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * DAYS_PER_400_YEARS + day_of_era - UNIX_EPOCH_DAYS_FROM_MARCH_ZERO
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + UNIX_EPOCH_DAYS_FROM_MARCH_ZERO;
    let era = days.div_euclid(DAYS_PER_400_YEARS);
    let day_of_era = days - era * DAYS_PER_400_YEARS;
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
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_time_reads_both_centuries() {
        assert_eq!(parse_utc_time(b"260101000000Z"), Some(1_767_225_600));
        assert_eq!(parse_utc_time(b"700101000000Z"), Some(0));
        assert_eq!(parse_utc_time(b"491231235959Z"), Some(2_524_607_999));
    }

    #[test]
    fn generalized_time_reads_four_digit_years() {
        assert_eq!(
            parse_generalized_time(b"20510101000000Z"),
            Some(2_556_144_000)
        );
        assert_eq!(parse_generalized_time(b"19700101000000Z"), Some(0));
    }

    #[test]
    fn invalid_times_are_refused() {
        for text in [
            &b"260101000000"[..],
            b"2601010000Z",
            b"261301000000Z",
            b"260230000000Z",
            b"250229000000Z",
            b"260101240000Z",
            b"260101006000Z",
            b"260101000060Z",
            b"2601010000+0Z",
            b"260101000000.5Z",
            b"",
            b"Z",
        ] {
            assert_eq!(parse_utc_time(text), None, "{text:?}");
        }
        assert_eq!(parse_generalized_time(b"260101000000Z"), None);
        assert_eq!(
            parse_generalized_time(b"20240229000000Z"),
            Some(1_709_164_800)
        );
    }

    #[test]
    fn encoding_picks_utc_time_before_2050() {
        assert_eq!(
            encode_certificate_time(1_767_225_600),
            Some((tag::UTC_TIME, "260101000000Z".to_string()))
        );
        assert_eq!(
            encode_certificate_time(2_556_144_000),
            Some((tag::GENERALIZED_TIME, "20510101000000Z".to_string()))
        );
        assert_eq!(
            encode_certificate_time(-1),
            Some((tag::UTC_TIME, "691231235959Z".to_string()))
        );
        assert_eq!(encode_certificate_time(i64::MAX / 2), None);
    }

    #[test]
    fn encoding_and_parsing_agree() {
        for seconds in [
            0,
            951_782_400,
            1_709_164_799,
            2_524_607_999,
            2_524_608_000,
            4_102_444_800,
        ] {
            let (time_tag, text) = encode_certificate_time(seconds).unwrap();
            let parsed = if time_tag == tag::UTC_TIME {
                parse_utc_time(text.as_bytes())
            } else {
                parse_generalized_time(text.as_bytes())
            };
            assert_eq!(parsed, Some(seconds), "{text}");
        }
    }
}
