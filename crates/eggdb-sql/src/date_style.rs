use crate::datetime::{
    MICROS_PER_DAY, MICROS_PER_SECOND, civil_from_days, format_date, format_offset,
    format_timestamp, format_timestamptz, is_infinite_date, is_infinite_timestamp, session_zone,
    to_local,
};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const SATURDAY: i64 = 6;
const TWO_DIGIT_YEAR_PIVOT: i64 = 70;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DateOutput {
    Iso,
    Sql,
    Postgres,
    German,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldOrder {
    Dmy,
    Mdy,
    Ymd,
}

/// PostgreSQL's `DateStyle`: how dates and timestamps are written, and the order of the day,
/// month and year in a date written with numbers only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DateStyle {
    pub(crate) output: DateOutput,
    pub(crate) order: FieldOrder,
}

impl DateStyle {
    pub(crate) const DEFAULT: Self = Self {
        output: DateOutput::Iso,
        order: FieldOrder::Mdy,
    };

    /// Reads a `SET DateStyle` value over `current`, as PostgreSQL does: a list of an output
    /// style and a field order, either of which keeps the current one when left out, except
    /// that `German` alone also means `DMY`. `None` for an unknown word or two conflicting ones.
    pub(crate) fn parse(text: &str, current: Self) -> Option<Self> {
        let mut output = None;
        let mut order = None;
        let words = text
            .split([',', ' ', '\t'])
            .map(str::trim)
            .filter(|word| !word.is_empty());
        for word in words {
            let lowered = word.trim_matches('\'').to_ascii_lowercase();
            let (new_output, new_order) = match lowered.as_str() {
                "iso" => (Some(DateOutput::Iso), None),
                "sql" => (Some(DateOutput::Sql), None),
                "postgres" => (Some(DateOutput::Postgres), None),
                "german" => (Some(DateOutput::German), None),
                "dmy" | "euro" | "european" => (None, Some(FieldOrder::Dmy)),
                "mdy" | "us" | "noneuro" | "noneuropean" => (None, Some(FieldOrder::Mdy)),
                "ymd" => (None, Some(FieldOrder::Ymd)),
                "default" => (Some(DateOutput::Iso), Some(FieldOrder::Mdy)),
                _ => return None,
            };
            if let Some(found) = new_output {
                if output.is_some_and(|seen| seen != found) {
                    return None;
                }
                output = Some(found);
            }
            if let Some(found) = new_order {
                if order.is_some_and(|seen| seen != found) {
                    return None;
                }
                order = Some(found);
            }
        }
        let output = output.unwrap_or(current.output);
        let order = order.unwrap_or(if output == DateOutput::German && current.output != DateOutput::German {
            FieldOrder::Dmy
        } else {
            current.order
        });
        Some(Self { output, order })
    }

    /// The setting's text as `SHOW DateStyle` gives it: `ISO, MDY`.
    pub(crate) fn setting_text(self) -> String {
        let output = match self.output {
            DateOutput::Iso => "ISO",
            DateOutput::Sql => "SQL",
            DateOutput::Postgres => "Postgres",
            DateOutput::German => "German",
        };
        let order = match self.order {
            FieldOrder::Dmy => "DMY",
            FieldOrder::Mdy => "MDY",
            FieldOrder::Ymd => "YMD",
        };
        format!("{output}, {order}")
    }

    /// The style of the statement running on this thread.
    pub(crate) fn current() -> Self {
        crate::pg_catalog::statement_settings()
            .get("DateStyle")
            .ok()
            .and_then(|value| Self::parse(&value, Self::DEFAULT))
            .unwrap_or(Self::DEFAULT)
    }
}

struct Fields {
    year: i64,
    month: i64,
    day: i64,
    weekday: usize,
    seconds_of_day: i64,
    fraction: i64,
}

impl Fields {
    fn of(local_micros: i64) -> Self {
        let days = local_micros.div_euclid(MICROS_PER_DAY);
        let of_day = local_micros.rem_euclid(MICROS_PER_DAY);
        Self::with_time(days, of_day)
    }

    fn of_date(days: i32) -> Self {
        Self::with_time(i64::from(days), 0)
    }

    fn with_time(days: i64, of_day: i64) -> Self {
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            weekday: (days + SATURDAY).rem_euclid(7) as usize,
            seconds_of_day: of_day / MICROS_PER_SECOND,
            fraction: of_day % MICROS_PER_SECOND,
        }
    }

    fn shown_year(&self) -> (i64, bool) {
        if self.year <= 0 {
            (1 - self.year, true)
        } else {
            (self.year, false)
        }
    }

    fn month_name(&self) -> &'static str {
        MONTHS[(self.month.clamp(1, 12) - 1) as usize]
    }

    fn clock(&self) -> String {
        let seconds = self.seconds_of_day;
        let mut out = format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        );
        if self.fraction != 0 {
            let digits = format!("{:06}", self.fraction);
            out.push('.');
            out.push_str(digits.trim_end_matches('0'));
        }
        out
    }

    fn numeric_date(&self, style: DateStyle) -> String {
        let (year, _) = self.shown_year();
        let (month, day) = (self.month, self.day);
        match (style.output, style.order) {
            (DateOutput::Sql, FieldOrder::Dmy) => format!("{day:02}/{month:02}/{year:04}"),
            (DateOutput::Sql, _) => format!("{month:02}/{day:02}/{year:04}"),
            (DateOutput::German, _) => format!("{day:02}.{month:02}.{year:04}"),
            (DateOutput::Postgres, FieldOrder::Dmy) => format!("{day:02}-{month:02}-{year:04}"),
            (DateOutput::Postgres, _) => format!("{month:02}-{day:02}-{year:04}"),
            (DateOutput::Iso, _) => format!("{year:04}-{month:02}-{day:02}"),
        }
    }

    fn era(&self) -> &'static str {
        if self.shown_year().1 { " BC" } else { "" }
    }
}

/// A `date` as `DateStyle` writes it.
pub(crate) fn styled_date(days: i32, style: DateStyle) -> String {
    if style.output == DateOutput::Iso || is_infinite_date(days) {
        return format_date(days);
    }
    let fields = Fields::of_date(days);
    format!("{}{}", fields.numeric_date(style), fields.era())
}

/// A `timestamp` as `DateStyle` writes it.
pub(crate) fn styled_timestamp(micros: i64, style: DateStyle) -> String {
    if style.output == DateOutput::Iso || is_infinite_timestamp(micros) {
        return format_timestamp(micros);
    }
    styled_local(&Fields::of(micros), style, None)
}

/// A `timestamp with time zone` as `DateStyle` writes it, in the session's time zone, with the
/// zone's abbreviation where it has one (`PST`, `CET`) and the numeric offset otherwise.
pub(crate) fn styled_timestamptz(micros: i64, style: DateStyle) -> String {
    if style.output == DateOutput::Iso || is_infinite_timestamp(micros) {
        return format_timestamptz(micros);
    }
    let zone = session_zone();
    let offset = zone.offset_at(micros);
    let zone_text = zone
        .abbreviation_at(micros)
        .filter(|abbreviation| !abbreviation.is_empty())
        .unwrap_or_else(|| format_offset(offset));
    styled_local(&Fields::of(to_local(micros, offset)), style, Some(&zone_text))
}

fn styled_local(fields: &Fields, style: DateStyle, zone: Option<&str>) -> String {
    let mut out = match style.output {
        DateOutput::Postgres => {
            let (year, _) = fields.shown_year();
            let weekday = WEEKDAYS[fields.weekday];
            let month = fields.month_name();
            let day = fields.day;
            let clock = fields.clock();
            if style.order == FieldOrder::Dmy {
                format!("{weekday} {day:02} {month} {clock} {year:04}")
            } else {
                format!("{weekday} {month} {day:02} {clock} {year:04}")
            }
        }
        _ => format!("{} {}", fields.numeric_date(style), fields.clock()),
    };
    if let Some(zone) = zone {
        out.push(' ');
        out.push_str(zone);
    }
    out.push_str(fields.era());
    out
}

/// A date written with numbers only in another order than ISO's (`12/17/1997`, `17.12.1997`,
/// `12-17-1997`, `97/12/17`) rewritten as ISO's `1997-12-17`, with whatever follows it kept, by
/// the field order of `DateStyle`; `None` when `text` does not start that way. A first field
/// of three or more digits is always the year; a two-digit year means 1970 to 2069.
pub(crate) fn iso_date_prefix(text: &str, order: impl FnOnce() -> FieldOrder) -> Option<String> {
    let bytes = text.as_bytes();
    let field = |from: usize| {
        let length = bytes[from..].iter().take_while(|byte| byte.is_ascii_digit()).count();
        (length > 0).then_some(from + length)
    };
    let first_end = field(0)?;
    let separator = *bytes.get(first_end)?;
    if !matches!(separator, b'/' | b'.' | b'-') {
        return None;
    }
    if separator == b'-' && first_end >= 3 {
        return None;
    }
    let second_end = field(first_end + 1)?;
    if bytes.get(second_end) != Some(&separator) {
        return None;
    }
    let third_end = field(second_end + 1)?;
    let number = |from: usize, to: usize| text[from..to].parse::<i64>().ok();
    let first = number(0, first_end)?;
    let second = number(first_end + 1, second_end)?;
    let third = number(second_end + 1, third_end)?;
    let first_digits = first_end;
    let third_digits = third_end - second_end - 1;
    let widen = |year: i64, digits: usize| match digits {
        1 | 2 if year < TWO_DIGIT_YEAR_PIVOT => year + 2000,
        1 | 2 => year + 1900,
        _ => year,
    };
    let (year, month, day) = if first_digits >= 3 {
        (first, second, third)
    } else {
        match order() {
            FieldOrder::Mdy => (widen(third, third_digits), first, second),
            FieldOrder::Dmy => (widen(third, third_digits), second, first),
            FieldOrder::Ymd => (widen(first, first_digits), second, third),
        }
    };
    Some(format!("{year:04}-{month:02}-{day:02}{}", &text[third_end..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datetime::days_from_civil;

    const SQL_DMY: DateStyle = DateStyle {
        output: DateOutput::Sql,
        order: FieldOrder::Dmy,
    };

    #[test]
    fn the_setting_keeps_what_is_left_out_and_writes_postgresql_spelling() {
        let sql = DateStyle::parse("sql, dmy", DateStyle::DEFAULT);
        assert_eq!(sql, Some(SQL_DMY));
        assert_eq!(sql.map(DateStyle::setting_text).as_deref(), Some("SQL, DMY"));
        assert_eq!(
            DateStyle::parse("YMD", SQL_DMY).map(DateStyle::setting_text).as_deref(),
            Some("SQL, YMD")
        );
        assert_eq!(
            DateStyle::parse("German", DateStyle::DEFAULT).map(DateStyle::setting_text).as_deref(),
            Some("German, DMY")
        );
        assert_eq!(
            DateStyle::parse("iso, mdy", SQL_DMY).map(DateStyle::setting_text).as_deref(),
            Some("ISO, MDY")
        );
        assert_eq!(DateStyle::parse("iso, sql", DateStyle::DEFAULT), None);
        assert_eq!(DateStyle::parse("klingon", DateStyle::DEFAULT), None);
    }

    #[test]
    fn dates_and_timestamps_follow_each_output_style() {
        let days = days_from_civil(1997, 12, 17) as i32;
        let style = |output, order| DateStyle { output, order };
        assert_eq!(styled_date(days, style(DateOutput::Iso, FieldOrder::Mdy)), "1997-12-17");
        assert_eq!(styled_date(days, style(DateOutput::Sql, FieldOrder::Mdy)), "12/17/1997");
        assert_eq!(styled_date(days, SQL_DMY), "17/12/1997");
        assert_eq!(styled_date(days, style(DateOutput::German, FieldOrder::Dmy)), "17.12.1997");
        assert_eq!(styled_date(days, style(DateOutput::Postgres, FieldOrder::Mdy)), "12-17-1997");
        let micros = i64::from(days) * MICROS_PER_DAY + (7 * 3600 + 37 * 60 + 16) * MICROS_PER_SECOND;
        assert_eq!(
            styled_timestamp(micros, style(DateOutput::Postgres, FieldOrder::Mdy)),
            "Wed Dec 17 07:37:16 1997"
        );
        assert_eq!(
            styled_timestamp(micros, style(DateOutput::Postgres, FieldOrder::Dmy)),
            "Wed 17 Dec 07:37:16 1997"
        );
        assert_eq!(
            styled_timestamp(micros + 500_000, style(DateOutput::Sql, FieldOrder::Mdy)),
            "12/17/1997 07:37:16.5"
        );
        assert_eq!(
            styled_timestamp(micros, style(DateOutput::German, FieldOrder::Dmy)),
            "17.12.1997 07:37:16"
        );
        let bc = days_from_civil(-43, 3, 15) as i32;
        assert_eq!(styled_date(bc, SQL_DMY), "15/03/0044 BC");
    }

    #[test]
    fn numeric_dates_read_in_the_field_order() {
        let mdy = || FieldOrder::Mdy;
        let dmy = || FieldOrder::Dmy;
        let ymd = || FieldOrder::Ymd;
        assert_eq!(iso_date_prefix("12/17/1997", mdy).as_deref(), Some("1997-12-17"));
        assert_eq!(iso_date_prefix("17/12/1997 07:37:16", dmy).as_deref(), Some("1997-12-17 07:37:16"));
        assert_eq!(iso_date_prefix("17.12.1997", dmy).as_deref(), Some("1997-12-17"));
        assert_eq!(iso_date_prefix("12-17-1997", mdy).as_deref(), Some("1997-12-17"));
        assert_eq!(iso_date_prefix("97/12/17", ymd).as_deref(), Some("1997-12-17"));
        assert_eq!(iso_date_prefix("1/2/03", mdy).as_deref(), Some("2003-01-02"));
        assert_eq!(iso_date_prefix("1997/12/17", dmy).as_deref(), Some("1997-12-17"));
        assert_eq!(iso_date_prefix("1997-12-17", mdy), None, "ISO dates are left alone");
        assert_eq!(iso_date_prefix("12/17", mdy), None);
        assert_eq!(iso_date_prefix("12/17-1997", mdy), None);
        assert_eq!(iso_date_prefix("today", mdy), None);
    }
}
