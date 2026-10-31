use super::follow::{FollowCounts, source_failed};
use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::JsonObject;
use eggdb::sql::Store as DatabaseStore;
use eggdb_platform::stop_signals;
use eggdb_sql::Dialect;
use eggdb_tdswire::client::{
    TdsClient, TdsClientError, TdsConnectOptions, TextRows, parse_sql_server_url, quote_name,
    quote_unicode,
};
use std::time::{Duration, Instant};

const PASSWORD_VARIABLE: &str = "SQLCMDPASSWORD";
const DEFAULT_POLL_SECONDS: u64 = 5;
const ROWS_PER_INSERT: usize = 200;
const STOP_CHECK: Duration = Duration::from_millis(200);
const INVALID_OBJECT_NAME: u32 = 208;

struct SourceColumn {
    name: String,
    data_type: String,
    declared: String,
}

struct SourceTable {
    schema: String,
    name: String,
    columns: Vec<SourceColumn>,
    key: Vec<String>,
}

impl SourceTable {
    fn shown(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    fn qualified(&self) -> String {
        format!("{}.{}", quote_name(&self.schema), quote_name(&self.name))
    }
}

fn is_binary(data_type: &str) -> bool {
    matches!(data_type, "binary" | "varbinary" | "image" | "timestamp" | "rowversion")
}

fn text_expression(column: &str, data_type: &str) -> String {
    let style = match data_type {
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion" => Some(1),
        "datetime" | "datetime2" | "smalldatetime" | "date" | "time" | "datetimeoffset" => Some(126),
        "float" | "real" => Some(3),
        "money" | "smallmoney" => Some(2),
        _ => None,
    };
    match style {
        Some(style) => format!("CONVERT(nvarchar(max), {column}, {style})"),
        None => format!("CAST({column} AS nvarchar(max))"),
    }
}

fn literal(value: &Option<String>, data_type: &str) -> String {
    match value {
        None => "NULL".to_owned(),
        Some(text) if is_binary(data_type) => format!("CONVERT(varbinary(max), {}, 1)", quote_unicode(text)),
        Some(text) => quote_unicode(text),
    }
}

fn declared_type(data_type: &str, length: Option<&str>, precision: Option<&str>, scale: Option<&str>, datetime_precision: Option<&str>) -> String {
    match data_type {
        "char" | "varchar" | "nchar" | "nvarchar" | "binary" | "varbinary" => match length {
            Some("-1") => format!("{data_type}(max)"),
            Some(length) => format!("{data_type}({length})"),
            None => data_type.to_owned(),
        },
        "decimal" | "numeric" => format!("{data_type}({}, {})", precision.unwrap_or("18"), scale.unwrap_or("0")),
        "datetime2" | "time" | "datetimeoffset" => format!("{data_type}({})", datetime_precision.unwrap_or("7")),
        "timestamp" | "rowversion" => "binary(8)".to_owned(),
        other => other.to_owned(),
    }
}

fn first_rows(results: Vec<TextRows>) -> TextRows {
    results.into_iter().next().unwrap_or_default()
}

fn text(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}

fn tds_failure(target: &str, error: &TdsClientError) -> Failure {
    source_failed(match error {
        TdsClientError::Io(error) => format!(
            "cannot connect to {target}: {error}. Check the host and port, and that SQL Server listens for TCP there"
        ),
        TdsClientError::Tls(message) => format!("cannot connect to {target} over TLS: {message}"),
        TdsClientError::Login(message) => format!(
            "cannot connect to {target}: {message}. Set SQLCMDPASSWORD, or pass --password-file"
        ),
        other => format!("cannot follow {target}: {other}"),
    })
}

impl Session<'_> {
    pub(super) fn follow_sql_server(&mut self) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Follow));
        if self.request.follow.publication.is_some() {
            return Err(usage("--publication is for PostgreSQL sources; SQL Server uses change tracking".to_owned()));
        }
        let mut options = parse_sql_server_url(&self.request.follow.source).map_err(usage)?;
        options.password = self.source_password(PASSWORD_VARIABLE)?;
        self.follow_tds(options)
    }

    fn follow_tds(&mut self, options: TdsConnectOptions) -> Result<Exit, Failure> {
        let target = format!("{}:{}/{}", options.host, options.port, options.database);
        let existed = self.request.file.exists();
        let started = Instant::now();
        let mut source = TdsClient::connect(&options).map_err(|error| tds_failure(&target, &error))?;
        let query = |source: &mut TdsClient, sql: &str| source.batch(sql).map_err(|error| tds_failure(&target, &error));
        self.follow_event(
            JsonObject::new()
                .string("event", "connected")
                .string("source", &options.shown())
                .string("tls", if source.encrypted() { "on" } else { "off" })
                .string("file", &self.file_display)
                .boolean("created", !existed),
            &format!(
                "following {} into {}{}\n  connected       SQL Server, {}\n",
                options.shown(),
                self.file_display,
                if existed { "" } else { " (created)" },
                if source.encrypted() { "encrypted" } else { "not encrypted" }
            ),
        );
        let tracking = match source.batch(
            "SELECT CAST(retention_period AS nvarchar(max)) FROM sys.change_tracking_databases WHERE database_id = DB_ID()",
        ) {
            Ok(results) => first_rows(results),
            Err(TdsClientError::Server { number: INVALID_OBJECT_NAME, .. }) => Vec::new(),
            Err(error) => return Err(tds_failure(&target, &error)),
        };
        if tracking.is_empty() {
            return Err(Failure::Database {
                message: format!(
                    "cannot follow {target}: change tracking is off for {}, so changes after the copy could not be seen. Turn it on, then follow again",
                    options.database
                ),
                try_command: Some(format!(
                    "ALTER DATABASE {} SET CHANGE_TRACKING = ON (CHANGE_RETENTION = 2 DAYS, AUTO_CLEANUP = ON);",
                    quote_name(&options.database)
                )),
                exit: Exit::DatabaseError,
                code: "source_failed",
            });
        }
        let schema_filter = if self.request.follow.schemas.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = self.request.follow.schemas.iter().map(|name| quote_unicode(name)).collect();
            format!(" AND s.name IN ({})", names.join(", "))
        };
        let listed = first_rows(query(
            &mut source,
            &format!(
                "SELECT CAST(s.name AS nvarchar(max)), CAST(t.name AS nvarchar(max)), \
                 CAST(CASE WHEN ct.object_id IS NULL THEN 0 ELSE 1 END AS nvarchar(max)) \
                 FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id \
                 LEFT JOIN sys.change_tracking_tables ct ON ct.object_id = t.object_id \
                 WHERE t.is_ms_shipped = 0{schema_filter} ORDER BY s.name, t.name"
            ),
        )?);
        let untracked: Vec<String> = listed
            .iter()
            .filter(|row| text(&row[2]) != "1")
            .map(|row| format!("{}.{}", text(&row[0]), text(&row[1])))
            .collect();
        if !untracked.is_empty() {
            return Err(Failure::Database {
                message: format!(
                    "cannot follow {target}: {} tables are not tracked: {}; their changes could not be seen",
                    untracked.len(),
                    untracked.join(", ")
                ),
                try_command: Some(format!("ALTER TABLE {} ENABLE CHANGE_TRACKING;", untracked[0])),
                exit: Exit::DatabaseError,
                code: "source_failed",
            });
        }
        let mut tables = Vec::new();
        for row in &listed {
            let (schema, name) = (text(&row[0]), text(&row[1]));
            let filter = format!("TABLE_SCHEMA = {} AND TABLE_NAME = {}", quote_unicode(&schema), quote_unicode(&name));
            let columns = first_rows(query(
                &mut source,
                &format!(
                    "SELECT CAST(COLUMN_NAME AS nvarchar(max)), CAST(DATA_TYPE AS nvarchar(max)), \
                     CAST(CHARACTER_MAXIMUM_LENGTH AS nvarchar(max)), CAST(NUMERIC_PRECISION AS nvarchar(max)), \
                     CAST(NUMERIC_SCALE AS nvarchar(max)), CAST(DATETIME_PRECISION AS nvarchar(max)), \
                     CAST(IS_NULLABLE AS nvarchar(max)) FROM INFORMATION_SCHEMA.COLUMNS WHERE {filter} ORDER BY ORDINAL_POSITION"
                ),
            )?)
            .iter()
            .map(|column| {
                let data_type = text(&column[1]);
                let declared = format!(
                    "{}{}",
                    declared_type(&data_type, column[2].as_deref(), column[3].as_deref(), column[4].as_deref(), column[5].as_deref()),
                    if text(&column[6]) == "NO" { " NOT NULL" } else { "" }
                );
                SourceColumn { name: text(&column[0]), data_type, declared }
            })
            .collect();
            let key = first_rows(query(
                &mut source,
                &format!(
                    "SELECT CAST(k.COLUMN_NAME AS nvarchar(max)) FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS c \
                     JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE k ON k.CONSTRAINT_NAME = c.CONSTRAINT_NAME AND k.TABLE_SCHEMA = c.TABLE_SCHEMA \
                     WHERE c.CONSTRAINT_TYPE = 'PRIMARY KEY' AND c.{} ORDER BY k.ORDINAL_POSITION",
                    filter.replace(" AND TABLE_NAME", " AND c.TABLE_NAME")
                ),
            )?)
            .iter()
            .map(|row| text(&row[0]))
            .collect();
            tables.push(SourceTable { schema, name, columns, key });
        }
        let mut version: i64 = first_rows(query(&mut source, "SELECT CAST(CHANGE_TRACKING_CURRENT_VERSION() AS nvarchar(max))")?)
            .first()
            .and_then(|row| row.first())
            .map(text)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);

        let db = self.open(Access::Write { create: self.request.create }, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::SqlServer);
        let mut run = |sql: &str| -> Result<(), Failure> {
            session
                .execute(sql, &mut |_| {})
                .map_err(|error| source_failed(format!("cannot write into the file: {} ({})", error.message(), sql.chars().take(200).collect::<String>())))
        };
        let mut counts = FollowCounts::default();
        let mut schemas: Vec<&str> = tables.iter().map(|table| table.schema.as_str()).collect();
        schemas.dedup();
        for schema in schemas.iter().filter(|schema| **schema != "dbo") {
            run(&format!("CREATE SCHEMA {}", quote_name(schema)))?;
        }
        for table in &tables {
            let mut definitions: Vec<String> = table
                .columns
                .iter()
                .map(|column| format!("{} {}", quote_name(&column.name), column.declared))
                .collect();
            if !table.key.is_empty() {
                let key: Vec<String> = table.key.iter().map(|name| quote_name(name)).collect();
                definitions.push(format!("PRIMARY KEY ({})", key.join(", ")));
            }
            run(&format!("CREATE TABLE {} ({})", table.qualified(), definitions.join(", "))).map_err(|failure| match failure {
                Failure::Database { message, .. } if message.contains("already") => Failure::Database {
                    message: format!(
                        "cannot follow into {}: it already has the source's table {}. Follow into a new file with --create, or drop it first",
                        self.file_display,
                        table.shown()
                    ),
                    try_command: None,
                    exit: Exit::DatabaseError,
                    code: "already_exists",
                },
                other => other,
            })?;
        }
        for table in &tables {
            let copied_at = Instant::now();
            let selected: Vec<String> = table
                .columns
                .iter()
                .map(|column| text_expression(&quote_name(&column.name), &column.data_type))
                .collect();
            let rows = first_rows(query(&mut source, &format!("SELECT {} FROM {}", selected.join(", "), table.qualified()))?);
            let names: Vec<String> = table.columns.iter().map(|column| quote_name(&column.name)).collect();
            run("BEGIN TRANSACTION")?;
            for chunk in rows.chunks(ROWS_PER_INSERT) {
                let values: Vec<String> = chunk
                    .iter()
                    .map(|row| {
                        let literals: Vec<String> = row
                            .iter()
                            .zip(&table.columns)
                            .map(|(value, column)| literal(value, &column.data_type))
                            .collect();
                        format!("({})", literals.join(", "))
                    })
                    .collect();
                run(&format!("INSERT INTO {} ({}) VALUES {}", table.qualified(), names.join(", "), values.join(", ")))?;
            }
            run("COMMIT TRANSACTION")?;
            counts.tables += 1;
            counts.rows_copied += rows.len() as u64;
            self.follow_event(
                JsonObject::new()
                    .string("event", "table_copied")
                    .string("table", &table.shown())
                    .number("rows", rows.len() as u64)
                    .number("elapsed_ms", copied_at.elapsed().as_millis() as u64),
                &format!(
                    "  copied          {:<28} {:>10} rows   {:.1} s\n",
                    table.shown(),
                    rows.len(),
                    copied_at.elapsed().as_secs_f64()
                ),
            );
        }
        self.follow_event(
            JsonObject::new()
                .string("event", "copied")
                .number("tables", counts.tables)
                .number("rows", counts.rows_copied)
                .number("elapsed_ms", started.elapsed().as_millis() as u64),
            &format!("  copied          {} tables, {} rows; now following\n", counts.tables, counts.rows_copied),
        );

        let _ = stop_signals::watch();
        let poll = Duration::from_secs(self.request.follow.poll_seconds.unwrap_or(DEFAULT_POLL_SECONDS));
        let mut polls = 0u64;
        let reason = loop {
            let current: i64 = first_rows(query(&mut source, "SELECT CAST(CHANGE_TRACKING_CURRENT_VERSION() AS nvarchar(max))")?)
                .first()
                .and_then(|row| row.first())
                .map(text)
                .and_then(|value| value.parse().ok())
                .unwrap_or(version);
            let mut statements = Vec::new();
            for table in &tables {
                let minimum: i64 = first_rows(query(
                    &mut source,
                    &format!(
                        "SELECT CAST(CHANGE_TRACKING_MIN_VALID_VERSION(OBJECT_ID({})) AS nvarchar(max))",
                        quote_unicode(&table.qualified())
                    ),
                )?)
                .first()
                .and_then(|row| row.first())
                .map(text)
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
                if minimum > version {
                    return Err(source_failed(format!(
                        "cannot follow {target}: the change retention of {} was exceeded while following: changes were lost. Follow again into a new file",
                        options.database
                    )));
                }
                if table.key.is_empty() || current <= version {
                    continue;
                }
                let joined: Vec<String> = table
                    .key
                    .iter()
                    .map(|key| format!("T.{0} = CT.{0}", quote_name(key)))
                    .collect();
                let key_texts: Vec<String> = table
                    .key
                    .iter()
                    .map(|key| {
                        let data_type = table.columns.iter().find(|column| &column.name == key).map_or("", |column| column.data_type.as_str());
                        text_expression(&format!("CT.{}", quote_name(key)), data_type)
                    })
                    .collect();
                let row_texts: Vec<String> = table
                    .columns
                    .iter()
                    .map(|column| text_expression(&format!("T.{}", quote_name(&column.name)), &column.data_type))
                    .collect();
                let changes = first_rows(query(
                    &mut source,
                    &format!(
                        "SELECT CAST(CT.SYS_CHANGE_OPERATION AS nvarchar(max)), \
                         CAST(CASE WHEN T.{} IS NULL THEN 0 ELSE 1 END AS nvarchar(max)), {}, {} \
                         FROM CHANGETABLE(CHANGES {}, {version}) AS CT LEFT JOIN {} AS T ON {} \
                         WHERE CT.SYS_CHANGE_VERSION <= {current} ORDER BY CT.SYS_CHANGE_VERSION",
                        quote_name(&table.key[0]),
                        key_texts.join(", "),
                        row_texts.join(", "),
                        table.qualified(),
                        table.qualified(),
                        joined.join(" AND ")
                    ),
                )?);
                for change in &changes {
                    let key_values = &change[2..2 + table.key.len()];
                    let condition: Vec<String> = table
                        .key
                        .iter()
                        .zip(key_values)
                        .map(|(key, value)| {
                            let data_type = table.columns.iter().find(|column| &column.name == key).map_or("", |column| column.data_type.as_str());
                            format!("{} = {}", quote_name(key), literal(value, data_type))
                        })
                        .collect();
                    statements.push(format!("DELETE FROM {} WHERE {}", table.qualified(), condition.join(" AND ")));
                    let operation = text(&change[0]);
                    if operation == "D" {
                        counts.deletes += 1;
                    } else if operation == "I" {
                        counts.inserts += 1;
                    } else {
                        counts.updates += 1;
                    }
                    if text(&change[1]) == "1" {
                        let row = &change[2 + table.key.len()..];
                        let values: Vec<String> = row.iter().zip(&table.columns).map(|(value, column)| literal(value, &column.data_type)).collect();
                        let names: Vec<String> = table.columns.iter().map(|column| quote_name(&column.name)).collect();
                        statements.push(format!(
                            "INSERT INTO {} ({}) VALUES ({})",
                            table.qualified(),
                            names.join(", "),
                            values.join(", ")
                        ));
                    }
                }
            }
            polls += 1;
            let changed = !statements.is_empty();
            if changed {
                run("BEGIN TRANSACTION")?;
                for statement in &statements {
                    run(statement)?;
                }
                run("COMMIT TRANSACTION")?;
                counts.transactions += 1;
            }
            version = version.max(current);
            if self.request.follow.until_caught_up && !changed {
                break "caught_up";
            }
            if self.format != Format::Json {
                self.messages.text(&format!(
                    "  following       poll every {} s, {polls} polls, {} changes applied, at version {version}\n",
                    poll.as_secs(),
                    counts.inserts + counts.updates + counts.deletes
                ));
            }
            let slept = Instant::now();
            while slept.elapsed() < poll && !stop_signals::requested() {
                std::thread::sleep(STOP_CHECK);
            }
            if stop_signals::requested() {
                break "interrupt";
            }
        };
        drop(run);
        let _ = session.close();
        let verdict = format!(
            "{} holds every change SQL Server tracked up to version {version}; point applications at it now",
            self.file_display
        );
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("event", "stopped")
                    .boolean("ok", true)
                    .string("reason", reason)
                    .number("elapsed_ms", started.elapsed().as_millis() as u64)
                    .number("tables", counts.tables)
                    .number("rows_copied", counts.rows_copied)
                    .number("transactions", counts.transactions)
                    .number("inserts", counts.inserts)
                    .number("updates", counts.updates)
                    .number("deletes", counts.deletes)
                    .number("change_version", u64::try_from(version).unwrap_or(0))
                    .number("polls", polls)
                    .string("verdict", &verdict)
                    .line(),
            ),
            _ => self.out.text(&format!(
                "followed {} into {} for {:.0} s ({})\n  tables          {}, {} rows copied\n  changes         {} polls applied inserts {}, updates {}, deletes {}\n  position        change tracking version {version}\nok: {verdict}\n",
                options.shown(),
                self.file_display,
                started.elapsed().as_secs_f64(),
                if reason == "caught_up" { "caught up, --until caught-up" } else { "stopped by Ctrl-C or SIGTERM" },
                counts.tables,
                counts.rows_copied,
                polls,
                counts.inserts,
                counts.updates,
                counts.deletes,
            )),
        }
        Ok(Exit::Done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_are_declared_as_sql_server_writes_them() {
        assert_eq!(declared_type("nvarchar", Some("-1"), None, None, None), "nvarchar(max)");
        assert_eq!(declared_type("varchar", Some("40"), None, None, None), "varchar(40)");
        assert_eq!(declared_type("decimal", None, Some("10"), Some("2"), None), "decimal(10, 2)");
        assert_eq!(declared_type("datetime2", None, None, None, Some("3")), "datetime2(3)");
        assert_eq!(declared_type("int", None, Some("10"), Some("0"), None), "int");
    }

    #[test]
    fn values_travel_as_text_and_binary_converts_back() {
        assert_eq!(text_expression("[b]", "varbinary"), "CONVERT(nvarchar(max), [b], 1)");
        assert_eq!(text_expression("[d]", "datetime2"), "CONVERT(nvarchar(max), [d], 126)");
        assert_eq!(text_expression("[n]", "int"), "CAST([n] AS nvarchar(max))");
        assert_eq!(literal(&Some("0x0A".to_owned()), "varbinary"), "CONVERT(varbinary(max), N'0x0A', 1)");
        assert_eq!(literal(&None, "int"), "NULL");
        assert_eq!(literal(&Some("O'Neil".to_owned()), "nvarchar"), "N'O''Neil'");
    }
}
