use super::{Failure, Run, fatal, node};
use crate::datetime::{self, MICROS_PER_SECOND};
use crate::error::{SqlError, SqlState};
use crate::json::Json;
use std::cmp::Ordering;

const MARKER: &str = "\u{0}datetime";
const MAX_PRECISION: u8 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Temporal {
    DateTime,
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
}

impl Temporal {
    pub(super) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "datetime" => Self::DateTime,
            "date" => Self::Date,
            "time" => Self::Time,
            "time_tz" => Self::TimeTz,
            "timestamp" => Self::Timestamp,
            "timestamp_tz" => Self::TimestampTz,
            _ => return None,
        })
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::DateTime => "datetime",
            Self::Date => "date",
            Self::Time => "time",
            Self::TimeTz => "time_tz",
            Self::Timestamp => "timestamp",
            Self::TimestampTz => "timestamp_tz",
        }
    }

    pub(super) fn takes_precision(self) -> bool {
        matches!(
            self,
            Self::Time | Self::TimeTz | Self::Timestamp | Self::TimestampTz
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum TemporalArgument {
    Template(String),
    Precision(u8),
}

pub(super) fn precision_argument(method: Temporal, text: &str) -> Result<u8, SqlError> {
    let invalid = || {
        SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!(
                "time precision of jsonpath item method .{}() is invalid",
                method.name()
            ),
        )
    };
    let precision: u64 = text.parse().map_err(|_| invalid())?;
    Ok(u8::try_from(precision.min(u64::from(MAX_PRECISION))).unwrap_or(MAX_PRECISION))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Moment {
    Date(i32),
    Time(i64),
    TimeTz(i64, i32),
    Timestamp(i64),
    TimestampTz(i64, i32),
}

fn soft(message: impl Into<String>) -> Failure {
    Failure::Suppressible(SqlError::new(
        SqlState::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION,
        message,
    ))
}

fn soft_error(error: SqlError) -> Failure {
    Failure::Suppressible(error)
}

fn iso_offset(offset: i32) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let magnitude = offset.unsigned_abs();
    let (hours, minutes, seconds) = (magnitude / 3600, magnitude % 3600 / 60, magnitude % 60);
    if seconds > 0 {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}")
    }
}

fn iso_local(micros: i64) -> String {
    datetime::format_timestamp(micros).replacen(' ', "T", 1)
}

impl Moment {
    pub(super) fn to_json(self) -> Json {
        let (kind, value, offset) = match self {
            Self::Date(days) => ("date", i64::from(days), 0),
            Self::Time(micros) => ("time", micros, 0),
            Self::TimeTz(micros, offset) => ("timetz", micros, offset),
            Self::Timestamp(micros) => ("timestamp", micros, 0),
            Self::TimestampTz(micros, offset) => ("timestamptz", micros, offset),
        };
        Json::Object(vec![
            (MARKER.to_owned(), node(Json::String(kind.to_owned()))),
            ("value".to_owned(), node(Json::Number(value.to_string()))),
            ("offset".to_owned(), node(Json::Number(offset.to_string()))),
        ])
    }

    pub(super) fn of(json: &Json) -> Option<Self> {
        let Json::Object(members) = json else {
            return None;
        };
        let [(marker, kind), (_, value), (_, offset)] = members.as_slice() else {
            return None;
        };
        if marker != MARKER {
            return None;
        }
        let (Json::String(kind), Json::Number(value), Json::Number(offset)) =
            (&kind.value, &value.value, &offset.value)
        else {
            return None;
        };
        let value: i64 = value.parse().ok()?;
        let offset: i32 = offset.parse().ok()?;
        Some(match kind.as_str() {
            "date" => Self::Date(i32::try_from(value).ok()?),
            "time" => Self::Time(value),
            "timetz" => Self::TimeTz(value, offset),
            "timestamp" => Self::Timestamp(value),
            "timestamptz" => Self::TimestampTz(value, offset),
            _ => return None,
        })
    }

    pub(super) fn type_name(self) -> &'static str {
        match self {
            Self::Date(_) => "date",
            Self::Time(_) => "time without time zone",
            Self::TimeTz(..) => "time with time zone",
            Self::Timestamp(_) => "timestamp without time zone",
            Self::TimestampTz(..) => "timestamp with time zone",
        }
    }

    fn kind_name(self) -> &'static str {
        match self {
            Self::Date(_) => "date",
            Self::Time(_) => "time",
            Self::TimeTz(..) => "timetz",
            Self::Timestamp(_) => "timestamp",
            Self::TimestampTz(..) => "timestamptz",
        }
    }

    pub(super) fn text(self) -> String {
        match self {
            Self::Date(days) => datetime::format_date(days),
            Self::Time(micros) => datetime::format_time(micros),
            Self::TimeTz(micros, offset) => {
                format!("{}{}", datetime::format_time(micros), iso_offset(offset))
            }
            Self::Timestamp(micros) => iso_local(micros),
            Self::TimestampTz(micros, _) if datetime::is_infinite_timestamp(micros) => {
                datetime::format_timestamp(micros)
            }
            Self::TimestampTz(micros, offset) => format!(
                "{}{}",
                iso_local(datetime::to_local(micros, offset)),
                iso_offset(offset)
            ),
        }
    }
}

/// Replaces a datetime item by its ISO text, the form it takes in a `jsonb` result.
pub(super) fn plain(json: Json) -> Json {
    match Moment::of(&json) {
        Some(moment) => Json::String(moment.text()),
        None => json,
    }
}

struct IsoShape<'t> {
    date: Option<&'t str>,
    time: Option<&'t str>,
    offset: Option<i32>,
}

fn digit_run(bytes: &[u8], at: usize, least: usize, most: usize) -> Option<usize> {
    let count = bytes
        .get(at..)?
        .iter()
        .take(most)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    (count >= least).then_some(at + count)
}

fn byte_is(bytes: &[u8], at: usize, wanted: u8) -> bool {
    bytes.get(at) == Some(&wanted)
}

fn date_end(bytes: &[u8]) -> Option<usize> {
    let year = digit_run(bytes, 0, 4, 4)?;
    if !byte_is(bytes, year, b'-') {
        return None;
    }
    let month = digit_run(bytes, year + 1, 2, 2)?;
    if !byte_is(bytes, month, b'-') {
        return None;
    }
    digit_run(bytes, month + 1, 2, 2)
}

fn time_end(bytes: &[u8], at: usize) -> Option<usize> {
    let hour = digit_run(bytes, at, 1, 2)?;
    if !byte_is(bytes, hour, b':') {
        return None;
    }
    let minute = digit_run(bytes, hour + 1, 2, 2)?;
    if !byte_is(bytes, minute, b':') {
        return None;
    }
    let second = digit_run(bytes, minute + 1, 2, 2)?;
    if byte_is(bytes, second, b'.') {
        return digit_run(bytes, second + 1, 1, 6);
    }
    Some(second)
}

fn zone_offset(text: &str, bytes: &[u8], at: usize) -> Option<i32> {
    let negative = match bytes.get(at)? {
        b'+' => false,
        b'-' => true,
        _ => return None,
    };
    let hours_end = digit_run(bytes, at + 1, 1, 2)?;
    let hours: i32 = text.get(at + 1..hours_end)?.parse().ok()?;
    let mut minutes = 0;
    let mut end = hours_end;
    if byte_is(bytes, hours_end, b':') {
        end = digit_run(bytes, hours_end + 1, 2, 2)?;
        minutes = text.get(hours_end + 1..end)?.parse().ok()?;
    }
    if end != bytes.len() || hours > 15 || minutes > 59 {
        return None;
    }
    let seconds = hours * 3600 + minutes * 60;
    Some(if negative { -seconds } else { seconds })
}

fn iso_shape(text: &str) -> Option<IsoShape<'_>> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut date = None;
    if let Some(end) = date_end(bytes) {
        date = Some(text.get(..end)?);
        if end == bytes.len() {
            return Some(IsoShape {
                date,
                time: None,
                offset: None,
            });
        }
        if !matches!(bytes.get(end), Some(b' ' | b'T')) {
            return None;
        }
        at = end + 1;
    }
    let end = time_end(bytes, at)?;
    let time = Some(text.get(at..end)?);
    let offset = if end == bytes.len() {
        None
    } else {
        Some(zone_offset(text, bytes, end)?)
    };
    Some(IsoShape { date, time, offset })
}

fn not_recognized(method: Temporal, text: &str) -> Failure {
    Failure::Suppressible(
        SqlError::new(
            SqlState::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION,
            format!("{} format is not recognized: \"{text}\"", method.name()),
        )
        .with_hint("Use a datetime template argument to specify the input data format."),
    )
}

fn offset_micros(offset: i32) -> i64 {
    i64::from(offset) * MICROS_PER_SECOND
}

fn recognized(method: Temporal, text: &str) -> Run<Moment> {
    let Some(shape) = iso_shape(text) else {
        return Err(not_recognized(method, text));
    };
    let parsed = match (shape.date, shape.time, shape.offset) {
        (Some(date), None, _) => datetime::parse_date(date).map(Moment::Date),
        (Some(date), Some(time), offset) => {
            datetime::parse_timestamp(&format!("{date} {time}"), false).map(|local| match offset {
                Some(offset) => {
                    Moment::TimestampTz(local.saturating_sub(offset_micros(offset)), offset)
                }
                None => Moment::Timestamp(local),
            })
        }
        (None, Some(time), offset) => {
            datetime::parse_time_of_day(time, false).map(|(micros, _)| match offset {
                Some(offset) => Moment::TimeTz(micros, offset),
                None => Moment::Time(micros),
            })
        }
        (None, None, _) => return Err(not_recognized(method, text)),
    };
    parsed.map_err(|_| not_recognized(method, text))
}

fn templated(text: &str, template: &str) -> Run<Moment> {
    let read = crate::pg_format::template_moment(text, template).map_err(soft_error)?;
    let offset = match read.offset {
        Some(offset) => Some(
            i32::try_from(offset)
                .map_err(|_| soft(format!("time zone offset in \"{text}\" is out of range")))?,
        ),
        None => None,
    };
    let time = datetime::time_of_timestamp(read.local);
    Ok(match (read.has_date, read.has_time, offset) {
        (true, true, Some(offset)) => {
            Moment::TimestampTz(read.local.saturating_sub(offset_micros(offset)), offset)
        }
        (true, true, None) => Moment::Timestamp(read.local),
        (true, false, _) => Moment::Date(datetime::timestamp_to_date(read.local)),
        (false, true, Some(offset)) => Moment::TimeTz(time, offset),
        (false, true, None) => Moment::Time(time),
        (false, false, _) => return Err(not_recognized(Temporal::DateTime, text)),
    })
}

fn needs_zone(from: Moment, to: &str) -> Failure {
    fatal(
        SqlError::new(
            SqlState::INVALID_ARGUMENT_FOR_SQL_JSON_DATETIME_FUNCTION,
            format!(
                "cannot convert value from {} to {to} without time zone usage",
                from.kind_name()
            ),
        )
        .with_hint("Use *_tz() function for time zone support."),
    )
}

fn local_to_utc(local: i64) -> Run<(i64, i32)> {
    let utc = datetime::local_to_utc(local).map_err(Failure::Suppressible)?;
    Ok((utc, datetime::session_offset_at(utc)))
}

fn utc_to_local(utc: i64) -> Run<i64> {
    datetime::utc_to_local(utc).map_err(Failure::Suppressible)
}

fn cannot_convert(method: Temporal, from: Moment) -> Failure {
    soft(format!(
        "jsonpath item method .{}() cannot convert a value of type {}",
        method.name(),
        from.type_name()
    ))
}

fn converted(method: Temporal, moment: Moment, with_zone: bool) -> Run<Moment> {
    let date_timestamp = |days: i32| datetime::date_to_timestamp(days).map_err(soft_error);
    Ok(match (method, moment) {
        (Temporal::DateTime, moment) => moment,
        (Temporal::Date, Moment::Date(days)) => Moment::Date(days),
        (Temporal::Date, Moment::Timestamp(local)) => {
            Moment::Date(datetime::timestamp_to_date(local))
        }
        (Temporal::Date, Moment::TimestampTz(utc, _)) if with_zone => {
            Moment::Date(datetime::timestamp_to_date(utc_to_local(utc)?))
        }
        (Temporal::Date, Moment::TimestampTz(..)) => return Err(needs_zone(moment, "date")),
        (Temporal::Time, Moment::Time(micros) | Moment::TimeTz(micros, _)) => Moment::Time(micros),
        (Temporal::Time, Moment::Timestamp(local)) => {
            Moment::Time(datetime::time_of_timestamp(local))
        }
        (Temporal::Time, Moment::TimestampTz(utc, _)) if with_zone => {
            Moment::Time(datetime::time_of_timestamp(utc_to_local(utc)?))
        }
        (Temporal::Time, Moment::TimestampTz(..)) => return Err(needs_zone(moment, "time")),
        (Temporal::TimeTz, Moment::TimeTz(micros, offset)) => Moment::TimeTz(micros, offset),
        (Temporal::TimeTz, Moment::Time(micros)) if with_zone => {
            Moment::TimeTz(micros, datetime::session_offset_now())
        }
        (Temporal::TimeTz, Moment::Time(_)) => return Err(needs_zone(moment, "timetz")),
        (Temporal::TimeTz, Moment::TimestampTz(utc, offset)) => Moment::TimeTz(
            datetime::time_of_timestamp(datetime::to_local(utc, offset)),
            offset,
        ),
        (Temporal::Timestamp, Moment::Timestamp(local)) => Moment::Timestamp(local),
        (Temporal::Timestamp, Moment::Date(days)) => Moment::Timestamp(date_timestamp(days)?),
        (Temporal::Timestamp, Moment::TimestampTz(utc, _)) if with_zone => {
            Moment::Timestamp(utc_to_local(utc)?)
        }
        (Temporal::Timestamp, Moment::TimestampTz(..)) => {
            return Err(needs_zone(moment, "timestamp"));
        }
        (Temporal::TimestampTz, Moment::TimestampTz(utc, offset)) => {
            Moment::TimestampTz(utc, offset)
        }
        (Temporal::TimestampTz, Moment::Timestamp(local)) if with_zone => {
            let (utc, offset) = local_to_utc(local)?;
            Moment::TimestampTz(utc, offset)
        }
        (Temporal::TimestampTz, Moment::Date(days)) if with_zone => {
            let (utc, offset) = local_to_utc(date_timestamp(days)?)?;
            Moment::TimestampTz(utc, offset)
        }
        (Temporal::TimestampTz, Moment::Timestamp(_) | Moment::Date(_)) => {
            return Err(needs_zone(moment, "timestamptz"));
        }
        (method, moment) => return Err(cannot_convert(method, moment)),
    })
}

fn rounded(moment: Moment, precision: u8) -> Run<Moment> {
    let timestamp = |micros: i64| {
        datetime::round_to_precision(micros, precision).map_err(Failure::Suppressible)
    };
    Ok(match moment {
        Moment::Time(micros) => Moment::Time(datetime::round_time(micros, precision)),
        Moment::TimeTz(micros, offset) => {
            Moment::TimeTz(datetime::round_time(micros, precision), offset)
        }
        Moment::Timestamp(micros) => Moment::Timestamp(timestamp(micros)?),
        Moment::TimestampTz(micros, offset) => Moment::TimestampTz(timestamp(micros)?, offset),
        Moment::Date(days) => Moment::Date(days),
    })
}

/// Applies one of the datetime item methods to `item`.
pub(super) fn apply(
    method: Temporal,
    argument: Option<&TemporalArgument>,
    item: &Json,
    with_zone: bool,
) -> Run<Json> {
    let moment = match (Moment::of(item), item) {
        (Some(moment), _) if method != Temporal::DateTime => moment,
        (None, Json::String(text)) => match argument {
            Some(TemporalArgument::Template(template)) => templated(text, template)?,
            _ => recognized(method, text)?,
        },
        _ => {
            let accepted = if method == Temporal::DateTime {
                "a string"
            } else {
                "a string or a datetime value"
            };
            return Err(soft(format!(
                "jsonpath item method .{}() can only be applied to {accepted}",
                method.name()
            )));
        }
    };
    let mut moment = converted(method, moment, with_zone)?;
    if let Some(TemporalArgument::Precision(precision)) = argument {
        moment = rounded(moment, *precision)?;
    }
    Ok(moment.to_json())
}

fn naive_local(moment: Moment) -> Run<Option<i64>> {
    Ok(match moment {
        Moment::Date(days) => {
            Some(datetime::date_to_timestamp(days).map_err(Failure::Suppressible)?)
        }
        Moment::Timestamp(local) => Some(local),
        _ => None,
    })
}

/// Orders two datetime items as PostgreSQL's jsonpath does: dates, time stamps and times with
/// a time zone each convert to the zoned type when compared with it, which needs the session's
/// time zone and so only the `_tz` functions do; a date and a time are not comparable.
pub(super) fn compare(left: Moment, right: Moment, with_zone: bool) -> Run<Option<Ordering>> {
    if let (Some(first), Some(second)) = (naive_local(left)?, naive_local(right)?) {
        return Ok(Some(first.cmp(&second)));
    }
    let zoned = |moment: Moment| -> Run<Option<i64>> {
        match moment {
            Moment::TimestampTz(utc, _) => Ok(Some(utc)),
            Moment::Date(_) | Moment::Timestamp(_) => {
                if !with_zone {
                    return Err(needs_zone(moment, "timestamptz"));
                }
                let local = naive_local(moment)?.unwrap_or(0);
                Ok(Some(local_to_utc(local)?.0))
            }
            _ => Ok(None),
        }
    };
    let timestamps = !matches!(left, Moment::Time(_) | Moment::TimeTz(..))
        && !matches!(right, Moment::Time(_) | Moment::TimeTz(..));
    if timestamps {
        return match (zoned(left)?, zoned(right)?) {
            (Some(first), Some(second)) => Ok(Some(first.cmp(&second))),
            _ => Ok(None),
        };
    }
    match (left, right) {
        (Moment::Time(first), Moment::Time(second)) => Ok(Some(first.cmp(&second))),
        (Moment::TimeTz(first, first_offset), Moment::TimeTz(second, second_offset)) => Ok(Some(
            datetime::time_tz_utc(first, first_offset)
                .cmp(&datetime::time_tz_utc(second, second_offset))
                .then_with(|| second_offset.cmp(&first_offset)),
        )),
        (Moment::Time(_), Moment::TimeTz(..)) | (Moment::TimeTz(..), Moment::Time(_)) => {
            if !with_zone {
                let naive = if matches!(left, Moment::Time(_)) {
                    left
                } else {
                    right
                };
                return Err(needs_zone(naive, "timetz"));
            }
            let as_zoned = |moment: Moment| match moment {
                Moment::Time(micros) => {
                    datetime::time_tz_utc(micros, datetime::session_offset_now())
                }
                Moment::TimeTz(micros, offset) => datetime::time_tz_utc(micros, offset),
                _ => 0,
            };
            Ok(Some(as_zoned(left).cmp(&as_zoned(right))))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn method(text: &str, method: Temporal) -> Result<String, String> {
        apply(method, None, &Json::String(text.to_owned()), false)
            .map(plain)
            .map(|json| match json {
                Json::String(text) => text,
                other => format!("{other:?}"),
            })
            .map_err(|failure| match failure {
                Failure::Suppressible(error) | Failure::Fatal(error) => {
                    error.state().code().to_owned()
                }
            })
    }

    #[test]
    fn iso_strings_become_the_first_matching_type() {
        assert_eq!(
            method("2017-03-10", Temporal::DateTime).unwrap(),
            "2017-03-10"
        );
        assert_eq!(method("12:34:56", Temporal::DateTime).unwrap(), "12:34:56");
        assert_eq!(
            method("12:34:56+03", Temporal::DateTime).unwrap(),
            "12:34:56+03:00"
        );
        assert_eq!(
            method("2017-03-10 12:34:56", Temporal::DateTime).unwrap(),
            "2017-03-10T12:34:56"
        );
        assert_eq!(
            method("2017-03-10T12:34:56.5-05:30", Temporal::DateTime).unwrap(),
            "2017-03-10T12:34:56.5-05:30"
        );
    }

    #[test]
    fn unrecognized_and_out_of_range_strings_fail_softly() {
        assert_eq!(
            method("10/03/2017", Temporal::DateTime).unwrap_err(),
            "22031"
        );
        assert_eq!(
            method("2017-13-10", Temporal::DateTime).unwrap_err(),
            "22031"
        );
        assert_eq!(
            method("12:34:56+99", Temporal::DateTime).unwrap_err(),
            "22031"
        );
        assert_eq!(method("", Temporal::Date).unwrap_err(), "22031");
    }

    #[test]
    fn conversions_that_need_a_time_zone_fail_without_one() {
        assert_eq!(
            method("2017-03-10 12:34:56", Temporal::Date).unwrap(),
            "2017-03-10"
        );
        assert_eq!(
            method("2017-03-10 12:34:56", Temporal::Time).unwrap(),
            "12:34:56"
        );
        assert_eq!(
            method("2017-03-10", Temporal::Timestamp).unwrap(),
            "2017-03-10T00:00:00"
        );
        assert_eq!(
            method("2017-03-10", Temporal::TimestampTz).unwrap_err(),
            "22031"
        );
        assert_eq!(method("12:34:56", Temporal::Date).unwrap_err(), "22031");
    }

    #[test]
    fn templates_choose_the_type_by_their_fields() {
        let templated = |text: &str, template: &str| {
            apply(
                Temporal::DateTime,
                Some(&TemporalArgument::Template(template.to_owned())),
                &Json::String(text.to_owned()),
                false,
            )
            .ok()
            .and_then(|json| Moment::of(&json))
        };
        assert!(matches!(
            templated("10-03-2017", "DD-MM-YYYY"),
            Some(Moment::Date(_))
        ));
        assert!(matches!(
            templated("12:34", "HH24:MI"),
            Some(Moment::Time(_))
        ));
        assert!(matches!(
            templated("10-03-2017 12:34 +05", "DD-MM-YYYY HH24:MI TZH"),
            Some(Moment::TimestampTz(_, 18_000))
        ));
    }

    #[test]
    fn moments_compare_within_their_families() {
        let date = Moment::Date(0);
        let midnight = Moment::Timestamp(0);
        let noon = Moment::Timestamp(12 * 3600 * MICROS_PER_SECOND);
        assert_eq!(
            compare(date, midnight, false).ok().flatten(),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare(date, noon, false).ok().flatten(),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare(Moment::Time(0), Moment::Date(0), false)
                .ok()
                .flatten(),
            None
        );
        assert!(compare(date, Moment::TimestampTz(0, 0), false).is_err());
        assert!(compare(date, Moment::TimestampTz(0, 0), true).is_ok());
    }

    #[test]
    fn a_marker_object_from_a_document_is_never_a_moment() {
        assert_eq!(Moment::of(&Json::Object(Vec::new())), None);
        let moment = Moment::Date(7);
        assert_eq!(Moment::of(&moment.to_json()), Some(moment));
    }
}
