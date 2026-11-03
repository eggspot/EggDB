use crate::types::Value;

/// The type affinity of a column whose rows may hold values of different storage classes, as
/// SQLite's datatype documentation defines it: what a value is turned into when it is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeAffinity {
    /// `TEXT`: a number is stored as its text; text and blobs are kept.
    Text,
    /// `NUMERIC`: text that is a well-formed number is stored as an integer or a real, and a
    /// real that is a whole number as an integer; other text and blobs are kept.
    Numeric,
    /// `REAL`: as `NUMERIC`, except that every number is stored as a real.
    Real,
    /// `BLOB` (and a column without a declared type): every value is kept in its class.
    Blob,
}

const LARGEST_WHOLE_REAL: f64 = 9_223_372_036_854_775_808.0;

impl TypeAffinity {
    pub(crate) const ALL: [Self; 4] = [Self::Text, Self::Numeric, Self::Real, Self::Blob];

    pub(crate) fn code(self) -> u8 {
        match self {
            Self::Text => 1,
            Self::Numeric => 2,
            Self::Real => 3,
            Self::Blob => 4,
        }
    }

    pub(crate) fn of_code(code: u8) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|affinity| affinity.code() == code)
    }

    pub(crate) fn sqlite_name(self) -> &'static str {
        match self {
            Self::Text => "TEXT",
            Self::Numeric => "NUMERIC",
            Self::Real => "REAL",
            Self::Blob => "BLOB",
        }
    }

    pub(crate) fn type_name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Numeric => "numeric",
            Self::Real => "real",
            Self::Blob => "blob",
        }
    }

    pub(crate) fn stored(self, value: Value) -> Value {
        match self {
            Self::Blob => value,
            Self::Text => as_text(value),
            Self::Numeric => as_number(value),
            Self::Real => match as_number(value) {
                Value::BigInt(number) => Value::Double(number as f64),
                other => other,
            },
        }
    }
}

fn as_text(value: Value) -> Value {
    match value {
        Value::BigInt(number) => Value::Text(number.to_string()),
        Value::Double(number) => {
            crate::sqlite_functions::real_text(number).map_or(Value::Null, Value::Text)
        }
        other => other,
    }
}

fn as_number(value: Value) -> Value {
    match value {
        Value::Text(text) => match number_of_text(&text) {
            Some(number) => whole_when_exact(number),
            None => Value::Text(text),
        },
        Value::Double(number) => whole_when_exact(Value::Double(number)),
        other => other,
    }
}

fn whole_when_exact(value: Value) -> Value {
    match value {
        Value::Double(number)
            if number.fract() == 0.0
                && (-LARGEST_WHOLE_REAL..LARGEST_WHOLE_REAL).contains(&number) =>
        {
            Value::BigInt(number as i64)
        }
        other => other,
    }
}

pub(crate) fn number_of_text(text: &str) -> Option<Value> {
    let literal = text.trim_matches(|character: char| character.is_ascii_whitespace());
    let unsigned = literal
        .strip_prefix(['+', '-'])
        .unwrap_or(literal)
        .as_bytes();
    let digits_before = unsigned
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let mut at = digits_before;
    let mut digits_after = 0;
    if unsigned.get(at) == Some(&b'.') {
        at += 1;
        digits_after = unsigned
            .get(at..)
            .unwrap_or(&[])
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        at += digits_after;
    }
    if digits_before + digits_after == 0 {
        return None;
    }
    let mut has_exponent = false;
    if matches!(unsigned.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(unsigned.get(at), Some(b'+' | b'-')) {
            at += 1;
        }
        let exponent_digits = unsigned
            .get(at..)
            .unwrap_or(&[])
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if exponent_digits == 0 {
            return None;
        }
        at += exponent_digits;
        has_exponent = true;
    }
    if at != unsigned.len() {
        return None;
    }
    let is_integer = !has_exponent && digits_before == at;
    if is_integer && let Ok(number) = literal.parse::<i64>() {
        return Some(Value::BigInt(number));
    }
    literal
        .parse::<f64>()
        .ok()
        .filter(|number| !number.is_nan())
        .map(Value::Double)
}

#[cfg(test)]
mod tests {
    use super::{TypeAffinity, number_of_text};
    use crate::types::Value;

    fn text(value: &str) -> Value {
        Value::Text(value.to_owned())
    }

    #[test]
    fn codes_name_every_affinity_once() {
        for affinity in TypeAffinity::ALL {
            assert_eq!(TypeAffinity::of_code(affinity.code()), Some(affinity));
        }
        assert_eq!(TypeAffinity::of_code(0), None);
        assert_eq!(TypeAffinity::of_code(5), None);
        assert_eq!(TypeAffinity::of_code(0xFF), None);
    }

    #[test]
    fn well_formed_numbers_are_read_and_the_rest_is_not() {
        assert_eq!(number_of_text("42"), Some(Value::BigInt(42)));
        assert_eq!(number_of_text(" -7 "), Some(Value::BigInt(-7)));
        assert_eq!(number_of_text("+3"), Some(Value::BigInt(3)));
        assert_eq!(number_of_text("2.5"), Some(Value::Double(2.5)));
        assert_eq!(number_of_text(".5"), Some(Value::Double(0.5)));
        assert_eq!(number_of_text("5."), Some(Value::Double(5.0)));
        assert_eq!(number_of_text("3.0e+5"), Some(Value::Double(300_000.0)));
        assert_eq!(
            number_of_text("9223372036854775808"),
            Some(Value::Double(9_223_372_036_854_775_808.0))
        );
        for refused in [
            "", " ", "abc", "0x10", "1e", "1e+", ".", "-", "1.2.3", "12abc", "inf", "NaN", "1_000",
            "--1", "e5",
        ] {
            assert_eq!(number_of_text(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn numeric_affinity_stores_whole_numbers_as_integers() {
        let numeric = TypeAffinity::Numeric;
        assert_eq!(numeric.stored(text("3.0e+5")), Value::BigInt(300_000));
        assert_eq!(numeric.stored(text("7")), Value::BigInt(7));
        assert_eq!(numeric.stored(text("2.5")), Value::Double(2.5));
        assert_eq!(numeric.stored(Value::Double(4.0)), Value::BigInt(4));
        assert_eq!(numeric.stored(Value::Double(-0.0)), Value::BigInt(0));
        assert_eq!(numeric.stored(text("2024-02-29")), text("2024-02-29"));
        assert_eq!(numeric.stored(text("0x10")), text("0x10"));
        assert_eq!(numeric.stored(Value::Bytea(vec![1])), Value::Bytea(vec![1]));
        assert_eq!(numeric.stored(Value::Null), Value::Null);
        assert_eq!(
            numeric.stored(Value::Double(1e300)),
            Value::Double(1e300),
            "a real too large for an integer stays a real"
        );
    }

    #[test]
    fn real_affinity_stores_every_number_as_a_real() {
        let real = TypeAffinity::Real;
        assert_eq!(real.stored(Value::BigInt(7)), Value::Double(7.0));
        assert_eq!(real.stored(text("7")), Value::Double(7.0));
        assert_eq!(real.stored(Value::Double(2.5)), Value::Double(2.5));
        assert_eq!(real.stored(text("seven")), text("seven"));
    }

    #[test]
    fn text_affinity_stores_numbers_as_their_text() {
        let affinity = TypeAffinity::Text;
        assert_eq!(affinity.stored(Value::BigInt(42)), text("42"));
        assert_eq!(affinity.stored(Value::Double(2.0)), text("2.0"));
        assert_eq!(affinity.stored(Value::Double(f64::NAN)), Value::Null);
        assert_eq!(affinity.stored(text("x")), text("x"));
        assert_eq!(
            affinity.stored(Value::Bytea(vec![0])),
            Value::Bytea(vec![0])
        );
    }

    #[test]
    fn blob_affinity_keeps_every_value() {
        for value in [
            Value::BigInt(1),
            Value::Double(1.0),
            text("1"),
            Value::Bytea(vec![]),
        ] {
            assert_eq!(TypeAffinity::Blob.stored(value.clone()), value);
        }
    }
}
