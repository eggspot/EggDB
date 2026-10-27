use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::{SqlType, Value};

pub(crate) const NAMESPACE: &[u8] = &[0xFF, b'S', b'Q', b'L', 0x00];
const META_SPACE: u8 = 0x01;
const TABLE_SPACE: u8 = 0x02;
const ROW_SPACE: u8 = 0x03;
const FORMAT_VERSION: u8 = 1;
const PRESENT: u8 = 0x01;
const NULL_IN_KEY: u8 = 0x02;

pub(crate) fn meta_key() -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(META_SPACE);
    key
}

pub(crate) fn table_key(name: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(TABLE_SPACE);
    key.extend_from_slice(name.as_bytes());
    key
}

pub(crate) fn rows_prefix(table_id: u32) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(ROW_SPACE);
    key.extend_from_slice(&table_id.to_be_bytes());
    key
}

pub(crate) fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last != 0xFF {
            end.push(last + 1);
            return end;
        }
    }
    vec![0xFF; prefix.len() + 1]
}

pub(crate) fn corrupt(what: &str) -> SqlError {
    SqlError::new(
        SqlState::DATA_CORRUPTED,
        format!("the SQL {what} stored in the database is damaged"),
    )
    .with_hint("Run `eggdb check` on the file.")
}

pub(crate) fn encode_meta(next_table_id: u32) -> Vec<u8> {
    let mut out = vec![FORMAT_VERSION];
    out.extend_from_slice(&next_table_id.to_le_bytes());
    out
}

pub(crate) fn decode_meta(bytes: &[u8]) -> SqlResult<u32> {
    let mut reader = Reader::new(bytes, "catalog");
    reader.version()?;
    let next = reader.u32()?;
    reader.finish()?;
    Ok(next)
}

fn type_tag(sql_type: SqlType) -> u8 {
    match sql_type {
        SqlType::SmallInt => 1,
        SqlType::Integer => 2,
        SqlType::BigInt => 3,
        SqlType::Real => 4,
        SqlType::Double => 5,
        SqlType::Boolean => 6,
        SqlType::Text => 7,
        SqlType::Varchar(_) => 8,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) sql_type: SqlType,
    pub(crate) not_null: bool,
    pub(crate) default: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableDefinition {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) columns: Vec<Column>,
    pub(crate) primary_key: Vec<usize>,
}

impl TableDefinition {
    pub(crate) fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = vec![FORMAT_VERSION];
        out.extend_from_slice(&self.id.to_le_bytes());
        put_varint(&mut out, self.columns.len() as u64);
        for column in &self.columns {
            put_bytes(&mut out, column.name.as_bytes());
            out.push(type_tag(column.sql_type));
            if let SqlType::Varchar(length) = column.sql_type {
                out.extend_from_slice(&length.unwrap_or(0).to_le_bytes());
            }
            out.push(u8::from(column.not_null));
            match &column.default {
                Some(text) => {
                    out.push(1);
                    put_bytes(&mut out, text.as_bytes());
                }
                None => out.push(0),
            }
        }
        put_varint(&mut out, self.primary_key.len() as u64);
        for &index in &self.primary_key {
            put_varint(&mut out, index as u64);
        }
        out
    }

    pub(crate) fn decode(name: &str, bytes: &[u8]) -> SqlResult<Self> {
        let mut reader = Reader::new(bytes, "table definition");
        reader.version()?;
        let id = reader.u32()?;
        let count = reader.count()?;
        let mut columns = Vec::new();
        for _ in 0..count {
            let name = reader.text()?;
            let sql_type = match reader.byte()? {
                1 => SqlType::SmallInt,
                2 => SqlType::Integer,
                3 => SqlType::BigInt,
                4 => SqlType::Real,
                5 => SqlType::Double,
                6 => SqlType::Boolean,
                7 => SqlType::Text,
                8 => match reader.u32()? {
                    0 => SqlType::Varchar(None),
                    length => SqlType::Varchar(Some(length)),
                },
                _ => return Err(reader.damaged()),
            };
            let not_null = match reader.byte()? {
                0 => false,
                1 => true,
                _ => return Err(reader.damaged()),
            };
            let default = match reader.byte()? {
                0 => None,
                1 => Some(reader.text()?),
                _ => return Err(reader.damaged()),
            };
            columns.push(Column {
                name,
                sql_type,
                not_null,
                default,
            });
        }
        let key_count = reader.count()?;
        let mut primary_key = Vec::new();
        for _ in 0..key_count {
            let index = usize::try_from(reader.varint()?).map_err(|_| reader.damaged())?;
            if index >= columns.len() {
                return Err(reader.damaged());
            }
            primary_key.push(index);
        }
        reader.finish()?;
        if columns.is_empty() && !primary_key.is_empty() {
            return Err(reader.damaged());
        }
        Ok(Self {
            id,
            name: name.to_owned(),
            columns,
            primary_key,
        })
    }
}

pub(crate) fn encode_key_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(NULL_IN_KEY),
        Value::Boolean(flag) => {
            out.push(PRESENT);
            out.push(u8::from(*flag));
        }
        Value::SmallInt(number) => {
            out.push(PRESENT);
            out.extend_from_slice(&((*number as u16) ^ 0x8000).to_be_bytes());
        }
        Value::Integer(number) => {
            out.push(PRESENT);
            out.extend_from_slice(&((*number as u32) ^ 0x8000_0000).to_be_bytes());
        }
        Value::BigInt(number) => {
            out.push(PRESENT);
            out.extend_from_slice(&((*number as u64) ^ 0x8000_0000_0000_0000).to_be_bytes());
        }
        Value::Real(number) => {
            out.push(PRESENT);
            let canonical = if number.is_nan() {
                f32::NAN
            } else if *number == 0.0 {
                0.0
            } else {
                *number
            };
            let bits = canonical.to_bits();
            let ordered = if bits >> 31 == 1 {
                !bits
            } else {
                bits | 1 << 31
            };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::Double(number) => {
            out.push(PRESENT);
            let canonical = if number.is_nan() {
                f64::NAN
            } else if *number == 0.0 {
                0.0
            } else {
                *number
            };
            let bits = canonical.to_bits();
            let ordered = if bits >> 63 == 1 {
                !bits
            } else {
                bits | 1 << 63
            };
            out.extend_from_slice(&ordered.to_be_bytes());
        }
        Value::Text(text) => {
            out.push(PRESENT);
            for &byte in text.as_bytes() {
                out.push(byte);
                if byte == 0 {
                    out.push(0xFF);
                }
            }
            out.extend_from_slice(&[0x00, 0x01]);
        }
    }
}

pub(crate) fn encode_row(values: &[Value]) -> Vec<u8> {
    let mut out = vec![FORMAT_VERSION];
    put_varint(&mut out, values.len() as u64);
    for value in values {
        match value {
            Value::Null => out.push(0),
            Value::SmallInt(number) => {
                out.push(1);
                out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Integer(number) => {
                out.push(2);
                out.extend_from_slice(&number.to_le_bytes());
            }
            Value::BigInt(number) => {
                out.push(3);
                out.extend_from_slice(&number.to_le_bytes());
            }
            Value::Real(number) => {
                out.push(4);
                out.extend_from_slice(&number.to_bits().to_le_bytes());
            }
            Value::Double(number) => {
                out.push(5);
                out.extend_from_slice(&number.to_bits().to_le_bytes());
            }
            Value::Boolean(flag) => {
                out.push(6);
                out.push(u8::from(*flag));
            }
            Value::Text(text) => {
                out.push(7);
                put_bytes(&mut out, text.as_bytes());
            }
        }
    }
    out
}

pub(crate) fn decode_row(bytes: &[u8], column_count: usize) -> SqlResult<Vec<Value>> {
    let mut reader = Reader::new(bytes, "row");
    reader.version()?;
    let count = reader.count()?;
    if count > column_count {
        return Err(reader.damaged());
    }
    let mut values = Vec::with_capacity(column_count);
    for _ in 0..count {
        values.push(match reader.byte()? {
            0 => Value::Null,
            1 => Value::SmallInt(i16::from_le_bytes(reader.array()?)),
            2 => Value::Integer(i32::from_le_bytes(reader.array()?)),
            3 => Value::BigInt(i64::from_le_bytes(reader.array()?)),
            4 => Value::Real(f32::from_bits(u32::from_le_bytes(reader.array()?))),
            5 => Value::Double(f64::from_bits(u64::from_le_bytes(reader.array()?))),
            6 => match reader.byte()? {
                0 => Value::Boolean(false),
                1 => Value::Boolean(true),
                _ => return Err(reader.damaged()),
            },
            7 => Value::Text(reader.text()?),
            _ => return Err(reader.damaged()),
        });
    }
    reader.finish()?;
    values.resize(column_count, Value::Null);
    Ok(values)
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
    what: &'static str,
}

impl<'b> Reader<'b> {
    fn new(bytes: &'b [u8], what: &'static str) -> Self {
        Self { bytes, at: 0, what }
    }

    fn damaged(&self) -> SqlError {
        corrupt(self.what)
    }

    fn version(&mut self) -> SqlResult<()> {
        match self.byte()? {
            FORMAT_VERSION => Ok(()),
            version => Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "the SQL {} was written in format version {version}, which this EggDB cannot read",
                    self.what
                ),
            )
            .with_hint("Open the file with the EggDB release that wrote it, or a newer one.")),
        }
    }

    fn byte(&mut self) -> SqlResult<u8> {
        let byte = *self.bytes.get(self.at).ok_or_else(|| self.damaged())?;
        self.at += 1;
        Ok(byte)
    }

    fn take(&mut self, length: usize) -> SqlResult<&'b [u8]> {
        let end = self.at.checked_add(length).ok_or_else(|| self.damaged())?;
        let slice = self.bytes.get(self.at..end).ok_or_else(|| self.damaged())?;
        self.at = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> SqlResult<[u8; N]> {
        let slice = self.take(N)?;
        let mut array = [0; N];
        array.copy_from_slice(slice);
        Ok(array)
    }

    fn u32(&mut self) -> SqlResult<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn varint(&mut self) -> SqlResult<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7F) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(self.damaged())
    }

    fn count(&mut self) -> SqlResult<usize> {
        let count = self.varint()?;
        if count > self.bytes.len() as u64 {
            return Err(self.damaged());
        }
        Ok(count as usize)
    }

    fn text(&mut self) -> SqlResult<String> {
        let length = self.count()?;
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| self.damaged())
    }

    fn finish(&self) -> SqlResult<()> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err(self.damaged())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TableDefinition, decode_row, encode_key_value, encode_row, prefix_end};
    use crate::encoding::Column;
    use crate::types::{SqlType, Value, compare_values};

    struct Seeded(u64);

    impl Seeded {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn key_of(value: &Value) -> Vec<u8> {
        let mut key = Vec::new();
        encode_key_value(&mut key, value);
        key
    }

    fn random_value(random: &mut Seeded, kind: u64) -> Value {
        let bits = random.next();
        match kind {
            0 => Value::SmallInt(bits as i16),
            1 => Value::Integer(bits as i32),
            2 => Value::BigInt(bits as i64),
            3 => Value::Real(match bits % 8 {
                0 => f32::INFINITY,
                1 => f32::NEG_INFINITY,
                2 => -0.0,
                3 => 0.0,
                _ => f32::from_bits(bits as u32),
            }),
            4 => Value::Double(match bits % 8 {
                0 => f64::INFINITY,
                1 => f64::NAN,
                2 => -0.0,
                3 => 0.0,
                _ => f64::from_bits(bits),
            }),
            5 => Value::Boolean(bits & 1 == 1),
            _ => {
                let length = (bits % 6) as usize;
                let text: String = (0..length)
                    .map(|_| match random.next() % 5 {
                        0 => '\0',
                        1 => '\u{ff}',
                        2 => 'a',
                        3 => 'b',
                        _ => '\u{1F95A}',
                    })
                    .collect();
                Value::Text(text)
            }
        }
    }

    #[test]
    fn key_encoding_preserves_the_order_of_every_type() {
        let mut random = Seeded(0x9E37_79B9_7F4A_7C15);
        for kind in 0..7 {
            for _ in 0..2_000 {
                let left = random_value(&mut random, kind);
                let right = random_value(&mut random, kind);
                assert_eq!(
                    key_of(&left).cmp(&key_of(&right)),
                    compare_values(&left, &right),
                    "{left:?} vs {right:?}"
                );
            }
        }
    }

    #[test]
    fn composite_keys_order_by_their_first_column_first() {
        let mut first = Vec::new();
        encode_key_value(&mut first, &Value::Text("a".into()));
        encode_key_value(&mut first, &Value::Integer(9));
        let mut second = Vec::new();
        encode_key_value(&mut second, &Value::Text("a\0".into()));
        encode_key_value(&mut second, &Value::Integer(1));
        assert!(first < second);
    }

    #[test]
    fn rows_round_trip_and_short_rows_read_as_null() {
        let values = vec![
            Value::Null,
            Value::SmallInt(-3),
            Value::Integer(7),
            Value::BigInt(i64::MIN),
            Value::Real(1.5),
            Value::Double(f64::NEG_INFINITY),
            Value::Boolean(true),
            Value::Text("héllo".into()),
        ];
        let encoded = encode_row(&values);
        assert_eq!(decode_row(&encoded, 8), Ok(values.clone()));
        let mut longer = values;
        longer.push(Value::Null);
        assert_eq!(decode_row(&encoded, 9), Ok(longer));
        assert!(decode_row(&encoded, 7).is_err());
        for cut in 0..encoded.len() {
            assert!(decode_row(&encoded[..cut], 8).is_err());
        }
    }

    #[test]
    fn table_definitions_round_trip_and_damage_is_an_error() {
        let table = TableDefinition {
            id: 7,
            name: "t".into(),
            columns: vec![
                Column {
                    name: "id".into(),
                    sql_type: SqlType::BigInt,
                    not_null: true,
                    default: None,
                },
                Column {
                    name: "name".into(),
                    sql_type: SqlType::Varchar(Some(20)),
                    not_null: false,
                    default: Some("'x'".into()),
                },
            ],
            primary_key: vec![0],
        };
        let encoded = table.encode();
        assert_eq!(TableDefinition::decode("t", &encoded), Ok(table));
        for cut in 0..encoded.len() {
            assert!(TableDefinition::decode("t", &encoded[..cut]).is_err());
        }
    }

    #[test]
    fn a_prefix_end_is_the_first_key_after_every_key_with_the_prefix() {
        assert_eq!(prefix_end(&[1, 2]), vec![1, 3]);
        assert_eq!(prefix_end(&[1, 0xFF]), vec![2]);
    }
}
