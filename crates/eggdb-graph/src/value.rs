use crate::temporal::{Point, Temporal};
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

/// A path as a query returns it: its nodes in walking order and the relationships between
/// them, so `relationships[i]` joins `nodes[i]` and `nodes[i + 1]`, in either direction.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    /// The nodes, from the start of the path to its end; one more than the relationships.
    pub nodes: Vec<Node>,
    /// The relationships, in walking order.
    pub relationships: Vec<Relationship>,
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
    /// A path.
    Path(Box<Path>),
    /// A date, time, date and time, or duration.
    Temporal(Temporal),
    /// A 2D or 3D point.
    Point(Point),
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
            Value::Path(_) => 4,
            Value::Point(_) => 5,
            Value::Temporal(temporal) => 6 + temporal.rank(),
            Value::String(_) => 12,
            Value::Boolean(_) => 13,
            Value::Integer(_) | Value::Float(_) => 14,
            Value::Null => 16,
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
            Value::Path(_) => "PATH",
            Value::Temporal(temporal) => temporal.type_name(),
            Value::Point(_) => "POINT",
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
        (a, b) if a.as_f64().is_some() => a
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&b.as_f64().unwrap_or(0.0)),
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
        (Value::Path(a), Value::Path(b)) => path_ids(a).cmp(&path_ids(b)),
        (Value::Temporal(a), Value::Temporal(b)) => a.order(b),
        (Value::Point(a), Value::Point(b)) => a
            .srid
            .cmp(&b.srid)
            .then_with(|| a.x.total_cmp(&b.x))
            .then_with(|| a.y.total_cmp(&b.y))
            .then_with(|| a.z.unwrap_or(0.0).total_cmp(&b.z.unwrap_or(0.0))),
        _ => Ordering::Equal,
    }
}

/// Equality as `=` defines it: `None` (null) when either side is null.
pub fn equals(left: &Value, right: &Value) -> Option<bool> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Integer(a), Value::Float(b)) | (Value::Float(b), Value::Integer(a)) => {
            Some(*a as f64 == *b)
        }
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
        (Value::Path(a), Value::Path(b)) => Some(path_ids(a) == path_ids(b)),
        (a, b) => Some(a == b),
    }
}

fn path_ids(path: &Path) -> (Vec<u64>, Vec<u64>) {
    (
        path.nodes.iter().map(|node| node.id).collect(),
        path.relationships
            .iter()
            .map(|relationship| relationship.id)
            .collect(),
    )
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
        Value::Temporal(temporal) => {
            let (kind, fields): (&str, Vec<(&str, i64)>) = match *temporal {
                Temporal::Date { days } => ("date", vec![("days", days)]),
                Temporal::LocalTime { nanos } => ("localtime", vec![("nanos", nanos)]),
                Temporal::Time { nanos, offset } => ("time", vec![("nanos", nanos), ("offset", i64::from(offset))]),
                Temporal::LocalDateTime { seconds, nanos } => {
                    ("localdatetime", vec![("seconds", seconds), ("nanos", i64::from(nanos))])
                }
                Temporal::DateTime { seconds, nanos, offset } => (
                    "datetime",
                    vec![("seconds", seconds), ("nanos", i64::from(nanos)), ("offset", i64::from(offset))],
                ),
                Temporal::Duration { months, days, seconds, nanos } => (
                    "duration",
                    vec![("months", months), ("days", days), ("seconds", seconds), ("nanos", i64::from(nanos))],
                ),
            };
            let mut stored = eggdb_bson::Document::new();
            stored.insert(STORED_KIND, Bson::String(kind.to_owned()));
            for (name, value) in fields {
                stored.insert(name, Bson::Int64(value));
            }
            Bson::Document(stored)
        }
        Value::Point(point) => {
            let mut stored = eggdb_bson::Document::new();
            stored.insert(STORED_KIND, Bson::String("point".to_owned()));
            stored.insert("srid", Bson::Int32(point.srid));
            stored.insert("x", Bson::Double(point.x));
            stored.insert("y", Bson::Double(point.y));
            if let Some(z) = point.z {
                stored.insert("z", Bson::Double(z));
            }
            Bson::Document(stored)
        }
        _ => return None,
    })
}

const STORED_KIND: &str = "$cypher";

fn stored_value(stored: &eggdb_bson::Document) -> Option<Value> {
    let number = |name: &str| stored.get_i64(name);
    let small = |name: &str| number(name).and_then(|value| i32::try_from(value).ok());
    let float = |name: &str| match stored.get(name)? {
        eggdb_bson::Bson::Double(value) => Some(*value),
        _ => None,
    };
    Some(match stored.get_str(STORED_KIND)? {
        "date" => Value::Temporal(Temporal::Date { days: number("days")? }),
        "localtime" => Value::Temporal(Temporal::LocalTime { nanos: number("nanos")? }),
        "time" => Value::Temporal(Temporal::Time { nanos: number("nanos")?, offset: small("offset")? }),
        "localdatetime" => Value::Temporal(Temporal::LocalDateTime { seconds: number("seconds")?, nanos: small("nanos")? }),
        "datetime" => Value::Temporal(Temporal::DateTime {
            seconds: number("seconds")?,
            nanos: small("nanos")?,
            offset: small("offset")?,
        }),
        "duration" => Value::Temporal(Temporal::Duration {
            months: number("months")?,
            days: number("days")?,
            seconds: number("seconds")?,
            nanos: small("nanos")?,
        }),
        "point" => Value::Point(Point { srid: small("srid")?, x: float("x")?, y: float("y")?, z: float("z") }),
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
        Bson::Document(stored) => stored_value(stored).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}
