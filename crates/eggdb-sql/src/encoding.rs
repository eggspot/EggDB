use crate::datetime;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::numeric::Numeric;
use crate::types::{NumericModifier, SqlType, Value};

pub(crate) const NAMESPACE: &[u8] = &[0xFF, b'S', b'Q', b'L', 0x00];
const META_SPACE: u8 = 0x01;
const TABLE_SPACE: u8 = 0x02;
const ROW_SPACE: u8 = 0x03;
const INDEX_SPACE: u8 = 0x04;
const FORMAT_VERSION: u8 = 1;
const AUTO_INCREMENT_FORMAT_VERSION: u8 = 2;
const INDEXES_FORMAT_VERSION: u8 = 3;
const TYPES_FORMAT_VERSION: u8 = 4;
const NO_PRECISION: u8 = 0xFF;
const DEFAULT_ABSENT: u8 = 0;
const DEFAULT_EXPRESSION: u8 = 1;
const DEFAULT_AUTO_INCREMENT: u8 = 2;
pub(crate) const PRESENT: u8 = 0x01;
pub(crate) const NULL_IN_KEY: u8 = 0x02;
pub(crate) const MAX_COLUMNS: usize = 1600;
pub(crate) const MAX_INDEX_COLUMNS: usize = 32;
pub(crate) const MAX_INDEXES: usize = 1024;

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

const TYPICAL_KEY_BYTES: usize = 48;

pub(crate) fn rows_prefix(table_id: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(TYPICAL_KEY_BYTES);
    key.extend_from_slice(NAMESPACE);
    key.push(ROW_SPACE);
    key.extend_from_slice(&table_id.to_be_bytes());
    key
}

pub(crate) fn table_indexes_prefix(table_id: u32) -> Vec<u8> {
    let mut key = Vec::with_capacity(TYPICAL_KEY_BYTES);
    key.extend_from_slice(NAMESPACE);
    key.push(INDEX_SPACE);
    key.extend_from_slice(&table_id.to_be_bytes());
    key
}

pub(crate) fn index_prefix(table_id: u32, index_id: u32) -> Vec<u8> {
    let mut key = table_indexes_prefix(table_id);
    key.extend_from_slice(&index_id.to_be_bytes());
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
    .with_hint(
        "`eggdb check` checks the file's pages and keys, not SQL rows and definitions, so it can pass: restore the table from a backup or an export.",
    )
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
        SqlType::Numeric(_) => 9,
        SqlType::Date => 10,
        SqlType::Timestamp(_) => 11,
        SqlType::TimestampTz(_) => 12,
        SqlType::Json => 13,
        SqlType::Jsonb => 14,
    }
}

fn is_version_4_type(sql_type: SqlType) -> bool {
    type_tag(sql_type) >= type_tag(SqlType::Numeric(None))
}

fn put_type(out: &mut Vec<u8>, sql_type: SqlType) {
    out.push(type_tag(sql_type));
    match sql_type {
        SqlType::Varchar(length) => out.extend_from_slice(&length.unwrap_or(0).to_le_bytes()),
        SqlType::Numeric(modifier) => {
            let (precision, scale) =
                modifier.map_or((0, 0), |modifier| (modifier.precision, modifier.scale));
            out.extend_from_slice(&precision.to_le_bytes());
            out.extend_from_slice(&scale.to_le_bytes());
        }
        SqlType::Timestamp(precision) | SqlType::TimestampTz(precision) => {
            out.push(precision.unwrap_or(NO_PRECISION));
        }
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) sql_type: SqlType,
    pub(crate) not_null: bool,
    pub(crate) default: Option<String>,
    pub(crate) auto_increment: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexDefinition {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) unique: bool,
    pub(crate) columns: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TableDefinition {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) columns: Vec<Column>,
    pub(crate) primary_key: Vec<usize>,
    pub(crate) indexes: Vec<IndexDefinition>,
}

impl TableDefinition {
    pub(crate) fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    pub(crate) fn auto_increment_column(&self) -> Option<usize> {
        self.columns.iter().position(|column| column.auto_increment)
    }

    pub(crate) fn index_named(&self, name: &str) -> Option<&IndexDefinition> {
        self.indexes.iter().find(|index| index.name == name)
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let version = if self
            .columns
            .iter()
            .any(|column| is_version_4_type(column.sql_type))
        {
            TYPES_FORMAT_VERSION
        } else if !self.indexes.is_empty() {
            INDEXES_FORMAT_VERSION
        } else if self.auto_increment_column().is_some() {
            AUTO_INCREMENT_FORMAT_VERSION
        } else {
            FORMAT_VERSION
        };
        let mut out = vec![version];
        out.extend_from_slice(&self.id.to_le_bytes());
        put_varint(&mut out, self.columns.len() as u64);
        for column in &self.columns {
            put_bytes(&mut out, column.name.as_bytes());
            put_type(&mut out, column.sql_type);
            out.push(u8::from(column.not_null));
            match &column.default {
                _ if column.auto_increment => out.push(DEFAULT_AUTO_INCREMENT),
                Some(text) => {
                    out.push(DEFAULT_EXPRESSION);
                    put_bytes(&mut out, text.as_bytes());
                }
                None => out.push(DEFAULT_ABSENT),
            }
        }
        put_varint(&mut out, self.primary_key.len() as u64);
        for &index in &self.primary_key {
            put_varint(&mut out, index as u64);
        }
        if version >= INDEXES_FORMAT_VERSION {
            put_varint(&mut out, self.indexes.len() as u64);
            for index in &self.indexes {
                out.extend_from_slice(&index.id.to_le_bytes());
                put_bytes(&mut out, index.name.as_bytes());
                out.push(u8::from(index.unique));
                put_varint(&mut out, index.columns.len() as u64);
                for &column in &index.columns {
                    put_varint(&mut out, column as u64);
                }
            }
        }
        out
    }

    pub(crate) fn decode(name: &str, bytes: &[u8]) -> SqlResult<Self> {
        let mut reader = Reader::new(bytes, "table definition");
        let version = reader.version_up_to(TYPES_FORMAT_VERSION)?;
        let auto_allowed = version >= AUTO_INCREMENT_FORMAT_VERSION;
        let new_types_allowed = version >= TYPES_FORMAT_VERSION;
        let id = reader.u32()?;
        let count = reader.count()?;
        if count > MAX_COLUMNS {
            return Err(reader.damaged());
        }
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
                9 if new_types_allowed => reader.numeric_type()?,
                10 if new_types_allowed => SqlType::Date,
                11 if new_types_allowed => SqlType::Timestamp(reader.precision()?),
                12 if new_types_allowed => SqlType::TimestampTz(reader.precision()?),
                13 if new_types_allowed => SqlType::Json,
                14 if new_types_allowed => SqlType::Jsonb,
                _ => return Err(reader.damaged()),
            };
            let not_null = match reader.byte()? {
                0 => false,
                1 => true,
                _ => return Err(reader.damaged()),
            };
            let (default, auto_increment) = match reader.byte()? {
                DEFAULT_ABSENT => (None, false),
                DEFAULT_EXPRESSION => (Some(reader.text()?), false),
                DEFAULT_AUTO_INCREMENT if auto_allowed => (None, true),
                _ => return Err(reader.damaged()),
            };
            if columns.iter().any(|column: &Column| column.name == name) {
                return Err(reader.damaged());
            }
            columns.push(Column {
                name,
                sql_type,
                not_null,
                default,
                auto_increment,
            });
        }
        let key_count = reader.count()?;
        let mut primary_key = Vec::new();
        for _ in 0..key_count {
            let index = usize::try_from(reader.varint()?).map_err(|_| reader.damaged())?;
            if index >= columns.len() || primary_key.contains(&index) {
                return Err(reader.damaged());
            }
            primary_key.push(index);
        }
        let indexes = match version {
            INDEXES_FORMAT_VERSION => decode_indexes(&mut reader, columns.len(), false)?,
            TYPES_FORMAT_VERSION => decode_indexes(&mut reader, columns.len(), true)?,
            _ => Vec::new(),
        };
        reader.finish()?;
        if new_types_allowed
            && !columns
                .iter()
                .any(|column| is_version_4_type(column.sql_type))
        {
            return Err(reader.damaged());
        }
        if columns.is_empty() && !primary_key.is_empty() {
            return Err(reader.damaged());
        }
        let auto_columns = columns
            .iter()
            .filter(|column| column.auto_increment)
            .count();
        let auto_first_key = primary_key
            .first()
            .is_some_and(|&first| columns[first].auto_increment);
        if version == AUTO_INCREMENT_FORMAT_VERSION && (auto_columns != 1 || !auto_first_key) {
            return Err(reader.damaged());
        }
        if (version >= INDEXES_FORMAT_VERSION && auto_columns > 1)
            || (auto_columns == 1 && !auto_first_key)
        {
            return Err(reader.damaged());
        }
        Ok(Self {
            id,
            name: name.to_owned(),
            columns,
            primary_key,
            indexes,
        })
    }
}

fn decode_indexes(
    reader: &mut Reader<'_>,
    column_count: usize,
    none_allowed: bool,
) -> SqlResult<Vec<IndexDefinition>> {
    let count = reader.count()?;
    if (count == 0 && !none_allowed) || count > MAX_INDEXES {
        return Err(reader.damaged());
    }
    let mut indexes: Vec<IndexDefinition> = Vec::new();
    for _ in 0..count {
        let id = reader.u32()?;
        let name = reader.text()?;
        let unique = match reader.byte()? {
            0 => false,
            1 => true,
            _ => return Err(reader.damaged()),
        };
        let width = reader.count()?;
        if width == 0 || width > MAX_INDEX_COLUMNS {
            return Err(reader.damaged());
        }
        let mut columns = Vec::with_capacity(width);
        for _ in 0..width {
            let column = usize::try_from(reader.varint()?).map_err(|_| reader.damaged())?;
            if column >= column_count || columns.contains(&column) {
                return Err(reader.damaged());
            }
            columns.push(column);
        }
        if name.is_empty()
            || indexes
                .iter()
                .any(|index| index.id == id || index.name == name)
        {
            return Err(reader.damaged());
        }
        indexes.push(IndexDefinition {
            id,
            name,
            unique,
            columns,
        });
    }
    Ok(indexes)
}

pub(crate) fn skip_key_values(
    key: &[u8],
    types: impl IntoIterator<Item = SqlType>,
) -> SqlResult<usize> {
    let mut reader = Reader::new(key, "index entry");
    for sql_type in types {
        reader.skip_key_value(sql_type)?;
    }
    Ok(reader.at)
}

pub(crate) fn decode_key_values(
    key: &[u8],
    types: impl IntoIterator<Item = SqlType>,
    out: &mut Vec<Value>,
) -> SqlResult<usize> {
    let mut reader = Reader::new(key, "index entry");
    for sql_type in types {
        out.push(reader.key_value(sql_type)?);
    }
    Ok(reader.at)
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
        Value::Text(text) | Value::Json(text) | Value::Jsonb(text) => {
            out.push(PRESENT);
            for &byte in text.as_bytes() {
                out.push(byte);
                if byte == 0 {
                    out.push(0xFF);
                }
            }
            out.extend_from_slice(&[0x00, 0x01]);
        }
        Value::Numeric(number) => {
            out.push(PRESENT);
            number.encode_key(out);
        }
        Value::Date(days) => {
            out.push(PRESENT);
            out.extend_from_slice(&((*days as u32) ^ 0x8000_0000).to_be_bytes());
        }
        Value::Timestamp(micros) | Value::TimestampTz(micros) => {
            out.push(PRESENT);
            out.extend_from_slice(&((*micros as u64) ^ 0x8000_0000_0000_0000).to_be_bytes());
        }
    }
}

pub(crate) fn encode_row(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_row_into(&mut out, values);
    out
}

pub(crate) fn encode_row_into(out: &mut Vec<u8>, values: &[Value]) {
    out.push(FORMAT_VERSION);
    put_varint(out, values.len() as u64);
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
                put_bytes(out, text.as_bytes());
            }
            Value::Numeric(number) => {
                out.push(9);
                number.encode_row(out);
            }
            Value::Date(days) => {
                out.push(10);
                out.extend_from_slice(&days.to_le_bytes());
            }
            Value::Timestamp(micros) => {
                out.push(11);
                out.extend_from_slice(&micros.to_le_bytes());
            }
            Value::TimestampTz(micros) => {
                out.push(12);
                out.extend_from_slice(&micros.to_le_bytes());
            }
            Value::Json(text) => {
                out.push(13);
                put_bytes(out, text.as_bytes());
            }
            Value::Jsonb(text) => {
                out.push(14);
                put_bytes(out, text.as_bytes());
            }
        }
    }
}

pub(crate) fn decode_row(bytes: &[u8], columns: &[Column]) -> SqlResult<Vec<Value>> {
    let mut values = Vec::with_capacity(columns.len());
    decode_row_into(bytes, columns, None, &mut values)?;
    Ok(values)
}

pub(crate) fn decode_row_into(
    bytes: &[u8],
    columns: &[Column],
    needed: Option<&[bool]>,
    out: &mut Vec<Value>,
) -> SqlResult<()> {
    let mut reader = Reader::new(bytes, "row");
    reader.version()?;
    let count = reader.count()?;
    if count > columns.len() {
        return Err(reader.damaged());
    }
    for (index, column) in columns[..count].iter().enumerate() {
        let tag = reader.byte()?;
        if tag != 0 && tag != value_tag(column.sql_type) {
            return Err(reader.damaged());
        }
        let wanted = needed.is_none_or(|needed| needed.get(index).copied().unwrap_or(true));
        out.push(match tag {
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
            7 if wanted => Value::Text(reader.text()?),
            13 if wanted => Value::Json(reader.text()?),
            14 if wanted => Value::Jsonb(reader.text()?),
            7 | 13 | 14 => {
                reader.skip_text()?;
                Value::Null
            }
            9 => Value::Numeric(reader.numeric()?),
            10 => {
                let days = i32::from_le_bytes(reader.array()?);
                if !datetime::valid_date(days) {
                    return Err(reader.damaged());
                }
                Value::Date(days)
            }
            11 | 12 => {
                let micros = i64::from_le_bytes(reader.array()?);
                if !datetime::valid_timestamp(micros) {
                    return Err(reader.damaged());
                }
                if tag == 11 {
                    Value::Timestamp(micros)
                } else {
                    Value::TimestampTz(micros)
                }
            }
            _ => return Err(reader.damaged()),
        });
    }
    reader.finish()?;
    for _ in count..columns.len() {
        out.push(Value::Null);
    }
    Ok(())
}

fn value_tag(sql_type: SqlType) -> u8 {
    match sql_type {
        SqlType::Varchar(_) => type_tag(SqlType::Text),
        other => type_tag(other),
    }
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
        self.version_up_to(FORMAT_VERSION).map(|_| ())
    }

    fn version_up_to(&mut self, newest: u8) -> SqlResult<u8> {
        match self.byte()? {
            version @ FORMAT_VERSION..=u8::MAX if version <= newest => Ok(version),
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

    fn numeric_type(&mut self) -> SqlResult<SqlType> {
        let precision = u16::from_le_bytes(self.array()?);
        let scale = i16::from_le_bytes(self.array()?);
        if precision == 0 {
            return if scale == 0 {
                Ok(SqlType::Numeric(None))
            } else {
                Err(self.damaged())
            };
        }
        crate::numeric::check_modifier(i64::from(precision), i64::from(scale))
            .map_err(|_| self.damaged())?;
        Ok(SqlType::Numeric(Some(NumericModifier { precision, scale })))
    }

    fn precision(&mut self) -> SqlResult<Option<u8>> {
        match self.byte()? {
            NO_PRECISION => Ok(None),
            precision @ 0..=6 => Ok(Some(precision)),
            _ => Err(self.damaged()),
        }
    }

    fn numeric(&mut self) -> SqlResult<Numeric> {
        let rest = self.bytes.get(self.at..).unwrap_or(&[]);
        let (value, used) = Numeric::decode_row(rest).ok_or_else(|| self.damaged())?;
        self.take(used)?;
        Ok(value)
    }

    fn numeric_key(&mut self) -> SqlResult<Numeric> {
        let rest = self.bytes.get(self.at..).unwrap_or(&[]);
        let (value, used) = Numeric::decode_key(rest).ok_or_else(|| self.damaged())?;
        self.take(used)?;
        Ok(value)
    }

    fn skip_text(&mut self) -> SqlResult<()> {
        let length = self.count()?;
        self.take(length)?;
        Ok(())
    }

    fn skip_key_value(&mut self, sql_type: SqlType) -> SqlResult<()> {
        match self.byte()? {
            NULL_IN_KEY => return Ok(()),
            PRESENT => {}
            _ => return Err(self.damaged()),
        }
        let width = match sql_type {
            SqlType::Boolean => 1,
            SqlType::SmallInt => 2,
            SqlType::Integer | SqlType::Real | SqlType::Date => 4,
            SqlType::BigInt | SqlType::Double | SqlType::Timestamp(_) | SqlType::TimestampTz(_) => {
                8
            }
            SqlType::Numeric(_) => {
                let rest = self.bytes.get(self.at..).unwrap_or(&[]);
                Numeric::key_length(rest).ok_or_else(|| self.damaged())?
            }
            SqlType::Text | SqlType::Varchar(_) | SqlType::Json | SqlType::Jsonb => {
                let rest = self.bytes.get(self.at..).unwrap_or(&[]);
                let mut at = 0;
                loop {
                    match (rest.get(at), rest.get(at + 1)) {
                        (Some(0), Some(0x01)) => break,
                        (Some(0), Some(0xFF)) => at += 2,
                        (Some(0), _) | (None, _) => return Err(self.damaged()),
                        (Some(_), _) => at += 1,
                    }
                }
                at + 2
            }
        };
        self.take(width).map(|_| ())
    }

    fn key_value(&mut self, sql_type: SqlType) -> SqlResult<Value> {
        match self.byte()? {
            NULL_IN_KEY => return Ok(Value::Null),
            PRESENT => {}
            _ => return Err(self.damaged()),
        }
        Ok(match sql_type {
            SqlType::Boolean => match self.byte()? {
                0 => Value::Boolean(false),
                1 => Value::Boolean(true),
                _ => return Err(self.damaged()),
            },
            SqlType::SmallInt => {
                Value::SmallInt((u16::from_be_bytes(self.array()?) ^ 0x8000) as i16)
            }
            SqlType::Integer => {
                Value::Integer((u32::from_be_bytes(self.array()?) ^ 0x8000_0000) as i32)
            }
            SqlType::BigInt => {
                Value::BigInt((u64::from_be_bytes(self.array()?) ^ 0x8000_0000_0000_0000) as i64)
            }
            SqlType::Real => {
                let ordered = u32::from_be_bytes(self.array()?);
                let bits = if ordered >> 31 == 1 {
                    ordered & !(1 << 31)
                } else {
                    !ordered
                };
                Value::Real(f32::from_bits(bits))
            }
            SqlType::Double => {
                let ordered = u64::from_be_bytes(self.array()?);
                let bits = if ordered >> 63 == 1 {
                    ordered & !(1 << 63)
                } else {
                    !ordered
                };
                Value::Double(f64::from_bits(bits))
            }
            SqlType::Numeric(_) => Value::Numeric(self.numeric_key()?),
            SqlType::Date => {
                let days = (u32::from_be_bytes(self.array()?) ^ 0x8000_0000) as i32;
                if !datetime::valid_date(days) {
                    return Err(self.damaged());
                }
                Value::Date(days)
            }
            SqlType::Timestamp(_) | SqlType::TimestampTz(_) => {
                let micros = (u64::from_be_bytes(self.array()?) ^ 0x8000_0000_0000_0000) as i64;
                if !datetime::valid_timestamp(micros) {
                    return Err(self.damaged());
                }
                if matches!(sql_type, SqlType::Timestamp(_)) {
                    Value::Timestamp(micros)
                } else {
                    Value::TimestampTz(micros)
                }
            }
            SqlType::Text | SqlType::Varchar(_) | SqlType::Json | SqlType::Jsonb => {
                let mut bytes = Vec::new();
                loop {
                    match self.byte()? {
                        0 => match self.byte()? {
                            0x01 => break,
                            0xFF => bytes.push(0),
                            _ => return Err(self.damaged()),
                        },
                        byte => bytes.push(byte),
                    }
                }
                let text = String::from_utf8(bytes).map_err(|_| self.damaged())?;
                match sql_type {
                    SqlType::Json => Value::Json(text),
                    SqlType::Jsonb => Value::Jsonb(text),
                    _ => Value::Text(text),
                }
            }
        })
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
    use super::{
        IndexDefinition, TableDefinition, decode_key_values, decode_row, encode_key_value,
        encode_row, prefix_end, skip_key_values,
    };
    use crate::encoding::Column;
    use crate::types::{NumericModifier, SqlType, Value, compare_values};

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
        let types = [
            SqlType::Text,
            SqlType::SmallInt,
            SqlType::Integer,
            SqlType::BigInt,
            SqlType::Real,
            SqlType::Double,
            SqlType::Boolean,
            SqlType::Varchar(Some(9)),
            SqlType::Integer,
        ];
        let columns: Vec<Column> = types
            .iter()
            .map(|&sql_type| Column {
                name: String::new(),
                sql_type,
                not_null: false,
                default: None,
                auto_increment: false,
            })
            .collect();
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
        assert_eq!(decode_row(&encoded, &columns[..8]), Ok(values.clone()));
        let mut longer = values;
        longer.push(Value::Null);
        assert_eq!(decode_row(&encoded, &columns), Ok(longer));
        assert!(decode_row(&encoded, &columns[..7]).is_err());
        assert!(decode_row(&encoded, &columns[1..]).is_err());
        for cut in 0..encoded.len() {
            assert!(decode_row(&encoded[..cut], &columns[..8]).is_err());
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
                    auto_increment: false,
                },
                Column {
                    name: "name".into(),
                    sql_type: SqlType::Varchar(Some(20)),
                    not_null: false,
                    default: Some("'x'".into()),
                    auto_increment: false,
                },
            ],
            primary_key: vec![0],
            indexes: Vec::new(),
        };
        let encoded = table.encode();
        assert_eq!(TableDefinition::decode("t", &encoded), Ok(table));
        for cut in 0..encoded.len() {
            assert!(TableDefinition::decode("t", &encoded[..cut]).is_err());
        }
    }

    fn two_columns(auto: [bool; 2], primary_key: Vec<usize>) -> TableDefinition {
        TableDefinition {
            id: 7,
            name: "t".into(),
            columns: vec![
                Column {
                    name: "id".into(),
                    sql_type: SqlType::BigInt,
                    not_null: true,
                    default: None,
                    auto_increment: auto[0],
                },
                Column {
                    name: "name".into(),
                    sql_type: SqlType::Varchar(Some(20)),
                    not_null: false,
                    default: (!auto[1]).then(|| "'x'".to_owned()),
                    auto_increment: auto[1],
                },
            ],
            primary_key,
            indexes: Vec::new(),
        }
    }

    fn damaged(bytes: &[u8]) -> bool {
        TableDefinition::decode("t", bytes).is_err_and(|error| error.state().code() == "XX001")
    }

    #[test]
    fn a_table_without_auto_increment_stays_version_1_byte_for_byte() {
        let encoded = two_columns([false, false], vec![0]).encode();
        assert_eq!(
            encoded,
            [
                1, 7, 0, 0, 0, 2, 2, b'i', b'd', 3, 1, 0, 4, b'n', b'a', b'm', b'e', 8, 20, 0, 0,
                0, 0, 1, 3, b'\'', b'x', b'\'', 1, 0,
            ]
        );
        let mut auto_marker = encoded.clone();
        assert_eq!(auto_marker[11], 0);
        auto_marker[11] = 2;
        assert!(damaged(&auto_marker));
    }

    #[test]
    fn an_auto_increment_table_is_version_2_and_round_trips() {
        let table = two_columns([true, false], vec![0]);
        let encoded = table.encode();
        assert_eq!(encoded[0], 2);
        assert_eq!(encoded[11], 2);
        assert_eq!(TableDefinition::decode("t", &encoded), Ok(table));
        for cut in 0..encoded.len() {
            assert!(TableDefinition::decode("t", &encoded[..cut]).is_err());
        }
    }

    #[test]
    fn version_2_definitions_with_a_wrong_auto_increment_column_are_damaged() {
        let mut no_auto = two_columns([false, false], vec![0]).encode();
        no_auto[0] = 2;
        assert!(damaged(&no_auto));
        assert!(damaged(&two_columns([true, true], vec![0]).encode()));
        assert!(damaged(&two_columns([false, true], vec![0]).encode()));
        assert!(damaged(&two_columns([false, true], vec![0, 1]).encode()));
        assert!(damaged(&two_columns([true, false], Vec::new()).encode()));
        assert!(!damaged(&two_columns([true, false], vec![0, 1]).encode()));
    }

    #[test]
    fn a_newer_table_definition_version_is_refused_by_name() {
        let mut newer = with_indexes([true, false]).encode();
        newer[0] = 5;
        let error = TableDefinition::decode("t", &newer).unwrap_err();
        assert_eq!(error.state().code(), "0A000");
        assert!(error.message().contains("format version 5"), "{error}");
    }

    fn with_indexes(auto: [bool; 2]) -> TableDefinition {
        let mut table = two_columns(auto, vec![0]);
        table.indexes = vec![
            IndexDefinition {
                id: 1,
                name: "t_name_idx".into(),
                unique: false,
                columns: vec![1],
            },
            IndexDefinition {
                id: 2,
                name: "both".into(),
                unique: true,
                columns: vec![1, 0],
            },
        ];
        table
    }

    #[test]
    fn a_table_with_indexes_is_version_3_and_round_trips_with_or_without_auto_increment() {
        for auto in [[false, false], [true, false]] {
            let table = with_indexes(auto);
            let encoded = table.encode();
            assert_eq!(encoded[0], 3);
            assert_eq!(TableDefinition::decode("t", &encoded), Ok(table));
            for cut in 0..encoded.len() {
                assert!(damaged(&encoded[..cut]), "cut at {cut}");
            }
        }
    }

    #[test]
    fn version_3_pins_its_bytes_after_the_version_2_body() {
        let encoded = with_indexes([false, false]).encode();
        let body = two_columns([false, false], vec![0]).encode();
        assert_eq!(encoded[1..body.len()], body[1..]);
        assert_eq!(
            encoded[body.len()..],
            [
                2, 1, 0, 0, 0, 10, b't', b'_', b'n', b'a', b'm', b'e', b'_', b'i', b'd', b'x', 0,
                1, 1, 2, 0, 0, 0, 4, b'b', b'o', b't', b'h', 1, 2, 1, 0,
            ]
        );
    }

    #[test]
    fn damaged_index_definitions_are_errors() {
        let good = with_indexes([false, false]).encode();
        let tail = two_columns([false, false], vec![0]).encode().len();
        let mut no_indexes = good[..tail].to_vec();
        no_indexes[0] = 3;
        no_indexes.push(0);
        assert!(damaged(&no_indexes));
        let mut unique_marker = good.clone();
        unique_marker[tail + 16] = 2;
        assert!(damaged(&unique_marker));
        let mut no_columns = good.clone();
        no_columns[tail + 17] = 0;
        assert!(damaged(&no_columns));
        let mut missing_column = good.clone();
        missing_column[tail + 18] = 9;
        assert!(damaged(&missing_column));
        let mut same_id = good.clone();
        same_id[tail + 19] = 1;
        assert!(damaged(&same_id));
        let mut repeated_column = good.clone();
        repeated_column[tail + 31] = 1;
        assert!(damaged(&repeated_column));
        assert!(damaged(&with_indexes([true, true]).encode()));
        assert!(damaged(&with_indexes([false, true]).encode()));
        let mut long = with_indexes([false, false]);
        long.indexes[0].columns = (0..33).map(|column| column % 2).collect();
        assert!(damaged(&long.encode()));
    }

    #[test]
    fn index_key_values_decode_back_to_what_was_encoded() {
        let mut random = Seeded(0x0123_4567_89AB_CDEF);
        let types = [
            SqlType::SmallInt,
            SqlType::Integer,
            SqlType::BigInt,
            SqlType::Real,
            SqlType::Double,
            SqlType::Boolean,
            SqlType::Text,
        ];
        for _ in 0..2_000 {
            let mut values = Vec::new();
            let mut key = Vec::new();
            for (kind, _) in types.iter().enumerate() {
                let value = if random.next().is_multiple_of(5) {
                    Value::Null
                } else {
                    random_value(&mut random, kind as u64)
                };
                encode_key_value(&mut key, &value);
                values.push(value);
            }
            key.extend_from_slice(&[9, 9]);
            let mut decoded = Vec::new();
            let used = decode_key_values(&key, types, &mut decoded).unwrap();
            assert_eq!(used, key.len() - 2);
            assert_eq!(skip_key_values(&key, types), Ok(used));
            for (left, right) in values.iter().zip(&decoded) {
                assert_eq!(compare_values(left, right), std::cmp::Ordering::Equal);
                assert_eq!(left.is_null(), right.is_null());
            }
            for cut in 0..used {
                let mut partial = Vec::new();
                assert!(decode_key_values(&key[..cut], types, &mut partial).is_err());
                assert!(skip_key_values(&key[..cut], types).is_err());
            }
        }
    }

    #[test]
    fn hostile_index_keys_and_definitions_are_errors_never_panics() {
        let mut random = Seeded(0xFEED_FACE_0BAD_CAFE);
        let good = with_indexes([true, false]).encode();
        for _ in 0..20_000 {
            let mut bytes = good.clone();
            let flips = 1 + random.next() % 4;
            for _ in 0..flips {
                let at = (random.next() as usize) % bytes.len();
                bytes[at] = random.next() as u8;
            }
            if random.next().is_multiple_of(3) {
                let keep = (random.next() as usize) % bytes.len();
                bytes.truncate(keep);
            }
            let _ = TableDefinition::decode("t", &bytes);
            let mut values = Vec::new();
            let types = [
                SqlType::Text,
                SqlType::Double,
                SqlType::Real,
                SqlType::BigInt,
            ];
            if let Ok(used) = decode_key_values(&bytes, types, &mut values) {
                assert_eq!(skip_key_values(&bytes, types), Ok(used));
            }
        }
    }

    fn typed_columns() -> Vec<Column> {
        let column = |name: &str, sql_type| Column {
            name: name.into(),
            sql_type,
            not_null: false,
            default: None,
            auto_increment: false,
        };
        vec![
            Column {
                auto_increment: true,
                not_null: true,
                ..column("id", SqlType::BigInt)
            },
            column(
                "price",
                SqlType::Numeric(Some(NumericModifier {
                    precision: 10,
                    scale: 2,
                })),
            ),
            column("ratio", SqlType::Numeric(None)),
            column("day", SqlType::Date),
            column("at", SqlType::Timestamp(Some(3))),
            column("zoned", SqlType::TimestampTz(None)),
            column("doc", SqlType::Json),
            column("body", SqlType::Jsonb),
        ]
    }

    fn typed_table(indexes: bool) -> TableDefinition {
        TableDefinition {
            id: 9,
            name: "typed".into(),
            columns: typed_columns(),
            primary_key: vec![0],
            indexes: if indexes {
                vec![IndexDefinition {
                    id: 1,
                    name: "typed_day".into(),
                    unique: false,
                    columns: vec![3, 1],
                }]
            } else {
                Vec::new()
            },
        }
    }

    #[test]
    fn a_table_with_the_new_types_is_version_4_and_round_trips() {
        for indexes in [false, true] {
            let table = typed_table(indexes);
            let encoded = table.encode();
            assert_eq!(encoded[0], 4);
            assert_eq!(TableDefinition::decode("typed", &encoded), Ok(table));
            for cut in 0..encoded.len() {
                assert!(damaged(&encoded[..cut]), "cut at {cut}");
            }
        }
    }

    #[test]
    fn version_4_pins_the_bytes_of_every_new_type() {
        let encoded = typed_table(false).encode();
        let expected: &[u8] = &[
            4, 9, 0, 0, 0, 8, 2, b'i', b'd', 3, 1, 2, 5, b'p', b'r', b'i', b'c', b'e', 9, 10, 0, 2,
            0, 0, 0, 5, b'r', b'a', b't', b'i', b'o', 9, 0, 0, 0, 0, 0, 0, 3, b'd', b'a', b'y', 10,
            0, 0, 2, b'a', b't', 11, 3, 0, 0, 5, b'z', b'o', b'n', b'e', b'd', 12, 0xFF, 0, 0, 3,
            b'd', b'o', b'c', 13, 0, 0, 4, b'b', b'o', b'd', b'y', 14, 0, 0, 1, 0, 0,
        ];
        assert_eq!(encoded, expected);
    }

    #[test]
    fn damaged_or_misplaced_type_tags_are_errors() {
        let good = typed_table(false).encode();
        let price_tag = 18;
        assert_eq!(good[price_tag], 9);
        let mut unknown = good.clone();
        unknown[price_tag] = 15;
        assert!(damaged(&unknown));
        for older in [1, 2, 3] {
            let mut older_version = good.clone();
            older_version[0] = older;
            assert!(damaged(&older_version), "version {older}");
        }
        let mut precision = good.clone();
        precision[price_tag + 1] = 0xE9;
        precision[price_tag + 2] = 0x03;
        assert!(damaged(&precision), "precision 1001");
        let mut scale_without_precision = good.clone();
        scale_without_precision[price_tag + 1] = 0;
        assert!(damaged(&scale_without_precision));
        let at_precision = good.iter().position(|&byte| byte == 11).unwrap() + 1;
        let mut fraction = good.clone();
        fraction[at_precision] = 7;
        assert!(damaged(&fraction));
        let mut plain = two_columns([false, false], vec![0]).encode();
        plain[0] = 4;
        plain.push(0);
        assert!(damaged(&plain), "version 4 without a new type");
        let mut random = Seeded(0xBAD_F00D_0000_0004);
        for _ in 0..20_000 {
            let mut bytes = good.clone();
            for _ in 0..1 + random.next() % 4 {
                let at = (random.next() as usize) % bytes.len();
                bytes[at] = random.next() as u8;
            }
            let _ = TableDefinition::decode("typed", &bytes);
        }
    }

    fn typed_values() -> Vec<Value> {
        vec![
            Value::BigInt(1),
            Value::Numeric(crate::numeric::Numeric::parse("-12.50").unwrap()),
            Value::Numeric(crate::numeric::Numeric::parse("NaN").unwrap()),
            Value::Date(-2_451_545),
            Value::Timestamp(crate::datetime::END_TIMESTAMP - 1),
            Value::TimestampTz(crate::datetime::MIN_TIMESTAMP),
            Value::Json("{\"a\": [1, 2]}".into()),
            Value::Jsonb("{\"a\": [1, 2]}".into()),
        ]
    }

    #[test]
    fn rows_of_the_new_types_round_trip_and_hostile_bytes_are_damage() {
        let columns = typed_columns();
        let values = typed_values();
        let encoded = encode_row(&values);
        assert_eq!(decode_row(&encoded, &columns), Ok(values));
        for cut in 0..encoded.len() {
            assert!(decode_row(&encoded[..cut], &columns).is_err());
        }
        let date_at = encoded.iter().position(|&byte| byte == 10).unwrap();
        let mut early = encoded.clone();
        early[date_at + 1..date_at + 5].copy_from_slice(&(-2_451_546i32).to_le_bytes());
        assert!(decode_row(&early, &columns).is_err());
        let mut random = Seeded(0x0DD_C0DE_0000_0019);
        for _ in 0..20_000 {
            let mut bytes = encoded.clone();
            for _ in 0..1 + random.next() % 4 {
                let at = (random.next() as usize) % bytes.len();
                bytes[at] = random.next() as u8;
            }
            if let Ok(decoded) = decode_row(&bytes, &columns) {
                assert_eq!(decoded.len(), columns.len());
            }
        }
    }

    #[test]
    fn keys_of_the_new_types_keep_their_order_and_decode_back() {
        let mut random = Seeded(0x1D1E_5EED_0000_0019);
        let types = [
            SqlType::Numeric(None),
            SqlType::Date,
            SqlType::Timestamp(None),
            SqlType::TimestampTz(None),
            SqlType::Jsonb,
        ];
        let value = |random: &mut Seeded, kind: usize| -> Value {
            let bits = random.next();
            match kind {
                0 => {
                    let text = format!(
                        "{}{}.{}",
                        if bits.is_multiple_of(2) { "-" } else { "" },
                        bits % 1000,
                        (bits >> 20) % 100
                    );
                    Value::Numeric(crate::numeric::Numeric::parse(&text).unwrap())
                }
                1 => Value::Date((bits % 4_000_000) as i32 - 2_000_000),
                2 => Value::Timestamp((bits >> 8) as i64 - (1 << 54)),
                3 => Value::TimestampTz((bits >> 8) as i64 - (1 << 54)),
                _ => Value::Jsonb(format!("[{}]", bits % 50)),
            }
        };
        for (kind, sql_type) in types.iter().enumerate() {
            for _ in 0..2_000 {
                let left = value(&mut random, kind);
                let right = value(&mut random, kind);
                assert_eq!(
                    key_of(&left).cmp(&key_of(&right)),
                    compare_values(&left, &right),
                    "{left:?} vs {right:?}"
                );
                let key = [key_of(&left), vec![7]].concat();
                let mut decoded = Vec::new();
                let used = decode_key_values(&key, [*sql_type], &mut decoded).unwrap();
                assert_eq!(used, key.len() - 1);
                assert_eq!(skip_key_values(&key, [*sql_type]), Ok(used));
                assert_eq!(
                    compare_values(&decoded[0], &left),
                    std::cmp::Ordering::Equal
                );
                for cut in 0..used {
                    let mut partial = Vec::new();
                    assert!(decode_key_values(&key[..cut], [*sql_type], &mut partial).is_err());
                }
            }
        }
        let mut hostile = Seeded(0xFACE_0000_0019);
        for _ in 0..20_000 {
            let length = (hostile.next() % 16) as usize;
            let bytes: Vec<u8> = (0..length).map(|_| hostile.next() as u8).collect();
            let mut values = Vec::new();
            if let Ok(used) = decode_key_values(&bytes, types, &mut values) {
                assert_eq!(skip_key_values(&bytes, types), Ok(used));
            }
        }
    }

    #[test]
    fn a_prefix_end_is_the_first_key_after_every_key_with_the_prefix() {
        assert_eq!(prefix_end(&[1, 2]), vec![1, 3]);
        assert_eq!(prefix_end(&[1, 0xFF]), vec![2]);
    }
}
