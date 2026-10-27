use crate::ast::{CreateIndex, DropIndex, IndexSpec, Name};
use crate::dialect::Dialect;
use crate::encoding::{
    IndexDefinition, MAX_INDEXES, NAMESPACE, TableDefinition, corrupt, decode_key_values,
    decode_row, encode_key_value, index_prefix, prefix_end, rows_prefix, skip_key_values,
    table_indexes_prefix, table_key,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::memory::WorkMemory;
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::{SqlType, Value};

pub(crate) const ROW_KEY_PREFIX_LENGTH: usize = NAMESPACE.len() + 1 + 4;
const ENTRY_KEY_PREFIX_LENGTH: usize = ROW_KEY_PREFIX_LENGTH + 4;
const BACKFILL_BATCH: usize = 1024;
const DELETE_BATCH: usize = 1024;

pub(crate) fn column_types<'a>(
    table: &'a TableDefinition,
    index: &'a IndexDefinition,
) -> impl Iterator<Item = SqlType> + Clone + 'a {
    index
        .columns
        .iter()
        .map(|&column| table.columns[column].sql_type)
}

pub(crate) fn entry_key(
    table: &TableDefinition,
    index: &IndexDefinition,
    row: &[Value],
    row_key: &[u8],
) -> Vec<u8> {
    let mut key = index_prefix(table.id, index.id);
    for &column in &index.columns {
        encode_key_value(&mut key, &row[column]);
    }
    key.extend_from_slice(row_key.get(ROW_KEY_PREFIX_LENGTH..).unwrap_or(&[]));
    key
}

pub(crate) fn row_key_of_entry(
    table: &TableDefinition,
    index: &IndexDefinition,
    entry: &[u8],
) -> SqlResult<Vec<u8>> {
    let body = entry
        .get(ENTRY_KEY_PREFIX_LENGTH..)
        .ok_or_else(|| corrupt("index entry"))?;
    let used = skip_key_values(body, column_types(table, index))?;
    let suffix = &body[used..];
    if suffix.is_empty() {
        return Err(corrupt("index entry"));
    }
    let mut key = rows_prefix(table.id);
    key.extend_from_slice(suffix);
    Ok(key)
}

pub(crate) fn unique_conflict(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    index: &IndexDefinition,
    row: &[Value],
    own_row_key: &[u8],
) -> SqlResult<Option<Vec<u8>>> {
    if !index.unique || index.columns.iter().any(|&column| row[column].is_null()) {
        return Ok(None);
    }
    let mut start = index_prefix(table.id, index.id);
    for &column in &index.columns {
        encode_key_value(&mut start, &row[column]);
    }
    let end = prefix_end(&start);
    let mut found = None;
    let mut failure = None;
    reader.scan(
        &start,
        &end,
        ScanDirection::Forward,
        &mut |entry, _| match row_key_of_entry(table, index, entry) {
            Ok(key) if key == own_row_key => Ok(true),
            Ok(key) => {
                found = Some(key);
                Ok(false)
            }
            Err(error) => {
                failure = Some(error);
                Ok(false)
            }
        },
    )?;
    match failure {
        Some(error) => Err(error),
        None => Ok(found),
    }
}

pub(crate) fn first_unique_conflict(
    reader: &dyn KeyValueRead,
    table: &TableDefinition,
    row: &[Value],
    own_row_key: &[u8],
) -> SqlResult<Option<(usize, Vec<u8>)>> {
    for (position, index) in table.indexes.iter().enumerate() {
        if let Some(key) = unique_conflict(reader, table, index, row, own_row_key)? {
            return Ok(Some((position, key)));
        }
    }
    Ok(None)
}

pub(crate) fn put_entries(
    writer: &mut dyn KeyValueWrite,
    table: &TableDefinition,
    row: &[Value],
    row_key: &[u8],
) -> SqlResult<()> {
    for index in &table.indexes {
        writer.put(&entry_key(table, index, row, row_key), &[])?;
    }
    Ok(())
}

pub(crate) fn delete_entries(
    writer: &mut dyn KeyValueWrite,
    table: &TableDefinition,
    row: &[Value],
    row_key: &[u8],
) -> SqlResult<()> {
    for index in &table.indexes {
        writer.delete(&entry_key(table, index, row, row_key))?;
    }
    Ok(())
}

fn key_text(table: &TableDefinition, index: &IndexDefinition, row: &[Value]) -> (String, String) {
    let names: Vec<&str> = index
        .columns
        .iter()
        .map(|&column| table.columns[column].name.as_str())
        .collect();
    let values: Vec<String> = index
        .columns
        .iter()
        .map(|&column| row[column].to_text().unwrap_or_else(|| "null".into()))
        .collect();
    (names.join(", "), values.join(", "))
}

pub(crate) fn unique_violation(
    table: &TableDefinition,
    index: &IndexDefinition,
    row: &[Value],
) -> SqlError {
    let (names, values) = key_text(table, index, row);
    SqlError::new(
        SqlState::UNIQUE_VIOLATION,
        format!(
            "duplicate key value violates unique constraint \"{}\"",
            index.name
        ),
    )
    .with_detail(format!("Key ({names})=({values}) already exists."))
    .with_table(&table.name)
}

fn duplicate_in_backfill(
    table: &TableDefinition,
    index: &IndexDefinition,
    row: &[Value],
) -> SqlError {
    let (names, values) = key_text(table, index, row);
    SqlError::new(
        SqlState::UNIQUE_VIOLATION,
        format!("could not create unique index \"{}\"", index.name),
    )
    .with_detail(format!("Key ({names})=({values}) is duplicated."))
    .with_table(&table.name)
}

pub(crate) fn every_table(reader: &dyn KeyValueRead) -> SqlResult<Vec<TableDefinition>> {
    let prefix = table_key("");
    let end = prefix_end(&prefix);
    let mut tables = Vec::new();
    let mut failure = None;
    reader.scan(&prefix, &end, ScanDirection::Forward, &mut |key, value| {
        let name = String::from_utf8_lossy(&key[prefix.len()..]).into_owned();
        match TableDefinition::decode(&name, value) {
            Ok(table) => {
                tables.push(table);
                Ok(true)
            }
            Err(error) => {
                failure = Some(error);
                Ok(false)
            }
        }
    })?;
    match failure {
        Some(error) => Err(error),
        None => Ok(tables),
    }
}

fn name_taken(
    tables: &[TableDefinition],
    table: &TableDefinition,
    name: &str,
    dialect: Dialect,
) -> bool {
    if dialect == Dialect::MySql {
        return table.index_named(name).is_some();
    }
    let relation = matches!(dialect, Dialect::PostgreSql | Dialect::Sqlite);
    table.index_named(name).is_some()
        || tables
            .iter()
            .any(|other| other.index_named(name).is_some() || relation && other.name == name)
}

fn generated_name(
    tables: &[TableDefinition],
    table: &TableDefinition,
    spec: &IndexSpec,
    dialect: Dialect,
    constraint: bool,
) -> String {
    let columns: Vec<&str> = spec.columns.iter().map(|name| name.text.as_str()).collect();
    let stem = match dialect {
        Dialect::MySql => columns.first().copied().unwrap_or("key").to_owned(),
        Dialect::Sqlite => format!("sqlite_autoindex_{}", table.name),
        Dialect::Oracle => "sys_c".to_owned(),
        Dialect::PostgreSql => format!(
            "{}_{}_{}",
            table.name,
            columns.join("_"),
            if constraint { "key" } else { "idx" }
        ),
    };
    let candidate = |number: usize| match dialect {
        Dialect::MySql if number == 1 => stem.clone(),
        Dialect::MySql => format!("{stem}_{number}"),
        Dialect::Sqlite => format!("{stem}_{number}"),
        Dialect::Oracle => format!("{stem}{number:07}"),
        Dialect::PostgreSql if number == 1 => stem.clone(),
        Dialect::PostgreSql => format!("{stem}{}", number - 1),
    };
    let mut number = match dialect {
        Dialect::Oracle => {
            1 + table.indexes.len()
                + tables
                    .iter()
                    .map(|other| other.indexes.len())
                    .sum::<usize>()
        }
        Dialect::Sqlite => 1 + table.indexes.len(),
        Dialect::MySql | Dialect::PostgreSql => 1,
    };
    loop {
        let name = candidate(number);
        if !name_taken(tables, table, &name, dialect) {
            return name;
        }
        number += 1;
    }
}

fn duplicate_name(name: &str, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("Duplicate key name '{name}'"),
        )
        .with_vendor_code(1061),
        Dialect::Sqlite => SqlError::new(
            SqlState::DUPLICATE_TABLE,
            format!("index {name} already exists"),
        )
        .with_sqlite_code(1),
        Dialect::PostgreSql | Dialect::Oracle => SqlError::new(
            SqlState::DUPLICATE_TABLE,
            format!("relation \"{name}\" already exists"),
        ),
    }
}

fn missing_column(name: &Name, dialect: Dialect) -> SqlError {
    let error = match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("Key column '{}' doesn't exist in table", name.text),
        )
        .with_vendor_code(1072),
        _ => SqlError::new(
            SqlState::UNDEFINED_COLUMN,
            format!("column \"{}\" does not exist", name.text),
        ),
    };
    error.at(name.position)
}

fn too_many_indexes(table: &TableDefinition) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!(
            "table \"{}\" can have at most {MAX_INDEXES} indexes",
            table.name
        ),
    )
}

fn next_index_id(table: &TableDefinition) -> SqlResult<u32> {
    table
        .indexes
        .iter()
        .map(|index| index.id)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| too_many_indexes(table))
}

pub(crate) fn define_index(
    tables: &[TableDefinition],
    table: &mut TableDefinition,
    spec: &IndexSpec,
    dialect: Dialect,
    constraint: bool,
) -> SqlResult<usize> {
    if table.indexes.len() >= MAX_INDEXES {
        return Err(too_many_indexes(table));
    }
    let mut columns = Vec::with_capacity(spec.columns.len());
    for name in &spec.columns {
        let column = table
            .column_index(&name.text)
            .ok_or_else(|| missing_column(name, dialect))?;
        if columns.contains(&column) {
            return Err(SqlError::new(
                SqlState::DUPLICATE_COLUMN,
                format!("column \"{}\" appears twice in index definition", name.text),
            )
            .at(name.position));
        }
        columns.push(column);
    }
    let given = match (&spec.name, dialect) {
        (Some(_), Dialect::Sqlite) if constraint => None,
        (Some(name), _) => Some(name.text.clone()),
        (None, _) => None,
    };
    let name = match given {
        Some(name) => {
            if name_taken(tables, table, &name, dialect) {
                return Err(duplicate_name(&name, dialect).at(spec
                    .name
                    .as_ref()
                    .map_or(spec.position, |name| name.position)));
            }
            name
        }
        None => generated_name(tables, table, spec, dialect, constraint),
    };
    let id = next_index_id(table)?;
    table.indexes.push(IndexDefinition {
        id,
        name,
        unique: spec.unique,
        columns,
    });
    Ok(table.indexes.len() - 1)
}

fn backfill(
    writer: &mut dyn KeyValueWrite,
    table: &TableDefinition,
    index: &IndexDefinition,
    memory: &mut WorkMemory,
) -> SqlResult<()> {
    let prefix = rows_prefix(table.id);
    let end = prefix_end(&prefix);
    let mut start = prefix;
    loop {
        let mut batch: Vec<(Vec<u8>, Vec<Value>)> = Vec::new();
        let mut charged = 0;
        writer.scan(&start, &end, ScanDirection::Forward, &mut |key, bytes| {
            let row = decode_row(bytes, &table.columns)?;
            let size = WorkMemory::row_bytes(&row) + key.len();
            memory.charge(size)?;
            charged += size;
            batch.push((key.to_vec(), row));
            Ok(batch.len() < BACKFILL_BATCH)
        })?;
        let Some((last, _)) = batch.last() else {
            return Ok(());
        };
        start = last.clone();
        start.push(0);
        let full = batch.len() == BACKFILL_BATCH;
        for (key, row) in &batch {
            if unique_conflict(writer, table, index, row, key)?.is_some() {
                return Err(duplicate_in_backfill(table, index, row));
            }
            writer.put(&entry_key(table, index, row, key), &[])?;
        }
        memory.release(charged);
        if !full {
            return Ok(());
        }
    }
}

pub(crate) fn create_index(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    create: &CreateIndex,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let table = tables.require(writer, &create.table)?;
    let mut definition = (*table).clone();
    let others: Vec<TableDefinition> = every_table(writer)?
        .into_iter()
        .filter(|other| other.name != table.name)
        .collect();
    if create.if_not_exists
        && let Some(name) = &create.index.name
        && name_taken(&others, &definition, &name.text, dialect)
    {
        let notice = match dialect {
            Dialect::PostgreSql => vec![format!(
                "NOTICE:  relation \"{}\" already exists, skipping",
                name.text
            )],
            _ => Vec::new(),
        };
        let mut result = QueryResult::command("CREATE INDEX");
        result.notices = notice;
        return Ok(result);
    }
    let position = define_index(&others, &mut definition, &create.index, dialect, false)?;
    let mut memory = WorkMemory::new();
    backfill(
        writer,
        &definition,
        &definition.indexes[position],
        &mut memory,
    )?;
    writer.put(&table_key(&definition.name), &definition.encode())?;
    Ok(QueryResult::command("CREATE INDEX"))
}

fn index_missing(name: &Name, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_SYNTAX_OR_ACCESS,
            format!("Can't DROP '{}'; check that column/key exists", name.text),
        )
        .with_vendor_code(1091),
        Dialect::Sqlite => SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("no such index: {}", name.text),
        )
        .with_sqlite_code(1),
        Dialect::Oracle => SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            "specified index does not exist".to_owned(),
        )
        .with_oracle_code(1418),
        Dialect::PostgreSql => SqlError::new(
            SqlState::UNDEFINED_OBJECT,
            format!("index \"{}\" does not exist", name.text),
        ),
    }
    .at(name.position)
}

pub(crate) fn delete_index_entries(
    writer: &mut dyn KeyValueWrite,
    start: &[u8],
    end: &[u8],
) -> SqlResult<()> {
    loop {
        let mut keys = Vec::new();
        writer.scan(start, end, ScanDirection::Forward, &mut |key, _| {
            keys.push(key.to_vec());
            Ok(keys.len() < DELETE_BATCH)
        })?;
        if keys.is_empty() {
            return Ok(());
        }
        for key in &keys {
            writer.delete(key)?;
        }
    }
}

pub(crate) fn delete_every_index_entry(
    writer: &mut dyn KeyValueWrite,
    table: &TableDefinition,
) -> SqlResult<()> {
    let prefix = table_indexes_prefix(table.id);
    delete_index_entries(writer, &prefix, &prefix_end(&prefix))
}

pub(crate) fn drop_index(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    drop: &DropIndex,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let mut result = QueryResult::command("DROP INDEX");
    for name in &drop.names {
        let holder = match &drop.table {
            Some(table) => {
                let definition = tables.require(writer, table)?;
                definition
                    .index_named(&name.text)
                    .is_some()
                    .then(|| (*definition).clone())
            }
            None => {
                let mut holders: Vec<TableDefinition> = every_table(writer)?
                    .into_iter()
                    .filter(|table| table.index_named(&name.text).is_some())
                    .collect();
                if holders.len() > 1 {
                    return Err(SqlError::not_supported(format!(
                        "index name \"{}\" is used by more than one table: drop it with MySQL's DROP INDEX {} ON table",
                        name.text, name.text
                    ))
                    .at(name.position));
                }
                holders.pop()
            }
        };
        let Some(mut definition) = holder else {
            if drop.if_exists {
                if dialect == Dialect::PostgreSql {
                    result = result.with_notice(format!(
                        "NOTICE:  index \"{}\" does not exist, skipping",
                        name.text
                    ));
                }
                continue;
            }
            return Err(index_missing(name, dialect));
        };
        let Some(position) = definition
            .indexes
            .iter()
            .position(|index| index.name == name.text)
        else {
            return Err(index_missing(name, dialect));
        };
        let index = definition.indexes.remove(position);
        let prefix = index_prefix(definition.id, index.id);
        delete_index_entries(writer, &prefix, &prefix_end(&prefix))?;
        writer.put(&table_key(&definition.name), &definition.encode())?;
    }
    Ok(result)
}

/// What [`check_indexes`] found: every table and index it read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexCheck {
    /// The SQL tables read.
    pub tables: u64,
    /// The secondary indexes checked against their tables' rows.
    pub indexes: u64,
    /// The index entries read.
    pub entries: u64,
}

fn index_damaged(table: &TableDefinition, index: &IndexDefinition, what: &str) -> SqlError {
    SqlError::new(
        SqlState::DATA_CORRUPTED,
        format!(
            "index \"{}\" of table \"{}\" {what}",
            index.name, table.name
        ),
    )
    .with_hint(
        "Drop the index and create it again: CREATE INDEX rebuilds it from the table's rows.",
    )
}

pub(crate) fn check_indexes(reader: &dyn KeyValueRead) -> SqlResult<IndexCheck> {
    let mut report = IndexCheck::default();
    for table in every_table(reader)? {
        report.tables += 1;
        if table.indexes.is_empty() {
            continue;
        }
        let prefix = rows_prefix(table.id);
        let mut rows = 0u64;
        let mut failure = None;
        reader.scan(
            &prefix,
            &prefix_end(&prefix),
            ScanDirection::Forward,
            &mut |key, bytes| {
                rows += 1;
                let row = decode_row(bytes, &table.columns)?;
                for index in &table.indexes {
                    if reader.get(&entry_key(&table, index, &row, key))?.is_none() {
                        failure = Some(index_damaged(
                            &table,
                            index,
                            "is missing the entry of a row",
                        ));
                        return Ok(false);
                    }
                }
                Ok(true)
            },
        )?;
        if let Some(error) = failure {
            return Err(error);
        }
        for index in &table.indexes {
            report.indexes += 1;
            let prefix = index_prefix(table.id, index.id);
            let mut entries = 0u64;
            let mut previous: Option<Vec<u8>> = None;
            let mut failure = None;
            reader.scan(
                &prefix,
                &prefix_end(&prefix),
                ScanDirection::Forward,
                &mut |entry, value| {
                    entries += 1;
                    let mut values = Vec::with_capacity(index.columns.len());
                    let body = entry.get(prefix.len()..).unwrap_or(&[]);
                    let used = decode_key_values(body, column_types(&table, index), &mut values)?;
                    if !value.is_empty() || used == body.len() {
                        failure = Some(index_damaged(&table, index, "has a damaged entry"));
                        return Ok(false);
                    }
                    let encoded = &body[..used];
                    let duplicate = index.unique
                        && values.iter().all(|value| !value.is_null())
                        && previous.as_deref() == Some(encoded);
                    if duplicate {
                        failure = Some(index_damaged(&table, index, "holds a value twice"));
                        return Ok(false);
                    }
                    previous = Some(encoded.to_vec());
                    Ok(true)
                },
            )?;
            if let Some(error) = failure {
                return Err(error);
            }
            if entries != rows {
                return Err(index_damaged(
                    &table,
                    index,
                    "has entries for rows that do not exist",
                ));
            }
            report.entries += entries;
        }
    }
    Ok(report)
}
