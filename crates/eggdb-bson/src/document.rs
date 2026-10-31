/// One BSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Bson {
    /// A 64-bit binary floating point number (`0x01`).
    Double(f64),
    /// A UTF-8 string (`0x02`).
    String(String),
    /// An embedded document (`0x03`).
    Document(Document),
    /// An array (`0x04`), its keys being the indexes.
    Array(Vec<Bson>),
    /// Binary data with its subtype (`0x05`).
    Binary {
        /// The subtype: `0` generic, `4` UUID, and so on.
        subtype: u8,
        /// The bytes.
        bytes: Vec<u8>,
    },
    /// The deprecated undefined value (`0x06`).
    Undefined,
    /// A 12-byte object id (`0x07`).
    ObjectId([u8; 12]),
    /// `true` or `false` (`0x08`).
    Boolean(bool),
    /// Milliseconds since the Unix epoch, UTC (`0x09`).
    DateTime(i64),
    /// `null` (`0x0A`).
    Null,
    /// A regular expression and its option letters (`0x0B`).
    Regex {
        /// The pattern.
        pattern: String,
        /// The options, in alphabetical order.
        options: String,
    },
    /// The deprecated database pointer (`0x0C`).
    DbPointer {
        /// The namespace.
        namespace: String,
        /// The object id.
        id: [u8; 12],
    },
    /// JavaScript code (`0x0D`).
    JavaScript(String),
    /// The deprecated symbol (`0x0E`).
    Symbol(String),
    /// JavaScript code with a scope document (`0x0F`).
    JavaScriptWithScope {
        /// The code.
        code: String,
        /// The scope.
        scope: Document,
    },
    /// A 32-bit integer (`0x10`).
    Int32(i32),
    /// A replication timestamp (`0x11`): seconds and an increment.
    Timestamp {
        /// Seconds since the Unix epoch.
        time: u32,
        /// The ordinal within the second.
        increment: u32,
    },
    /// A 64-bit integer (`0x12`).
    Int64(i64),
    /// An IEEE 754-2008 128-bit decimal, as its little-endian bytes (`0x13`).
    Decimal128([u8; 16]),
    /// The value below every other (`0xFF`).
    MinKey,
    /// The value above every other (`0x7F`).
    MaxKey,
}

impl Bson {
    /// The number as `f64` when the value is a number (decimals approximately).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Double(value) => Some(*value),
            Self::Int32(value) => Some(f64::from(*value)),
            Self::Int64(value) => Some(*value as f64),
            Self::Decimal128(bytes) => Some(decimal128_to_f64(bytes)),
            _ => None,
        }
    }

    /// The number as `i64` when the value is an integer, or a double with no fraction in range.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int32(value) => Some(i64::from(*value)),
            Self::Int64(value) => Some(*value),
            Self::Double(value) if value.fract() == 0.0 && value.abs() < 9.223_372_036_854_775e18 => Some(*value as i64),
            _ => None,
        }
    }

    /// Whether the value is a number.
    pub fn is_number(&self) -> bool {
        matches!(self, Self::Double(_) | Self::Int32(_) | Self::Int64(_) | Self::Decimal128(_))
    }

    /// The string when the value is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    /// The document when the value is one.
    pub fn as_document(&self) -> Option<&Document> {
        match self {
            Self::Document(document) => Some(document),
            _ => None,
        }
    }

    /// The value as a condition: `false`, `null`, undefined and numeric zero are false.
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Boolean(value) => *value,
            Self::Null | Self::Undefined => false,
            other => other.as_f64().is_none_or(|value| value != 0.0),
        }
    }

    /// The type's name as the `$type` operator and error messages spell it.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Double(_) => "double",
            Self::String(_) => "string",
            Self::Document(_) => "object",
            Self::Array(_) => "array",
            Self::Binary { .. } => "binData",
            Self::Undefined => "undefined",
            Self::ObjectId(_) => "objectId",
            Self::Boolean(_) => "bool",
            Self::DateTime(_) => "date",
            Self::Null => "null",
            Self::Regex { .. } => "regex",
            Self::DbPointer { .. } => "dbPointer",
            Self::JavaScript(_) => "javascript",
            Self::Symbol(_) => "symbol",
            Self::JavaScriptWithScope { .. } => "javascriptWithScope",
            Self::Int32(_) => "int",
            Self::Timestamp { .. } => "timestamp",
            Self::Int64(_) => "long",
            Self::Decimal128(_) => "decimal",
            Self::MinKey => "minKey",
            Self::MaxKey => "maxKey",
        }
    }

    /// The type's number in the BSON specification, as the `$type` operator takes it.
    pub fn type_number(&self) -> i32 {
        match self {
            Self::Double(_) => 1,
            Self::String(_) => 2,
            Self::Document(_) => 3,
            Self::Array(_) => 4,
            Self::Binary { .. } => 5,
            Self::Undefined => 6,
            Self::ObjectId(_) => 7,
            Self::Boolean(_) => 8,
            Self::DateTime(_) => 9,
            Self::Null => 10,
            Self::Regex { .. } => 11,
            Self::DbPointer { .. } => 12,
            Self::JavaScript(_) => 13,
            Self::Symbol(_) => 14,
            Self::JavaScriptWithScope { .. } => 15,
            Self::Int32(_) => 16,
            Self::Timestamp { .. } => 17,
            Self::Int64(_) => 18,
            Self::Decimal128(_) => 19,
            Self::MinKey => -1,
            Self::MaxKey => 127,
        }
    }
}

fn decimal128_to_f64(bytes: &[u8; 16]) -> f64 {
    let bits = u128::from_le_bytes(*bytes);
    let negative = bits >> 127 == 1;
    let combination = (bits >> 122) & 0x1f;
    if combination == 0x1f {
        return f64::NAN;
    }
    if combination == 0x1e {
        return if negative { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    let (exponent, coefficient) = if (bits >> 125) & 0b11 == 0b11 {
        (((bits >> 111) & 0x3fff) as i32, (bits & ((1u128 << 111) - 1)) | (0b100u128 << 111))
    } else {
        (((bits >> 113) & 0x3fff) as i32, bits & ((1u128 << 113) - 1))
    };
    let value = coefficient as f64 * 10f64.powi(exponent - 6176);
    if negative { -value } else { value }
}

/// A BSON document: named values in order. A name may appear twice when a document read from
/// the wire has it so; [`Document::insert`] keeps one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document {
    entries: Vec<(String, Bson)>,
}

impl Document {
    /// An empty document.
    pub fn new() -> Self {
        Self::default()
    }

    /// The document holding `entries` in their order.
    pub fn from_entries(entries: Vec<(String, Bson)>) -> Self {
        Self { entries }
    }

    /// The value named `key`, the first when it appears twice.
    pub fn get(&self, key: &str) -> Option<&Bson> {
        self.entries.iter().find(|(name, _)| name == key).map(|(_, value)| value)
    }

    /// The value named `key`, to change.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Bson> {
        self.entries.iter_mut().find(|(name, _)| name == key).map(|(_, value)| value)
    }

    /// Sets `key` to `value`: in place when it is there, else at the end.
    pub fn insert(&mut self, key: impl Into<String>, value: Bson) {
        let key = key.into();
        match self.get_mut(&key) {
            Some(existing) => *existing = value,
            None => self.entries.push((key, value)),
        }
    }

    /// Puts `key` first, replacing any value it had.
    pub fn insert_first(&mut self, key: impl Into<String>, value: Bson) {
        let key = key.into();
        self.entries.retain(|(name, _)| *name != key);
        self.entries.insert(0, (key, value));
    }

    /// Removes `key` and returns its value.
    pub fn remove(&mut self, key: &str) -> Option<Bson> {
        let at = self.entries.iter().position(|(name, _)| name == key)?;
        Some(self.entries.remove(at).1)
    }

    /// Whether `key` is there.
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// The entries in order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Bson)> {
        self.entries.iter().map(|(name, value)| (name, value))
    }

    /// The names in order.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.iter().map(|(name, _)| name)
    }

    /// The entries, to change in place.
    pub fn entries_mut(&mut self) -> &mut Vec<(String, Bson)> {
        &mut self.entries
    }

    /// The entries, taken.
    pub fn into_entries(self) -> Vec<(String, Bson)> {
        self.entries
    }

    /// The first entry.
    pub fn first(&self) -> Option<(&String, &Bson)> {
        self.entries.first().map(|(name, value)| (name, value))
    }

    /// The number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the document has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The string named `key`.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Bson::as_str)
    }

    /// The document named `key`.
    pub fn get_document(&self, key: &str) -> Option<&Document> {
        self.get(key).and_then(Bson::as_document)
    }

    /// The array named `key`.
    pub fn get_array(&self, key: &str) -> Option<&Vec<Bson>> {
        match self.get(key) {
            Some(Bson::Array(items)) => Some(items),
            _ => None,
        }
    }

    /// The boolean named `key`, numbers counting as their truth.
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key).map(Bson::is_truthy)
    }

    /// The integer named `key`.
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(Bson::as_i64)
    }
}

impl FromIterator<(String, Bson)> for Document {
    fn from_iter<I: IntoIterator<Item = (String, Bson)>>(iter: I) -> Self {
        Self { entries: iter.into_iter().collect() }
    }
}
