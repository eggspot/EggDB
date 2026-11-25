//! PostgreSQL's bit strings `bit(n)` and `bit varying(n)` (D1330): `B'1010'` and `X'1F'`
//! literals, values kept as their digits, lengths checked on columns and fitted by casts, and
//! the bitwise operators and functions PostgreSQL gives them.

use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{DefinedType, DefinedValue, SqlType, Value};

pub(crate) const BIT_OID: u32 = 1560;
pub(crate) const VARBIT_OID: u32 = 1562;

/// The internal function that fits a value to a cast's length: `(value, length)`.
pub(crate) const FIT_FUNCTION: &str = "eggdb_bit_fit";
/// The internal function behind a column's length check: `(value, length)`.
pub(crate) const LENGTH_CHECK_FUNCTION: &str = "eggdb_bit_length_check";

/// A bit string: its digits, `0` and `1`, and whether its type is `bit varying`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BitString {
    /// Whether it is a `bit varying` value rather than a `bit` value.
    pub varying: bool,
    /// Its digits, each `0` or `1`.
    pub bits: String,
}

fn invalid_digit(digit: char, hex: bool) -> SqlError {
    SqlError::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        if hex {
            format!("\"{digit}\" is not a valid hexadecimal digit")
        } else {
            format!("\"{digit}\" is not a valid binary digit")
        },
    )
}

/// The binary digits of `text` written in binary (`hex` false) or hexadecimal digits.
pub(crate) fn digits(text: &str, hex: bool) -> SqlResult<String> {
    let mut bits = String::with_capacity(text.len() * if hex { 4 } else { 1 });
    for digit in text.chars() {
        if hex {
            let value = digit.to_digit(16).ok_or_else(|| invalid_digit(digit, true))?;
            bits.push_str(&format!("{value:04b}"));
        } else if digit == '0' || digit == '1' {
            bits.push(digit);
        } else {
            return Err(invalid_digit(digit, false));
        }
    }
    Ok(bits)
}

/// A bit string written as `text`, as `bit`'s input reads it: digits, or `B` or `X` and digits.
pub(crate) fn parse(text: &str, varying: bool) -> SqlResult<BitString> {
    let bits = match text.as_bytes().first() {
        Some(b'x' | b'X') => digits(&text[1..], true)?,
        Some(b'b' | b'B') => digits(&text[1..], false)?,
        _ => digits(text, false)?,
    };
    Ok(BitString { varying, bits })
}

pub(crate) fn value(bits: BitString) -> Value {
    Value::Defined(Box::new(DefinedValue::Bits(bits)))
}

pub(crate) fn bits_of(value: &Value) -> Option<&BitString> {
    match value {
        Value::Defined(defined) => match defined.as_ref() {
            DefinedValue::Bits(bits) => Some(bits),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn is_bit_type(sql_type: SqlType) -> bool {
    matches!(sql_type, SqlType::Defined(defined) if defined == DefinedType::BIT || defined == DefinedType::VARBIT)
}

/// Converts `value` to `bit` (`varying` false) or `bit varying`: another bit string keeps its
/// digits, an integer gives its low 32 or 64 bits, text is read as input.
pub(crate) fn convert(value: &Value, varying: bool) -> Option<SqlResult<Value>> {
    if let Some(bits) = bits_of(value) {
        return Some(Ok(self::value(BitString { varying, bits: bits.bits.clone() })));
    }
    let bits = match value {
        Value::Integer(number) => format!("{:032b}", *number as u32),
        Value::BigInt(number) => format!("{:064b}", *number as u64),
        Value::Text(text) => return Some(parse(text, varying).map(self::value)),
        _ => return None,
    };
    Some(Ok(self::value(BitString { varying, bits })))
}

/// The value fitted to `length` bits by a cast: `bit(n)` pads with zeros on the right or cuts,
/// `bit varying(n)` cuts.
pub(crate) fn fit(bits: &BitString, length: usize) -> BitString {
    let mut fitted = bits.bits.chars().take(length).collect::<String>();
    if !bits.varying {
        while fitted.len() < length {
            fitted.push('0');
        }
    }
    BitString { varying: bits.varying, bits: fitted }
}

fn integer_fit(value: &Value, length: usize) -> Option<BitString> {
    let number = match value {
        Value::Integer(number) => i64::from(*number),
        Value::BigInt(number) => *number,
        Value::SmallInt(number) => i64::from(*number),
        _ => return None,
    };
    let all = format!("{:064b}", number as u64);
    let bits = if length <= 64 {
        all[64 - length..].to_owned()
    } else {
        let fill = if number < 0 { '1' } else { '0' };
        std::iter::repeat_n(fill, length - 64).collect::<String>() + &all
    };
    Some(BitString { varying: false, bits })
}

/// Checks a stored value against its column's length, with PostgreSQL's errors.
pub(crate) fn check_length(bits: &BitString, length: usize) -> SqlResult<()> {
    if bits.varying && bits.bits.len() > length {
        return Err(SqlError::new(
            SqlState::STRING_DATA_LENGTH_MISMATCH,
            format!("bit string too long for type bit varying({length})"),
        ));
    }
    if !bits.varying && bits.bits.len() != length {
        return Err(SqlError::new(
            SqlState::STRING_DATA_LENGTH_MISMATCH,
            format!("bit string length {} does not match type bit({length})", bits.bits.len()),
        ));
    }
    Ok(())
}

/// The check that keeps a `bit(n)` or `bit varying(n)` column's values at its length.
pub(crate) fn length_check(column: &crate::ast::Name, length: u32) -> crate::ast::ConstraintSpec {
    use crate::ast::{ConstraintKind, ConstraintSpec, Expr, ExprKind, Literal};
    let position = column.position;
    let at = |kind: ExprKind| Expr { kind, position };
    let expr = at(ExprKind::Function {
        name: LENGTH_CHECK_FUNCTION.to_owned(),
        arguments: vec![
            at(ExprKind::Column { qualifier: None, name: column.text.clone() }),
            at(ExprKind::Literal(Literal::Integer(i64::from(length)))),
        ],
    });
    ConstraintSpec {
        name: None,
        kind: ConstraintKind::Check {
            expr,
            text: format!("{LENGTH_CHECK_FUNCTION}(\"{}\", {length})", column.text.replace('"', "\"\"")),
        },
        position,
        column: Some(column.clone()),
        using_index: None,
        not_valid: false,
    }
}

/// A cast to `bit(n)` or `bit varying(n)`, made into a call that fits its operand to `n` bits:
/// `(operand, n, varying)`.
pub(crate) fn fitted_cast(cast: crate::ast::Expr, length: u32) -> crate::ast::Expr {
    use crate::ast::{Expr, ExprKind, Literal};
    let position = cast.position;
    let ExprKind::Cast { expr, sql_type } = cast.kind else {
        return cast;
    };
    let varying = sql_type == SqlType::Defined(DefinedType::VARBIT);
    let literal = |number: i64| Expr { kind: ExprKind::Literal(Literal::Integer(number)), position };
    Expr {
        kind: ExprKind::Function {
            name: FIT_FUNCTION.to_owned(),
            arguments: vec![*expr, literal(i64::from(length)), literal(i64::from(varying))],
        },
        position,
    }
}

/// The type of a fitting call made by [`fitted_cast`].
pub(crate) fn fitted_type(arguments: &[crate::ast::Expr]) -> SqlType {
    let varying = matches!(
        arguments.get(2).map(|argument| &argument.kind),
        Some(crate::ast::ExprKind::Literal(crate::ast::Literal::Integer(1)))
    );
    SqlType::Defined(if varying { DefinedType::VARBIT } else { DefinedType::BIT })
}

/// The value in PostgreSQL's binary form: the bit count as four bytes, then the bits packed
/// from the high bit of each byte, the last byte padded with zeros.
pub(crate) fn binary(bits: &BitString) -> Option<Vec<u8>> {
    let mut bytes = i32::try_from(bits.bits.len()).ok()?.to_be_bytes().to_vec();
    for chunk in bits.bits.as_bytes().chunks(8) {
        let mut byte = 0u8;
        for (index, digit) in chunk.iter().enumerate() {
            if *digit == b'1' {
                byte |= 0x80 >> index;
            }
        }
        bytes.push(byte);
    }
    Some(bytes)
}

/// The digits of a value sent in PostgreSQL's binary form, or `None` when the bytes do not fit it.
pub(crate) fn text_from_binary(bytes: &[u8]) -> Option<String> {
    let (count, packed) = bytes.split_first_chunk::<4>()?;
    let count = usize::try_from(i32::from_be_bytes(*count)).ok()?;
    if packed.len() != count.div_ceil(8) {
        return None;
    }
    Some((0..count).map(|at| if packed[at / 8] & (0x80 >> (at % 8)) != 0 { '1' } else { '0' }).collect())
}

fn combined(left: &BitString, right: &BitString, symbol: &str, operation: fn(bool, bool) -> bool) -> SqlResult<Value> {
    if left.bits.len() != right.bits.len() {
        return Err(SqlError::new(
            SqlState::STRING_DATA_LENGTH_MISMATCH,
            format!("cannot {symbol} bit strings of different sizes"),
        ));
    }
    let bits = left
        .bits
        .chars()
        .zip(right.bits.chars())
        .map(|(a, b)| if operation(a == '1', b == '1') { '1' } else { '0' })
        .collect();
    Ok(value(BitString { varying: left.varying, bits }))
}

fn shifted(bits: &BitString, by: i64) -> Value {
    let length = bits.bits.len();
    let by_left = by.clamp(-(length as i64), length as i64);
    let shifted = if by_left >= 0 {
        let by = by_left as usize;
        bits.bits[by..].to_owned() + &"0".repeat(by)
    } else {
        let by = by_left.unsigned_abs() as usize;
        "0".repeat(by) + &bits.bits[..length - by]
    };
    value(BitString { varying: bits.varying, bits: shifted })
}

/// The operators `&`, `|`, `#`, `~`, `<<` and `>>` on bit strings, by the internal names the
/// parser gives them, or `None` when the operands are no bit strings.
pub(crate) fn operator(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    let first = bits_of(values.first()?)?;
    Some(match (name, values) {
        (crate::ranges::INTEGER_NOT, [_]) => Ok(value(BitString {
            varying: first.varying,
            bits: first.bits.chars().map(|bit| if bit == '1' { '0' } else { '1' }).collect(),
        })),
        (_, [_, Value::Null]) => Ok(Value::Null),
        (crate::ranges::LEFT_OF, [_, by]) => Ok(shifted(first, by.as_integer()?)),
        (crate::ranges::RIGHT_OF, [_, by]) => Ok(shifted(first, -by.as_integer()?)),
        (crate::ranges::INTEGER_AND, [_, other]) => combined(first, bits_of(other)?, "AND", |a, b| a && b),
        (crate::ranges::INTEGER_OR, [_, other]) => combined(first, bits_of(other)?, "OR", |a, b| a || b),
        (crate::ranges::INTEGER_XOR, [_, other]) => combined(first, bits_of(other)?, "XOR", |a, b| a != b),
        _ => return None,
    })
}

/// `left || right` when both are bit strings.
pub(crate) fn concatenated(left: &Value, right: &Value) -> Option<SqlResult<Value>> {
    let (a, b) = (bits_of(left)?, bits_of(right)?);
    Some(Ok(value(BitString { varying: true, bits: format!("{}{}", a.bits, b.bits) })))
}

fn bit_index(bits: &BitString, at: &Value) -> SqlResult<usize> {
    at.as_integer()
        .and_then(|at| usize::try_from(at).ok())
        .filter(|at| *at < bits.bits.len())
        .ok_or_else(|| {
            SqlError::new(
                SqlState::ARRAY_SUBSCRIPT_ERROR,
                format!(
                    "bit index {} out of valid range (0..{})",
                    at.as_integer().unwrap_or(0),
                    bits.bits.len().saturating_sub(1)
                ),
            )
        })
}

/// The functions on bit strings, when their first argument is one.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    match (name, values) {
        (FIT_FUNCTION, [Value::Null, ..]) | (LENGTH_CHECK_FUNCTION, [Value::Null, _]) => {
            return Some(Ok(Value::Null));
        }
        (FIT_FUNCTION, [found, length, varying]) => {
            let length = usize::try_from(length.as_integer()?).ok()?;
            let varying = varying.as_integer() == Some(1);
            let fitted = match (bits_of(found), found) {
                (Some(bits), _) => fit(&BitString { varying, bits: bits.bits.clone() }, length),
                (None, Value::Text(text)) => match parse(text, varying) {
                    Ok(bits) => fit(&bits, length),
                    Err(error) => return Some(Err(error)),
                },
                (None, _) => match integer_fit(found, length) {
                    Some(bits) => BitString { varying, bits: bits.bits },
                    None => {
                        return Some(Err(SqlError::new(
                            SqlState::CANNOT_COERCE,
                            format!(
                                "cannot cast type {} to {}",
                                found.type_name(),
                                if varying { "bit varying" } else { "bit" }
                            ),
                        )));
                    }
                },
            };
            return Some(Ok(value(fitted)));
        }
        (LENGTH_CHECK_FUNCTION, [found, length]) => {
            let length = usize::try_from(length.as_integer()?).ok()?;
            return Some(check_length(bits_of(found)?, length).map(|()| Value::Boolean(true)));
        }
        _ => {}
    }
    let first = bits_of(values.first()?)?;
    let integer = |at: usize| values.get(at).and_then(Value::as_integer);
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    Some(Ok(match name {
        "length" | "bit_length" => Value::Integer(first.bits.len() as i32),
        "octet_length" => Value::Integer(first.bits.len().div_ceil(8) as i32),
        "bit_count" => Value::BigInt(first.bits.chars().filter(|bit| *bit == '1').count() as i64),
        "get_bit" => match values.get(1).map(|at| bit_index(first, at)) {
            Some(Ok(at)) => Value::Integer(i32::from(first.bits.as_bytes()[at] == b'1')),
            Some(Err(error)) => return Some(Err(error)),
            None => return None,
        },
        "set_bit" => {
            let at = match values.get(1).map(|at| bit_index(first, at)) {
                Some(Ok(at)) => at,
                Some(Err(error)) => return Some(Err(error)),
                None => return None,
            };
            let new = match integer(2) {
                Some(0) => '0',
                Some(1) => '1',
                _ => {
                    return Some(Err(SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "new bit must be 0 or 1",
                    )));
                }
            };
            let mut bits: Vec<char> = first.bits.chars().collect();
            bits[at] = new;
            value(BitString { varying: first.varying, bits: bits.into_iter().collect() })
        }
        "substring" | "substr" => {
            let start = integer(1)?;
            let count = integer(2);
            let from = start.max(1) - 1;
            let end = match count {
                Some(count) if count < 0 => {
                    return Some(Err(SqlError::new(
                        SqlState::SUBSTRING_ERROR,
                        "negative substring length not allowed",
                    )));
                }
                Some(count) => start.saturating_add(count).saturating_sub(1).max(from),
                None => first.bits.len() as i64,
            };
            let bits = first
                .bits
                .chars()
                .skip(from as usize)
                .take((end - from).max(0) as usize)
                .collect();
            value(BitString { varying: true, bits })
        }
        "position" | "strpos" => {
            let needle = bits_of(values.get(1)?)?;
            Value::Integer(if needle.bits.is_empty() {
                1
            } else {
                first.bits.find(&needle.bits).map_or(0, |at| at as i32 + 1)
            })
        }
        "overlay" => {
            let replacement = bits_of(values.get(1)?)?;
            let start = usize::try_from(integer(2)?.max(1) - 1).ok()?;
            let count = integer(3).map_or(replacement.bits.len(), |count| usize::try_from(count.max(0)).unwrap_or(0));
            let head: String = first.bits.chars().take(start).collect();
            let tail: String = first.bits.chars().skip(start + count).collect();
            value(BitString { varying: true, bits: format!("{head}{}{tail}", replacement.bits) })
        }
        _ => return None,
    }))
}

/// The type a bit-string function returns when its first argument has type `first`.
pub(crate) fn result_type(name: &str, first: Option<SqlType>) -> Option<SqlType> {
    if name == LENGTH_CHECK_FUNCTION {
        return Some(SqlType::Boolean);
    }
    if !first.is_some_and(is_bit_type) {
        return None;
    }
    match name {
        "length" | "bit_length" | "octet_length" | "get_bit" | "position" | "strpos" => Some(SqlType::Integer),
        "bit_count" => Some(SqlType::BigInt),
        "set_bit"
        | crate::ranges::INTEGER_AND
        | crate::ranges::INTEGER_OR
        | crate::ranges::INTEGER_XOR
        | crate::ranges::INTEGER_NOT
        | crate::ranges::LEFT_OF
        | crate::ranges::RIGHT_OF => first,
        "substring" | "substr" | "overlay" => Some(SqlType::Defined(DefinedType::VARBIT)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits(text: &str) -> BitString {
        parse(text, false).unwrap()
    }

    #[test]
    fn literals_read_binary_and_hexadecimal_digits() {
        assert_eq!(bits("1010").bits, "1010");
        assert_eq!(bits("X1F").bits, "00011111");
        assert_eq!(digits("1F", true).unwrap(), "00011111");
        assert!(parse("102", false).is_err());
        assert_eq!(fit(&bits("101"), 5).bits, "10100");
        assert_eq!(fit(&BitString { varying: true, bits: "101".into() }, 5).bits, "101");
        assert_eq!(fit(&bits("10110"), 3).bits, "101");
        assert_eq!(integer_fit(&Value::Integer(5), 8).unwrap().bits, "00000101");
        assert!(check_length(&bits("101"), 4).is_err());
        let packed = binary(&bits("101000011")).unwrap();
        assert_eq!(packed, [0, 0, 0, 9, 0b1010_0001, 0b1000_0000]);
        assert_eq!(text_from_binary(&packed).as_deref(), Some("101000011"));
        assert!(text_from_binary(&[0, 0, 0, 9, 1]).is_none());
    }

    #[test]
    fn operators_and_functions_follow_postgresql() {
        let shown = |result: Option<SqlResult<Value>>| result.unwrap().unwrap().to_text().unwrap_or_default();
        let a = value(bits("1100"));
        let b = value(bits("1010"));
        assert_eq!(shown(operator(crate::ranges::INTEGER_AND, &[a.clone(), b.clone()])), "1000");
        assert_eq!(shown(operator(crate::ranges::INTEGER_OR, &[a.clone(), b.clone()])), "1110");
        assert_eq!(shown(operator(crate::ranges::INTEGER_XOR, &[a.clone(), b.clone()])), "0110");
        assert_eq!(shown(operator(crate::ranges::INTEGER_NOT, std::slice::from_ref(&a))), "0011");
        assert_eq!(shown(operator(crate::ranges::LEFT_OF, &[a.clone(), Value::Integer(1)])), "1000");
        assert_eq!(shown(operator(crate::ranges::RIGHT_OF, &[a.clone(), Value::Integer(2)])), "0011");
        assert!(operator(crate::ranges::INTEGER_AND, &[a.clone(), value(bits("1"))]).unwrap().is_err());
        assert_eq!(shown(concatenated(&a, &b)), "11001010");
        assert_eq!(shown(function("bit_count", std::slice::from_ref(&a))), "2");
        assert_eq!(shown(function("get_bit", &[b.clone(), Value::Integer(2)])), "1");
        assert_eq!(shown(function("set_bit", &[b.clone(), Value::Integer(0), Value::Integer(0)])), "0010");
        assert_eq!(shown(function("substring", &[b.clone(), Value::Integer(2), Value::Integer(2)])), "01");
        assert_eq!(shown(function("position", &[b.clone(), value(bits("01"))])), "2");
        assert_eq!(shown(function("overlay", &[a, value(bits("11")), Value::Integer(3)])), "1111");
        assert!(function("length", &[Value::Text("abc".into())]).is_none());
    }

    fn answer(session: &mut crate::Session<'_>, sql: &str) -> String {
        let mut outputs = Vec::new();
        session
            .execute(sql, &mut |result| outputs.push(crate::tests::render(&result)))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        outputs.pop().unwrap_or_default()
    }

    #[test]
    fn bit_columns_check_lengths_and_operators_work() {
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        answer(
            &mut session,
            "CREATE TABLE flags (id integer PRIMARY KEY, f bit(4), v varbit(6));
             INSERT INTO flags VALUES (1, B'1010', B'11'), (2, '0110', X'F')",
        );
        assert_eq!(
            answer(&mut session, "SELECT f, v, pg_typeof(f), pg_typeof(v) FROM flags ORDER BY f"),
            "f|v|pg_typeof|pg_typeof\n0110|1111|bit|bit varying\n1010|11|bit|bit varying\nSELECT 2"
        );
        assert_eq!(
            answer(
                &mut session,
                "SELECT f & B'1100' AS a, f | B'0001' AS o, ~f AS n, f << 1 AS s, f || v AS c, length(v) AS l, get_bit(f, 0) AS g FROM flags WHERE id = 1"
            ),
            "a|o|n|s|c|l|g\n1000|1011|0101|0100|101011|2|1\nSELECT 1"
        );
        assert_eq!(
            answer(&mut session, "SELECT 5::bit(8), B'101'::bit(5), CAST(B'10110' AS varbit(3))"),
            "bit|bit|varbit\n00000101|10100|101\nSELECT 1"
        );
        let refused = session.execute("INSERT INTO flags VALUES (3, B'101', B'1')", &mut |_| {}).unwrap_err();
        assert_eq!(refused.state(), SqlState::STRING_DATA_LENGTH_MISMATCH);
        let refused = session.execute("INSERT INTO flags VALUES (3, B'1010', B'1111111')", &mut |_| {}).unwrap_err();
        assert_eq!(refused.state(), SqlState::STRING_DATA_LENGTH_MISMATCH);
    }
}
