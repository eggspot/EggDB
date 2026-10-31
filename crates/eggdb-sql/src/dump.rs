use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::relations::RelationKind;
use crate::schema::{ColumnSchema, TableSchema};
use crate::session::Session;
use crate::store::KeyValueStore;
use crate::types::{SqlType, Value};
use std::io::Write;

const ROWS_PER_INSERT: usize = 100;

/// What [`dump_sql`] wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DumpReport {
    /// Tables written.
    pub tables: u64,
    /// Rows written.
    pub rows: u64,
    /// Secondary indexes written.
    pub indexes: u64,
    /// Views not written: the catalog does not keep a view's text in a form to write back yet.
    pub views_skipped: u64,
}

fn io_error(error: std::io::Error) -> SqlError {
    SqlError::new(SqlState::IO_ERROR, format!("cannot write the dump: {error}"))
}

fn name_in(dialect: Dialect, name: &str) -> String {
    match dialect {
        Dialect::MySql => format!("`{}`", name.replace('`', "``")),
        Dialect::SqlServer => format!("[{}]", name.replace(']', "]]")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

fn base_name(table: &TableSchema) -> &str {
    table.name.strip_prefix(&format!("{}.", table.schema)).unwrap_or(&table.name)
}

fn table_name_in(dialect: Dialect, table: &TableSchema) -> String {
    match dialect {
        Dialect::PostgreSql => table.qualified_name(),
        _ => name_in(dialect, base_name(table)),
    }
}

fn mysql_type(sql_type: SqlType) -> String {
    match sql_type {
        SqlType::SmallInt => "SMALLINT".to_owned(),
        SqlType::Integer => "INT".to_owned(),
        SqlType::BigInt => "BIGINT".to_owned(),
        SqlType::Real => "FLOAT".to_owned(),
        SqlType::Double => "DOUBLE".to_owned(),
        SqlType::Boolean => "BOOLEAN".to_owned(),
        SqlType::Varchar(Some(length)) if length <= 16_383 => format!("VARCHAR({length})"),
        SqlType::Bpchar(Some(length)) if length <= 255 => format!("CHAR({length})"),
        SqlType::Numeric(Some(modifier)) if modifier.precision <= 65 && (0..=30).contains(&modifier.scale) => {
            format!("DECIMAL({},{})", modifier.precision, modifier.scale)
        }
        SqlType::Numeric(_) => "DECIMAL(65,30)".to_owned(),
        SqlType::Date => "DATE".to_owned(),
        SqlType::Timestamp(_) | SqlType::TimestampTz(_) => "DATETIME(6)".to_owned(),
        SqlType::Time(_) => "TIME(6)".to_owned(),
        SqlType::Json | SqlType::Jsonb => "JSON".to_owned(),
        SqlType::Bytea => "LONGBLOB".to_owned(),
        SqlType::Uuid => "CHAR(36)".to_owned(),
        _ => "LONGTEXT".to_owned(),
    }
}

fn sqlserver_type(sql_type: SqlType) -> String {
    match sql_type {
        SqlType::SmallInt => "smallint".to_owned(),
        SqlType::Integer => "int".to_owned(),
        SqlType::BigInt => "bigint".to_owned(),
        SqlType::Real => "real".to_owned(),
        SqlType::Double => "float".to_owned(),
        SqlType::Boolean => "bit".to_owned(),
        SqlType::Varchar(Some(length)) if length <= 4_000 => format!("nvarchar({length})"),
        SqlType::Bpchar(Some(length)) if length <= 4_000 => format!("nchar({length})"),
        SqlType::Numeric(Some(modifier)) if modifier.precision <= 38 && (0..=38).contains(&modifier.scale) => {
            format!("decimal({},{})", modifier.precision, modifier.scale)
        }
        SqlType::Numeric(_) => "decimal(38,10)".to_owned(),
        SqlType::Date => "date".to_owned(),
        SqlType::Timestamp(_) => "datetime2(6)".to_owned(),
        SqlType::TimestampTz(_) => "datetimeoffset(6)".to_owned(),
        SqlType::Time(_) => "time(6)".to_owned(),
        SqlType::Bytea => "varbinary(max)".to_owned(),
        SqlType::Uuid => "uniqueidentifier".to_owned(),
        _ => "nvarchar(max)".to_owned(),
    }
}

fn create_table(dialect: Dialect, table: &TableSchema) -> String {
    match dialect {
        Dialect::PostgreSql | Dialect::Sqlite | Dialect::Oracle => table.create_statement_in(dialect),
        Dialect::MySql | Dialect::SqlServer => {
            let column = |column: &ColumnSchema| {
                let sql_type = if dialect == Dialect::MySql { mysql_type(column.sql_type) } else { sqlserver_type(column.sql_type) };
                let identity = match (dialect, column.auto_increment) {
                    (Dialect::MySql, true) => " AUTO_INCREMENT",
                    _ => "",
                };
                format!("{} {sql_type}{}{identity}", name_in(dialect, &column.name), if column.not_null { " NOT NULL" } else { "" })
            };
            let mut parts: Vec<String> = table.columns.iter().map(column).collect();
            if !table.primary_key.is_empty() {
                let keys: Vec<String> = table.primary_key.iter().map(|key| name_in(dialect, key)).collect();
                parts.push(format!("PRIMARY KEY ({})", keys.join(", ")));
            }
            format!("CREATE TABLE {} (\n  {}\n);", table_name_in(dialect, table), parts.join(",\n  "))
        }
    }
}

fn quoted_text(dialect: Dialect, text: &str) -> String {
    let doubled = text.replace('\'', "''");
    match dialect {
        Dialect::MySql => format!("'{}'", text.replace('\\', "\\\\").replace('\'', "''")),
        Dialect::SqlServer => format!("N'{doubled}'"),
        _ => format!("'{doubled}'"),
    }
}

pub(crate) fn sql_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

/// `value` as a literal of `dialect`.
fn literal(dialect: Dialect, value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Boolean(flag) => match dialect {
            Dialect::PostgreSql => if *flag { "true" } else { "false" }.to_owned(),
            Dialect::MySql => if *flag { "TRUE" } else { "FALSE" }.to_owned(),
            _ => if *flag { "1" } else { "0" }.to_owned(),
        },
        Value::SmallInt(number) => number.to_string(),
        Value::Integer(number) => number.to_string(),
        Value::BigInt(number) => number.to_string(),
        Value::Real(number) if number.is_finite() => number.to_string(),
        Value::Double(number) if number.is_finite() => number.to_string(),
        Value::Real(_) | Value::Double(_) => match dialect {
            Dialect::PostgreSql => quoted_text(dialect, &value.to_text().unwrap_or_default()),
            _ => "NULL".to_owned(),
        },
        Value::Bytea(bytes) => match dialect {
            Dialect::PostgreSql => format!("'\\x{}'", hex(bytes)),
            Dialect::SqlServer => format!("0x{}", hex(bytes)),
            Dialect::Oracle => format!("HEXTORAW('{}')", hex(bytes)),
            _ => format!("X'{}'", hex(bytes)),
        },
        Value::Date(_) if dialect == Dialect::Oracle => format!("DATE {}", quoted_text(dialect, &value.to_text().unwrap_or_default())),
        Value::Timestamp(_) if dialect == Dialect::Oracle => format!("TIMESTAMP {}", quoted_text(dialect, &value.to_text().unwrap_or_default())),
        Value::TimestampTz(micros) if dialect != Dialect::PostgreSql => {
            let utc = Value::Timestamp(*micros).to_text().unwrap_or_default();
            match dialect {
                Dialect::Oracle => format!("TIMESTAMP {}", quoted_text(dialect, &utc)),
                Dialect::SqlServer => quoted_text(dialect, &format!("{utc} +00:00")),
                _ => quoted_text(dialect, &utc),
            }
        }
        other => quoted_text(dialect, &other.to_text().unwrap_or_default()),
    }
}

fn write_rows(out: &mut dyn Write, dialect: Dialect, table: &TableSchema, rows: &[Vec<Value>]) -> SqlResult<()> {
    let columns: Vec<String> = table.columns.iter().map(|column| name_in(dialect, &column.name)).collect();
    let head = format!("INSERT INTO {} ({}) VALUES", table_name_in(dialect, table), columns.join(", "));
    let one_row_each = dialect == Dialect::Oracle;
    for chunk in rows.chunks(if one_row_each { 1 } else { ROWS_PER_INSERT }) {
        let tuples: Vec<String> = chunk
            .iter()
            .map(|row| format!("({})", row.iter().map(|value| literal(dialect, value)).collect::<Vec<_>>().join(", ")))
            .collect();
        writeln!(out, "{head}\n  {};", tuples.join(",\n  ")).map_err(io_error)?;
    }
    Ok(())
}

/// Writes every table of the database as plain SQL in `dialect`: `CREATE TABLE`, the rows as
/// `INSERT` statements, then the secondary indexes, so that dialect's own client, or `eggdb
/// restore`, loads it into an empty database. Views are counted, not written.
pub fn dump_sql<S: KeyValueStore>(session: &mut Session<'_, S>, dialect: Dialect, out: &mut dyn Write) -> SqlResult<DumpReport> {
    let mut report = DumpReport::default();
    let relations = session.relations()?;
    writeln!(out, "-- EggDB {} dump of {} tables", dialect.name(), relations.iter().filter(|relation| matches!(relation.kind, RelationKind::Table(_))).count()).map_err(io_error)?;
    if dialect == Dialect::MySql {
        writeln!(out, "SET FOREIGN_KEY_CHECKS=0;").map_err(io_error)?;
    }
    for relation in &relations {
        let RelationKind::Table(table) = &relation.kind else {
            continue;
        };
        if dialect == Dialect::PostgreSql && table.schema != crate::schemas::DEFAULT_SCHEMA {
            writeln!(out, "CREATE SCHEMA IF NOT EXISTS {};", name_in(dialect, &table.schema)).map_err(io_error)?;
        }
        writeln!(out, "\n{}", create_table(dialect, table)).map_err(io_error)?;
        report.tables += 1;
        let mut batch: Vec<Vec<Value>> = Vec::new();
        let mut failure: Option<SqlError> = None;
        session.read_stored_rows(relation, None, &mut |_, values| {
            batch.push(values);
            if batch.len() >= ROWS_PER_INSERT {
                if let Err(error) = write_rows(out, dialect, table, &batch) {
                    failure = Some(error);
                    return Ok(false);
                }
                report.rows += batch.len() as u64;
                batch.clear();
            }
            Ok(true)
        })?;
        if let Some(error) = failure {
            return Err(error);
        }
        write_rows(out, dialect, table, &batch)?;
        report.rows += batch.len() as u64;
        for index in &table.indexes {
            let columns: Vec<String> = index.columns.iter().map(|column| name_in(dialect, column)).collect();
            writeln!(
                out,
                "CREATE {}INDEX {} ON {} ({});",
                if index.unique { "UNIQUE " } else { "" },
                name_in(dialect, &index.name),
                table_name_in(dialect, table),
                columns.join(", ")
            )
            .map_err(io_error)?;
            report.indexes += 1;
        }
    }
    report.views_skipped = relations.iter().filter(|relation| matches!(relation.kind, RelationKind::View(_))).count() as u64;
    if dialect == Dialect::MySql {
        writeln!(out, "SET FOREIGN_KEY_CHECKS=1;").map_err(io_error)?;
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
