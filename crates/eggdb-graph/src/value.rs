use std::cmp::Ordering;

/// A node as a query returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    /// Its id.
    pub id: u64,
    /// Its labels.
    pub labels: Vec<String>,
    /// Its properties, in name order.
    pub properties: Vec<(String, Value)>,
}

/// A relationship as a query returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct Relationship {
    /// Its id.
    pub id: u64,
    /// The node it starts at.
    pub start: u64,
    /// The node it ends at.
    pub end: u64,
    /// Its type.
    pub kind: String,
    /// Its properties, in name order.
    pub properties: Vec<(String, Value)>,
}

/// A Cypher value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null`.
    Null,
    /// A boolean.
    Boolean(bool),
    /// A 64-bit integer.
    Integer(i64),
    /// A float.
    Float(f64),
    /// A string.
    String(String),
    /// A list.
    List(Vec<Value>),
    /// A map.
    Map(Vec<(String, Value)>),
    /// A node.
    Node(Box<Node>),
    /// A relationship.
    Relationship(Box<Relationship>),
}

impl Value {
    /// Whether the value is true; null and non-booleans are not.
    pub fn is_true(&self) -> bool {
        matches!(self, Value::Boolean(true))
    }

    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Integer(number) => Some(*number as f64),
            Value::Float(number) => Some(*number),
            _ => None,
        }
    }

    pub(crate) fn kind_rank(&self) -> u8 {
        match self {
            Value::Map(_) => 0,
            Value::Node(_) => 1,
            Value::Relationship(_) => 2,
            Value::List(_) => 3,
            Value::String(_) => 5,
            Value::Boolean(_) => 6,
            Value::Integer(_) | Value::Float(_) => 7,
            Value::Null => 9,
        }
    }

    /// The type's name, as `valueType`-style messages spell it.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Boolean(_) => "BOOLEAN",
            Value::Integer(_) => "INTEGER",
            Value::Float(_) => "FLOAT",
            Value::String(_) => "STRING",
            Value::List(_) => "LIST",
            Value::Map(_) => "MAP",
            Value::Node(_) => "NODE",
            Value::Relationship(_) => "RELATIONSHIP",
        }
    }
}

/// The order `ORDER BY` uses, as openCypher defines it: maps, nodes, relationships, lists,
/// strings, booleans, numbers, then null; numbers by value.
pub fn order(left: &Value, right: &Value) -> Ordering {
    let ranks = left.kind_rank().cmp(&right.kind_rank());
    if ranks != Ordering::Equal {
        return ranks;
    }
    match (left, right) {
        (Value::Integer(a), Value::Integer(b)) => a.cmp(b),
        (a, b) if a.as_f64().is_some() => a.as_f64().unwrap_or(0.0).total_cmp(&b.as_f64().unwrap_or(0.0)),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
        (Value::List(a), Value::List(b)) => {
            for (x, y) in a.iter().zip(b) {
                let ordering = order(x, y);
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            a.len().cmp(&b.len())
        }
        (Value::Node(a), Value::Node(b)) => a.id.cmp(&b.id),
        (Value::Relationship(a), Value::Relationship(b)) => a.id.cmp(&b.id),
        _ => Ordering::Equal,
    }
}

/// Equality as `=` defines it: `None` (null) when either side is null.
pub fn equals(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Integer(a), Value::Float(b)) | (Value::Float(b), Value::Integer(a)) => Some(*a as f64 == *b),
        (Value::List(a), Value::List(b)) => {
            if a.len() != b.len() {
                return Some(false);
            }
            let mut unknown = false;
            for (x, y) in a.iter().zip(b) {
                match equals(x, y) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    Some(true) => {}
                }
            }
            if unknown { None } else { Some(true) }
        }
        (Value::Node(a), Value::Node(b)) => Some(a.id == b.id),
        (Value::Relationship(a), Value::Relationship(b)) => Some(a.id == b.id),
        (a, b) => Some(a == b),
    }
}

/// Converts a property value to BSON for storage; nodes, relationships and maps cannot be
/// stored as properties.
pub(crate) fn to_bson(value: &Value) -> Option<eggdb_bson::Bson> {
    use eggdb_bson::Bson;
    Some(match value {
        Value::Null => Bson::Null,
        Value::Boolean(flag) => Bson::Boolean(*flag),
        Value::Integer(number) => Bson::Int64(*number),
        Value::Float(number) => Bson::Double(*number),
        Value::String(text) => Bson::String(text.clone()),
        Value::List(items) => Bson::Array(items.iter().map(to_bson).collect::<Option<_>>()?),
        _ => return None,
    })
}

pub(crate) fn from_bson(value: &eggdb_bson::Bson) -> Value {
    use eggdb_bson::Bson;
    match value {
        Bson::Boolean(flag) => Value::Boolean(*flag),
        Bson::Int32(number) => Value::Integer(i64::from(*number)),
        Bson::Int64(number) => Value::Integer(*number),
        Bson::Double(number) => Value::Float(*number),
        Bson::String(text) => Value::String(text.clone()),
        Bson::Array(items) => Value::List(items.iter().map(from_bson).collect()),
        _ => Value::Null,
    }
}
