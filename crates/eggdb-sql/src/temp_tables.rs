use crate::ast::Name;
use crate::dialect::Dialect;
use crate::encoding::{Column, TableDefinition, encode_row, rows_prefix};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::memory::WorkMemory;
use crate::session::OutputColumn;
use crate::store::{EntryVisitor, KeyValueRead, ScanDirection};
use crate::types::{Conversion, SqlType, Value, convert};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub(crate) const TEMP_TABLE_MARKER: char = '\u{1}';
const MOST_TEMP_TABLES: u32 = 1 << 20;
const ID_SPACE: u64 = 1 << 24;
const ENTRY_OVERHEAD: usize = 64;

thread_local! {
    static NEXT_NUMBER: Cell<u64> = const { Cell::new(0) };
    static DEFINITIONS: RefCell<HashMap<String, Arc<TableDefinition>>> =
        RefCell::new(HashMap::new());
}

#[derive(Default)]
pub(crate) struct TempTableNames {
    made: Cell<u32>,
}

impl TempTableNames {
    fn next(&self) -> SqlResult<(u32, String)> {
        let made = self.made.get();
        if made >= MOST_TEMP_TABLES {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("a statement can build at most {MOST_TEMP_TABLES} intermediate results"),
            ));
        }
        self.made.set(made + 1);
        let number = NEXT_NUMBER.with(|next| {
            let number = next.get();
            next.set(number.wrapping_add(1));
            number
        });
        let id = u32::MAX - u32::try_from(number % ID_SPACE).unwrap_or(0);
        Ok((id, format!("{TEMP_TABLE_MARKER}intermediate {number}")))
    }
}

pub(crate) fn is_temp_table(name: &str) -> bool {
    name.starts_with(TEMP_TABLE_MARKER)
}

pub(crate) fn temp_definition(name: &str) -> Option<Arc<TableDefinition>> {
    DEFINITIONS.with(|definitions| definitions.try_borrow().ok()?.get(name).cloned())
}

pub(crate) fn display_name(name: &str) -> &str {
    name.split(TEMP_TABLE_MARKER).next().unwrap_or(name)
}

pub(crate) fn is_repeated(table: &TableDefinition, name: &str) -> bool {
    is_temp_table(&table.name)
        && table.columns.iter().any(|column| {
            column
                .name
                .strip_prefix(name)
                .is_some_and(|rest| rest.starts_with(TEMP_TABLE_MARKER))
        })
}

pub(crate) fn ambiguous(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::AMBIGUOUS_COLUMN,
        format!("column reference \"{name}\" is ambiguous"),
    )
    .at(position)
}

pub(crate) struct TempTables {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
    registered: Vec<String>,
    memory: WorkMemory,
}

impl Drop for TempTables {
    fn drop(&mut self) {
        DEFINITIONS.with(|definitions| {
            if let Ok(mut definitions) = definitions.try_borrow_mut() {
                for name in &self.registered {
                    definitions.remove(name);
                }
            }
        });
    }
}

fn unique_names(names: Vec<String>, dialect: Dialect, position: usize) -> SqlResult<Vec<String>> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut unique = Vec::with_capacity(names.len());
    for (index, name) in names.into_iter().enumerate() {
        let count = seen.entry(name.clone()).or_insert(0);
        *count += 1;
        if *count == 1 {
            unique.push(name);
            continue;
        }
        match dialect {
            Dialect::MySql => {
                return Err(SqlError::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{name}\" specified more than once"),
                )
                .at(position));
            }
            Dialect::Oracle => return Err(ambiguous(&name, position)),
            Dialect::PostgreSql | Dialect::Sqlite => {
                unique.push(format!("{name}{TEMP_TABLE_MARKER}{index}"));
            }
        }
    }
    Ok(unique)
}

pub(crate) struct Intermediate<'a> {
    pub(crate) columns: &'a [OutputColumn],
    pub(crate) renamed: &'a [Name],
    pub(crate) label: &'a str,
    pub(crate) dialect: Dialect,
    pub(crate) position: usize,
}

impl TempTables {
    pub(crate) fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            registered: Vec::new(),
            memory: WorkMemory::new(),
        }
    }

    pub(crate) fn add(
        &mut self,
        names: &TempTableNames,
        shape: Intermediate<'_>,
        rows: Vec<Vec<Value>>,
    ) -> SqlResult<String> {
        let Intermediate {
            columns,
            renamed,
            label,
            dialect,
            position,
        } = shape;
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
        let shown: Vec<String> = columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                renamed
                    .get(index)
                    .map_or_else(|| column.name.clone(), |name| name.text.clone())
            })
            .collect();
        let unique = unique_names(shown, dialect, position)?;
        let mut definition = TableDefinition {
            id,
            name: name.clone(),
            columns: Vec::with_capacity(columns.len()),
            primary_key: Vec::new(),
            indexes: Vec::new(),
        };
        for (index, (column, column_name)) in columns.iter().zip(unique).enumerate() {
            let sql_type = column.sql_type.unwrap_or_else(|| value_type(&rows, index));
            definition.columns.push(Column {
                name: column_name,
                sql_type,
                not_null: false,
                default: None,
                auto_increment: false,
            });
        }
        let column_bytes: usize = definition
            .columns
            .iter()
            .map(|column| column.name.len() + ENTRY_OVERHEAD)
            .sum();
        self.memory.charge(column_bytes + ENTRY_OVERHEAD)?;
        let prefix = rows_prefix(id);
        let width = definition.columns.len();
        let mut stored = Vec::with_capacity(rows.len());
        for (number, mut row) in rows.into_iter().enumerate() {
            row.resize(width, Value::Null);
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
            stored.push((key, bytes));
        }
        self.entries.extend(stored);
        let registered = DEFINITIONS.with(|definitions| {
            definitions
                .try_borrow_mut()
                .map(|mut definitions| {
                    definitions.insert(name.clone(), Arc::new(definition));
                })
                .is_ok()
        });
        if !registered {
            return Err(SqlError::new(
                SqlState::INTERNAL_ERROR,
                "an intermediate result could not be registered",
            ));
        }
        self.registered.push(name.clone());
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
        if self.temp.registered.is_empty() {
            self.base.committed_version()
        } else {
            None
        }
    }
}
