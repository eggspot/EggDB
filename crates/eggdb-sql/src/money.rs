//! PostgreSQL's `money` type (D1329): a currency amount kept as a whole number of cents, read
//! and printed as PostgreSQL does with `lc_monetary` set to `C` (`$1,234.56`), with its
//! arithmetic, casts and `sum`.

use crate::ast::BinaryOperator;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::types::{DefinedType, DefinedValue, SqlType, Value};

pub(crate) const MONEY_OID: u32 = 790;

const CENTS_PER_UNIT: i64 = 100;

fn out_of_range(text: &str) -> SqlError {
    SqlError::new(
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("value \"{text}\" is out of range for type money"),
    )
}

fn invalid(text: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type money: \"{text}\""),
    )
}

fn overflow() -> SqlError {
    SqlError::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "money out of range")
}

fn division_by_zero() -> SqlError {
    SqlError::new(SqlState::DIVISION_BY_ZERO, "division by zero")
}

/// Reads `text` as `money`'s input does: spaces, a sign or parentheses, an optional `$`,
/// digits with `,` between groups, and a fraction rounded half away from zero to cents.
pub(crate) fn parse(text: &str) -> SqlResult<i64> {
    let mut rest = text.trim();
    let mut negative = false;
    if let Some(inner) = rest.strip_prefix('(').and_then(|inner| inner.strip_suffix(')')) {
        negative = true;
        rest = inner.trim();
    }
    if let Some(after) = rest.strip_prefix('-') {
        negative = !negative;
        rest = after.trim_start();
    } else if let Some(after) = rest.strip_prefix('+') {
        rest = after.trim_start();
    }
    rest = rest.strip_prefix('$').map_or(rest, str::trim_start);
    if let Some(after) = rest.strip_prefix('-') {
        negative = !negative;
        rest = after.trim_start();
    }
    if let Some(before) = rest.strip_suffix('-') {
        negative = !negative;
        rest = before.trim_end();
    }
    let (whole, fraction) = rest.split_once('.').unwrap_or((rest, ""));
    let whole: String = whole.chars().filter(|character| *character != ',').collect();
    if whole.is_empty() && fraction.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid(text));
    }
    let mut cents: i128 = 0;
    for digit in whole.bytes() {
        cents = cents * 10 + i128::from(digit - b'0');
        if cents > i128::from(i64::MAX) {
            return Err(out_of_range(text));
        }
    }
    let mut digits = fraction.bytes().map(|digit| i128::from(digit - b'0'));
    let tenths = digits.next().unwrap_or(0);
    let hundredths = digits.next().unwrap_or(0);
    cents = cents * 100 + tenths * 10 + hundredths;
    if digits.next().is_some_and(|thousandths| thousandths >= 5) {
        cents += 1;
    }
    let cents = if negative { -cents } else { cents };
    i64::try_from(cents).map_err(|_| out_of_range(text))
}

/// The amount as PostgreSQL prints it: `$` and the units grouped by threes with `,`, a `.`
/// and two digits of cents, with `-` before the `$` when negative.
pub(crate) fn to_text(cents: i64) -> String {
    let magnitude = cents.unsigned_abs();
    let units = (magnitude / 100).to_string();
    let mut grouped = String::with_capacity(units.len() + units.len() / 3);
    for (index, digit) in units.chars().enumerate() {
        if index > 0 && (units.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}${grouped}.{:02}", magnitude % 100)
}

pub(crate) fn value(cents: i64) -> Value {
    Value::Defined(Box::new(DefinedValue::Money(cents)))
}

/// The cents inside `value`, if it is a `money` value.
pub(crate) fn cents_of(value: &Value) -> Option<i64> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Money(cents) => Some(*cents),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn is_money(sql_type: SqlType) -> bool {
    sql_type == SqlType::Defined(DefinedType::MONEY)
}

/// The cents of a number cast to `money`: integers as whole units, `numeric` rounded to cents.
pub(crate) fn from_number(value: &Value) -> Option<SqlResult<i64>> {
    Some(match value {
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => value
            .as_integer()
            .and_then(|units| units.checked_mul(CENTS_PER_UNIT))
            .ok_or_else(overflow),
        Value::Numeric(number) => number
            .multiply(&Numeric::from_i64(CENTS_PER_UNIT))
            .ok()
            .and_then(|scaled| scaled.to_i128_rounded())
            .and_then(|cents| i64::try_from(cents).ok())
            .ok_or_else(overflow),
        Value::Real(_) | Value::Double(_) => {
            let cents = (value.as_float().unwrap_or(f64::NAN) * 100.0).round();
            if cents.is_finite() && cents.abs() < 9.2e18 { Ok(cents as i64) } else { Err(overflow()) }
        }
        _ => return None,
    })
}

/// The amount as a `numeric` with two decimal places.
pub(crate) fn to_numeric(cents: i64) -> Numeric {
    Numeric::from_scaled(i128::from(cents), 2)
}

fn scaled(cents: i64, factor: f64) -> SqlResult<i64> {
    let product = (cents as f64 * factor).round();
    if product.is_finite() && product.abs() < 9.2e18 { Ok(product as i64) } else { Err(overflow()) }
}

fn number_of(value: &Value) -> Option<Numeric> {
    match value {
        Value::Numeric(number) => Some(number.clone()),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => value.as_integer().map(Numeric::from_i64),
        _ => None,
    }
}

/// `money`'s arithmetic, or `None` when neither side is `money`.
pub(crate) fn arithmetic(operator: BinaryOperator, left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    let (left_cents, right_cents) = (cents_of(left), cents_of(right));
    if left_cents.is_none() && right_cents.is_none() {
        return None;
    }
    if left.is_null() || right.is_null() {
        return Some(Ok(Value::Null));
    }
    Some(match (operator, left_cents, right_cents) {
        (BinaryOperator::Add, Some(a), Some(b)) => a.checked_add(b).map(value).ok_or_else(overflow),
        (BinaryOperator::Subtract, Some(a), Some(b)) => a.checked_sub(b).map(value).ok_or_else(overflow),
        (BinaryOperator::Divide, Some(_), Some(0)) => Err(division_by_zero()),
        (BinaryOperator::Divide, Some(a), Some(b)) => Ok(Value::Double(a as f64 / b as f64)),
        (BinaryOperator::Multiply, Some(cents), None) => multiply(cents, right),
        (BinaryOperator::Multiply, None, Some(cents)) => multiply(cents, left),
        (BinaryOperator::Divide, Some(cents), None) => divide(cents, right),
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!(
                "operator does not exist: {} {} {}",
                left.type_name(),
                operator.symbol(),
                right.type_name()
            ),
        )),
    })
}

fn multiply(cents: i64, factor: &Value) -> SqlResult<Value> {
    if let Some(number) = number_of(factor) {
        let product = to_numeric(cents).multiply(&number)?;
        let cents = product
            .rounded(2)?
            .multiply(&Numeric::from_i64(CENTS_PER_UNIT))?
            .to_i128_rounded()
            .and_then(|cents| i64::try_from(cents).ok())
            .ok_or_else(overflow)?;
        return Ok(value(cents));
    }
    match factor.as_float() {
        Some(factor) => scaled(cents, factor).map(value),
        None => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "operator does not exist: money * unknown")),
    }
}

fn divide(cents: i64, divisor: &Value) -> SqlResult<Value> {
    if let Some(whole) = match divisor {
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => divisor.as_integer(),
        _ => None,
    } {
        if whole == 0 {
            return Err(division_by_zero());
        }
        return cents.checked_div(whole).map(value).ok_or_else(overflow);
    }
    match divisor.as_float() {
        Some(divisor) if divisor == 0.0 => Err(division_by_zero()),
        Some(divisor) => scaled(cents, 1.0 / divisor).map(value),
        None => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "operator does not exist: money / unknown")),
    }
}

/// The type of `left operator right` when either side is `money`.
pub(crate) fn arithmetic_type(operator: BinaryOperator, left: SqlType, right: SqlType) -> Option<SqlType> {
    let money = SqlType::Defined(DefinedType::MONEY);
    match (operator, is_money(left), is_money(right)) {
        (BinaryOperator::Divide, true, true) => Some(SqlType::Double),
        (BinaryOperator::Add | BinaryOperator::Subtract, true, true)
        | (BinaryOperator::Multiply, true, false)
        | (BinaryOperator::Multiply, false, true)
        | (BinaryOperator::Divide, true, false) => Some(money),
        _ => None,
    }
}

const ONES: [&str; 20] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve",
    "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen",
];
const TENS: [&str; 10] = ["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"];

fn below_thousand(number: u64) -> String {
    let hundreds = number / 100;
    let rest = number % 100;
    let mut words = Vec::new();
    if hundreds > 0 {
        words.push(format!("{} hundred", ONES[hundreds as usize]));
    }
    if rest >= 20 {
        let tens = TENS[(rest / 10) as usize];
        words.push(if rest % 10 == 0 { tens.to_owned() } else { format!("{tens} {}", ONES[(rest % 10) as usize]) });
    } else if rest > 0 {
        words.push(ONES[rest as usize].to_owned());
    }
    words.join(" ")
}

/// The amount in English words, as PostgreSQL's `cash_words` spells it.
pub(crate) fn words(cents: i64) -> String {
    const GROUPS: [(u64, &str); 6] = [
        (1_000_000_000_000_000_000, "quintillion"),
        (1_000_000_000_000_000, "quadrillion"),
        (1_000_000_000_000, "trillion"),
        (1_000_000_000, "billion"),
        (1_000_000, "million"),
        (1_000, "thousand"),
    ];
    let magnitude = cents.unsigned_abs();
    let mut units = magnitude / 100;
    let cents_part = magnitude % 100;
    let dollars = units;
    let mut parts = Vec::new();
    for (size, name) in GROUPS {
        if units >= size {
            parts.push(format!("{} {name}", below_thousand(units / size)));
            units %= size;
        }
    }
    if units > 0 {
        parts.push(below_thousand(units));
    }
    if parts.is_empty() {
        parts.push("zero".to_owned());
    }
    let mut text = if cents < 0 { "minus ".to_owned() } else { String::new() };
    text.push_str(&parts.join(" "));
    text.push_str(if dollars == 1 { " dollar and " } else { " dollars and " });
    text.push_str(&if cents_part == 0 { "zero".to_owned() } else { below_thousand(cents_part) });
    text.push_str(if cents_part == 1 { " cent" } else { " cents" });
    let mut characters = text.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
        .unwrap_or_default()
}

/// `cash_words`, or `None` for another name.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if name != "cash_words" {
        return None;
    }
    Some(match values {
        [Value::Null] => Ok(Value::Null),
        [single] => match cents_of(single) {
            Some(cents) => Ok(Value::Text(words(cents))),
            None => match single {
                Value::Text(text) => parse(text).map(|cents| Value::Text(words(cents))),
                _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "function cash_words takes money")),
            },
        },
        _ => Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "function cash_words takes one argument")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_read_and_print_as_postgresql_does() {
        assert_eq!(parse("$1,234.56").unwrap(), 123_456);
        assert_eq!(parse("1234.565").unwrap(), 123_457);
        assert_eq!(parse("-$12.5").unwrap(), -1_250);
        assert_eq!(parse("($3.00)").unwrap(), -300);
        assert_eq!(parse("  7 ").unwrap(), 700);
        assert!(parse("12a").is_err());
        assert!(parse("").is_err());
        assert!(parse("99999999999999999999").is_err());
        assert_eq!(to_text(123_456), "$1,234.56");
        assert_eq!(to_text(-5), "-$0.05");
        assert_eq!(to_text(100_000_000), "$1,000,000.00");
    }

    #[test]
    fn arithmetic_keeps_cents() {
        let shown = |result: Option<SqlResult<Value>>| result.unwrap().unwrap().to_text().unwrap_or_default();
        assert_eq!(shown(arithmetic(BinaryOperator::Add, &value(150), &value(275))), "$4.25");
        assert_eq!(shown(arithmetic(BinaryOperator::Multiply, &value(1_000), &Value::Double(1.5))), "$15.00");
        assert_eq!(shown(arithmetic(BinaryOperator::Divide, &value(1_000), &Value::Integer(3))), "$3.33");
        assert_eq!(shown(arithmetic(BinaryOperator::Divide, &value(1_000), &value(400))), "2.5");
        assert!(arithmetic(BinaryOperator::Divide, &value(1), &Value::Integer(0)).unwrap().is_err());
        assert!(arithmetic(BinaryOperator::Add, &Value::Integer(1), &Value::Integer(2)).is_none());
    }

    #[test]
    fn cash_words_spells_the_amount() {
        assert_eq!(words(123_456), "One thousand two hundred thirty four dollars and fifty six cents");
        assert_eq!(words(100), "One dollar and zero cents");
        assert_eq!(words(1), "Zero dollars and one cent");
    }

    fn answer(session: &mut crate::Session<'_>, sql: &str) -> String {
        let mut outputs = Vec::new();
        session
            .execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        outputs.pop().unwrap_or_default()
    }

    #[test]
    fn money_columns_store_sum_and_convert() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        answer(
            &mut session,
            "CREATE TABLE invoices (id integer PRIMARY KEY, total money);
             CREATE INDEX invoices_total ON invoices (total);
             INSERT INTO invoices VALUES (1, '$1,234.56'), (2, 10.5), (3, '-$0.75')",
        );
        assert_eq!(
            answer(&mut session, "SELECT id, total FROM invoices ORDER BY total"),
            "id|total\n3|-$0.75\n2|$10.50\n1|$1,234.56\nSELECT 3"
        );
        assert_eq!(
            answer(&mut session, "SELECT sum(total), pg_typeof(sum(total)) FROM invoices"),
            "sum|pg_typeof\n$1,244.31|money\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT total * 2, total / 4, -total, total::numeric FROM invoices WHERE id = 2"),
            "?column?|?column?|?column?|total\n$21.00|$2.62|-$10.50|10.50\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT id FROM invoices WHERE total > '$5' ORDER BY id"),
            "id\n1\n2\nSELECT 2"
        );
        assert_eq!(
            answer(&mut session, "SELECT cash_words('$12.01'::money)"),
            "cash_words\nTwelve dollars and one cent\nSELECT 1"
        );
    }
}
