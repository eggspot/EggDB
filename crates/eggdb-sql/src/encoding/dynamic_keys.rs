use super::{NULL_IN_KEY, PRESENT, Reader, double_of_ordered, ordered_double, push_escaped_text};
use crate::error::SqlResult;
use crate::types::Value;

pub(super) const NUMBER_CLASS: u8 = 0x01;
pub(super) const TEXT_CLASS: u8 = 0x02;
pub(super) const BLOB_CLASS: u8 = 0x03;
const TWO_TO_63: f64 = 9_223_372_036_854_775_808.0;

pub(super) fn encode_dynamic_key(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(NULL_IN_KEY),
        Value::Boolean(flag) => integer_key(out, i64::from(*flag)),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => {
            integer_key(out, value.as_integer().unwrap_or(0));
        }
        Value::Real(number) => number_key(out, f64::from(*number), 0),
        Value::Double(number) => number_key(out, *number, 0),
        Value::Numeric(number) => match number.integral_value().map(i64::try_from) {
            Some(Ok(whole)) => integer_key(out, whole),
            _ => number_key(out, number.to_f64(), 0),
        },
        Value::Text(text) | Value::Bpchar(text) => class_key(out, TEXT_CLASS, text.as_bytes()),
        Value::Bytea(bytes) => class_key(out, BLOB_CLASS, bytes),
        other => match other.to_text() {
            Some(text) => class_key(out, TEXT_CLASS, text.as_bytes()),
            None => out.push(NULL_IN_KEY),
        },
    }
}

fn integer_key(out: &mut Vec<u8>, integer: i64) {
    let nearest = integer as f64;
    let offset = i128::from(integer) - nearest as i128;
    number_key(out, nearest, i16::try_from(offset).unwrap_or(0));
}

fn number_key(out: &mut Vec<u8>, nearest: f64, offset: i16) {
    out.push(PRESENT);
    out.push(NUMBER_CLASS);
    out.extend_from_slice(&ordered_double(nearest));
    out.extend_from_slice(&((offset as u16) ^ 0x8000).to_be_bytes());
}

fn class_key(out: &mut Vec<u8>, class: u8, bytes: &[u8]) {
    out.push(PRESENT);
    out.push(class);
    push_escaped_text(out, bytes);
}

fn number_of_key(nearest: f64, offset: i128) -> Option<Value> {
    let canonical = if nearest.is_nan() {
        nearest.to_bits() == f64::NAN.to_bits()
    } else {
        !(nearest == 0.0 && nearest.is_sign_negative())
    };
    if !canonical {
        return None;
    }
    let whole = nearest.fract() == 0.0 && (-TWO_TO_63..=TWO_TO_63).contains(&nearest);
    if offset == 0 {
        return Some(if whole && nearest < TWO_TO_63 {
            Value::BigInt(nearest as i64)
        } else {
            Value::Double(nearest)
        });
    }
    if !whole {
        return None;
    }
    let integer = i64::try_from(nearest as i128 + offset).ok()?;
    (integer as f64 == nearest).then_some(Value::BigInt(integer))
}

impl Reader<'_> {
    pub(super) fn dynamic_key_width(&self) -> SqlResult<usize> {
        let mut probe = Reader {
            bytes: self.bytes,
            at: self.at,
            what: self.what,
        };
        probe.dynamic_key_payload()?;
        Ok(probe.at - self.at)
    }

    pub(super) fn dynamic_key_value(&mut self) -> SqlResult<Value> {
        match self.byte()? {
            NULL_IN_KEY => Ok(Value::Null),
            PRESENT => self.dynamic_key_payload(),
            _ => Err(self.damaged()),
        }
    }

    fn dynamic_key_payload(&mut self) -> SqlResult<Value> {
        match self.byte()? {
            NUMBER_CLASS => {
                let nearest = double_of_ordered(u64::from_be_bytes(self.array()?));
                let offset = i128::from((u16::from_be_bytes(self.array()?) ^ 0x8000) as i16);
                number_of_key(nearest, offset).ok_or_else(|| self.damaged())
            }
            TEXT_CLASS => {
                let bytes = self.escaped_key_bytes()?;
                String::from_utf8(bytes)
                    .map(Value::Text)
                    .map_err(|_| self.damaged())
            }
            BLOB_CLASS => self.escaped_key_bytes().map(Value::Bytea),
            _ => Err(self.damaged()),
        }
    }
}
