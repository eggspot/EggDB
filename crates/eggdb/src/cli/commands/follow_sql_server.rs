use super::follow::{FollowCounts, source_failed};
use super::{Access, Session};
use crate::cli::args::{CommandName, Format, SqlServerChanges};
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
const TRACKING_VERSION_QUERY: &str =
    "SELECT CAST(CHANGE_TRACKING_CURRENT_VERSION() AS nvarchar(max))";
const CAPTURE_ENABLED_QUERY: &str =
    "SELECT CAST(is_cdc_enabled AS nvarchar(max)) FROM sys.databases WHERE database_id = DB_ID()";
const MAX_LSN_QUERY: &str = "SELECT CONVERT(nvarchar(max), sys.fn_cdc_get_max_lsn(), 1)";
const CAPTURE_INSTANCES_QUERY: &str = "SELECT CAST(s.name AS nvarchar(max)), CAST(t.name AS nvarchar(max)), \
     CAST(c.capture_instance AS nvarchar(max)), \
     CAST(CASE WHEN (SELECT COUNT(*) FROM cdc.captured_columns cc WHERE cc.object_id = c.object_id) \
     = (SELECT COUNT(*) FROM sys.columns sc WHERE sc.object_id = t.object_id) THEN 1 ELSE 0 END AS nvarchar(max)) \
     FROM cdc.change_tables c JOIN sys.tables t ON t.object_id = c.source_object_id \
     JOIN sys.schemas s ON s.schema_id = t.schema_id ORDER BY c.create_date DESC";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Lsn(u128);

impl Lsn {
    const BYTES: usize = 10;

    fn parse(text: &str) -> Option<Self> {
        let digits = text
            .strip_prefix("0x")
            .or_else(|| text.strip_prefix("0X"))?;
        if digits.is_empty()
            || digits.len() > 2 * Self::BYTES
            || !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        u128::from_str_radix(digits, 16).ok().map(Self)
    }

    fn literal(self) -> String {
        format!("0x{:020X}", self.0)
    }

    fn is_valid(self) -> bool {
        self.0 != 0
    }
}

fn parse_lsn(value: &Option<String>) -> Option<Lsn> {
    value.as_deref().and_then(Lsn::parse)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowChange {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureRow {
    BeforeUpdate,
    Change(RowChange),
}

fn capture_operation(code: &str) -> Option<CaptureRow> {
    match code {
        "1" => Some(CaptureRow::Change(RowChange::Delete)),
        "2" => Some(CaptureRow::Change(RowChange::Insert)),
        "3" => Some(CaptureRow::BeforeUpdate),
        "4" => Some(CaptureRow::Change(RowChange::Update)),
        _ => None,
    }
}

fn tracking_operation(code: &str) -> RowChange {
    match code {
        "D" => RowChange::Delete,
        "I" => RowChange::Insert,
        _ => RowChange::Update,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChangeSource {
    Tracking,
    Capture { instance: String, next: Lsn },
}

impl ChangeSource {
    fn name(&self) -> &'static str {
        match self {
            Self::Tracking => "change_tracking",
            Self::Capture { .. } => "cdc",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Tracking => "change tracking",
            Self::Capture { .. } => "CDC",
        }
    }
}

fn choose_changes(
    forced: Option<SqlServerChanges>,
    tracked: bool,
    instance: Option<&str>,
) -> Option<ChangeSource> {
    let capture = |instance: &str| ChangeSource::Capture {
        instance: instance.to_owned(),
        next: Lsn(0),
    };
    match (forced, instance) {
        (Some(SqlServerChanges::ChangeTracking), _) | (None, None) => {
            tracked.then_some(ChangeSource::Tracking)
        }
        (Some(SqlServerChanges::ChangeDataCapture) | None, Some(instance)) => {
            Some(capture(instance))
        }
        (Some(SqlServerChanges::ChangeDataCapture), None) => None,
    }
}

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
    changes: ChangeSource,
}

impl SourceTable {
    fn shown(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    fn qualified(&self) -> String {
        format!("{}.{}", quote_name(&self.schema), quote_name(&self.name))
    }

    fn column_type(&self, name: &str) -> &str {
        self.columns
            .iter()
            .find(|column| column.name == name)
            .map_or("", |column| column.data_type.as_str())
    }

    fn column_names(&self) -> Vec<String> {
        self.columns
            .iter()
            .map(|column| quote_name(&column.name))
            .collect()
    }
}

struct Unfollowable {
    schema: String,
    name: String,
    partly_captured: bool,
}

impl Unfollowable {
    fn shown(&self) -> String {
        if self.partly_captured {
            format!(
                "{}.{} (its CDC capture instance leaves out columns)",
                self.schema, self.name
            )
        } else {
            format!("{}.{}", self.schema, self.name)
        }
    }
}

fn is_binary(data_type: &str) -> bool {
    matches!(
        data_type,
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion"
    )
}

fn text_expression(column: &str, data_type: &str) -> String {
    let style = match data_type {
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion" => Some(1),
        "datetime" | "datetime2" | "smalldatetime" | "date" | "time" | "datetimeoffset" => {
            Some(126)
        }
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
        Some(text) if is_binary(data_type) => {
            format!("CONVERT(varbinary(max), {}, 1)", quote_unicode(text))
        }
        Some(text) => quote_unicode(text),
    }
}

fn declared_type(
    data_type: &str,
    length: Option<&str>,
    precision: Option<&str>,
    scale: Option<&str>,
    datetime_precision: Option<&str>,
) -> String {
    match data_type {
        "char" | "varchar" | "nchar" | "nvarchar" | "binary" | "varbinary" => match length {
            Some("-1") => format!("{data_type}(max)"),
            Some(length) => format!("{data_type}({length})"),
            None => data_type.to_owned(),
        },
        "decimal" | "numeric" => format!(
            "{data_type}({}, {})",
            precision.unwrap_or("18"),
            scale.unwrap_or("0")
        ),
        "datetime2" | "time" | "datetimeoffset" => {
            format!("{data_type}({})", datetime_precision.unwrap_or("7"))
        }
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

fn cell(row: &[Option<String>], index: usize) -> String {
    row.get(index).map(text).unwrap_or_default()
}

fn precondition_failed(message: String, try_command: String) -> Failure {
    Failure::Database {
        message,
        try_command: Some(try_command),
        exit: Exit::DatabaseError,
        code: "source_failed",
    }
}

fn enable_capture_command(schema: &str, name: &str) -> String {
    format!(
        "EXEC sys.sp_cdc_enable_table @source_schema = {}, @source_name = {}, @role_name = NULL;",
        quote_unicode(schema),
        quote_unicode(name)
    )
}

fn key_condition(table: &SourceTable, key_values: &[Option<String>]) -> String {
    let condition: Vec<String> = table
        .key
        .iter()
        .zip(key_values)
        .map(|(key, value)| {
            format!(
                "{} = {}",
                quote_name(key),
                literal(value, table.column_type(key))
            )
        })
        .collect();
    condition.join(" AND ")
}

fn change_statements(
    table: &SourceTable,
    key_values: &[Option<String>],
    row: Option<&[Option<String>]>,
) -> Option<Vec<String>> {
    if key_values.len() != table.key.len() {
        return None;
    }
    let mut statements = vec![format!(
        "DELETE FROM {} WHERE {}",
        table.qualified(),
        key_condition(table, key_values)
    )];
    if let Some(row) = row {
        if row.len() != table.columns.len() {
            return None;
        }
        let values: Vec<String> = row
            .iter()
            .zip(&table.columns)
            .map(|(value, column)| literal(value, &column.data_type))
            .collect();
        statements.push(format!(
            "INSERT INTO {} ({}) VALUES ({})",
            table.qualified(),
            table.column_names().join(", "),
            values.join(", ")
        ));
    }
    Some(statements)
}

fn capture_key_values(table: &SourceTable, row: &[Option<String>]) -> Vec<Option<String>> {
    table
        .key
        .iter()
        .map(|key| {
            table
                .columns
                .iter()
                .position(|column| &column.name == key)
                .and_then(|index| row.get(index).cloned().flatten())
        })
        .collect()
}

fn tracking_changes_query(table: &SourceTable, from: i64, to: i64) -> String {
    let joined: Vec<String> = table
        .key
        .iter()
        .map(|key| format!("T.{0} = CT.{0}", quote_name(key)))
        .collect();
    let key_texts: Vec<String> = table
        .key
        .iter()
        .map(|key| text_expression(&format!("CT.{}", quote_name(key)), table.column_type(key)))
        .collect();
    let row_texts: Vec<String> = table
        .columns
        .iter()
        .map(|column| {
            text_expression(
                &format!("T.{}", quote_name(&column.name)),
                &column.data_type,
            )
        })
        .collect();
    format!(
        "SELECT CAST(CT.SYS_CHANGE_OPERATION AS nvarchar(max)), \
         CAST(CASE WHEN T.{} IS NULL THEN 0 ELSE 1 END AS nvarchar(max)), {}, {} \
         FROM CHANGETABLE(CHANGES {}, {from}) AS CT LEFT JOIN {} AS T ON {} \
         WHERE CT.SYS_CHANGE_VERSION <= {to} ORDER BY CT.SYS_CHANGE_VERSION",
        quote_name(table.key.first().map_or("", String::as_str)),
        key_texts.join(", "),
        row_texts.join(", "),
        table.qualified(),
        table.qualified(),
        joined.join(" AND ")
    )
}

fn capture_changes_query(table: &SourceTable, instance: &str, from: Lsn, to: Lsn) -> String {
    let row_texts: Vec<String> = table
        .columns
        .iter()
        .map(|column| {
            text_expression(
                &format!("C.{}", quote_name(&column.name)),
                &column.data_type,
            )
        })
        .collect();
    format!(
        "SELECT CAST(C.[__$operation] AS nvarchar(max)), {} \
         FROM cdc.{}({}, {}, N'all update old') AS C \
         ORDER BY C.[__$start_lsn], C.[__$seqval], C.[__$operation]",
        row_texts.join(", "),
        quote_name(&format!("fn_cdc_get_all_changes_{instance}")),
        from.literal(),
        to.literal()
    )
}

fn capture_bounds_query(instance: &str, after: Lsn) -> String {
    format!(
        "SELECT CONVERT(nvarchar(max), sys.fn_cdc_get_min_lsn({}), 1), \
         CONVERT(nvarchar(max), sys.fn_cdc_increment_lsn({}), 1)",
        quote_unicode(instance),
        after.literal()
    )
}

fn position_text(version: Option<i64>, lsn: Option<Lsn>) -> String {
    let mut parts = Vec::new();
    if let Some(version) = version {
        parts.push(format!("change tracking version {version}"));
    }
    if let Some(lsn) = lsn {
        parts.push(format!("CDC LSN {}", lsn.literal()));
    }
    if parts.is_empty() {
        "the copy (no tables)".to_owned()
    } else {
        parts.join(", ")
    }
}

fn count_change(counts: &mut FollowCounts, change: RowChange) {
    match change {
        RowChange::Insert => counts.inserts += 1,
        RowChange::Update => counts.updates += 1,
        RowChange::Delete => counts.deletes += 1,
    }
}

fn first_value<T>(
    rows: &[Vec<Option<String>>],
    parse: impl Fn(&Option<String>) -> Option<T>,
) -> Option<T> {
    rows.first().and_then(|row| row.first()).and_then(parse)
}

fn whole_number(value: &Option<String>) -> Option<i64> {
    value.as_deref().and_then(|text| text.parse().ok())
}

impl Session<'_> {
    pub(super) fn follow_sql_server(&mut self) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Follow));
        if self.request.follow.publication.is_some() {
            return Err(usage(
                "--publication is for PostgreSQL sources; SQL Server uses change tracking or CDC"
                    .to_owned(),
            ));
        }
        let mut options = parse_sql_server_url(&self.request.follow.source).map_err(usage)?;
        options.password = self.source_password(PASSWORD_VARIABLE)?;
        self.follow_tds(options)
    }

    fn follow_tds(&mut self, options: TdsConnectOptions) -> Result<Exit, Failure> {
        let target = format!("{}:{}/{}", options.host, options.port, options.database);
        let existed = self.request.file.exists();
        let started = Instant::now();
        let forced = self.request.follow.sql_server_changes;
        let mut source =
            TdsClient::connect(&options).map_err(|error| tds_failure(&target, &error))?;
        let query = |source: &mut TdsClient, sql: &str| {
            source
                .batch(sql)
                .map_err(|error| tds_failure(&target, &error))
        };
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
                if source.encrypted() {
                    "encrypted"
                } else {
                    "not encrypted"
                }
            ),
        );
        let tracking_on = if forced == Some(SqlServerChanges::ChangeDataCapture) {
            false
        } else {
            match source.batch(
                "SELECT CAST(retention_period AS nvarchar(max)) FROM sys.change_tracking_databases WHERE database_id = DB_ID()",
            ) {
                Ok(results) => !first_rows(results).is_empty(),
                Err(TdsClientError::Server { number: INVALID_OBJECT_NAME, .. }) => false,
                Err(error) => return Err(tds_failure(&target, &error)),
            }
        };
        let capture_on = if forced == Some(SqlServerChanges::ChangeTracking) {
            false
        } else {
            match source.batch(CAPTURE_ENABLED_QUERY) {
                Ok(results) => first_value(&first_rows(results), |value| Some(text(value)))
                    .is_some_and(|enabled| enabled == "1"),
                Err(TdsClientError::Server { .. }) => false,
                Err(error) => return Err(tds_failure(&target, &error)),
            }
        };
        match forced {
            Some(SqlServerChanges::ChangeDataCapture) if !capture_on => {
                return Err(precondition_failed(
                    format!(
                        "cannot follow {target}: change data capture (CDC) is off for {}, so --changes cdc could not see changes after the copy. Turn it on, then follow again",
                        options.database
                    ),
                    format!(
                        "USE {}; EXEC sys.sp_cdc_enable_db;",
                        quote_name(&options.database)
                    ),
                ));
            }
            Some(SqlServerChanges::ChangeTracking) | None if !tracking_on && !capture_on => {
                return Err(precondition_failed(
                    format!(
                        "cannot follow {target}: change tracking is off for {}{}, so changes after the copy could not be seen. Turn it on, then follow again",
                        options.database,
                        if forced.is_none() {
                            ", and so is change data capture (CDC, sys.sp_cdc_enable_db)"
                        } else {
                            ""
                        }
                    ),
                    format!(
                        "ALTER DATABASE {} SET CHANGE_TRACKING = ON (CHANGE_RETENTION = 2 DAYS, AUTO_CLEANUP = ON);",
                        quote_name(&options.database)
                    ),
                ));
            }
            _ => {}
        }
        let instances: Vec<(String, String, String, bool)> = if capture_on {
            first_rows(query(&mut source, CAPTURE_INSTANCES_QUERY)?)
                .iter()
                .map(|row| {
                    (
                        cell(row, 0),
                        cell(row, 1),
                        cell(row, 2),
                        cell(row, 3) == "1",
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        let schema_filter = if self.request.follow.schemas.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = self
                .request
                .follow
                .schemas
                .iter()
                .map(|name| quote_unicode(name))
                .collect();
            format!(" AND s.name IN ({})", names.join(", "))
        };
        let listed = first_rows(query(
            &mut source,
            &format!(
                "SELECT CAST(s.name AS nvarchar(max)), CAST(t.name AS nvarchar(max)), \
                 CAST(CASE WHEN ct.object_id IS NULL THEN 0 ELSE 1 END AS nvarchar(max)) \
                 FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id \
                 LEFT JOIN sys.change_tracking_tables ct ON ct.object_id = t.object_id \
                 WHERE t.is_ms_shipped = 0 AND s.name <> N'cdc' \
                 AND NOT (s.name = N'dbo' AND t.name = N'systranschemas'){schema_filter} ORDER BY s.name, t.name"
            ),
        )?);
        let mut chosen = Vec::new();
        let mut unfollowable = Vec::new();
        for row in &listed {
            let (schema, name) = (cell(row, 0), cell(row, 1));
            let tracked = tracking_on && cell(row, 2) == "1";
            let instance = instances
                .iter()
                .find(|(instance_schema, instance_table, _, _)| {
                    *instance_schema == schema && *instance_table == name
                });
            let complete = instance
                .filter(|(_, _, _, complete)| *complete)
                .map(|(_, _, instance, _)| instance.as_str());
            match choose_changes(forced, tracked, complete) {
                Some(changes) => chosen.push((schema, name, changes)),
                None => unfollowable.push(Unfollowable {
                    partly_captured: instance.is_some() && complete.is_none(),
                    schema,
                    name,
                }),
            }
        }
        if let Some(first) = unfollowable.first() {
            let listed: Vec<String> = unfollowable.iter().map(Unfollowable::shown).collect();
            let (what, try_command) = match forced {
                Some(SqlServerChanges::ChangeTracking) => (
                    "are not tracked",
                    format!(
                        "ALTER TABLE {}.{} ENABLE CHANGE_TRACKING;",
                        first.schema, first.name
                    ),
                ),
                Some(SqlServerChanges::ChangeDataCapture) => (
                    "have no CDC capture instance of every column",
                    enable_capture_command(&first.schema, &first.name),
                ),
                None if capture_on && !tracking_on => (
                    "have no CDC capture instance of every column",
                    enable_capture_command(&first.schema, &first.name),
                ),
                None if capture_on => (
                    "have neither change tracking nor a CDC capture instance of every column",
                    format!(
                        "ALTER TABLE {}.{} ENABLE CHANGE_TRACKING;",
                        first.schema, first.name
                    ),
                ),
                None => (
                    "are not tracked",
                    format!(
                        "ALTER TABLE {}.{} ENABLE CHANGE_TRACKING;",
                        first.schema, first.name
                    ),
                ),
            };
            return Err(precondition_failed(
                format!(
                    "cannot follow {target}: {} tables {what}: {}; their changes could not be seen",
                    unfollowable.len(),
                    listed.join(", ")
                ),
                try_command,
            ));
        }
        let mut tables = Vec::new();
        for (schema, name, changes) in chosen {
            let filter = format!(
                "TABLE_SCHEMA = {} AND TABLE_NAME = {}",
                quote_unicode(&schema),
                quote_unicode(&name)
            );
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
                let data_type = cell(column, 1);
                let declared = format!(
                    "{}{}",
                    declared_type(
                        &data_type,
                        column.get(2).and_then(Option::as_deref),
                        column.get(3).and_then(Option::as_deref),
                        column.get(4).and_then(Option::as_deref),
                        column.get(5).and_then(Option::as_deref),
                    ),
                    if cell(column, 6) == "NO" { " NOT NULL" } else { "" }
                );
                SourceColumn { name: cell(column, 0), data_type, declared }
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
            .map(|row| cell(row, 0))
            .collect();
            tables.push(SourceTable {
                schema,
                name,
                columns,
                key,
                changes,
            });
        }
        let keyless: Vec<String> = tables
            .iter()
            .filter(|table| table.key.is_empty() && table.changes != ChangeSource::Tracking)
            .map(SourceTable::shown)
            .collect();
        if !keyless.is_empty() {
            return Err(source_failed(format!(
                "cannot follow {target}: {} tables read through CDC have no primary key, so their changes could not be applied by key: {}. Add a primary key, then follow again",
                keyless.len(),
                keyless.join(", ")
            )));
        }
        let uses_tracking = tables
            .iter()
            .any(|table| table.changes == ChangeSource::Tracking);
        let uses_capture = tables
            .iter()
            .any(|table| table.changes != ChangeSource::Tracking);
        let mut version: Option<i64> = if uses_tracking {
            Some(
                first_value(
                    &first_rows(query(&mut source, TRACKING_VERSION_QUERY)?),
                    whole_number,
                )
                .unwrap_or(0),
            )
        } else {
            None
        };
        let mut lsn: Option<Lsn> = if uses_capture {
            Some(
                first_value(&first_rows(query(&mut source, MAX_LSN_QUERY)?), parse_lsn)
                    .unwrap_or(Lsn(0)),
            )
        } else {
            None
        };
        if let Some(start) = lsn {
            for table in &mut tables {
                let shown = table.shown();
                if let ChangeSource::Capture { instance, next } = &mut table.changes {
                    let bounds =
                        first_rows(query(&mut source, &capture_bounds_query(instance, start))?);
                    let minimum = first_value(&bounds, parse_lsn).filter(|lsn| lsn.is_valid());
                    let following = bounds
                        .first()
                        .and_then(|row| row.get(1))
                        .and_then(parse_lsn);
                    let (Some(minimum), Some(following)) = (minimum, following) else {
                        return Err(source_failed(format!(
                            "cannot follow {target}: the CDC changes of {shown} cannot be read: capture instance {instance} is gone or this login may not read it. Grant the login its gating role, then follow again"
                        )));
                    };
                    *next = minimum.max(following);
                }
            }
        }

        let db = self.open(
            Access::Write {
                create: self.request.create,
            },
            None,
        )?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::SqlServer);
        let mut run = |sql: &str| -> Result<(), Failure> {
            session.execute(sql, &mut |_| {}).map_err(|error| {
                source_failed(format!(
                    "cannot write into the file: {} ({})",
                    error.message(),
                    sql.chars().take(200).collect::<String>()
                ))
            })
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
            let rows = first_rows(query(
                &mut source,
                &format!("SELECT {} FROM {}", selected.join(", "), table.qualified()),
            )?);
            let names = table.column_names();
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
                run(&format!(
                    "INSERT INTO {} ({}) VALUES {}",
                    table.qualified(),
                    names.join(", "),
                    values.join(", ")
                ))?;
            }
            run("COMMIT TRANSACTION")?;
            counts.tables += 1;
            counts.rows_copied += rows.len() as u64;
            self.follow_event(
                JsonObject::new()
                    .string("event", "table_copied")
                    .string("table", &table.shown())
                    .number("rows", rows.len() as u64)
                    .number("elapsed_ms", copied_at.elapsed().as_millis() as u64)
                    .string("changes", table.changes.name()),
                &format!(
                    "  copied          {:<28} {:>10} rows   {:.1} s   {}\n",
                    table.shown(),
                    rows.len(),
                    copied_at.elapsed().as_secs_f64(),
                    table.changes.label()
                ),
            );
        }
        self.follow_event(
            JsonObject::new()
                .string("event", "copied")
                .number("tables", counts.tables)
                .number("rows", counts.rows_copied)
                .number("elapsed_ms", started.elapsed().as_millis() as u64),
            &format!(
                "  copied          {} tables, {} rows; now following\n",
                counts.tables, counts.rows_copied
            ),
        );

        let _ = stop_signals::watch();
        let poll = Duration::from_secs(
            self.request
                .follow
                .poll_seconds
                .unwrap_or(DEFAULT_POLL_SECONDS),
        );
        let mut polls = 0u64;
        let reason = loop {
            let current_version = match version {
                Some(version) => Some(
                    first_value(
                        &first_rows(query(&mut source, TRACKING_VERSION_QUERY)?),
                        whole_number,
                    )
                    .unwrap_or(version),
                ),
                None => None,
            };
            let current_lsn = match lsn {
                Some(lsn) => Some(
                    first_value(&first_rows(query(&mut source, MAX_LSN_QUERY)?), parse_lsn)
                        .unwrap_or(lsn)
                        .max(lsn),
                ),
                None => None,
            };
            let mut statements = Vec::new();
            for table in &mut tables {
                let advanced = match &table.changes {
                    ChangeSource::Tracking => {
                        let (Some(from), Some(current)) = (version, current_version) else {
                            continue;
                        };
                        let minimum = first_value(
                            &first_rows(query(
                                &mut source,
                                &format!(
                                    "SELECT CAST(CHANGE_TRACKING_MIN_VALID_VERSION(OBJECT_ID({})) AS nvarchar(max))",
                                    quote_unicode(&table.qualified())
                                ),
                            )?),
                            whole_number,
                        )
                        .unwrap_or(0);
                        if minimum > from {
                            return Err(source_failed(format!(
                                "cannot follow {target}: the change retention of {} was exceeded while following: changes were lost. Follow again into a new file",
                                options.database
                            )));
                        }
                        if table.key.is_empty() || current <= from {
                            continue;
                        }
                        let changes = first_rows(query(
                            &mut source,
                            &tracking_changes_query(table, from, current),
                        )?);
                        let key_length = table.key.len();
                        for change in &changes {
                            let key_values = change.get(2..2 + key_length).unwrap_or_default();
                            let row = (cell(change, 1) == "1")
                                .then(|| change.get(2 + key_length..).unwrap_or_default());
                            statements.extend(
                                change_statements(table, key_values, row)
                                    .ok_or_else(|| malformed_change(&target, &table.shown()))?,
                            );
                            count_change(&mut counts, tracking_operation(&cell(change, 0)));
                        }
                        None
                    }
                    ChangeSource::Capture { instance, next } => {
                        let (Some(to), next) = (current_lsn, *next) else {
                            continue;
                        };
                        let bounds =
                            first_rows(query(&mut source, &capture_bounds_query(instance, to))?);
                        let minimum = first_value(&bounds, parse_lsn).unwrap_or(Lsn(0));
                        if !minimum.is_valid() || minimum > next {
                            return Err(source_failed(capture_lost(
                                &target,
                                &table.shown(),
                                instance,
                                minimum,
                                next,
                            )));
                        }
                        if next > to {
                            continue;
                        }
                        let following = bounds
                            .first()
                            .and_then(|row| row.get(1))
                            .and_then(parse_lsn)
                            .ok_or_else(|| malformed_change(&target, &table.shown()))?;
                        let changes = first_rows(query(
                            &mut source,
                            &capture_changes_query(table, instance, next, to),
                        )?);
                        for change in &changes {
                            let Some(step) = capture_operation(&cell(change, 0)) else {
                                continue;
                            };
                            let row = change.get(1..).unwrap_or_default();
                            let image = match step {
                                CaptureRow::BeforeUpdate
                                | CaptureRow::Change(RowChange::Delete) => None,
                                CaptureRow::Change(_) => Some(row),
                            };
                            statements.extend(
                                change_statements(table, &capture_key_values(table, row), image)
                                    .ok_or_else(|| malformed_change(&target, &table.shown()))?,
                            );
                            if let CaptureRow::Change(operation) = step {
                                count_change(&mut counts, operation);
                            }
                        }
                        Some(following)
                    }
                };
                if let (Some(advanced), ChangeSource::Capture { next, .. }) =
                    (advanced, &mut table.changes)
                {
                    *next = advanced;
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
            version = version.max(current_version);
            lsn = lsn.max(current_lsn);
            if self.request.follow.until_caught_up && !changed {
                break "caught_up";
            }
            if self.format != Format::Json {
                self.messages.text(&format!(
                    "  following       poll every {} s, {polls} polls, {} changes applied, at {}\n",
                    poll.as_secs(),
                    counts.inserts + counts.updates + counts.deletes,
                    position_text(version, lsn)
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
        let position = position_text(version, lsn);
        let verdict = format!(
            "{} holds every change SQL Server recorded up to {position}; point applications at it now",
            self.file_display
        );
        match self.format {
            Format::Json => {
                let summary = JsonObject::new()
                    .string("event", "stopped")
                    .boolean("ok", true)
                    .string("reason", reason)
                    .number("elapsed_ms", started.elapsed().as_millis() as u64)
                    .number("tables", counts.tables)
                    .number("rows_copied", counts.rows_copied)
                    .number("transactions", counts.transactions)
                    .number("inserts", counts.inserts)
                    .number("updates", counts.updates)
                    .number("deletes", counts.deletes);
                let summary = match version {
                    Some(version) => {
                        summary.number("change_version", u64::try_from(version).unwrap_or(0))
                    }
                    None => summary,
                };
                self.out.text(
                    &summary
                        .string_if_any("lsn", lsn.map(Lsn::literal).as_deref())
                        .number("polls", polls)
                        .string("verdict", &verdict)
                        .line(),
                )
            }
            _ => self.out.text(&format!(
                "followed {} into {} for {:.0} s ({})\n  tables          {}, {} rows copied\n  changes         {} polls applied inserts {}, updates {}, deletes {}\n  position        {position}\nok: {verdict}\n",
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

fn malformed_change(target: &str, table: &str) -> Failure {
    source_failed(format!(
        "cannot follow {target}: SQL Server sent a change of {table} whose values do not match its columns"
    ))
}

fn capture_lost(target: &str, table: &str, instance: &str, minimum: Lsn, next: Lsn) -> String {
    if minimum.is_valid() {
        format!(
            "cannot follow {target}: CDC cleanup removed changes of {table} before they were applied (capture instance {instance} now starts at {}, the copy needs {}). Follow again into a new file to copy it afresh, and raise the cleanup job's retention",
            minimum.literal(),
            next.literal()
        )
    } else {
        format!(
            "cannot follow {target}: the CDC capture instance {instance} of {table} was dropped or can no longer be read while following. Follow again into a new file to copy it afresh"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_are_declared_as_sql_server_writes_them() {
        assert_eq!(
            declared_type("nvarchar", Some("-1"), None, None, None),
            "nvarchar(max)"
        );
        assert_eq!(
            declared_type("varchar", Some("40"), None, None, None),
            "varchar(40)"
        );
        assert_eq!(
            declared_type("decimal", None, Some("10"), Some("2"), None),
            "decimal(10, 2)"
        );
        assert_eq!(
            declared_type("datetime2", None, None, None, Some("3")),
            "datetime2(3)"
        );
        assert_eq!(
            declared_type("int", None, Some("10"), Some("0"), None),
            "int"
        );
    }

    #[test]
    fn values_travel_as_text_and_binary_converts_back() {
        assert_eq!(
            text_expression("[b]", "varbinary"),
            "CONVERT(nvarchar(max), [b], 1)"
        );
        assert_eq!(
            text_expression("[d]", "datetime2"),
            "CONVERT(nvarchar(max), [d], 126)"
        );
        assert_eq!(text_expression("[n]", "int"), "CAST([n] AS nvarchar(max))");
        assert_eq!(
            literal(&Some("0x0A".to_owned()), "varbinary"),
            "CONVERT(varbinary(max), N'0x0A', 1)"
        );
        assert_eq!(literal(&None, "int"), "NULL");
        assert_eq!(
            literal(&Some("O'Neil".to_owned()), "nvarchar"),
            "N'O''Neil'"
        );
    }

    fn orders() -> SourceTable {
        let column = |name: &str, data_type: &str| SourceColumn {
            name: name.to_owned(),
            data_type: data_type.to_owned(),
            declared: data_type.to_owned(),
        };
        SourceTable {
            schema: "dbo".to_owned(),
            name: "orders".to_owned(),
            columns: vec![
                column("note", "nvarchar"),
                column("id", "int"),
                column("tag", "varbinary"),
            ],
            key: vec!["id".to_owned()],
            changes: ChangeSource::Capture {
                instance: "dbo_orders".to_owned(),
                next: Lsn(1),
            },
        }
    }

    #[test]
    fn lsns_read_and_write_as_sql_server_binary_ten_literals() {
        let lsn = Lsn::parse("0x0000002A000001F80003").unwrap();
        assert_eq!(lsn.literal(), "0x0000002A000001F80003");
        assert_eq!(
            Lsn::parse("0x2a").unwrap().literal(),
            "0x0000000000000000002A"
        );
        assert!(Lsn::parse("0x0000002A000001F80003") > Lsn::parse("0x0000002A000001F80002"));
        assert!(Lsn::parse("0x00000030000000100001") > Lsn::parse("0x0000002F00000FFF00FF"));
        for bad in ["", "0x", "2A", "0xZZ", "0x+1", "0x000000000000000000001"] {
            assert_eq!(Lsn::parse(bad), None, "{bad}");
        }
        assert!(!Lsn::parse("0x00000000000000000000").unwrap().is_valid());
        assert_eq!(parse_lsn(&None), None);
    }

    #[test]
    fn cdc_operations_map_to_row_changes_and_before_images_only_remove_the_old_key() {
        assert_eq!(
            capture_operation("1"),
            Some(CaptureRow::Change(RowChange::Delete))
        );
        assert_eq!(
            capture_operation("2"),
            Some(CaptureRow::Change(RowChange::Insert))
        );
        assert_eq!(capture_operation("3"), Some(CaptureRow::BeforeUpdate));
        assert_eq!(
            capture_operation("4"),
            Some(CaptureRow::Change(RowChange::Update))
        );
        assert_eq!(capture_operation("5"), None);
        assert_eq!(capture_operation(""), None);
        assert_eq!(tracking_operation("D"), RowChange::Delete);
        assert_eq!(tracking_operation("I"), RowChange::Insert);
        assert_eq!(tracking_operation("U"), RowChange::Update);
    }

    #[test]
    fn cdc_is_chosen_when_the_table_has_a_capture_instance_unless_forced() {
        let capture = Some(ChangeSource::Capture {
            instance: "dbo_t".to_owned(),
            next: Lsn(0),
        });
        assert_eq!(choose_changes(None, true, Some("dbo_t")), capture);
        assert_eq!(choose_changes(None, false, Some("dbo_t")), capture);
        assert_eq!(
            choose_changes(None, true, None),
            Some(ChangeSource::Tracking)
        );
        assert_eq!(choose_changes(None, false, None), None);
        let tracking = Some(SqlServerChanges::ChangeTracking);
        assert_eq!(
            choose_changes(tracking, true, Some("dbo_t")),
            Some(ChangeSource::Tracking)
        );
        assert_eq!(choose_changes(tracking, false, Some("dbo_t")), None);
        let cdc = Some(SqlServerChanges::ChangeDataCapture);
        assert_eq!(choose_changes(cdc, true, Some("dbo_t")), capture);
        assert_eq!(choose_changes(cdc, true, None), None);
    }

    #[test]
    fn cdc_queries_read_one_lsn_range_of_the_capture_instance() {
        let from = Lsn::parse("0x0000002A000001F80003").unwrap();
        let to = Lsn::parse("0x0000002B000000100001").unwrap();
        assert_eq!(
            capture_changes_query(&orders(), "dbo_orders", from, to),
            "SELECT CAST(C.[__$operation] AS nvarchar(max)), CAST(C.[note] AS nvarchar(max)), \
             CAST(C.[id] AS nvarchar(max)), CONVERT(nvarchar(max), C.[tag], 1) \
             FROM cdc.[fn_cdc_get_all_changes_dbo_orders](0x0000002A000001F80003, 0x0000002B000000100001, N'all update old') AS C \
             ORDER BY C.[__$start_lsn], C.[__$seqval], C.[__$operation]"
        );
        assert_eq!(
            capture_bounds_query("it's", to),
            "SELECT CONVERT(nvarchar(max), sys.fn_cdc_get_min_lsn(N'it''s'), 1), \
             CONVERT(nvarchar(max), sys.fn_cdc_increment_lsn(0x0000002B000000100001), 1)"
        );
    }

    #[test]
    fn a_change_deletes_by_key_and_inserts_the_row_image() {
        let table = orders();
        let row = vec![Some("hi".to_owned()), Some("7".to_owned()), None];
        let key = capture_key_values(&table, &row);
        assert_eq!(key, vec![Some("7".to_owned())]);
        assert_eq!(
            change_statements(&table, &key, Some(&row)).unwrap(),
            vec![
                "DELETE FROM [dbo].[orders] WHERE [id] = N'7'".to_owned(),
                "INSERT INTO [dbo].[orders] ([note], [id], [tag]) VALUES (N'hi', N'7', NULL)"
                    .to_owned(),
            ]
        );
        assert_eq!(
            change_statements(&table, &key, None).unwrap(),
            vec!["DELETE FROM [dbo].[orders] WHERE [id] = N'7'".to_owned()]
        );
        assert_eq!(change_statements(&table, &key, Some(&row[..2])), None);
        assert_eq!(change_statements(&table, &[], None), None);
        assert_eq!(capture_key_values(&table, &row[..1]), vec![None]);
    }

    #[test]
    fn the_position_names_each_change_source_in_use() {
        assert_eq!(position_text(Some(12), None), "change tracking version 12");
        assert_eq!(
            position_text(None, Some(Lsn(0x2a))),
            "CDC LSN 0x0000000000000000002A"
        );
        assert_eq!(
            position_text(Some(3), Some(Lsn(1))),
            "change tracking version 3, CDC LSN 0x00000000000000000001"
        );
        assert_eq!(position_text(None, None), "the copy (no tables)");
    }
}
