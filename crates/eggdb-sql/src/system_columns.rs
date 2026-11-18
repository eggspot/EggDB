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

const ORACLE_ROWID: (&str, &str) = ("rowid", "\u{2}rowid");
const ROWID_DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) const ROW_VERSION_FUNCTION: &str = "\u{2}row_version";
pub(crate) const ORACLE_ROWID_FUNCTION: &str = "\u{2}oracle_rowid";
pub(crate) const ROW_VERSION_TYPE: SqlType = XID;

const MAX_HEAP_TUPLES_PER_PAGE: u64 = 291;
const FIRST_NORMAL_TRANSACTION_ID: u32 = 3;

pub(crate) fn hidden_name(name: &str) -> Option<&'static str> {
    if crate::constraints::current_dialect() == crate::Dialect::Oracle {
        return (name == ORACLE_ROWID.0).then_some(ORACLE_ROWID.1);
    }
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
        && COLUMNS
            .iter()
            .map(|(written, hidden, _)| (written, hidden))
            .chain(std::iter::once((&ORACLE_ROWID.0, &ORACLE_ROWID.1)))
            .all(|(written, hidden)| {
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
    columns.push(OutputColumn {
        name: ORACLE_ROWID.1.to_owned(),
        sql_type: Some(SqlType::Text),
    });
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
            let (block, offset) = tuple_place(&table, &prefix, key);
            row.extend([
                Value::Oid(table_oid),
                Value::Text(format!("({block},{offset})")),
                Value::Oid(version),
                Value::Oid(0),
                Value::Oid(0),
                Value::Oid(0),
                Value::Text(oracle_rowid(table_oid, block, offset)),
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

fn base64_digits(value: u64, digits: usize, out: &mut String) {
    for at in (0..digits).rev() {
        let digit = (value >> (6 * at)) & 63;
        out.push(char::from(ROWID_DIGITS[digit as usize]));
    }
}

fn oracle_rowid(object: u32, block: u64, row: u64) -> String {
    let mut out = String::with_capacity(18);
    base64_digits(u64::from(object), 6, &mut out);
    base64_digits(1, 3, &mut out);
    base64_digits(block, 6, &mut out);
    base64_digits(row, 3, &mut out);
    out
}

fn tuple_place(table: &TableDefinition, prefix: &[u8], key: &[u8]) -> (u64, u64) {
    let suffix = key.get(prefix.len()..).unwrap_or_default();
    let hidden_row_id = <[u8; 8]>::try_from(suffix)
        .ok()
        .filter(|_| table.primary_key.is_empty())
        .map(u64::from_be_bytes);
    match hidden_row_id {
        Some(row_id) => place_of(row_id.saturating_sub(1)),
        None => keyed_place(suffix),
    }
}

fn keyed_place(suffix: &[u8]) -> (u64, u64) {
    place_of(fnv64(suffix) % (u64::from(u32::MAX) * MAX_HEAP_TUPLES_PER_PAGE))
}

fn place_of(number: u64) -> (u64, u64) {
    (number / MAX_HEAP_TUPLES_PER_PAGE, number % MAX_HEAP_TUPLES_PER_PAGE + 1)
}

/// The Oracle `ROWID` of the row whose primary key holds `values[1..]` in the table whose OID
/// is `values[0]`: the address a query of the table shows for it (D1251).
pub(crate) fn oracle_rowid_of(values: &[Value]) -> Value {
    let Some((Value::Oid(table_oid), key)) = values.split_first().map(|(first, rest)| (first.clone(), rest)) else {
        return Value::Null;
    };
    if key.iter().any(Value::is_null) {
        return Value::Null;
    }
    let mut suffix = Vec::new();
    for value in key {
        crate::encoding::encode_key_value(&mut suffix, value);
    }
    let (block, offset) = keyed_place(&suffix);
    Value::Text(oracle_rowid(table_oid, block, offset))
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
    let dialect = crate::constraints::current_dialect();
    if dialect != crate::Dialect::PostgreSql && dialect != crate::Dialect::Oracle {
        return Ok(None);
    }
    Ok(match tables.relation(reader, name)? {
        Some(Relation::Table(table)) if dialect == crate::Dialect::PostgreSql || !table.primary_key.is_empty() => {
            Some(table)
        }
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
    oracle_key: Option<Vec<String>>,
}

impl Replacements {
    fn of(
        reader: &dyn KeyValueRead,
        table: &TableDefinition,
        returning: &Option<Vec<SelectItem>>,
        filter: Option<&Expr>,
    ) -> SqlResult<Self> {
        let oracle = crate::constraints::current_dialect() == crate::Dialect::Oracle;
        let names_table_oid = filter
            .into_iter()
            .chain(returned(returning))
            .any(|expr| names_column(expr, "tableoid"));
        let table_oid = if names_table_oid || oracle {
            crate::pg_catalog::snapshot_for(reader)?
                .table_oid(&table.name)
                .unwrap_or(0)
        } else {
            0
        };
        Ok(Self {
            table_oid,
            columns: table.columns.iter().map(|column| column.name.clone()).collect(),
            oracle_key: oracle.then(|| {
                table
                    .primary_key
                    .iter()
                    .map(|at| table.columns[*at].name.clone())
                    .collect()
            }),
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
        let column = |name: &str| Expr {
            kind: ExprKind::Column {
                qualifier: None,
                name: name.to_owned(),
            },
            position,
        };
        if let Some(key) = &self.oracle_key {
            return (name == ORACLE_ROWID.0).then(|| Expr {
                kind: ExprKind::Function {
                    name: ORACLE_ROWID_FUNCTION.to_owned(),
                    arguments: std::iter::once(constant(OID, self.table_oid))
                        .chain(key.iter().map(|name| column(name)))
                        .collect(),
                },
                position,
            });
        }
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
