use crate::ast::Name;
use crate::encoding::{Column, TableDefinition, encode_row, rows_prefix, table_key};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::memory::WorkMemory;
use crate::session::OutputColumn;
use crate::store::{EntryVisitor, KeyValueRead, ScanDirection};
use crate::types::{Conversion, SqlType, Value, convert};
use std::cell::Cell;
use std::collections::BTreeMap;

pub(crate) const TEMP_TABLE_MARKER: char = '\u{1}';
const MOST_TEMP_TABLES: u32 = 1 << 24;
const ENTRY_OVERHEAD: usize = 64;

#[derive(Default)]
pub(crate) struct TempTableNames {
    next: Cell<u32>,
}

impl TempTableNames {
    fn next(&self) -> SqlResult<(u32, String)> {
        let number = self.next.get();
        if number >= MOST_TEMP_TABLES {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("a statement can build at most {MOST_TEMP_TABLES} intermediate results"),
            ));
        }
        self.next.set(number + 1);
        Ok((
            u32::MAX - number,
            format!("{TEMP_TABLE_MARKER}intermediate {number}"),
        ))
    }
}

pub(crate) fn is_temp_table(name: &str) -> bool {
    name.starts_with(TEMP_TABLE_MARKER)
}

pub(crate) struct TempTables {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
    memory: WorkMemory,
}

impl TempTables {
    pub(crate) fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            memory: WorkMemory::new(),
        }
    }

    pub(crate) fn add(
        &mut self,
        names: &TempTableNames,
        columns: &[OutputColumn],
        renamed: &[Name],
        label: &str,
        rows: Vec<Vec<Value>>,
    ) -> SqlResult<String> {
        if renamed.len() > columns.len() {
            let position = renamed.get(columns.len()).map_or(0, |name| name.position);
            return Err(SqlError::new(
                SqlState::INVALID_COLUMN_REFERENCE,
                format!(
                    "table \"{label}\" has {} columns available but {} columns specified",
                    columns.len(),
                    renamed.len()
                ),
            )
            .at(position));
        }
        let (id, name) = names.next()?;
        let mut definition = TableDefinition {
            id,
            name: name.clone(),
            columns: Vec::with_capacity(columns.len()),
            primary_key: Vec::new(),
            indexes: Vec::new(),
        };
        for (index, column) in columns.iter().enumerate() {
            let sql_type = column.sql_type.unwrap_or_else(|| value_type(&rows, index));
            definition.columns.push(Column {
                name: renamed
                    .get(index)
                    .map_or_else(|| column.name.clone(), |name| name.text.clone()),
                sql_type,
                not_null: false,
                default: None,
                auto_increment: false,
            });
        }
        let encoded = definition.encode();
        self.memory.charge(encoded.len() + ENTRY_OVERHEAD)?;
        self.entries.insert(table_key(&name), encoded);
        let prefix = rows_prefix(id);
        for (number, mut row) in rows.into_iter().enumerate() {
            row.resize(definition.columns.len(), Value::Null);
            for (value, column) in row.iter_mut().zip(&definition.columns) {
                if !value.is_null() && value.sql_type() != Some(column.sql_type) {
                    let original = std::mem::replace(value, Value::Null);
                    *value = stored_as(original, column.sql_type)?;
                }
            }
            let mut key = prefix.clone();
            key.extend_from_slice(&(number as u64).to_be_bytes());
            let bytes = encode_row(&row);
            self.memory
                .charge(key.len() + bytes.len() + ENTRY_OVERHEAD)?;
            self.entries.insert(key, bytes);
        }
        Ok(name)
    }
}

fn stored_as(value: Value, sql_type: SqlType) -> SqlResult<Value> {
    match (&value, sql_type) {
        (Value::Text(_), SqlType::Varchar(_) | SqlType::Text) => Ok(value),
        _ => convert(value, sql_type, Conversion::Explicit),
    }
}

fn value_type(rows: &[Vec<Value>], index: usize) -> SqlType {
    let mut found: Option<SqlType> = None;
    for row in rows {
        let Some(sql_type) = row.get(index).and_then(Value::sql_type) else {
            continue;
        };
        match found {
            None => found = Some(sql_type),
            Some(known) if known == sql_type => {}
            Some(_) => return SqlType::Text,
        }
    }
    found.unwrap_or(SqlType::Text)
}

pub(crate) struct Overlay<'a> {
    base: &'a dyn KeyValueRead,
    temp: &'a TempTables,
}

impl<'a> Overlay<'a> {
    pub(crate) fn new(base: &'a dyn KeyValueRead, temp: &'a TempTables) -> Self {
        Self { base, temp }
    }
}

impl KeyValueRead for Overlay<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        match self.temp.entries.get(key) {
            Some(value) => Ok(Some(value.clone())),
            None => self.base.get(key),
        }
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        match self.temp.entries.get(key) {
            Some(value) => read(value).map(|()| true),
            None => self.base.get_with(key, read),
        }
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        if start >= end {
            return Ok(());
        }
        let range = self.temp.entries.range(start.to_vec()..end.to_vec());
        let mut own: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match direction {
            ScanDirection::Forward => Box::new(range),
            ScanDirection::Backward => Box::new(range.rev()),
        };
        let mut pending = own.next();
        if pending.is_none() {
            return self.base.scan(start, end, direction, visit);
        }
        let mut going = true;
        self.base.scan(start, end, direction, &mut |key, value| {
            while let Some((own_key, own_value)) = pending {
                let first = match direction {
                    ScanDirection::Forward => own_key.as_slice() < key,
                    ScanDirection::Backward => own_key.as_slice() > key,
                };
                if !first {
                    break;
                }
                if !visit(own_key, own_value)? {
                    going = false;
                    return Ok(false);
                }
                pending = own.next();
            }
            going = visit(key, value)?;
            Ok(going)
        })?;
        while going && let Some((key, value)) = pending {
            going = visit(key, value)?;
            pending = own.next();
        }
        Ok(())
    }

    fn committed_version(&self) -> Option<u64> {
        if self.temp.entries.is_empty() {
            self.base.committed_version()
        } else {
            None
        }
    }
}
