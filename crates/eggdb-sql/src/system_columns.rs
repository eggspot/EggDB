use crate::OutputColumn;
use crate::ast::{Delete, Expr, ExprKind, InsertValue, OrderItem, Select, SelectItem, Update};
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

pub(crate) const ROW_VERSION_FUNCTION: &str = "\u{2}row_version";
pub(crate) const ROW_VERSION_TYPE: SqlType = XID;

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
            let version = row_version(&row);
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

fn row_version(row: &[Value]) -> u32 {
    let folded = fnv64(&encode_row(row));
    let version = (folded ^ (folded >> 32)) as u32;
    version.max(FIRST_NORMAL_TRANSACTION_ID)
}

pub(crate) fn row_version_of(values: &[Value]) -> Value {
    Value::Oid(row_version(values))
}

pub(crate) fn rewrite_update(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    update: &mut Update,
) -> SqlResult<()> {
    let Some(table) = plain_table(reader, tables, &update.table.name.text)? else {
        return Ok(());
    };
    let assigned = update.assignments.iter().filter_map(|(_, value)| match value {
        InsertValue::Expr(expr) => Some(expr),
        InsertValue::Default => None,
    });
    if !dml_names_them(&table, update.filter.as_ref(), &update.returning, assigned) {
        return Ok(());
    }
    let replacements = Replacements::of(reader, &table, &update.returning, update.filter.as_ref())?;
    for expr in crate::execute::update_exprs(update) {
        replacements.apply(expr);
    }
    Ok(())
}

pub(crate) fn rewrite_delete(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    delete: &mut Delete,
) -> SqlResult<()> {
    let Some(table) = plain_table(reader, tables, &delete.table.name.text)? else {
        return Ok(());
    };
    if !dml_names_them(&table, delete.filter.as_ref(), &delete.returning, std::iter::empty()) {
        return Ok(());
    }
    let replacements = Replacements::of(reader, &table, &delete.returning, delete.filter.as_ref())?;
    for expr in crate::execute::delete_exprs(delete) {
        replacements.apply(expr);
    }
    Ok(())
}

fn plain_table(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    name: &str,
) -> SqlResult<Option<std::sync::Arc<TableDefinition>>> {
    if crate::constraints::current_dialect() != crate::Dialect::PostgreSql {
        return Ok(None);
    }
    Ok(match tables.relation(reader, name)? {
        Some(Relation::Table(table)) => Some(table),
        _ => None,
    })
}

fn dml_names_them<'e>(
    table: &TableDefinition,
    filter: Option<&'e Expr>,
    returning: &'e Option<Vec<SelectItem>>,
    others: impl Iterator<Item = &'e Expr>,
) -> bool {
    wanted_for(table)
        && filter
            .into_iter()
            .chain(returned(returning))
            .chain(others)
            .any(names_a_system_column)
}

fn returned(returning: &Option<Vec<SelectItem>>) -> impl Iterator<Item = &Expr> {
    returning.iter().flatten().filter_map(|item| match item {
        SelectItem::Expr { expr, .. } => Some(expr),
        SelectItem::Wildcard { .. } => None,
    })
}

struct Replacements {
    table_oid: u32,
    columns: Vec<String>,
}

impl Replacements {
    fn of(
        reader: &dyn KeyValueRead,
        table: &TableDefinition,
        returning: &Option<Vec<SelectItem>>,
        filter: Option<&Expr>,
    ) -> SqlResult<Self> {
        let names_table_oid = filter
            .into_iter()
            .chain(returned(returning))
            .any(|expr| names_column(expr, "tableoid"));
        let table_oid = if names_table_oid {
            crate::pg_catalog::snapshot_for(reader)?
                .table_oid(&table.name)
                .unwrap_or(0)
        } else {
            0
        };
        Ok(Self {
            table_oid,
            columns: table.columns.iter().map(|column| column.name.clone()).collect(),
        })
    }

    fn apply(&self, expr: &mut Expr) {
        if let ExprKind::Column { name, .. } = &expr.kind
            && let Some(replacement) = self.replacement(name, expr.position)
        {
            *expr = replacement;
            return;
        }
        for child in crate::eval::children_mut(expr) {
            self.apply(child);
        }
    }

    fn replacement(&self, name: &str, position: usize) -> Option<Expr> {
        let constant = |sql_type: SqlType, value: u32| Expr {
            kind: ExprKind::Constant {
                sql_type,
                value: Value::Oid(value),
            },
            position,
        };
        Some(match name {
            "tableoid" => constant(OID, self.table_oid),
            "xmax" => constant(XID, 0),
            "cmin" | "cmax" => constant(CID, 0),
            "xmin" => Expr {
                kind: ExprKind::Function {
                    name: ROW_VERSION_FUNCTION.to_owned(),
                    arguments: self
                        .columns
                        .iter()
                        .map(|column| Expr {
                            kind: ExprKind::Column {
                                qualifier: None,
                                name: column.clone(),
                            },
                            position,
                        })
                        .collect(),
                },
                position,
            },
            _ => return None,
        })
    }
}

fn names_column(expr: &Expr, wanted: &str) -> bool {
    match &expr.kind {
        ExprKind::Column { name, .. } => name == wanted,
        _ => crate::eval::children(expr)
            .into_iter()
            .any(|child| names_column(child, wanted)),
    }
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
