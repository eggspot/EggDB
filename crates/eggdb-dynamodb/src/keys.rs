use eggdb_sql::json::{Json, Node};

pub(crate) const TABLE: u8 = b't';
pub(crate) const ITEM: u8 = b'i';

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum KeyValue {
    S(String),
    N(f64),
    B(String),
}

pub(crate) fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, node)| node),
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
        _ => text(attribute, "N").and_then(|text| text.parse::<f64>().ok().filter(|number| number.is_finite()).map(KeyValue::N)),
    };
    value.ok_or_else(|| format!("a key attribute must be of type {wanted}"))
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
            let bits = number.to_bits();
            let ordered = if bits >> 63 == 1 { !bits } else { bits | (1 << 63) };
            out.extend_from_slice(&ordered.to_be_bytes());
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
