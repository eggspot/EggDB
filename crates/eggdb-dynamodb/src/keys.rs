use eggdb_sql::json::{Json, Node};

pub(crate) const TABLE: u8 = b't';
pub(crate) const ITEM: u8 = b'i';

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum KeyValue {
    S(String),
    N(Decimal),
    B(String),
}

pub(crate) fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, node)| node),
        _ => None,
    }
}

pub(crate) fn text<'n>(node: &'n Node, name: &str) -> Option<&'n str> {
    match &member(node, name)?.value {
        Json::String(text) => Some(text),
        _ => None,
    }
}

pub(crate) fn key_value(attribute: &Node, wanted: &str) -> Result<KeyValue, String> {
    let value = match wanted {
        "S" => text(attribute, "S").map(|text| KeyValue::S(text.to_owned())),
        "B" => text(attribute, "B").map(|text| KeyValue::B(text.to_owned())),
        _ => text(attribute, "N")
            .and_then(Decimal::parse)
            .map(KeyValue::N),
    };
    value.ok_or_else(|| format!("a key attribute must be of type {wanted}"))
}

const MOST_DIGITS: usize = 38;

/// A DynamoDB number exactly: its sign, its significant digits without leading or trailing
/// zeros, and where the decimal point falls relative to the first digit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decimal {
    negative: bool,
    digits: Vec<u8>,
    exponent: i32,
}

impl Decimal {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (negative, unsigned) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let (mantissa, power) = match unsigned.split_once(['e', 'E']) {
            Some((mantissa, power)) => (mantissa, power.parse::<i32>().ok()?),
            None => (unsigned, 0),
        };
        let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
        if whole.is_empty() && fraction.is_empty()
            || !whole
                .bytes()
                .chain(fraction.bytes())
                .all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let all: Vec<u8> = whole
            .bytes()
            .chain(fraction.bytes())
            .map(|byte| byte - b'0')
            .collect();
        let Some(first) = all.iter().position(|digit| *digit != 0) else {
            return Some(Self {
                negative: false,
                digits: Vec::new(),
                exponent: 0,
            });
        };
        let last = all.iter().rposition(|digit| *digit != 0).unwrap_or(first);
        let digits = all[first..=last].to_vec();
        if digits.len() > MOST_DIGITS {
            return None;
        }
        let point = i32::try_from(whole.len()).ok()?;
        let exponent = point
            .checked_sub(i32::try_from(first).ok()?)?
            .checked_add(power)?;
        if !(-129..=126).contains(&exponent) {
            return None;
        }
        Some(Self {
            negative,
            digits,
            exponent,
        })
    }

    fn encode(&self, out: &mut Vec<u8>) {
        if self.digits.is_empty() {
            out.push(1);
            return;
        }
        let biased = u16::try_from(self.exponent + 1000)
            .unwrap_or(0)
            .to_be_bytes();
        let mut body: Vec<u8> = biased.to_vec();
        body.extend(self.digits.iter().map(|digit| digit + 1));
        body.push(0);
        if self.negative {
            out.push(0);
            out.extend(body.iter().map(|byte| !byte));
        } else {
            out.push(2);
            out.extend(body);
        }
    }
}

fn escaped(bytes: &[u8], out: &mut Vec<u8>) {
    for byte in bytes {
        out.push(*byte);
        if *byte == 0 {
            out.push(0xff);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

/// The value's bytes, ordered as DynamoDB orders keys of its type.
pub(crate) fn encode(value: &KeyValue, out: &mut Vec<u8>) {
    match value {
        KeyValue::S(text) => {
            out.push(1);
            escaped(text.as_bytes(), out);
        }
        KeyValue::B(text) => {
            out.push(2);
            escaped(text.as_bytes(), out);
        }
        KeyValue::N(number) => {
            out.push(3);
            number.encode(out);
        }
    }
}

pub(crate) fn table_key(table: &str) -> Vec<u8> {
    let mut key = vec![TABLE];
    key.extend_from_slice(table.as_bytes());
    key
}

pub(crate) fn items_prefix(table: &str) -> Vec<u8> {
    let mut key = vec![ITEM];
    key.extend_from_slice(table.as_bytes());
    key.push(0);
    key
}

pub(crate) fn item_key(table: &str, partition: &KeyValue, sort: Option<&KeyValue>) -> Vec<u8> {
    let mut key = items_prefix(table);
    encode(partition, &mut key);
    if let Some(sort) = sort {
        encode(sort, &mut key);
    }
    key
}
