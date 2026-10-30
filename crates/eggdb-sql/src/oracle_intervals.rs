use crate::ast::BinaryOperator;
use crate::datetime;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::interval::{Interval, IntervalFields, MICROS_PER_DAY, MICROS_PER_SECOND};
use crate::numeric::Numeric;
use crate::types::{SqlType, Value};

const MICROS_PER_MINUTE: i64 = 60 * MICROS_PER_SECOND;
const MICROS_PER_HOUR: i64 = 60 * MICROS_PER_MINUTE;
const MONTHS_PER_YEAR: i64 = 12;
pub(crate) const DEFAULT_LEADING_PRECISION: u8 = 2;
pub(crate) const MAX_PRECISION: u8 = 9;
pub(crate) const DEFAULT_FRACTION_DIGITS: u8 = 6;
const KEPT_FRACTION_DIGITS: u8 = 6;
const MAX_TEXT_BYTES: usize = 200;
const SHOWN_LEADING_DIGITS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IntervalField {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

impl IntervalField {
    pub(crate) fn named(word: &str) -> Option<Self> {
        Some(match word {
            "year" => Self::Year,
            "month" => Self::Month,
            "day" => Self::Day,
            "hour" => Self::Hour,
            "minute" => Self::Minute,
            "second" => Self::Second,
            _ => return None,
        })
    }

    fn of_months(self) -> bool {
        matches!(self, Self::Year | Self::Month)
    }

    fn micros(self) -> i64 {
        match self {
            Self::Day => MICROS_PER_DAY,
            Self::Hour => MICROS_PER_HOUR,
            Self::Minute => MICROS_PER_MINUTE,
            _ => MICROS_PER_SECOND,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntervalKind {
    YearToMonth,
    DayToSecond,
}

impl IntervalKind {
    pub(crate) fn fields(self) -> IntervalFields {
        match self {
            Self::YearToMonth => IntervalFields::YearToMonth,
            Self::DayToSecond => IntervalFields::DayToSecond,
        }
    }

    pub(crate) fn sql_type(self, fraction_digits: u8) -> SqlType {
        match self {
            Self::YearToMonth => SqlType::Interval {
                fields: Some(IntervalFields::YearToMonth),
                precision: None,
            },
            Self::DayToSecond => SqlType::Interval {
                fields: Some(IntervalFields::DayToSecond),
                precision: Some(fraction_digits.min(KEPT_FRACTION_DIGITS)),
            },
        }
    }

    fn of_type(sql_type: SqlType) -> Option<(Self, u8)> {
        let SqlType::Interval { fields, precision } = sql_type else {
            return None;
        };
        let kind = match fields {
            Some(IntervalFields::Year | IntervalFields::Month | IntervalFields::YearToMonth) => {
                Self::YearToMonth
            }
            _ => Self::DayToSecond,
        };
        Some((kind, precision.unwrap_or(KEPT_FRACTION_DIGITS)))
    }

    fn of_value(interval: Interval) -> Option<Self> {
        match (interval.months, interval.days, interval.micros) {
            (0, _, _) => Some(Self::DayToSecond),
            (_, 0, 0) => Some(Self::YearToMonth),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Qualifier {
    pub(crate) leading: IntervalField,
    pub(crate) trailing: IntervalField,
    pub(crate) leading_precision: u8,
    pub(crate) fraction_digits: u8,
}

impl Qualifier {
    pub(crate) fn kind(self) -> IntervalKind {
        if self.leading.of_months() {
            IntervalKind::YearToMonth
        } else {
            IntervalKind::DayToSecond
        }
    }

    pub(crate) fn is_valid(self) -> bool {
        self.leading <= self.trailing && self.leading.of_months() == self.trailing.of_months()
    }

    pub(crate) fn sql_type(self) -> SqlType {
        self.kind().sql_type(self.fraction_digits)
    }

    fn widest(kind: IntervalKind) -> Self {
        match kind {
            IntervalKind::YearToMonth => Self {
                leading: IntervalField::Year,
                trailing: IntervalField::Month,
                leading_precision: MAX_PRECISION,
                fraction_digits: 0,
            },
            IntervalKind::DayToSecond => Self {
                leading: IntervalField::Day,
                trailing: IntervalField::Second,
                leading_precision: MAX_PRECISION,
                fraction_digits: MAX_PRECISION,
            },
        }
    }
}

pub(crate) fn invalid_interval() -> SqlError {
    SqlError::new(SqlState::INVALID_DATETIME_FORMAT, "the interval is invalid").with_oracle_code(1867)
}

fn leading_precision_too_small() -> SqlError {
    SqlError::new(
        SqlState::INTERVAL_FIELD_OVERFLOW,
        "the leading precision of the interval is too small",
    )
    .with_oracle_code(1873)
}

fn field_out_of_range(field: IntervalField) -> SqlError {
    let (code, message) = match field {
        IntervalField::Month => (1843, "not a valid month"),
        IntervalField::Hour => (1850, "hour must be between 0 and 23"),
        IntervalField::Minute => (1851, "minutes must be between 0 and 59"),
        _ => (1852, "seconds must be between 0 and 59"),
    };
    SqlError::new(SqlState::DATETIME_FIELD_OVERFLOW, message).with_oracle_code(code)
}

fn illegal_argument() -> SqlError {
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        "illegal argument for function",
    )
    .with_oracle_code(1760)
}

fn mixed_kinds() -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        "invalid data type for datetime/interval arithmetic",
    )
    .with_oracle_code(30081)
}

fn out_of_range() -> SqlError {
    crate::interval::out_of_range()
}

pub(crate) fn day_to_second(total_micros: i128) -> SqlResult<Interval> {
    let day = i128::from(MICROS_PER_DAY);
    let days = i32::try_from(total_micros / day).map_err(|_| out_of_range())?;
    let micros = i64::try_from(total_micros % day).map_err(|_| out_of_range())?;
    Ok(Interval {
        months: 0,
        days,
        micros,
    })
}

fn year_to_month(months: i128) -> SqlResult<Interval> {
    Ok(Interval {
        months: i32::try_from(months).map_err(|_| out_of_range())?,
        days: 0,
        micros: 0,
    })
}

fn total_micros(interval: Interval) -> i128 {
    i128::from(interval.days) * i128::from(MICROS_PER_DAY) + i128::from(interval.micros)
}

struct Cursor<'t> {
    bytes: &'t [u8],
    at: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn take(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.at += 1;
            return true;
        }
        false
    }

    fn digits(&mut self) -> Option<(i128, usize)> {
        let start = self.at;
        let mut value = 0i128;
        while let Some(byte) = self.peek().filter(u8::is_ascii_digit) {
            value = value.checked_mul(10)?.checked_add(i128::from(byte - b'0'))?;
            self.at += 1;
        }
        let count = self.at - start;
        (count > 0).then_some((value, count))
    }

    fn fraction(&mut self, kept: u8) -> SqlResult<i128> {
        if !self.take(b'.') {
            return Ok(0);
        }
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        let digits = &self.bytes[start..self.at];
        if digits.is_empty() {
            return Err(invalid_interval());
        }
        let kept = usize::from(kept.min(KEPT_FRACTION_DIGITS));
        let mut micros = 0i128;
        for index in 0..KEPT_FRACTION_DIGITS as usize {
            let digit = if index < kept {
                digits.get(index).map_or(0, |digit| i128::from(digit - b'0'))
            } else {
                0
            };
            micros = micros * 10 + digit;
        }
        if digits.get(kept).is_some_and(|digit| *digit >= b'5') {
            micros += 10i128.pow(u32::from(KEPT_FRACTION_DIGITS) - kept as u32);
        }
        Ok(micros)
    }
}

fn separator_before(field: IntervalField) -> u8 {
    match field {
        IntervalField::Month => b'-',
        IntervalField::Hour => b' ',
        _ => b':',
    }
}

fn limit_of(field: IntervalField) -> i128 {
    match field {
        IntervalField::Month => 11,
        IntervalField::Hour => 23,
        _ => 59,
    }
}

const FIELD_ORDER: [IntervalField; 6] = [
    IntervalField::Year,
    IntervalField::Month,
    IntervalField::Day,
    IntervalField::Hour,
    IntervalField::Minute,
    IntervalField::Second,
];

fn read_fields(cursor: &mut Cursor<'_>, qualifier: Qualifier) -> SqlResult<i128> {
    let mut total = 0i128;
    for field in FIELD_ORDER {
        if field < qualifier.leading || field > qualifier.trailing {
            continue;
        }
        let leading = field == qualifier.leading;
        if !leading && !cursor.take(separator_before(field)) {
            return Err(invalid_interval());
        }
        let (value, count) = cursor.digits().ok_or_else(invalid_interval)?;
        if leading && count > usize::from(qualifier.leading_precision) {
            return Err(leading_precision_too_small());
        }
        if !leading && value > limit_of(field) {
            return Err(field_out_of_range(field));
        }
        let unit = if field.of_months() {
            if field == IntervalField::Year {
                i128::from(MONTHS_PER_YEAR)
            } else {
                1
            }
        } else {
            i128::from(field.micros())
        };
        total = value
            .checked_mul(unit)
            .and_then(|scaled| total.checked_add(scaled))
            .ok_or_else(out_of_range)?;
        if field == IntervalField::Second {
            total = total
                .checked_add(cursor.fraction(qualifier.fraction_digits)?)
                .ok_or_else(out_of_range)?;
        }
    }
    Ok(total)
}

pub(crate) fn parse_literal(text: &str, qualifier: Qualifier) -> SqlResult<Interval> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(invalid_interval());
    }
    let trimmed = text.trim();
    let mut cursor = Cursor {
        bytes: trimmed.as_bytes(),
        at: 0,
    };
    let negative = if cursor.take(b'-') {
        true
    } else {
        cursor.take(b'+');
        false
    };
    let total = read_fields(&mut cursor, qualifier)?;
    if cursor.at != cursor.bytes.len() {
        return Err(invalid_interval());
    }
    let total = if negative { -total } else { total };
    match qualifier.kind() {
        IntervalKind::YearToMonth => year_to_month(total),
        IntervalKind::DayToSecond => day_to_second(total),
    }
}

fn iso_duration(text: &str, kind: IntervalKind) -> SqlResult<Interval> {
    let trimmed = text.trim();
    let (negative, body) = match trimmed.strip_prefix('-') {
        Some(body) => (true, body),
        None => (false, trimmed),
    };
    let body = body.strip_prefix(['P', 'p']).ok_or_else(invalid_interval)?;
    let mut cursor = Cursor {
        bytes: body.as_bytes(),
        at: 0,
    };
    let mut months = 0i128;
    let mut micros = 0i128;
    let mut in_time = false;
    let mut any = false;
    while cursor.peek().is_some() {
        if !in_time && cursor.take(b'T') {
            in_time = true;
            continue;
        }
        let (value, _) = cursor.digits().ok_or_else(invalid_interval)?;
        let fraction = if in_time { cursor.fraction(MAX_PRECISION)? } else { 0 };
        let designator = cursor.peek().ok_or_else(invalid_interval)?.to_ascii_uppercase();
        cursor.at += 1;
        let (unit, of_months) = match (in_time, designator) {
            (false, b'Y') => (MONTHS_PER_YEAR, true),
            (false, b'M') => (1, true),
            (false, b'D') => (MICROS_PER_DAY, false),
            (true, b'H') => (MICROS_PER_HOUR, false),
            (true, b'M') => (MICROS_PER_MINUTE, false),
            (true, b'S') => (MICROS_PER_SECOND, false),
            _ => return Err(invalid_interval()),
        };
        let amount = value
            .checked_mul(i128::from(unit))
            .and_then(|amount| amount.checked_add(fraction))
            .ok_or_else(out_of_range)?;
        let total = if of_months { &mut months } else { &mut micros };
        *total = total.checked_add(amount).ok_or_else(out_of_range)?;
        if fraction != 0 && designator != b'S' {
            return Err(invalid_interval());
        }
        any = true;
    }
    if !any {
        return Err(invalid_interval());
    }
    let sign = if negative { -1 } else { 1 };
    match kind {
        IntervalKind::YearToMonth => year_to_month(sign * months),
        IntervalKind::DayToSecond => day_to_second(sign * micros),
    }
}

pub(crate) fn parse_text(text: &str, kind: IntervalKind) -> SqlResult<Interval> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(invalid_interval());
    }
    let trimmed = text.trim();
    let iso = trimmed
        .trim_start_matches('-')
        .starts_with(['P', 'p']);
    if iso {
        return iso_duration(trimmed, kind);
    }
    parse_literal(trimmed, Qualifier::widest(kind))
}

pub(crate) fn text(interval: Interval, kind: Option<IntervalKind>, fraction_digits: u8) -> String {
    let Some(kind) = kind.or_else(|| IntervalKind::of_value(interval)) else {
        return interval.to_string();
    };
    match kind {
        IntervalKind::YearToMonth => {
            let months = i64::from(interval.months);
            let sign = if months < 0 { '-' } else { '+' };
            let magnitude = months.unsigned_abs();
            format!(
                "{sign}{:0width$}-{:02}",
                magnitude / 12,
                magnitude % 12,
                width = SHOWN_LEADING_DIGITS
            )
        }
        IntervalKind::DayToSecond => {
            let total = total_micros(interval);
            let sign = if total < 0 { '-' } else { '+' };
            let magnitude = total.unsigned_abs();
            let day = MICROS_PER_DAY.unsigned_abs() as u128;
            let of_day = magnitude % day;
            let mut shown = format!(
                "{sign}{:0width$} {:02}:{:02}:{:02}",
                magnitude / day,
                of_day / MICROS_PER_HOUR as u128,
                of_day / MICROS_PER_MINUTE as u128 % 60,
                of_day / MICROS_PER_SECOND as u128 % 60,
                width = SHOWN_LEADING_DIGITS
            );
            if fraction_digits > 0 {
                let micros = format!("{:06}", of_day % MICROS_PER_SECOND as u128);
                let digits = usize::from(fraction_digits);
                shown.push('.');
                if digits <= 6 {
                    shown.push_str(&micros[..digits]);
                } else {
                    shown.push_str(&micros);
                    shown.extend(std::iter::repeat_n('0', digits - 6));
                }
            }
            shown
        }
    }
}

pub(crate) fn typed_text(interval: Interval, sql_type: Option<SqlType>) -> String {
    match sql_type.and_then(IntervalKind::of_type) {
        Some((kind, digits)) => text(interval, Some(kind), digits),
        None => text(interval, None, KEPT_FRACTION_DIGITS),
    }
}

pub(crate) fn conformed(interval: Interval, sql_type: SqlType) -> SqlResult<Value> {
    let Some((kind, digits)) = IntervalKind::of_type(sql_type) else {
        return Ok(Value::Interval(interval));
    };
    let shaped = match kind {
        IntervalKind::YearToMonth => {
            if interval.days != 0 || interval.micros != 0 {
                return Err(mixed_kinds());
            }
            interval
        }
        IntervalKind::DayToSecond => {
            if interval.months != 0 {
                return Err(mixed_kinds());
            }
            day_to_second(total_micros(interval))?
        }
    };
    shaped
        .with_modifier(Some(kind.fields()), (kind == IntervalKind::DayToSecond).then_some(digits))
        .map(Value::Interval)
}

fn unit_named(value: &Value) -> SqlResult<Option<IntervalField>> {
    let Some(name) = crate::oracle_functions::text_of(value) else {
        return Ok(None);
    };
    IntervalField::named(&name.trim().to_ascii_lowercase())
        .map(Some)
        .ok_or_else(illegal_argument)
}

fn scaled_whole(amount: &Numeric, unit: i64) -> SqlResult<i128> {
    amount
        .multiply(&Numeric::from_i64(unit))
        .and_then(|scaled| scaled.rounded(0))?
        .integral_value()
        .ok_or_else(out_of_range)
}

pub(crate) fn number_to_day_to_second(amount: &Value, unit: &Value) -> SqlResult<Value> {
    let (Some(amount), Some(unit)) = (crate::oracle_functions::decimal_of(amount)?, unit_named(unit)?)
    else {
        return Ok(Value::Null);
    };
    if unit.of_months() {
        return Err(illegal_argument());
    }
    day_to_second(scaled_whole(&amount, unit.micros())?).map(Value::Interval)
}

pub(crate) fn number_to_year_to_month(amount: &Value, unit: &Value) -> SqlResult<Value> {
    let (Some(amount), Some(unit)) = (crate::oracle_functions::decimal_of(amount)?, unit_named(unit)?)
    else {
        return Ok(Value::Null);
    };
    let months_per_unit = match unit {
        IntervalField::Year => MONTHS_PER_YEAR,
        IntervalField::Month => 1,
        _ => return Err(illegal_argument()),
    };
    year_to_month(scaled_whole(&amount, months_per_unit)?).map(Value::Interval)
}

pub(crate) fn text_to_interval(value: &Value, kind: IntervalKind) -> SqlResult<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Interval(interval) => conformed(*interval, kind.sql_type(KEPT_FRACTION_DIGITS)),
        other => match crate::oracle_functions::text_of(other) {
            Some(text) => parse_text(&text, kind).map(Value::Interval),
            None => Ok(Value::Null),
        },
    }
}

pub(crate) fn kind_of_type(sql_type: SqlType) -> Option<IntervalKind> {
    IntervalKind::of_type(sql_type).map(|(kind, _)| kind)
}

pub(crate) fn extract(field: &str, interval: Interval, kind: Option<IntervalKind>) -> Option<Numeric> {
    let kind = kind.or_else(|| IntervalKind::of_value(interval))?;
    let field = IntervalField::named(field)?;
    if field.of_months() != (kind == IntervalKind::YearToMonth) {
        return None;
    }
    let months = i64::from(interval.months);
    let total = total_micros(interval);
    Some(match field {
        IntervalField::Year => Numeric::from_i64(months / MONTHS_PER_YEAR),
        IntervalField::Month => Numeric::from_i64(months % MONTHS_PER_YEAR),
        IntervalField::Day => Numeric::from_i128(total / i128::from(MICROS_PER_DAY)),
        IntervalField::Hour => Numeric::from_i128(
            total % i128::from(MICROS_PER_DAY) / i128::from(MICROS_PER_HOUR),
        ),
        IntervalField::Minute => Numeric::from_i128(
            total % i128::from(MICROS_PER_HOUR) / i128::from(MICROS_PER_MINUTE),
        ),
        IntervalField::Second => {
            let micros = total % i128::from(MICROS_PER_MINUTE);
            let written = Numeric::from_scaled(micros, 6).to_string();
            Numeric::parse(written.trim_end_matches('0').trim_end_matches('.')).ok()?
        }
    })
}

pub(crate) fn timestamp_difference(left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    let instant = |value: &Value| -> Option<SqlResult<i64>> {
        Some(match value {
            Value::Timestamp(micros) => Ok(*micros),
            Value::TimestampTz(micros) => datetime::utc_to_local(*micros),
            Value::Date(days) => datetime::date_to_timestamp(*days),
            _ => return None,
        })
    };
    let later = instant(left)?;
    let earlier = instant(right)?;
    Some(later.and_then(|later| {
        earlier.and_then(|earlier| {
            day_to_second(i128::from(later) - i128::from(earlier)).map(Value::Interval)
        })
    }))
}

fn shifted_by_months(local: i64, interval: Interval, sign: i64) -> SqlResult<Option<i64>> {
    if interval.months == 0 {
        return Ok(None);
    }
    let days = local.div_euclid(MICROS_PER_DAY);
    let of_day = local.rem_euclid(MICROS_PER_DAY);
    let (year, month, day) = datetime::civil_from_days(days);
    let total = year * MONTHS_PER_YEAR + month - 1 + sign * i64::from(interval.months);
    let (new_year, new_month) = (
        total.div_euclid(MONTHS_PER_YEAR),
        total.rem_euclid(MONTHS_PER_YEAR) + 1,
    );
    if !crate::oracle_datetime::year_in_range(new_year) {
        return Err(crate::oracle_datetime::year_out_of_range());
    }
    if day > datetime::days_in_month(new_year, new_month) {
        return Err(SqlError::new(
            SqlState::DATETIME_FIELD_OVERFLOW,
            "date not valid for month specified",
        )
        .with_oracle_code(1839));
    }
    Ok(Some(
        datetime::days_from_civil(new_year, new_month, day) * MICROS_PER_DAY + of_day,
    ))
}

fn shifted(stamp: &Value, interval: Interval, sign: i64) -> Option<SqlResult<Value>> {
    let local = match stamp {
        Value::Timestamp(micros) => *micros,
        _ => return None,
    };
    let moved = (|| -> SqlResult<Value> {
        let by_months = shifted_by_months(local, interval, sign)?.unwrap_or(local);
        let micros = i128::from(by_months) + i128::from(sign) * total_micros(interval);
        let micros = datetime::checked_timestamp(micros)?;
        let (year, _, _) = datetime::civil_from_days(micros.div_euclid(MICROS_PER_DAY));
        if !crate::oracle_datetime::year_in_range(year) {
            return Err(crate::oracle_datetime::year_out_of_range());
        }
        Ok(Value::Timestamp(micros))
    })();
    Some(moved)
}

fn factor_of(value: &Value) -> Option<SqlResult<Numeric>> {
    if !value.sql_type().is_some_and(SqlType::is_numeric) {
        return None;
    }
    Some(
        crate::oracle_functions::decimal_of(value)
            .and_then(|decimal| decimal.ok_or_else(out_of_range)),
    )
}

fn scaled_interval(interval: Interval, factor: &Numeric, divide: bool) -> SqlResult<Value> {
    let Some(kind) = IntervalKind::of_value(interval) else {
        return Err(mixed_kinds());
    };
    let amount = match kind {
        IntervalKind::YearToMonth => Numeric::from_i64(i64::from(interval.months)),
        IntervalKind::DayToSecond => Numeric::from_i128(total_micros(interval)),
    };
    let result = if divide {
        if factor.is_zero() {
            return Err(SqlError::new(SqlState::DIVISION_BY_ZERO, "divisor is equal to zero")
                .with_oracle_code(1476));
        }
        amount.divide(factor)?
    } else {
        amount.multiply(factor)?
    };
    let whole = result.rounded(0)?.integral_value().ok_or_else(out_of_range)?;
    match kind {
        IntervalKind::YearToMonth => year_to_month(whole).map(Value::Interval),
        IntervalKind::DayToSecond => day_to_second(whole).map(Value::Interval),
    }
}

pub(crate) fn arithmetic(
    operator: BinaryOperator,
    left: &Value,
    right: &Value,
) -> Option<SqlResult<Value>> {
    match (operator, left, right) {
        (BinaryOperator::Add, stamp, Value::Interval(interval))
        | (BinaryOperator::Add, Value::Interval(interval), stamp)
            if !matches!(stamp, Value::Interval(_)) =>
        {
            shifted(stamp, *interval, 1)
        }
        (BinaryOperator::Subtract, stamp, Value::Interval(interval))
            if !matches!(stamp, Value::Interval(_)) =>
        {
            shifted(stamp, *interval, -1)
        }
        (BinaryOperator::Add | BinaryOperator::Subtract, Value::Interval(a), Value::Interval(b)) => {
            let (Some(first), Some(second)) = (IntervalKind::of_value(*a), IntervalKind::of_value(*b))
            else {
                return None;
            };
            let nonzero = |interval: &Interval| {
                interval.months != 0 || interval.days != 0 || interval.micros != 0
            };
            if first != second && nonzero(a) && nonzero(b) {
                return Some(Err(mixed_kinds()));
            }
            None
        }
        (BinaryOperator::Multiply, Value::Interval(interval), number)
        | (BinaryOperator::Multiply, number, Value::Interval(interval)) => {
            let factor = factor_of(number)?;
            Some(factor.and_then(|factor| scaled_interval(*interval, &factor, false)))
        }
        (BinaryOperator::Divide, Value::Interval(interval), number) => {
            let factor = factor_of(number)?;
            Some(factor.and_then(|factor| scaled_interval(*interval, &factor, true)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        IntervalField, IntervalKind, Qualifier, arithmetic, extract, number_to_day_to_second,
        number_to_year_to_month, parse_literal, parse_text, text, timestamp_difference,
    };
    use crate::ast::BinaryOperator;
    use crate::datetime::{MICROS_PER_DAY, MICROS_PER_SECOND, days_from_civil};
    use crate::interval::Interval;
    use crate::types::Value;

    fn qualifier(leading: IntervalField, trailing: IntervalField, precision: u8, digits: u8) -> Qualifier {
        Qualifier {
            leading,
            trailing,
            leading_precision: precision,
            fraction_digits: digits,
        }
    }

    fn literal(text_value: &str, leading: IntervalField, trailing: IntervalField) -> Result<String, u16> {
        let qualifier = qualifier(leading, trailing, 2, 6);
        parse_literal(text_value, qualifier)
            .map(|interval| text(interval, Some(qualifier.kind()), 6))
            .map_err(|error| error.vendor_code().unwrap_or(0))
    }

    #[test]
    fn literals_read_as_the_interval_literals_section_shows() {
        use IntervalField::{Day, Hour, Minute, Month, Second, Year};
        let shown = |text_value: &str, q: Qualifier| {
            parse_literal(text_value, q)
                .map(|interval| text(interval, Some(q.kind()), q.fraction_digits))
                .map_err(|error| error.vendor_code().unwrap_or(0))
        };
        assert_eq!(
            shown("123-2", qualifier(Year, Month, 3, 0)).as_deref(),
            Ok("+123-02")
        );
        assert_eq!(literal("4", Year, Year).as_deref(), Ok("+04-00"));
        assert_eq!(literal("50", Month, Month).as_deref(), Ok("+04-02"));
        assert_eq!(literal("123", Year, Year), Err(1873));
        assert_eq!(literal("1-12", Year, Month), Err(1843));
        assert_eq!(
            shown("4 5:12:10.222", qualifier(Day, Second, 2, 3)).as_deref(),
            Ok("+04 05:12:10.222")
        );
        assert_eq!(literal("4 5:12", Day, Minute).as_deref(), Ok("+04 05:12:00.000000"));
        assert_eq!(
            shown("400 5", qualifier(Day, Hour, 3, 6)).as_deref(),
            Ok("+400 05:00:00.000000")
        );
        assert_eq!(literal("25", Hour, Hour).as_deref(), Ok("+01 01:00:00.000000"));
        assert_eq!(literal("10:22", Minute, Second).as_deref(), Ok("+00 00:10:22.000000"));
        assert_eq!(
            shown("30.12345", qualifier(Second, Second, 2, 4)).as_deref(),
            Ok("+00 00:00:30.1235")
        );
        assert_eq!(literal("-1 2:00:00", Day, Second).as_deref(), Ok("-01 02:00:00.000000"));
        assert_eq!(literal("4 24:00", Day, Minute), Err(1850));
        assert_eq!(literal("4 5:60", Day, Minute), Err(1851));
        assert_eq!(literal("4:5", Day, Minute), Err(1867));
        assert_eq!(literal("x", Day, Day), Err(1867));
        assert_eq!(literal("", Day, Day), Err(1867));
    }

    #[test]
    fn text_forms_accept_sql_and_iso_durations() {
        let read = |value: &str, kind: IntervalKind| {
            parse_text(value, kind)
                .map(|interval| text(interval, Some(kind), 6))
                .map_err(|error| error.vendor_code().unwrap_or(0))
        };
        assert_eq!(
            read("100 00:00:00", IntervalKind::DayToSecond).as_deref(),
            Ok("+100 00:00:00.000000")
        );
        assert_eq!(
            read("P100DT5H", IntervalKind::DayToSecond).as_deref(),
            Ok("+100 05:00:00.000000")
        );
        assert_eq!(
            read("-P1DT0.5S", IntervalKind::DayToSecond).as_deref(),
            Ok("-01 00:00:00.500000")
        );
        assert_eq!(read("1-2", IntervalKind::YearToMonth).as_deref(), Ok("+01-02"));
        assert_eq!(read("P1Y2M", IntervalKind::YearToMonth).as_deref(), Ok("+01-02"));
        assert_eq!(read("P", IntervalKind::YearToMonth), Err(1867));
        assert_eq!(read("P1X", IntervalKind::YearToMonth), Err(1867));
    }

    #[test]
    fn numbers_become_intervals_of_their_unit() {
        let text_of = |value: Value| match value {
            Value::Interval(interval) => text(interval, None, 6),
            other => format!("{other:?}"),
        };
        let unit = |name: &str| Value::Text(name.to_owned());
        assert_eq!(
            number_to_day_to_second(&Value::BigInt(36), &unit("HOUR")).map(text_of).as_deref(),
            Ok("+01 12:00:00.000000")
        );
        assert_eq!(
            number_to_year_to_month(&Value::Numeric(crate::numeric::Numeric::parse("1.5").expect("1.5")), &unit("year"))
                .map(text_of)
                .as_deref(),
            Ok("+01-06")
        );
        assert_eq!(
            number_to_day_to_second(&Value::BigInt(1), &unit("YEAR"))
                .err()
                .and_then(|error| error.vendor_code()),
            Some(1760)
        );
        assert_eq!(
            number_to_day_to_second(&Value::Null, &unit("DAY")).ok(),
            Some(Value::Null)
        );
    }

    #[test]
    fn arithmetic_moves_dates_and_scales_intervals() {
        let date = |year: i64, month: i64, day: i64| {
            Value::Timestamp(days_from_civil(year, month, day) * MICROS_PER_DAY)
        };
        let month = Value::Interval(Interval {
            months: 1,
            days: 0,
            micros: 0,
        });
        assert_eq!(
            arithmetic(BinaryOperator::Add, &date(2024, 1, 15), &month).and_then(Result::ok),
            Some(date(2024, 2, 15))
        );
        assert_eq!(
            arithmetic(BinaryOperator::Add, &date(2024, 1, 31), &month)
                .and_then(Result::err)
                .and_then(|error| error.vendor_code()),
            Some(1839)
        );
        let day_and_half = Value::Interval(Interval {
            months: 0,
            days: 1,
            micros: 12 * 3600 * MICROS_PER_SECOND,
        });
        assert_eq!(
            arithmetic(BinaryOperator::Multiply, &day_and_half, &Value::BigInt(2))
                .and_then(Result::ok),
            Some(Value::Interval(Interval {
                months: 0,
                days: 3,
                micros: 0
            }))
        );
        assert_eq!(
            arithmetic(BinaryOperator::Subtract, &date(2024, 3, 1), &day_and_half)
                .and_then(Result::ok),
            Some(Value::Timestamp(
                days_from_civil(2024, 2, 28) * MICROS_PER_DAY + 12 * 3600 * MICROS_PER_SECOND
            ))
        );
        assert!(
            arithmetic(BinaryOperator::Add, &month, &day_and_half)
                .is_some_and(|result| result.is_err())
        );
        let difference = timestamp_difference(&date(2024, 1, 2), &date(2024, 1, 1))
            .and_then(Result::ok);
        assert_eq!(
            difference,
            Some(Value::Interval(Interval {
                months: 0,
                days: 1,
                micros: 0
            }))
        );
    }

    #[test]
    fn extract_reads_the_fields_of_its_kind() {
        let interval = Interval {
            months: 0,
            days: 4,
            micros: (5 * 3600 + 12 * 60 + 10) * MICROS_PER_SECOND + 222_000,
        };
        let field = |name: &str| extract(name, interval, None).map(|value| value.to_string());
        assert_eq!(field("day").as_deref(), Some("4"));
        assert_eq!(field("hour").as_deref(), Some("5"));
        assert_eq!(field("minute").as_deref(), Some("12"));
        assert_eq!(field("second").as_deref(), Some("10.222"));
        assert_eq!(field("year"), None);
        let months = Interval {
            months: -14,
            days: 0,
            micros: 0,
        };
        assert_eq!(extract("year", months, None).map(|value| value.to_string()).as_deref(), Some("-1"));
        assert_eq!(extract("month", months, None).map(|value| value.to_string()).as_deref(), Some("-2"));
    }

    #[test]
    fn seeded_random_interval_texts_never_panic() {
        let cases: usize = std::env::var("EGGDB_FUZZ_CASES")
            .ok()
            .and_then(|cases| cases.parse().ok())
            .unwrap_or(3000);
        let mut state: u64 = std::env::var("EGGDB_FUZZ_SEED")
            .ok()
            .and_then(|seed| seed.parse().ok())
            .unwrap_or(0x0049_7e11);
        let pieces = [
            "1", "99", "-", "+", " ", ":", ".", "5", "P", "T", "Y", "M", "D", "H", "S", "x",
            "99999999999999999999", "é",
        ];
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        use IntervalField::{Day, Hour, Minute, Month, Second, Year};
        let fields = [Year, Month, Day, Hour, Minute, Second];
        for _ in 0..cases {
            let mut input = String::new();
            for _ in 0..next() % 8 {
                input.push_str(pieces[next() as usize % pieces.len()]);
            }
            let leading = fields[next() as usize % fields.len()];
            let trailing = fields[next() as usize % fields.len()];
            let q = qualifier(leading, trailing, (next() % 10) as u8, (next() % 10) as u8);
            if q.is_valid() {
                let _ = parse_literal(&input, q);
            }
            let _ = parse_text(&input, IntervalKind::YearToMonth);
            let _ = parse_text(&input, IntervalKind::DayToSecond);
        }
    }
}
