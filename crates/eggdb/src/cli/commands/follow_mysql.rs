use super::follow::{FollowCounts, copy_stopped, source_failed};
use super::follow_state::{SavedFollow, update_statement};
use super::{Access, Session};
use crate::cli::args::CommandName;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::JsonObject;
use eggdb::sql::Store as DatabaseStore;
use eggdb_mysqlwire::binlog::{BinlogDecoder, BinlogEvent, BinlogValue, RowsKind};
use eggdb_mysqlwire::client::{MySqlClient, MySqlClientError, MySqlConnectOptions, TextResult, parse_mysql_url};
use eggdb_platform::stop_signals;
use eggdb_sql::Dialect;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const PASSWORD_VARIABLE: &str = "MYSQL_PWD";
const MYSQL: &str = "mysql";
const BINLOG: &str = "binlog";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HEARTBEAT: Duration = Duration::from_secs(1);
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);
const COPY_BATCH: usize = 500;
const SYSTEM_SCHEMAS: [&str; 5] = ["mysql", "sys", "performance_schema", "information_schema", "eggdb_follow"];
const BINARY_TYPES: [&str; 7] = ["binary", "varbinary", "tinyblob", "blob", "mediumblob", "longblob", "geometry"];
const REPLICA_SERVER_ID_BASE: u32 = 1_900_000_000;

#[derive(Debug, Clone)]
struct SourceColumn {
    name: String,
    data_type: String,
    column_type: String,
}

#[derive(Debug, Clone)]
struct SourceTable {
    schema: String,
    name: String,
    columns: Vec<SourceColumn>,
    key: Vec<usize>,
}

impl SourceTable {
    fn qualified(&self) -> String {
        format!("{}.{}", quoted(&self.schema), quoted(&self.name))
    }

    fn shown(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

fn quoted(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
}

fn hex_literal(bytes: &[u8]) -> String {
    let digits: String = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    format!("X'{digits}'")
}

fn members_of(column_type: &str) -> Vec<String> {
    let Some(inside) = column_type
        .split_once('(')
        .and_then(|(_, rest)| rest.rsplit_once(')'))
        .map(|(inside, _)| inside)
    else {
        return Vec::new();
    };
    let mut members = Vec::new();
    let mut current = String::new();
    let mut quoted_now = false;
    let mut characters = inside.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\'' if quoted_now && characters.peek() == Some(&'\'') => {
                current.push('\'');
                characters.next();
            }
            '\'' => {
                quoted_now = !quoted_now;
                if !quoted_now {
                    members.push(std::mem::take(&mut current));
                }
            }
            other if quoted_now => current.push(other),
            _ => {}
        }
    }
    members
}

fn sql_value(value: &BinlogValue, column: &SourceColumn) -> String {
    match value {
        BinlogValue::Null => "NULL".to_owned(),
        BinlogValue::Integer { value, bytes } if column.column_type.contains("unsigned") && *value < 0 => {
            let width = u32::from(*bytes) * 8;
            let unsigned = (*value as u64) & if width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
            unsigned.to_string()
        }
        BinlogValue::Integer { value, .. } => value.to_string(),
        BinlogValue::Float(number) if number.is_finite() => format!("{number:?}"),
        BinlogValue::Float(_) => "NULL".to_owned(),
        BinlogValue::Decimal(text) => text.clone(),
        BinlogValue::Temporal(text) | BinlogValue::Json(text) => literal(text),
        BinlogValue::Year(year) => year.to_string(),
        BinlogValue::Bits(bits) => bits.to_string(),
        BinlogValue::Bytes(bytes) if BINARY_TYPES.contains(&column.data_type.as_str()) => hex_literal(bytes),
        BinlogValue::Bytes(bytes) => literal(&String::from_utf8_lossy(bytes)),
        BinlogValue::Enum(number) => {
            let members = members_of(&column.column_type);
            match usize::from(*number).checked_sub(1).and_then(|at| members.get(at)) {
                Some(member) => literal(member),
                None => literal(""),
            }
        }
        BinlogValue::Set(bits) => {
            let chosen: Vec<String> = members_of(&column.column_type)
                .into_iter()
                .enumerate()
                .filter(|(at, _)| *at < 64 && bits & (1u64 << at) != 0)
                .map(|(_, member)| member)
                .collect();
            literal(&chosen.join(","))
        }
    }
}

fn replace_statement(table: &SourceTable, row: &[BinlogValue]) -> String {
    let names: Vec<String> = table.columns.iter().map(|column| quoted(&column.name)).collect();
    let values: Vec<String> = table
        .columns
        .iter()
        .zip(row)
        .map(|(column, value)| sql_value(value, column))
        .collect();
    format!(
        "REPLACE INTO {} ({}) VALUES ({})",
        table.qualified(),
        names.join(", "),
        values.join(", ")
    )
}

fn delete_statement(table: &SourceTable, row: &[BinlogValue]) -> String {
    let conditions: Vec<String> = table
        .key
        .iter()
        .filter_map(|&at| {
            let column = table.columns.get(at)?;
            let value = row.get(at)?;
            Some(match value {
                BinlogValue::Null => format!("{} IS NULL", quoted(&column.name)),
                other => format!("{} = {}", quoted(&column.name), sql_value(other, column)),
            })
        })
        .collect();
    format!("DELETE FROM {} WHERE {}", table.qualified(), conditions.join(" AND "))
}

fn key_of(table: &SourceTable, row: &[BinlogValue]) -> Vec<BinlogValue> {
    table.key.iter().filter_map(|&at| row.get(at).cloned()).collect()
}

fn mysql_failure(target: &str, error: &MySqlClientError) -> Failure {
    source_failed(match error {
        MySqlClientError::Io(error) => format!(
            "cannot reach {target}: {error}. Check the host and port, and that MySQL listens there (bind_address)"
        ),
        MySqlClientError::Server { code: 1045, .. } => {
            format!("cannot sign in to {target}: the user or password is wrong")
        }
        MySqlClientError::Server { code: 1227, message, .. } => format!(
            "{target} refused: {message}. The user needs REPLICATION SLAVE and REPLICATION CLIENT, and SELECT on the followed databases"
        ),
        other => format!("{target}: {other}"),
    })
}

fn failure_text(failure: &Failure) -> String {
    match failure {
        Failure::Database { message, .. } => message.clone(),
        _ => "the changes could not be applied".to_owned(),
    }
}

fn first_cell(results: &[TextResult], row: usize, column: usize) -> Option<String> {
    results.last()?.rows.get(row)?.get(column)?.clone()
}

fn position_text(file: &str, position: u64) -> String {
    format!("{file}:{position}")
}

fn read_position(text: &str) -> Option<(String, u64)> {
    let (file, position) = text.rsplit_once(':')?;
    Some((file.to_owned(), position.parse().ok()?))
}

impl Session<'_> {
    pub(super) fn follow_mysql(&mut self) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Follow));
        if self.request.follow.publication.is_some()
            || self.request.follow.poll_seconds.is_some()
            || self.request.follow.sql_server_changes.is_some()
        {
            return Err(usage(
                "--publication, --poll and --changes are not for MySQL sources, which stream their binary log".to_owned(),
            ));
        }
        let mut options = parse_mysql_url(&self.request.follow.source).map_err(usage)?;
        options.password = self.source_password(PASSWORD_VARIABLE)?;
        self.follow_mysql_from(&options)
    }

    fn follows_schema(&self, schema: &str) -> bool {
        if SYSTEM_SCHEMAS.contains(&schema) {
            return false;
        }
        let chosen = &self.request.follow.schemas;
        chosen.is_empty() || chosen.iter().any(|name| name == schema)
    }

    fn run_in_mysql_dialect(&self, statements: &[String]) -> Result<(), Failure> {
        let db = self.open(Access::Write { create: false }, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::MySql);
        let mut script = String::from("START TRANSACTION;\n");
        for statement in statements {
            script.push_str(statement);
            script.push_str(";\n");
        }
        script.push_str("COMMIT;");
        let ran = session.execute(&script, &mut |_| {});
        let _ = session.close();
        ran.map_err(|error| {
            source_failed(format!("cannot apply the source's changes to {}: {}", self.file_display, error.message()))
        })
    }

    fn follow_mysql_from(&mut self, options: &MySqlConnectOptions) -> Result<Exit, Failure> {
        let target = options.shown();
        let existed = self.request.file.exists();
        let started = Instant::now();
        let failed = |error: MySqlClientError| mysql_failure(&target, &error);
        let mut catalog = MySqlClient::connect(options, CONNECT_TIMEOUT).map_err(failed)?;
        let settings = catalog
            .query("SELECT @@log_bin, @@binlog_format, @@binlog_row_image, @@server_uuid, @@version")
            .map_err(failed)?;
        let setting = |column: usize| first_cell(&settings, 0, column).unwrap_or_default();
        if setting(0) != "1" || !setting(1).eq_ignore_ascii_case("ROW") || !setting(2).eq_ignore_ascii_case("FULL") {
            return Err(Failure::Database {
                message: format!(
                    "cannot follow {target}: it needs log_bin on, binlog_format=ROW and binlog_row_image=FULL (it has log_bin={}, binlog_format={}, binlog_row_image={})",
                    setting(0),
                    setting(1),
                    setting(2)
                ),
                try_command: Some("SET PERSIST binlog_format = 'ROW'; SET PERSIST binlog_row_image = 'FULL';".to_owned()),
                exit: Exit::DatabaseError,
                code: "source_failed",
            });
        }
        let identity = format!("server {}", setting(3));
        self.follow_event(
            JsonObject::new()
                .string("event", "connected")
                .string("source", &target)
                .string("server", &setting(4))
                .string("tls", "no TLS")
                .string("file", &self.file_display)
                .boolean("created", !existed),
            &format!(
                "following {target} into {}{}\n  connected       MySQL {}, no TLS\n",
                self.file_display,
                if existed { "" } else { " (created)" },
                setting(4)
            ),
        );
        let tables = self.mysql_tables(&mut catalog, &target)?;
        if let Some(table) = tables.iter().find(|table| table.key.is_empty()) {
            return Err(source_failed(format!(
                "cannot follow {}: it has no primary key, so its changes cannot be matched to rows; add one or leave its database out with --schema",
                table.shown()
            )));
        }
        let _ = stop_signals::watch();
        let mut counts = FollowCounts::default();
        let saved = if existed { self.saved_follow()? } else { None };
        let (mut file, mut position) = match &saved {
            Some(saved) => {
                if let Some(refusal) = saved.refusal(MYSQL, &identity, &self.file_display) {
                    return Err(source_failed(refusal));
                }
                let (file, position) = saved
                    .get(BINLOG)
                    .and_then(read_position)
                    .ok_or_else(|| source_failed(format!("{} holds no binary log position", self.file_display)))?;
                self.follow_event(
                    JsonObject::new().string("event", "resumed").string("position", &position_text(&file, position)),
                    &format!("  resumed         at {}\n", position_text(&file, position)),
                );
                (file, position)
            }
            None => {
                if !self.request.create && !existed {
                    return Err(Failure::usage(
                        format!("{} does not exist; add --create to make it", self.file_display),
                        Some(CommandName::Follow),
                    ));
                }
                let status = catalog
                    .query("SHOW BINARY LOG STATUS")
                    .or_else(|_| catalog.query("SHOW MASTER STATUS"))
                    .map_err(failed)?;
                let file = first_cell(&status, 0, 0).ok_or_else(|| {
                    source_failed(format!("{target} reports no binary log position; is log_bin on?"))
                })?;
                let position: u64 = first_cell(&status, 0, 1).and_then(|text| text.parse().ok()).unwrap_or(4);
                self.copy_mysql(&mut catalog, &target, &tables, &identity, &file, position, &mut counts)?;
                self.follow_event(
                    JsonObject::new()
                        .string("event", "copied")
                        .number("tables", counts.tables)
                        .number("rows", counts.rows_copied)
                        .number("ms", started.elapsed().as_millis() as u64),
                    &format!(
                        "  copied          {} tables, {} rows in {:.1} s\n",
                        counts.tables,
                        counts.rows_copied,
                        started.elapsed().as_secs_f64()
                    ),
                );
                (file, position)
            }
        };
        let by_name: BTreeMap<(String, String), SourceTable> = tables
            .into_iter()
            .map(|table| ((table.schema.clone(), table.name.clone()), table))
            .collect();
        let deadline_span = self.reconnect_for();
        let mut attempt = 0;
        let caught_up = loop {
            match self.stream_binlog(options, &target, &by_name, &mut file, &mut position, &mut counts) {
                Ok(caught_up) => break caught_up,
                Err(error) if matches!(error, MySqlClientError::Io(_)) => {
                    let deadline = Instant::now() + deadline_span;
                    if !self.wait_to_reconnect(&mut attempt, &error.to_string(), deadline) {
                        return Err(source_failed(format!(
                            "the connection to {target} stayed lost; {} holds every transaction up to {}, and running the same command again resumes",
                            self.file_display,
                            position_text(&file, position)
                        )));
                    }
                    self.reconnected_event(attempt, &position_text(&file, position));
                }
                Err(MySqlClientError::Protocol(reason)) => return Err(source_failed(reason)),
                Err(error) => return Err(mysql_failure(&target, &error)),
            }
        };
        let reason = if caught_up {
            ("caught_up", "caught up, --until caught-up")
        } else {
            ("interrupt", "stopped by Ctrl-C or SIGTERM")
        };
        self.follow_event(
            JsonObject::new()
                .string("event", "stopped")
                .string("reason", reason.0)
                .string("position", &position_text(&file, position))
                .number("transactions", counts.transactions),
            &format!(
                "ok: {} holds every source transaction committed up to {} ({}; {} inserts, {} updates, {} deletes)\n",
                self.file_display,
                position_text(&file, position),
                reason.1,
                counts.inserts,
                counts.updates,
                counts.deletes
            ),
        );
        Ok(Exit::Done)
    }

    fn mysql_tables(&self, catalog: &mut MySqlClient, target: &str) -> Result<Vec<SourceTable>, Failure> {
        let failed = |error: MySqlClientError| mysql_failure(target, &error);
        let columns = catalog
            .query(
                "SELECT c.table_schema, c.table_name, c.column_name, c.data_type, c.column_type, c.column_key \
                 FROM information_schema.columns c JOIN information_schema.tables t \
                 ON t.table_schema = c.table_schema AND t.table_name = c.table_name \
                 WHERE t.table_type = 'BASE TABLE' ORDER BY c.table_schema, c.table_name, c.ordinal_position",
            )
            .map_err(failed)?;
        let mut tables: Vec<SourceTable> = Vec::new();
        for row in columns.last().map(|result| result.rows.as_slice()).unwrap_or_default() {
            let cell = |at: usize| row.get(at).cloned().flatten().unwrap_or_default();
            let (schema, name) = (cell(0), cell(1));
            if !self.follows_schema(&schema) {
                continue;
            }
            if tables.last().is_none_or(|table| table.schema != schema || table.name != name) {
                tables.push(SourceTable { schema, name, columns: Vec::new(), key: Vec::new() });
            }
            if let Some(table) = tables.last_mut() {
                if cell(5) == "PRI" {
                    table.key.push(table.columns.len());
                }
                table.columns.push(SourceColumn {
                    name: cell(2),
                    data_type: cell(3).to_ascii_lowercase(),
                    column_type: cell(4).to_ascii_lowercase(),
                });
            }
        }
        Ok(tables)
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_mysql(
        &mut self,
        catalog: &mut MySqlClient,
        target: &str,
        tables: &[SourceTable],
        identity: &str,
        file: &str,
        position: u64,
        counts: &mut FollowCounts,
    ) -> Result<(), Failure> {
        let failed = |error: MySqlClientError| mysql_failure(target, &error);
        let mut state = SavedFollow::new(MYSQL, identity);
        state.set(BINLOG, &position_text(file, position));
        let mut setup = state.create_statements();
        let mut schemas: Vec<&str> = tables.iter().map(|table| table.schema.as_str()).collect();
        schemas.dedup();
        for schema in schemas {
            setup.push(format!("CREATE DATABASE IF NOT EXISTS {}", quoted(schema)));
        }
        for table in tables {
            let created = catalog
                .query(&format!("SHOW CREATE TABLE {}", table.qualified()))
                .map_err(failed)?;
            let definition = first_cell(&created, 0, 1)
                .ok_or_else(|| source_failed(format!("{target} gave no definition for {}", table.shown())))?;
            setup.push(format!("USE {}", quoted(&table.schema)));
            setup.push(definition);
        }
        self.run_in_mysql_dialect(&setup)?;
        for table in tables {
            if stop_signals::requested() {
                return Err(copy_stopped(&self.file_display));
            }
            let selected: Vec<String> = table
                .columns
                .iter()
                .map(|column| {
                    if BINARY_TYPES.contains(&column.data_type.as_str()) {
                        format!("HEX({})", quoted(&column.name))
                    } else {
                        quoted(&column.name)
                    }
                })
                .collect();
            let rows = catalog
                .query(&format!("SELECT {} FROM {}", selected.join(", "), table.qualified()))
                .map_err(failed)?;
            let rows = rows.into_iter().last().map(|result| result.rows).unwrap_or_default();
            for batch in rows.chunks(COPY_BATCH) {
                let values: Vec<String> = batch
                    .iter()
                    .map(|row| {
                        let cells: Vec<String> = table
                            .columns
                            .iter()
                            .zip(row)
                            .map(|(column, cell)| match cell {
                                None => "NULL".to_owned(),
                                Some(hex) if BINARY_TYPES.contains(&column.data_type.as_str()) => format!("X'{hex}'"),
                                Some(text) => literal(text),
                            })
                            .collect();
                        format!("({})", cells.join(", "))
                    })
                    .collect();
                let names: Vec<String> = table.columns.iter().map(|column| quoted(&column.name)).collect();
                self.run_in_mysql_dialect(&[format!(
                    "INSERT INTO {} ({}) VALUES {}",
                    table.qualified(),
                    names.join(", "),
                    values.join(", ")
                )])?;
            }
            counts.tables += 1;
            counts.rows_copied += rows.len() as u64;
            self.follow_event(
                JsonObject::new()
                    .string("event", "table_copied")
                    .string("table", &table.shown())
                    .number("rows", rows.len() as u64),
                &format!("  copied          {}: {} rows\n", table.shown(), rows.len()),
            );
        }
        Ok(())
    }

    fn change_statements(
        &self,
        tables: &BTreeMap<(String, String), SourceTable>,
        kind: RowsKind,
        schema: &str,
        table: &str,
        rows: &[Vec<BinlogValue>],
        counts: &mut FollowCounts,
    ) -> Vec<String> {
        let Some(table) = tables.get(&(schema.to_owned(), table.to_owned())) else {
            return Vec::new();
        };
        let mut statements = Vec::new();
        match kind {
            RowsKind::Write => {
                for row in rows {
                    statements.push(replace_statement(table, row));
                    counts.inserts += 1;
                }
            }
            RowsKind::Delete => {
                for row in rows {
                    statements.push(delete_statement(table, row));
                    counts.deletes += 1;
                }
            }
            RowsKind::Update => {
                for pair in rows.chunks(2) {
                    let [before, after] = pair else {
                        continue;
                    };
                    if key_of(table, before) != key_of(table, after) {
                        statements.push(delete_statement(table, before));
                    }
                    statements.push(replace_statement(table, after));
                    counts.updates += 1;
                }
            }
        }
        statements
    }

    fn stream_binlog(
        &mut self,
        options: &MySqlConnectOptions,
        target: &str,
        tables: &BTreeMap<(String, String), SourceTable>,
        file: &mut String,
        position: &mut u64,
        counts: &mut FollowCounts,
    ) -> Result<bool, MySqlClientError> {
        let mut stream = MySqlClient::connect(options, CONNECT_TIMEOUT)?;
        let server_id = REPLICA_SERVER_ID_BASE + std::process::id() % 1_000_000;
        let start = u32::try_from(*position).unwrap_or(u32::MAX);
        stream.start_binlog_dump(server_id, file, start, HEARTBEAT)?;
        stream.set_read_timeout(Some(STREAM_READ_TIMEOUT))?;
        self.follow_event(
            JsonObject::new().string("event", "following").string("position", &position_text(file, *position)),
            &format!("  following       from {}\n", position_text(file, *position)),
        );
        let mut decoder = BinlogDecoder::new();
        let mut pending: Vec<String> = Vec::new();
        let mut in_transaction = false;
        let mut quiet_since = Instant::now();
        loop {
            if stop_signals::requested() && !in_transaction {
                return Ok(false);
            }
            let Some(event) = stream.next_binlog_event()? else {
                return Err(MySqlClientError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the server ended the binary log dump",
                )));
            };
            let (next, decoded) = decoder.decode(&event).map_err(MySqlClientError::Protocol)?;
            match decoded {
                BinlogEvent::Rotate { file: rotated, position: at } => {
                    *file = rotated;
                    *position = at;
                    continue;
                }
                BinlogEvent::Query { schema, sql } => {
                    let statement = sql.trim_start().to_ascii_uppercase();
                    if statement.starts_with("BEGIN") || statement.starts_with("XA ") {
                        in_transaction = true;
                    } else if statement.starts_with("COMMIT") {
                        in_transaction = false;
                    } else if !statement.starts_with("SAVEPOINT")
                        && !statement.starts_with("ROLLBACK")
                        && (schema.is_empty() || self.follows_schema(&schema))
                        && ["CREATE", "ALTER", "DROP", "RENAME", "TRUNCATE"]
                            .iter()
                            .any(|word| statement.starts_with(word))
                    {
                        return Err(MySqlClientError::Protocol(format!(
                            "{target} changed its schema ({}); {} holds every transaction up to {}. Apply the change to the file with eggdb sql, then run the same command again",
                            sql.chars().take(120).collect::<String>(),
                            self.file_display,
                            position_text(file, *position)
                        )));
                    }
                }
                BinlogEvent::Rows { kind, schema, table, rows } => {
                    if self.follows_schema(&schema) {
                        pending.extend(self.change_statements(tables, kind, &schema, &table, &rows, counts));
                    }
                    in_transaction = true;
                }
                BinlogEvent::Commit => in_transaction = false,
                BinlogEvent::Other => {}
            }
            if next > 0 {
                *position = u64::from(next);
            }
            if !in_transaction {
                if !pending.is_empty() {
                    pending.push(update_statement(BINLOG, &position_text(file, *position)));
                    self.run_in_mysql_dialect(&pending)
                        .map_err(|failure| MySqlClientError::Protocol(failure_text(&failure)))?;
                    pending.clear();
                    counts.transactions += 1;
                    quiet_since = Instant::now();
                } else if self.request.follow.until_caught_up && quiet_since.elapsed() >= HEARTBEAT * 2 {
                    self.run_in_mysql_dialect(&[update_statement(BINLOG, &position_text(file, *position))])
                        .map_err(|failure| MySqlClientError::Protocol(failure_text(&failure)))?;
                    return Ok(true);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_images_become_mysql_statements() {
        let table = SourceTable {
            schema: "shop".to_owned(),
            name: "items".to_owned(),
            columns: vec![
                SourceColumn { name: "id".to_owned(), data_type: "int".to_owned(), column_type: "int unsigned".to_owned() },
                SourceColumn { name: "size".to_owned(), data_type: "enum".to_owned(), column_type: "enum('s','m','it''s')".to_owned() },
            ],
            key: vec![0],
        };
        let row = vec![BinlogValue::Integer { value: -1, bytes: 4 }, BinlogValue::Enum(3)];
        assert_eq!(
            replace_statement(&table, &row),
            "REPLACE INTO `shop`.`items` (`id`, `size`) VALUES (4294967295, 'it''s')"
        );
        assert_eq!(delete_statement(&table, &row), "DELETE FROM `shop`.`items` WHERE `id` = 4294967295");
        assert_eq!(read_position("binlog.000003:157"), Some(("binlog.000003".to_owned(), 157)));
        assert_eq!(members_of("set('a','b')"), ["a", "b"]);
    }
}
