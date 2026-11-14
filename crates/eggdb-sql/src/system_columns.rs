use crate::OutputColumn;
use crate::ast::{Expr, ExprKind, OrderItem, Select};
use crate::encoding::{TableDefinition, decode_row, encode_row, prefix_end, rows_prefix};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{Relation, TableCache};
use crate::memory::WorkMemory;
use crate::pg_catalog::CatalogType;
use crate::set_returning::GeneratedRows;
use crate::store::{KeyValueRead, ScanDirection};
use crate::types::{SqlType, Value};

const OID: SqlType = SqlType::Catalog(CatalogType::Oid);
const TID: SqlType = SqlType::Catalog(CatalogType::Tid);
const XID: SqlType = SqlType::Catalog(CatalogType::Xid);
const CID: SqlType = SqlType::Catalog(CatalogType::Cid);

const COLUMNS: [(&str, &str, SqlType); 6] = [
    ("tableoid", crate::temp_tables::TABLE_OID_COLUMN, OID),
    ("ctid", "\u{2}ctid", TID),
    ("xmin", "\u{2}xmin", XID),
    ("cmin", "\u{2}cmin", CID),
    ("xmax", "\u{2}xmax", XID),
    ("cmax", "\u{2}cmax", CID),
];

const MAX_HEAP_TUPLES_PER_PAGE: u64 = 291;
const FIRST_NORMAL_TRANSACTION_ID: u32 = 3;

pub(crate) fn hidden_name(name: &str) -> Option<&'static str> {
    COLUMNS
        .iter()
        .find(|(written, _, _)| *written == name)
        .map(|(_, hidden, _)| *hidden)
}

pub(crate) fn named_in(select: &Select, query_order: &[OrderItem]) -> bool {
    crate::subquery::select_expressions(select)
        .chain(query_order.iter().map(|item| &item.expr))
        .any(names_a_system_column)
}

fn names_a_system_column(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Column { name, .. } => hidden_name(name).is_some(),
        _ => crate::eval::children(expr)
            .into_iter()
            .any(names_a_system_column),
    }
}

pub(crate) fn wanted_for(table: &TableDefinition) -> bool {
    !crate::temp_tables::is_temp_table(&table.name)
        && COLUMNS.iter().all(|(written, hidden, _)| {
            table
                .columns
                .iter()
                .all(|column| column.name != *written && column.name != *hidden)
        })
}

pub(crate) fn generated_rows(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &str,
    columns_only: bool,
) -> SqlResult<GeneratedRows> {
    let Some(Relation::Table(table)) = tables.relation(reader, name)? else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_TABLE,
            format!(
                "relation \"{}\" does not exist",
                crate::schemas::display(name)
            ),
        ));
    };
    let mut columns: Vec<OutputColumn> = table
        .columns
        .iter()
        .map(|column| OutputColumn {
            name: column.name.clone(),
            sql_type: Some(column.sql_type),
        })
        .collect();
    columns.extend(COLUMNS.iter().map(|(_, hidden, sql_type)| OutputColumn {
        name: (*hidden).to_owned(),
        sql_type: Some(*sql_type),
    }));
    if columns_only {
        return Ok(GeneratedRows::empty(columns));
    }
    let table_oid = crate::pg_catalog::snapshot_for(reader)?
        .table_oid(&table.name)
        .unwrap_or(0);
    let mut charge = WorkMemory::new();
    let mut rows = Vec::new();
    let prefix = rows_prefix(table.id);
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, bytes| {
            crate::cancel::check()?;
            let mut row = decode_row(bytes, &table.columns)?;
            let version = row_version(key, &row);
            let tuple = tuple_id(&table, &prefix, key);
            row.extend([
                Value::Oid(table_oid),
                Value::Text(tuple),
                Value::Oid(version),
                Value::Oid(0),
                Value::Oid(0),
                Value::Oid(0),
            ]);
            charge.charge_row(&row)?;
            rows.push(row);
            Ok(true)
        },
    )?;
    Ok(GeneratedRows {
        columns,
        rows,
        charge,
    })
}

fn tuple_id(table: &TableDefinition, prefix: &[u8], key: &[u8]) -> String {
    let suffix = key.get(prefix.len()..).unwrap_or_default();
    let hidden_row_id = <[u8; 8]>::try_from(suffix)
        .ok()
        .filter(|_| table.primary_key.is_empty())
        .map(u64::from_be_bytes);
    let number = match hidden_row_id {
        Some(row_id) => row_id.saturating_sub(1),
        None => fnv64(suffix) % (u64::from(u32::MAX) * MAX_HEAP_TUPLES_PER_PAGE),
    };
    let block = number / MAX_HEAP_TUPLES_PER_PAGE;
    let offset = number % MAX_HEAP_TUPLES_PER_PAGE + 1;
    format!("({block},{offset})")
}

fn row_version(key: &[u8], row: &[Value]) -> u32 {
    let mut bytes = key.to_vec();
    bytes.extend_from_slice(&encode_row(row));
    let folded = fnv64(&bytes);
    let version = (folded ^ (folded >> 32)) as u32;
    version.max(FIRST_NORMAL_TRANSACTION_ID)
}

fn fnv64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

pub(crate) fn parse_tid(text: &str) -> SqlResult<Value> {
    let invalid = || {
        SqlError::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("invalid input syntax for type tid: \"{text}\""),
        )
    };
    let inner = text
        .trim()
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(invalid)?;
    let (block, offset) = inner.split_once(',').ok_or_else(invalid)?;
    let block: u32 = block.trim().parse().map_err(|_| invalid())?;
    let offset: u16 = offset.trim().parse().map_err(|_| invalid())?;
    Ok(Value::Text(format!("({block},{offset})")))
}
