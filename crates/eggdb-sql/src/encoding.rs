use crate::ast::ReferentialAction;
use crate::datetime;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::interval::{Interval, IntervalFields};
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
const MORE_TYPES_FORMAT_VERSION: u8 = 5;
const CONSTRAINTS_FORMAT_VERSION: u8 = 6;
const SEQUENCE_SPACE: u8 = 0x05;
const REFERENCE_SPACE: u8 = 0x06;
const NO_PRECISION: u8 = 0xFF;
const DEFAULT_ABSENT: u8 = 0;
const DEFAULT_EXPRESSION: u8 = 1;
const DEFAULT_AUTO_INCREMENT: u8 = 2;
pub(crate) const PRESENT: u8 = 0x01;
pub(crate) const NULL_IN_KEY: u8 = 0x02;
pub(crate) const MAX_COLUMNS: usize = 1600;
pub(crate) const MAX_INDEX_COLUMNS: usize = 32;
pub(crate) const MAX_INDEXES: usize = 1024;
pub(crate) const MAX_CHARACTER_LENGTH: u32 = 10_485_760;
pub(crate) const MAX_CONSTRAINTS: usize = 1024;

pub(crate) fn sequence_key(name: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(SEQUENCE_SPACE);
    key.extend_from_slice(name.as_bytes());
    key
}

pub(crate) fn sequences_prefix() -> Vec<u8> {
    sequence_key("")
}

pub(crate) fn references_prefix(parent: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(REFERENCE_SPACE);
    put_bytes(&mut key, parent.as_bytes());
    key
}

pub(crate) fn reference_key(parent: &str, child: &str) -> Vec<u8> {
    let mut key = references_prefix(parent);
    key.extend_from_slice(child.as_bytes());
    key
}

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
        SqlType::Time(_) => 15,
        SqlType::TimeTz(_) => 16,
        SqlType::Interval { .. } => 17,
        SqlType::Bytea => 18,
        SqlType::Uuid => 19,
        SqlType::Bpchar(_) => 20,
    }
}

fn is_version_4_type(sql_type: SqlType) -> bool {
    type_tag(sql_type) >= type_tag(SqlType::Numeric(None))
}

fn is_version_5_type(sql_type: SqlType) -> bool {
    type_tag(sql_type) >= type_tag(SqlType::Time(None))
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
        SqlType::Timestamp(precision)
        | SqlType::TimestampTz(precision)
        | SqlType::Time(precision)
        | SqlType::TimeTz(precision) => {
            out.push(precision.unwrap_or(NO_PRECISION));
        }
        SqlType::Interval { fields, precision } => {
            out.push(fields.map_or(0, IntervalFields::code));
            out.push(precision.unwrap_or(NO_PRECISION));
        }
        SqlType::Bpchar(length) => out.extend_from_slice(&length.unwrap_or(0).to_le_bytes()),
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
pub(crate) struct CheckConstraint {
    pub(crate) name: String,
    pub(crate) dialect: crate::Dialect,
    pub(crate) expression: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ForeignKey {
    pub(crate) name: String,
    pub(crate) columns: Vec<usize>,
    pub(crate) parent: String,
    pub(crate) parent_columns: Vec<String>,
    pub(crate) on_delete: ReferentialAction,
    pub(crate) on_update: ReferentialAction,
    pub(crate) deferrable: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct TableDefinition {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) columns: Vec<Column>,
    pub(crate) primary_key: Vec<usize>,
    pub(crate) indexes: Vec<IndexDefinition>,
    pub(crate) primary_key_name: Option<String>,
    pub(crate) checks: Vec<CheckConstraint>,
    pub(crate) foreign_keys: Vec<ForeignKey>,
    pub(crate) referenced: bool,
    pub(crate) owns_sequence: bool,
}

fn dialect_code(dialect: crate::Dialect) -> u8 {
    match dialect {
        crate::Dialect::PostgreSql => 0,
        crate::Dialect::MySql => 1,
        crate::Dialect::Sqlite => 2,
        crate::Dialect::Oracle => 3,
    }
}

fn dialect_of(code: u8) -> Option<crate::Dialect> {
    Some(match code {
        0 => crate::Dialect::PostgreSql,
        1 => crate::Dialect::MySql,
        2 => crate::Dialect::Sqlite,
        3 => crate::Dialect::Oracle,
        _ => return None,
    })
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

    pub(crate) fn has_constraints(&self) -> bool {
        self.primary_key_name.is_some()
            || !self.checks.is_empty()
            || !self.foreign_keys.is_empty()
            || self.referenced
            || self.owns_sequence
    }

    pub(crate) fn primary_key_constraint_name(&self) -> String {
        self.primary_key_name
            .clone()
            .unwrap_or_else(|| format!("{}_pkey", self.name))
    }

    pub(crate) fn constraint_name_taken(&self, name: &str) -> bool {
        self.checks.iter().any(|check| check.name == name)
            || self.foreign_keys.iter().any(|key| key.name == name)
            || self.indexes.iter().any(|index| index.name == name)
            || (!self.primary_key.is_empty() && self.primary_key_constraint_name() == name)
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let version = if self.has_constraints() {
            CONSTRAINTS_FORMAT_VERSION
        } else if self
            .columns
            .iter()
            .any(|column| is_version_5_type(column.sql_type))
        {
            MORE_TYPES_FORMAT_VERSION
        } else if self
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
        if version >= CONSTRAINTS_FORMAT_VERSION {
            self.encode_constraints(&mut out);
        }
        out
    }

    fn encode_constraints(&self, out: &mut Vec<u8>) {
        match &self.primary_key_name {
            Some(name) => {
                out.push(1);
                put_bytes(out, name.as_bytes());
            }
            None => out.push(0),
        }
        out.push(u8::from(self.referenced) | (u8::from(self.owns_sequence) << 1));
        put_varint(out, self.checks.len() as u64);
        for check in &self.checks {
            put_bytes(out, check.name.as_bytes());
            out.push(dialect_code(check.dialect));
            put_bytes(out, check.expression.as_bytes());
        }
        put_varint(out, self.foreign_keys.len() as u64);
        for key in &self.foreign_keys {
            put_bytes(out, key.name.as_bytes());
            put_varint(out, key.columns.len() as u64);
            for &column in &key.columns {
                put_varint(out, column as u64);
            }
            put_bytes(out, key.parent.as_bytes());
            for column in &key.parent_columns {
                put_bytes(out, column.as_bytes());
            }
            out.push(key.on_delete.code());
            out.push(key.on_update.code());
            out.push(u8::from(key.deferrable));
        }
    }

    pub(crate) fn decode(name: &str, bytes: &[u8]) -> SqlResult<Self> {
        let mut reader = Reader::new(bytes, "table definition");
        let version = reader.version_up_to(CONSTRAINTS_FORMAT_VERSION)?;
        let auto_allowed = version >= AUTO_INCREMENT_FORMAT_VERSION;
        let new_types_allowed = version >= TYPES_FORMAT_VERSION;
        let more_types_allowed = version >= MORE_TYPES_FORMAT_VERSION;
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
                15 if more_types_allowed => SqlType::Time(reader.precision()?),
                16 if more_types_allowed => SqlType::TimeTz(reader.precision()?),
                17 if more_types_allowed => reader.interval_type()?,
                18 if more_types_allowed => SqlType::Bytea,
                19 if more_types_allowed => SqlType::Uuid,
                20 if more_types_allowed => match reader.u32()? {
                    0 => SqlType::Bpchar(None),
                    length if length <= MAX_CHARACTER_LENGTH => SqlType::Bpchar(Some(length)),
                    _ => return Err(reader.damaged()),
                },
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
            TYPES_FORMAT_VERSION | MORE_TYPES_FORMAT_VERSION | CONSTRAINTS_FORMAT_VERSION => {
                decode_indexes(&mut reader, columns.len(), true)?
            }
            _ => Vec::new(),
        };
        let constraints = if version >= CONSTRAINTS_FORMAT_VERSION {
            Some(decode_constraints(&mut reader, &columns, &indexes)?)
        } else {
            None
        };
        reader.finish()?;
        let newest_type_needed = if more_types_allowed {
            is_version_5_type
        } else {
            is_version_4_type
        };
        if new_types_allowed
            && version < CONSTRAINTS_FORMAT_VERSION
            && !columns
                .iter()
                .any(|column| newest_type_needed(column.sql_type))
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
        let DecodedConstraints {
            primary_key_name,
            checks,
            foreign_keys,
            referenced,
            owns_sequence,
        } = constraints.unwrap_or_default();
        if primary_key_name.is_some() && primary_key.is_empty() {
            return Err(reader.damaged());
        }
        let table = Self {
            id,
            name: name.to_owned(),
            columns,
            primary_key,
            indexes,
            primary_key_name,
            checks,
            foreign_keys,
            referenced,
            owns_sequence,
        };
        if version == CONSTRAINTS_FORMAT_VERSION && !table.has_constraints() {
            return Err(reader.damaged());
        }
        Ok(table)
    }
}

#[derive(Default)]
struct DecodedConstraints {
    primary_key_name: Option<String>,
    checks: Vec<CheckConstraint>,
    foreign_keys: Vec<ForeignKey>,
    referenced: bool,
    owns_sequence: bool,
}

fn decode_constraints(
    reader: &mut Reader<'_>,
    columns: &[Column],
    indexes: &[IndexDefinition],
) -> SqlResult<DecodedConstraints> {
    let primary_key_name = match reader.byte()? {
        0 => None,
        1 => Some(reader.name()?),
        _ => return Err(reader.damaged()),
    };
    let flags = reader.byte()?;
    if flags > 3 {
        return Err(reader.damaged());
    }
    let mut names: Vec<String> = indexes.iter().map(|index| index.name.clone()).collect();
    names.extend(primary_key_name.iter().cloned());
    let check_count = reader.count()?;
    if check_count > MAX_CONSTRAINTS {
        return Err(reader.damaged());
    }
    let mut checks = Vec::new();
    for _ in 0..check_count {
        let name = reader.name()?;
        let dialect = dialect_of(reader.byte()?).ok_or_else(|| reader.damaged())?;
        let expression = reader.text()?;
        if expression.trim().is_empty() || names.contains(&name) {
            return Err(reader.damaged());
        }
        names.push(name.clone());
        checks.push(CheckConstraint {
            name,
            dialect,
            expression,
        });
    }
    let key_count = reader.count()?;
    if key_count > MAX_CONSTRAINTS {
        return Err(reader.damaged());
    }
    let mut foreign_keys = Vec::new();
    for _ in 0..key_count {
        let name = reader.name()?;
        let width = reader.count()?;
        if width == 0 || width > MAX_INDEX_COLUMNS {
            return Err(reader.damaged());
        }
        let mut key_columns = Vec::with_capacity(width);
        for _ in 0..width {
            let column = usize::try_from(reader.varint()?).map_err(|_| reader.damaged())?;
            if column >= columns.len() || key_columns.contains(&column) {
                return Err(reader.damaged());
            }
            key_columns.push(column);
        }
        let parent = reader.name()?;
        let mut parent_columns: Vec<String> = Vec::with_capacity(width);
        for _ in 0..width {
            let column = reader.name()?;
            if parent_columns.contains(&column) {
                return Err(reader.damaged());
            }
            parent_columns.push(column);
        }
        let on_delete =
            ReferentialAction::from_code(reader.byte()?).ok_or_else(|| reader.damaged())?;
        let on_update =
            ReferentialAction::from_code(reader.byte()?).ok_or_else(|| reader.damaged())?;
        let deferrable = match reader.byte()? {
            0 => false,
            1 => true,
            _ => return Err(reader.damaged()),
        };
        if names.contains(&name) {
            return Err(reader.damaged());
        }
        names.push(name.clone());
        foreign_keys.push(ForeignKey {
            name,
            columns: key_columns,
            parent,
            parent_columns,
            on_delete,
            on_update,
            deferrable,
        });
    }
    Ok(DecodedConstraints {
        primary_key_name,
        checks,
        foreign_keys,
        referenced: flags & 1 == 1,
        owns_sequence: flags & 2 == 2,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SequenceDefinition {
    pub(crate) sql_type: SqlType,
    pub(crate) start: i64,
    pub(crate) increment: i64,
    pub(crate) min: i64,
    pub(crate) max: i64,
    pub(crate) cache: i64,
    pub(crate) cycle: bool,
    pub(crate) last_value: i64,
    pub(crate) is_called: bool,
    pub(crate) owned_by: Option<(String, String)>,
}

pub(crate) fn integer_type_range(sql_type: SqlType) -> Option<(i64, i64)> {
    match sql_type {
        SqlType::SmallInt => Some((i64::from(i16::MIN), i64::from(i16::MAX))),
        SqlType::Integer => Some((i64::from(i32::MIN), i64::from(i32::MAX))),
        SqlType::BigInt => Some((i64::MIN, i64::MAX)),
        _ => None,
    }
}

impl SequenceDefinition {
    pub(crate) fn valid(&self) -> bool {
        let Some((low, high)) = integer_type_range(self.sql_type) else {
            return false;
        };
        self.increment != 0
            && self.cache >= 1
            && low <= self.min
            && self.min < self.max
            && self.max <= high
            && self.min <= self.start
            && self.start <= self.max
            && self.min <= self.last_value
            && self.last_value <= self.max
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = vec![FORMAT_VERSION];
        out.push(type_tag(self.sql_type));
        for number in [
            self.start,
            self.increment,
            self.min,
            self.max,
            self.cache,
            self.last_value,
        ] {
            out.extend_from_slice(&number.to_le_bytes());
        }
        out.push(u8::from(self.cycle) | (u8::from(self.is_called) << 1));
        match &self.owned_by {
            Some((table, column)) => {
                out.push(1);
                put_bytes(&mut out, table.as_bytes());
                put_bytes(&mut out, column.as_bytes());
            }
            None => out.push(0),
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> SqlResult<Self> {
        let mut reader = Reader::new(bytes, "sequence");
        reader.version()?;
        let sql_type = match reader.byte()? {
            1 => SqlType::SmallInt,
            2 => SqlType::Integer,
            3 => SqlType::BigInt,
            _ => return Err(reader.damaged()),
        };
        let mut number = || -> SqlResult<i64> { Ok(i64::from_le_bytes(reader.array()?)) };
        let start = number()?;
        let increment = number()?;
        let min = number()?;
        let max = number()?;
        let cache = number()?;
        let last_value = number()?;
        let flags = reader.byte()?;
        if flags > 3 {
            return Err(reader.damaged());
        }
        let owned_by = match reader.byte()? {
            0 => None,
            1 => Some((reader.name()?, reader.name()?)),
            _ => return Err(reader.damaged()),
        };
        reader.finish()?;
        let sequence = Self {
            sql_type,
            start,
            increment,
            min,
            max,
            cache,
            cycle: flags & 1 == 1,
            last_value,
            is_called: flags & 2 == 2,
            owned_by,
        };
        if !sequence.valid() {
            return Err(reader.damaged());
        }
        Ok(sequence)
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
        Value::Jsonb(text) => {
            out.push(PRESENT);
            push_escaped_text(out, crate::json::comparable(text).as_bytes());
        }
        Value::Text(text) | Value::Json(text) => {
            out.push(PRESENT);
            push_escaped_text(out, text.as_bytes());
        }
        Value::Bpchar(text) => {
            out.push(PRESENT);
            push_escaped_text(out, crate::types::without_trailing_spaces(text).as_bytes());
        }
        Value::Bytea(bytes) => {
            out.push(PRESENT);
            push_escaped_text(out, bytes);
        }
        Value::Uuid(bytes) => {
            out.push(PRESENT);
            out.extend_from_slice(bytes);
        }
        Value::Time(micros) => {
            out.push(PRESENT);
            out.extend_from_slice(&((*micros as u64) ^ 0x8000_0000_0000_0000).to_be_bytes());
        }
        Value::TimeTz {
            micros,
            offset_seconds,
        } => {
            out.push(PRESENT);
            let utc = datetime::time_tz_utc(*micros, *offset_seconds);
            out.extend_from_slice(&((utc as u64) ^ 0x8000_0000_0000_0000).to_be_bytes());
            let west = offset_seconds.wrapping_neg();
            out.extend_from_slice(&((west as u32) ^ 0x8000_0000).to_be_bytes());
        }
        Value::Interval(interval) => {
            out.push(PRESENT);
            out.extend_from_slice(&((interval.span() as u128) ^ (1 << 127)).to_be_bytes());
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

fn push_escaped_text(out: &mut Vec<u8>, bytes: &[u8]) {
    for &byte in bytes {
        out.push(byte);
        if byte == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0x00, 0x01]);
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
            Value::Time(micros) => {
                out.push(15);
                out.extend_from_slice(&micros.to_le_bytes());
            }
            Value::TimeTz {
                micros,
                offset_seconds,
            } => {
                out.push(16);
                out.extend_from_slice(&micros.to_le_bytes());
                out.extend_from_slice(&offset_seconds.to_le_bytes());
            }
            Value::Interval(interval) => {
                out.push(17);
                out.extend_from_slice(&interval.months.to_le_bytes());
                out.extend_from_slice(&interval.days.to_le_bytes());
                out.extend_from_slice(&interval.micros.to_le_bytes());
            }
            Value::Bytea(bytes) => {
                out.push(18);
                put_bytes(out, bytes);
            }
            Value::Uuid(bytes) => {
                out.push(19);
                out.extend_from_slice(bytes);
            }
            Value::Bpchar(text) => {
                out.push(20);
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
            13 if wanted => Value::Json(reader.json(false)?),
            14 if wanted => Value::Jsonb(reader.json(true)?),
            18 if wanted => {
                let length = reader.count()?;
                Value::Bytea(reader.take(length)?.to_vec())
            }
            20 if wanted => Value::Bpchar(reader.text()?),
            7 | 13 | 14 | 18 | 20 => {
                reader.skip_text()?;
                Value::Null
            }
            9 => Value::Numeric(reader.numeric()?),
            15 => {
                let micros = i64::from_le_bytes(reader.array()?);
                if !datetime::valid_time(micros) {
                    return Err(reader.damaged());
                }
                Value::Time(micros)
            }
            16 => {
                let micros = i64::from_le_bytes(reader.array()?);
                let offset_seconds = i32::from_le_bytes(reader.array()?);
                if !datetime::valid_time(micros) || !datetime::valid_offset(offset_seconds) {
                    return Err(reader.damaged());
                }
                Value::TimeTz {
                    micros,
                    offset_seconds,
                }
            }
            17 => Value::Interval(Interval {
                months: i32::from_le_bytes(reader.array()?),
                days: i32::from_le_bytes(reader.array()?),
                micros: i64::from_le_bytes(reader.array()?),
            }),
            19 => Value::Uuid(reader.array()?),
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

    fn name(&mut self) -> SqlResult<String> {
        let name = self.text()?;
        if name.is_empty() {
            return Err(self.damaged());
        }
        Ok(name)
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

    fn interval_type(&mut self) -> SqlResult<SqlType> {
        let fields = match self.byte()? {
            0 => None,
            code => Some(IntervalFields::from_code(code).ok_or_else(|| self.damaged())?),
        };
        let precision = self.precision()?;
        if precision.is_some() && fields.is_some_and(|fields| !fields.ends_in_seconds()) {
            return Err(self.damaged());
        }
        Ok(SqlType::Interval { fields, precision })
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

    fn json(&mut self, binary: bool) -> SqlResult<String> {
        let text = self.text()?;
        crate::json::parse(&text, binary).map_err(|_| self.damaged())?;
        Ok(text)
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
            SqlType::BigInt
            | SqlType::Double
            | SqlType::Timestamp(_)
            | SqlType::TimestampTz(_)
            | SqlType::Time(_) => 8,
            SqlType::TimeTz(_) => 12,
            SqlType::Interval { .. } | SqlType::Uuid => 16,
            SqlType::Numeric(_) => {
                let rest = self.bytes.get(self.at..).unwrap_or(&[]);
                Numeric::key_length(rest).ok_or_else(|| self.damaged())?
            }
            SqlType::Text
            | SqlType::Varchar(_)
            | SqlType::Json
            | SqlType::Jsonb
            | SqlType::Bytea
            | SqlType::Bpchar(_) => {
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
            SqlType::Time(_) => {
                let micros = (u64::from_be_bytes(self.array()?) ^ 0x8000_0000_0000_0000) as i64;
                if !datetime::valid_time(micros) {
                    return Err(self.damaged());
                }
                Value::Time(micros)
            }
            SqlType::TimeTz(_) => {
                let utc = (u64::from_be_bytes(self.array()?) ^ 0x8000_0000_0000_0000) as i64;
                let west = (u32::from_be_bytes(self.array()?) ^ 0x8000_0000) as i32;
                let offset_seconds = west.wrapping_neg();
                if !datetime::valid_offset(offset_seconds) {
                    return Err(self.damaged());
                }
                let micros = utc
                    .checked_add(i64::from(offset_seconds) * datetime::MICROS_PER_SECOND)
                    .filter(|&micros| datetime::valid_time(micros))
                    .ok_or_else(|| self.damaged())?;
                Value::TimeTz {
                    micros,
                    offset_seconds,
                }
            }
            SqlType::Interval { .. } => {
                let span = (u128::from_be_bytes(self.array()?) ^ (1 << 127)) as i128;
                Value::Interval(Interval::from_span(span).ok_or_else(|| self.damaged())?)
            }
            SqlType::Uuid => Value::Uuid(self.array()?),
            SqlType::Text
            | SqlType::Varchar(_)
            | SqlType::Json
            | SqlType::Jsonb
            | SqlType::Bytea
            | SqlType::Bpchar(_) => {
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
                if sql_type == SqlType::Bytea {
                    return Ok(Value::Bytea(bytes));
                }
                let text = String::from_utf8(bytes).map_err(|_| self.damaged())?;
                match sql_type {
                    SqlType::Bpchar(_) => Value::Bpchar(text),
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
        IndexDefinition, MAX_CHARACTER_LENGTH, TableDefinition, decode_key_values, decode_row,
        encode_key_value, encode_row, prefix_end, skip_key_values,
    };
    use crate::encoding::Column;
    use crate::interval::{Interval, IntervalFields};
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
            ..TableDefinition::default()
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
            ..TableDefinition::default()
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
        newer[0] = 7;
        let error = TableDefinition::decode("t", &newer).unwrap_err();
        assert_eq!(error.state().code(), "0A000");
        assert!(error.message().contains("format version 7"), "{error}");
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
            ..TableDefinition::default()
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
        let mut broken_json = typed_values();
        broken_json[6] = Value::Json("{\"a\": [1, 2}".into());
        assert!(decode_row(&encode_row(&broken_json), &columns).is_err());
        let mut broken_jsonb = typed_values();
        broken_jsonb[7] = Value::Jsonb("[".repeat(300));
        assert!(decode_row(&encode_row(&broken_jsonb), &columns).is_err());
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

    fn item_31_columns() -> Vec<Column> {
        let column = |name: &str, sql_type| Column {
            name: name.into(),
            sql_type,
            not_null: false,
            default: None,
            auto_increment: false,
        };
        vec![
            column("id", SqlType::Integer),
            column("at", SqlType::Time(Some(2))),
            column("zoned", SqlType::TimeTz(None)),
            column(
                "span",
                SqlType::Interval {
                    fields: Some(IntervalFields::DayToSecond),
                    precision: Some(3),
                },
            ),
            column("raw", SqlType::Bytea),
            column("tag", SqlType::Uuid),
            column("code", SqlType::Bpchar(Some(4))),
            column("price", SqlType::Numeric(None)),
        ]
    }

    fn item_31_table() -> TableDefinition {
        TableDefinition {
            id: 11,
            name: "more".into(),
            columns: item_31_columns(),
            primary_key: vec![0],
            indexes: vec![IndexDefinition {
                id: 1,
                name: "more_span".into(),
                unique: false,
                columns: vec![3, 6],
            }],
            ..TableDefinition::default()
        }
    }

    #[test]
    fn a_table_with_item_31_types_is_version_5_and_round_trips() {
        let table = item_31_table();
        let encoded = table.encode();
        assert_eq!(encoded[0], 5);
        assert_eq!(TableDefinition::decode("more", &encoded), Ok(table));
        for cut in 0..encoded.len() {
            assert!(damaged(&encoded[..cut]), "cut at {cut}");
        }
        assert_eq!(typed_table(true).encode()[0], 4);
        assert_eq!(with_indexes([false, false]).encode()[0], 3);
    }

    #[test]
    fn version_5_pins_the_bytes_of_every_item_31_type() {
        let mut table = item_31_table();
        table.indexes.clear();
        let expected: &[u8] = &[
            5, 11, 0, 0, 0, 8, 2, b'i', b'd', 2, 0, 0, 2, b'a', b't', 15, 2, 0, 0, 5, b'z', b'o',
            b'n', b'e', b'd', 16, 0xFF, 0, 0, 4, b's', b'p', b'a', b'n', 17, 10, 3, 0, 0, 3, b'r',
            b'a', b'w', 18, 0, 0, 3, b't', b'a', b'g', 19, 0, 0, 4, b'c', b'o', b'd', b'e', 20, 4,
            0, 0, 0, 0, 0, 5, b'p', b'r', b'i', b'c', b'e', 9, 0, 0, 0, 0, 0, 0, 1, 0, 0,
        ];
        assert_eq!(table.encode(), expected);
    }

    #[test]
    fn damaged_item_31_type_tags_and_modifiers_are_errors() {
        let good = item_31_table().encode();
        let at_tag = |tag: u8| good.iter().position(|&byte| byte == tag).unwrap();
        let mut older = good.clone();
        older[0] = 4;
        assert!(damaged(&older), "an item 31 type in version 4");
        let mut unknown = good.clone();
        unknown[at_tag(20)] = 21;
        assert!(damaged(&unknown));
        let mut fraction = good.clone();
        fraction[at_tag(15) + 1] = 7;
        assert!(damaged(&fraction));
        let mut fields = good.clone();
        fields[at_tag(17) + 1] = 14;
        assert!(damaged(&fields), "fields code 14");
        let mut precision_without_seconds = good.clone();
        precision_without_seconds[at_tag(17) + 1] = 7;
        assert!(damaged(&precision_without_seconds), "year to month(3)");
        let mut long = good.clone();
        let length_at = at_tag(20) + 1;
        long[length_at..length_at + 4].copy_from_slice(&(MAX_CHARACTER_LENGTH + 1).to_le_bytes());
        assert!(damaged(&long));
        let mut plain = typed_table(false).encode();
        plain[0] = 5;
        assert!(damaged(&plain), "version 5 without an item 31 type");
        let mut random = Seeded(0xBAD_F00D_0000_0031);
        for _ in 0..20_000 {
            let mut bytes = good.clone();
            for _ in 0..1 + random.next() % 4 {
                let at = (random.next() as usize) % bytes.len();
                bytes[at] = random.next() as u8;
            }
            let _ = TableDefinition::decode("more", &bytes);
        }
    }

    fn item_31_values() -> Vec<Value> {
        vec![
            Value::Integer(1),
            Value::Time(crate::datetime::MICROS_PER_DAY),
            Value::TimeTz {
                micros: 0,
                offset_seconds: -crate::datetime::MAX_OFFSET,
            },
            Value::Interval(Interval {
                months: i32::MIN,
                days: i32::MAX,
                micros: i64::MIN,
            }),
            Value::Bytea(vec![0, 0xFF, 0]),
            Value::Uuid([0xAB; 16]),
            Value::Bpchar("ab  ".into()),
            Value::Null,
        ]
    }

    #[test]
    fn rows_of_item_31_types_round_trip_and_hostile_bytes_are_damage() {
        let columns = item_31_columns();
        let values = item_31_values();
        let encoded = encode_row(&values);
        assert_eq!(decode_row(&encoded, &columns), Ok(values.clone()));
        for cut in 0..encoded.len() {
            assert!(decode_row(&encoded[..cut], &columns).is_err());
        }
        let mut late = values.clone();
        late[1] = Value::Time(crate::datetime::MICROS_PER_DAY + 1);
        assert!(decode_row(&encode_row(&late), &columns).is_err());
        let mut early = values.clone();
        early[1] = Value::Time(-1);
        assert!(decode_row(&encode_row(&early), &columns).is_err());
        let mut far = values.clone();
        far[2] = Value::TimeTz {
            micros: 0,
            offset_seconds: crate::datetime::MAX_OFFSET + 1,
        };
        assert!(decode_row(&encode_row(&far), &columns).is_err());
        let mut not_utf8 = encode_row(&values);
        let code_at = not_utf8.len() - 5;
        assert_eq!(not_utf8[code_at], b'a');
        not_utf8[code_at] = 0xFF;
        assert!(decode_row(&not_utf8, &columns).is_err());
        let mut random = Seeded(0x0DD_C0DE_0000_0031);
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
    fn keys_of_item_31_types_keep_their_order_and_decode_back() {
        let mut random = Seeded(0x1D1E_5EED_0000_0031);
        let types = [
            SqlType::Time(None),
            SqlType::TimeTz(None),
            SqlType::INTERVAL,
            SqlType::Bytea,
            SqlType::Uuid,
            SqlType::Bpchar(None),
        ];
        let value = |random: &mut Seeded, kind: usize| -> Value {
            let bits = random.next();
            match kind {
                0 => Value::Time((bits % (crate::datetime::MICROS_PER_DAY as u64 + 1)) as i64),
                1 => Value::TimeTz {
                    micros: (bits % 5) as i64 * 3_600_000_000,
                    offset_seconds: ((bits >> 8) % 5) as i32 * 3600 - 7200,
                },
                2 => Value::Interval(match bits % 4 {
                    0 => Interval {
                        months: (bits >> 8) as i32,
                        days: (bits >> 16) as i32,
                        micros: (bits >> 1) as i64,
                    },
                    1 => Interval {
                        months: i32::MIN,
                        days: i32::MIN,
                        micros: i64::MIN,
                    },
                    2 => Interval {
                        months: (bits >> 8) as i32 % 3,
                        days: (bits >> 16) as i32 % 90,
                        micros: 0,
                    },
                    _ => Interval {
                        months: i32::MAX,
                        days: i32::MAX,
                        micros: i64::MAX,
                    },
                }),
                3 => Value::Bytea(
                    (0..bits % 5)
                        .map(|shift| [0u8, 1, 0xFF][((bits >> (shift * 2)) % 3) as usize])
                        .collect(),
                ),
                4 => Value::Uuid([(bits % 3) as u8; 16]),
                _ => {
                    Value::Bpchar(["a", "a  ", "a ", "", "b", " "][(bits % 6) as usize].to_owned())
                }
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
        let mut hostile = Seeded(0xFACE_0000_0031);
        for _ in 0..20_000 {
            let length = (hostile.next() % 40) as usize;
            let bytes: Vec<u8> = (0..length).map(|_| hostile.next() as u8).collect();
            let mut values = Vec::new();
            if let Ok(used) = decode_key_values(&bytes, types, &mut values) {
                assert_eq!(skip_key_values(&bytes, types), Ok(used));
            }
        }
    }

    #[test]
    fn equal_values_of_item_31_types_share_a_key_and_an_equality_key() {
        let pairs = [
            (
                Value::Interval(Interval {
                    months: 1,
                    days: 0,
                    micros: 0,
                }),
                Value::Interval(Interval {
                    months: 0,
                    days: 0,
                    micros: 30 * crate::datetime::MICROS_PER_DAY,
                }),
            ),
            (Value::Bpchar("ab".into()), Value::Bpchar("ab   ".into())),
            (Value::Bpchar("ab".into()), Value::Text("ab".into())),
        ];
        for (left, right) in pairs {
            assert_eq!(compare_values(&left, &right), std::cmp::Ordering::Equal);
            if left.sql_type() == right.sql_type() {
                assert_eq!(key_of(&left), key_of(&right), "{left:?} {right:?}");
            }
            let mut a = Vec::new();
            let mut b = Vec::new();
            crate::types::equality_key(&mut a, &left);
            crate::types::equality_key(&mut b, &right);
            assert_eq!(a, b, "{left:?} {right:?}");
        }
        let mut a = Vec::new();
        let mut b = Vec::new();
        crate::types::equality_key(&mut a, &Value::Bpchar("ab".into()));
        crate::types::equality_key(&mut b, &Value::Text("ab ".into()));
        assert_ne!(a, b);
        assert_ne!(
            compare_values(&Value::Bpchar("ab".into()), &Value::Text("ab ".into())),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn a_prefix_end_is_the_first_key_after_every_key_with_the_prefix() {
        assert_eq!(prefix_end(&[1, 2]), vec![1, 3]);
        assert_eq!(prefix_end(&[1, 0xFF]), vec![2]);
    }

    fn constrained_table() -> TableDefinition {
        let mut table = with_indexes([false, false]);
        table.primary_key_name = Some("t_key".into());
        table.checks = vec![super::CheckConstraint {
            name: "t_id_check".into(),
            dialect: crate::Dialect::MySql,
            expression: "`id` > 0".into(),
        }];
        table.foreign_keys = vec![super::ForeignKey {
            name: "t_name_fkey".into(),
            columns: vec![1, 0],
            parent: "parent".into(),
            parent_columns: vec!["code".into(), "id".into()],
            on_delete: crate::ast::ReferentialAction::Cascade,
            on_update: crate::ast::ReferentialAction::SetDefault,
            deferrable: true,
        }];
        table
    }

    #[test]
    fn constraints_are_written_in_format_version_6_and_read_back() {
        let table = constrained_table();
        let bytes = table.encode();
        assert_eq!(bytes[0], 6);
        assert_eq!(TableDefinition::decode("t", &bytes).unwrap(), table);
        for kept in [
            with_indexes([false, false]),
            two_columns([true, false], vec![0, 1]),
            typed_table(true),
            item_31_table(),
        ] {
            let bytes = kept.encode();
            assert!(bytes[0] < 6);
            assert_eq!(TableDefinition::decode(&kept.name, &bytes).unwrap(), kept);
        }
        for (referenced, owns_sequence) in [(true, false), (false, true), (true, true)] {
            let mut flagged = with_indexes([false, false]);
            flagged.referenced = referenced;
            flagged.owns_sequence = owns_sequence;
            let bytes = flagged.encode();
            assert_eq!(
                bytes[0], 6,
                "a release that reads up to version 5 refuses it by name"
            );
            assert_eq!(TableDefinition::decode("t", &bytes).unwrap(), flagged);
            let flags_at = bytes.len() - 3;
            assert_eq!(
                bytes[flags_at],
                u8::from(referenced) | (u8::from(owns_sequence) << 1)
            );
            let mut unknown = bytes.clone();
            unknown[flags_at] = 4;
            assert!(damaged(&unknown));
        }
        let mut only_checks = with_indexes([false, false]);
        only_checks.checks = constrained_table().checks;
        assert_eq!(
            TableDefinition::decode("t", &only_checks.encode()).unwrap(),
            only_checks
        );
    }

    #[test]
    fn damaged_constraint_definitions_are_refused_without_panicking() {
        let bytes = constrained_table().encode();
        for cut in 0..bytes.len() {
            assert!(damaged(&bytes[..cut]), "cut at {cut}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(damaged(&longer));
        let mut without = with_indexes([false, false]).encode();
        without[0] = 6;
        without.extend_from_slice(&[0, 0, 0]);
        assert!(damaged(&without));
        let mut duplicate = constrained_table();
        duplicate.foreign_keys[0].name = "t_id_check".into();
        assert!(damaged(&duplicate.encode()));
        let mut clashing = constrained_table();
        clashing.checks[0].name = "both".into();
        assert!(damaged(&clashing.encode()));
        let mut out_of_range = constrained_table();
        out_of_range.foreign_keys[0].columns = vec![1, 9];
        assert!(damaged(&out_of_range.encode()));
        let mut empty_name = constrained_table();
        empty_name.checks[0].name.clear();
        assert!(damaged(&empty_name.encode()));
        let mut empty_check = constrained_table();
        empty_check.checks[0].expression = " ".into();
        assert!(damaged(&empty_check.encode()));
        let mut keyless = constrained_table();
        keyless.primary_key.clear();
        assert!(damaged(&keyless.encode()));
        let flags_at = bytes.len() - 1;
        for (at, values) in [
            (flags_at, [2u8, 0xFF]),
            (flags_at - 1, [5, 0xFF]),
            (flags_at - 2, [5, 0x80]),
        ] {
            for value in values {
                let mut changed = bytes.clone();
                changed[at] = value;
                assert!(damaged(&changed), "byte {at} = {value}");
            }
        }
        let mut random = Seeded(0x5EED_C0DE);
        for _ in 0..20_000 {
            let mut changed = bytes.clone();
            let at = (random.next() as usize) % changed.len();
            changed[at] = random.next() as u8;
            let _ = TableDefinition::decode("t", &changed);
        }
    }

    fn sequence() -> super::SequenceDefinition {
        super::SequenceDefinition {
            sql_type: SqlType::Integer,
            start: 5,
            increment: -3,
            min: -100,
            max: 50,
            cache: 20,
            cycle: true,
            last_value: 8,
            is_called: true,
            owned_by: Some(("t".into(), "id".into())),
        }
    }

    #[test]
    fn sequences_are_read_back_and_damaged_ones_refused() {
        let bytes = sequence().encode();
        assert_eq!(
            super::SequenceDefinition::decode(&bytes).unwrap(),
            sequence()
        );
        let mut unowned = sequence();
        unowned.owned_by = None;
        unowned.is_called = false;
        assert_eq!(
            super::SequenceDefinition::decode(&unowned.encode()).unwrap(),
            unowned
        );
        for cut in 0..bytes.len() {
            assert!(super::SequenceDefinition::decode(&bytes[..cut]).is_err());
        }
        let mut newer = bytes.clone();
        newer[0] = 2;
        assert_eq!(
            super::SequenceDefinition::decode(&newer)
                .unwrap_err()
                .state()
                .code(),
            "0A000"
        );
        let invalid = |change: fn(&mut super::SequenceDefinition)| {
            let mut changed = sequence();
            change(&mut changed);
            super::SequenceDefinition::decode(&changed.encode()).is_err()
        };
        assert!(invalid(|sequence| sequence.increment = 0));
        assert!(invalid(|sequence| sequence.cache = 0));
        assert!(invalid(|sequence| sequence.min = sequence.max));
        assert!(invalid(|sequence| sequence.start = 51));
        assert!(invalid(|sequence| sequence.max = i64::from(i32::MAX) + 1));
        assert!(invalid(|sequence| sequence.sql_type = SqlType::Text));
        assert!(invalid(
            |sequence| sequence.owned_by = Some((String::new(), "id".into()))
        ));
        let flags_at = 1 + 1 + 6 * 8;
        let mut flags = bytes.clone();
        flags[flags_at] = 4;
        assert!(super::SequenceDefinition::decode(&flags).is_err());
        let mut random = Seeded(0xA11_5EC);
        for _ in 0..20_000 {
            let mut changed = bytes.clone();
            let at = (random.next() as usize) % changed.len();
            changed[at] = random.next() as u8;
            let _ = super::SequenceDefinition::decode(&changed);
        }
    }
}
