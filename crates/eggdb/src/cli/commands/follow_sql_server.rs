use super::follow::{FollowCounts, copy_stopped, source_failed};
use super::follow_state::{SQL_SERVER, SavedFollow, table_item, update_statement};
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
const LIVENESS_QUERY: &str = "SELECT CAST(1 AS nvarchar(max))";
const MAX_LSN_QUERY: &str = "SELECT CONVERT(nvarchar(max), sys.fn_cdc_get_max_lsn(), 1)";
const COPY_START_QUERY: &str = "SELECT CONVERT(nvarchar(max), sys.fn_cdc_get_max_lsn(), 1), \
     CONVERT(nvarchar(max), CAST(DATEADD(second, -1, SYSDATETIME()) AS datetime), 121), \
     CAST(DATEPART(TZOFFSET, SYSDATETIMEOFFSET()) AS nvarchar(max))";
const EXACT_AFTER_QUERY: &str = "SELECT CONVERT(nvarchar(max), CAST(DATEADD(second, 1, SYSDATETIME()) AS datetime), 121), \
     CAST(DATEPART(TZOFFSET, SYSDATETIMEOFFSET()) AS nvarchar(max))";
const CAPTURE_INSTANCES_QUERY: &str = "SELECT CAST(s.name AS nvarchar(max)), CAST(t.name AS nvarchar(max)), \
     CAST(c.capture_instance AS nvarchar(max)), \
     CAST(CASE WHEN c.supports_net_changes = 1 AND EXISTS (SELECT 1 FROM sys.indexes i \
     WHERE i.object_id = t.object_id AND i.is_primary_key = 1 AND i.name = c.index_name) THEN 1 ELSE 0 END AS nvarchar(max)), \
     CONVERT(nvarchar(max), sys.fn_cdc_get_min_lsn(c.capture_instance), 1), CAST(cc.column_name AS nvarchar(max)) \
     FROM cdc.change_tables c JOIN sys.tables t ON t.object_id = c.source_object_id \
     JOIN sys.schemas s ON s.schema_id = t.schema_id JOIN cdc.captured_columns cc ON cc.object_id = c.object_id \
     ORDER BY c.create_date DESC, c.object_id DESC, cc.column_ordinal";

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

    fn before(self) -> Self {
        Self(self.0.saturating_sub(1))
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
    Capture {
        instance: String,
        next: Lsn,
        net_changes: bool,
    },
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
            Self::Capture {
                net_changes: true, ..
            } => "CDC, net changes",
            Self::Capture { .. } => "CDC",
        }
    }

    fn net_changes(&self) -> bool {
        matches!(
            self,
            Self::Capture {
                net_changes: true,
                ..
            }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CaptureInstance {
    schema: String,
    table: String,
    name: String,
    net_changes: bool,
    minimum: Option<Lsn>,
    columns: Vec<String>,
}

impl CaptureInstance {
    fn is_of(&self, table: &SourceTable) -> bool {
        self.schema == table.schema && self.table == table.name
    }

    fn captures(&self, columns: &[String]) -> bool {
        columns.iter().all(|column| self.columns.contains(column))
    }
}

fn capture_instances(rows: &TextRows) -> Vec<CaptureInstance> {
    let mut instances: Vec<CaptureInstance> = Vec::new();
    for row in rows {
        let (schema, table, name, column) =
            (cell(row, 0), cell(row, 1), cell(row, 2), cell(row, 5));
        match instances.last_mut() {
            Some(last) if last.name == name && last.schema == schema && last.table == table => {
                last.columns.push(column);
            }
            _ => instances.push(CaptureInstance {
                schema,
                table,
                name,
                net_changes: cell(row, 3) == "1",
                minimum: row.get(4).and_then(parse_lsn).filter(|lsn| lsn.is_valid()),
                columns: vec![column],
            }),
        }
    }
    instances
}

fn instances_of<'a>(
    instances: &'a [CaptureInstance],
    schema: &str,
    table: &str,
) -> Vec<&'a CaptureInstance> {
    instances
        .iter()
        .filter(|instance| instance.schema == schema && instance.table == table)
        .collect()
}

fn capture_source(instance: &CaptureInstance) -> ChangeSource {
    ChangeSource::Capture {
        instance: instance.name.clone(),
        next: Lsn(0),
        net_changes: instance.net_changes,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyLeftOut {
    instance: String,
    columns: Vec<String>,
}

fn choose_changes(
    forced: Option<SqlServerChanges>,
    tracked: bool,
    instances: &[&CaptureInstance],
    columns: &[String],
    key: &[String],
) -> Result<ChangeSource, Option<KeyLeftOut>> {
    let usable: Vec<&CaptureInstance> = instances
        .iter()
        .copied()
        .filter(|instance| instance.captures(key))
        .collect();
    let complete = usable
        .iter()
        .copied()
        .find(|instance| instance.captures(columns));
    let partial = usable.first().copied();
    let tracking = || tracked.then_some(ChangeSource::Tracking);
    let chosen = match forced {
        Some(SqlServerChanges::ChangeTracking) => tracking(),
        Some(SqlServerChanges::ChangeDataCapture) => complete.or(partial).map(capture_source),
        None => complete
            .map(capture_source)
            .or_else(tracking)
            .or_else(|| partial.map(capture_source)),
    };
    chosen.ok_or_else(|| {
        if usable.is_empty() && forced != Some(SqlServerChanges::ChangeTracking) {
            instances.first().map(|instance| KeyLeftOut {
                instance: instance.name.clone(),
                columns: key
                    .iter()
                    .filter(|column| !instance.columns.contains(column))
                    .cloned()
                    .collect(),
            })
        } else {
            None
        }
    })
}

fn successor_of<'a>(
    instances: &'a [CaptureInstance],
    table: &SourceTable,
    current: &str,
) -> Option<&'a CaptureInstance> {
    let theirs: Vec<&CaptureInstance> = instances
        .iter()
        .filter(|instance| instance.is_of(table))
        .collect();
    let newer = match theirs.iter().position(|instance| instance.name == current) {
        Some(at) => &theirs[..at],
        None => &theirs[..],
    };
    newer
        .iter()
        .rev()
        .copied()
        .find(|instance| instance.captures(&table.key) && instance.minimum.is_some())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceColumn {
    name: String,
    data_type: String,
    declared: String,
    not_null: bool,
}

impl SourceColumn {
    fn definition(&self) -> String {
        format!(
            "{} {}{}",
            quote_name(&self.name),
            self.declared,
            if self.not_null { " NOT NULL" } else { "" }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceTable {
    schema: String,
    name: String,
    columns: Vec<SourceColumn>,
    key: Vec<String>,
    changes: ChangeSource,
    left_out: Vec<String>,
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

    fn followed_names(&self) -> Vec<String> {
        self.columns
            .iter()
            .map(|column| column.name.clone())
            .collect()
    }

    fn follows(&self, name: &str) -> bool {
        self.columns.iter().any(|column| column.name == name)
    }
}

fn split_captured(
    columns: Vec<SourceColumn>,
    captured: &[String],
) -> (Vec<SourceColumn>, Vec<String>) {
    let (followed, left_out): (Vec<SourceColumn>, Vec<SourceColumn>) = columns
        .into_iter()
        .partition(|column| captured.contains(&column.name));
    (
        followed,
        left_out.into_iter().map(|column| column.name).collect(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumedColumns {
    followed: Vec<SourceColumn>,
    left_out: Vec<String>,
    dropped: Vec<String>,
}

fn resume_capture_columns(
    table: &SourceTable,
    target: &[String],
    captured: Option<&[String]>,
) -> Result<ResumedColumns, String> {
    let dropped: Vec<String> = target
        .iter()
        .filter(|name| !table.follows(name))
        .cloned()
        .collect();
    if let Some(key) = dropped.iter().find(|name| table.key.contains(name)) {
        return Err(format!("primary key column {key} was dropped"));
    }
    let (followed, left_out): (Vec<SourceColumn>, Vec<SourceColumn>) = table
        .columns
        .iter()
        .cloned()
        .partition(|column| target.contains(&column.name));
    if let Some(captured) = captured {
        let uncaptured: Vec<&str> = followed
            .iter()
            .filter(|column| !captured.contains(&column.name))
            .map(|column| column.name.as_str())
            .collect();
        if !uncaptured.is_empty() {
            return Err(format!(
                "the file has {}, which its CDC capture instance does not capture",
                uncaptured.join(", ")
            ));
        }
    }
    Ok(ResumedColumns {
        followed,
        left_out: left_out.into_iter().map(|column| column.name).collect(),
        dropped,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SwitchPlan {
    dropped: Vec<String>,
    added: Vec<String>,
    left_out: Vec<String>,
}

fn switch_plan(table: &SourceTable, successor: &CaptureInstance, now: &[String]) -> SwitchPlan {
    let followed: Vec<&String> = now
        .iter()
        .filter(|name| successor.columns.contains(name))
        .collect();
    SwitchPlan {
        dropped: table
            .columns
            .iter()
            .map(|column| column.name.clone())
            .filter(|name| !followed.contains(&name))
            .collect(),
        added: followed
            .iter()
            .filter(|name| !table.follows(name))
            .map(|name| (*name).clone())
            .collect(),
        left_out: now
            .iter()
            .filter(|name| !followed.contains(name))
            .cloned()
            .collect(),
    }
}

struct Unfollowable {
    schema: String,
    name: String,
    key_left_out: Option<KeyLeftOut>,
}

impl Unfollowable {
    fn shown(&self) -> String {
        match &self.key_left_out {
            Some(left_out) => format!(
                "{}.{} (its CDC capture instance {} leaves out primary key column {})",
                self.schema,
                self.name,
                left_out.instance,
                left_out.columns.join(", ")
            ),
            None => format!("{}.{}", self.schema, self.name),
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

fn table_filter(schema: &str, name: &str) -> String {
    format!(
        "TABLE_SCHEMA = {} AND TABLE_NAME = {}",
        quote_unicode(schema),
        quote_unicode(name)
    )
}

fn table_columns_query(schema: &str, name: &str) -> String {
    format!(
        "SELECT CAST(COLUMN_NAME AS nvarchar(max)), CAST(DATA_TYPE AS nvarchar(max)), \
         CAST(CHARACTER_MAXIMUM_LENGTH AS nvarchar(max)), CAST(NUMERIC_PRECISION AS nvarchar(max)), \
         CAST(NUMERIC_SCALE AS nvarchar(max)), CAST(DATETIME_PRECISION AS nvarchar(max)), \
         CAST(IS_NULLABLE AS nvarchar(max)) FROM INFORMATION_SCHEMA.COLUMNS WHERE {} ORDER BY ORDINAL_POSITION",
        table_filter(schema, name)
    )
}

fn source_columns(rows: &TextRows) -> Vec<SourceColumn> {
    rows.iter()
        .map(|column| {
            let data_type = cell(column, 1);
            SourceColumn {
                name: cell(column, 0),
                declared: declared_type(
                    &data_type,
                    column.get(2).and_then(Option::as_deref),
                    column.get(3).and_then(Option::as_deref),
                    column.get(4).and_then(Option::as_deref),
                    column.get(5).and_then(Option::as_deref),
                ),
                data_type,
                not_null: cell(column, 6) == "NO",
            }
        })
        .collect()
}

fn key_query(schema: &str, name: &str) -> String {
    format!(
        "SELECT CAST(k.COLUMN_NAME AS nvarchar(max)) FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS c \
         JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE k ON k.CONSTRAINT_NAME = c.CONSTRAINT_NAME AND k.TABLE_SCHEMA = c.TABLE_SCHEMA \
         WHERE c.CONSTRAINT_TYPE = 'PRIMARY KEY' AND c.{} ORDER BY k.ORDINAL_POSITION",
        table_filter(schema, name).replace(" AND TABLE_NAME", " AND c.TABLE_NAME")
    )
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

fn enable_capture_command(schema: &str, name: &str, instance: Option<&str>) -> String {
    format!(
        "EXEC sys.sp_cdc_enable_table @source_schema = {}, @source_name = {}, @role_name = NULL{};",
        quote_unicode(schema),
        quote_unicode(name),
        instance.map_or(String::new(), |instance| format!(
            ", @capture_instance = {}",
            quote_unicode(instance)
        ))
    )
}

fn second_instance_command(schema: &str, name: &str, current: &str) -> String {
    let default = format!("{schema}_{name}");
    let second = if current == format!("{default}_2") {
        default
    } else {
        format!("{default}_2")
    };
    enable_capture_command(schema, name, Some(&second))
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

fn drop_column_statement(table: &SourceTable, column: &str) -> String {
    format!(
        "ALTER TABLE {} DROP COLUMN {}",
        table.qualified(),
        quote_name(column)
    )
}

fn add_column_statement(table: &SourceTable, column: &SourceColumn) -> String {
    format!(
        "ALTER TABLE {} ADD {} {}",
        table.qualified(),
        quote_name(&column.name),
        column.declared
    )
}

fn backfill_query(table: &SourceTable, added: &[SourceColumn]) -> String {
    let keys = table
        .key
        .iter()
        .map(|key| text_expression(&quote_name(key), table.column_type(key)));
    let values = added
        .iter()
        .map(|column| text_expression(&quote_name(&column.name), &column.data_type));
    let selected: Vec<String> = keys.chain(values).collect();
    format!("SELECT {} FROM {}", selected.join(", "), table.qualified())
}

fn backfill_statement(
    table: &SourceTable,
    added: &[SourceColumn],
    row: &[Option<String>],
) -> Option<String> {
    let key_values = row.get(..table.key.len())?;
    let values = row.get(table.key.len()..)?;
    if values.len() != added.len() || added.is_empty() {
        return None;
    }
    let assignments: Vec<String> = added
        .iter()
        .zip(values)
        .map(|(column, value)| {
            format!(
                "{} = {}",
                quote_name(&column.name),
                literal(value, &column.data_type)
            )
        })
        .collect();
    Some(format!(
        "UPDATE {} SET {} WHERE {}",
        table.qualified(),
        assignments.join(", "),
        key_condition(table, key_values)
    ))
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

fn capture_changes_query(
    table: &SourceTable,
    instance: &str,
    from: Lsn,
    to: Lsn,
    net: bool,
) -> String {
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
    let (function, option, order) = if net {
        (
            "fn_cdc_get_net_changes_",
            "N'all'",
            "C.[__$operation], C.[__$start_lsn]",
        )
    } else {
        (
            "fn_cdc_get_all_changes_",
            "N'all update old'",
            "C.[__$start_lsn], C.[__$seqval], C.[__$operation]",
        )
    };
    format!(
        "SELECT CAST(C.[__$operation] AS nvarchar(max)), {} FROM cdc.{}({}, {}, {option}) AS C ORDER BY {order}",
        row_texts.join(", "),
        quote_name(&format!("{function}{instance}")),
        from.literal(),
        to.literal()
    )
}

fn capture_bounds_query(instance: &str, after: Lsn, committed_before_copy: Option<&str>) -> String {
    let in_copy = committed_before_copy.map_or("CAST(NULL AS nvarchar(max))".to_owned(), |time| {
        format!(
            "CONVERT(nvarchar(max), sys.fn_cdc_increment_lsn(sys.fn_cdc_map_time_to_lsn(N'largest less than or equal', CONVERT(datetime, {}, 121))), 1)",
            quote_unicode(time)
        )
    });
    format!(
        "SELECT CONVERT(nvarchar(max), sys.fn_cdc_get_min_lsn({}), 1), \
         CONVERT(nvarchar(max), sys.fn_cdc_increment_lsn({}), 1), {in_copy}",
        quote_unicode(instance),
        after.literal()
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServerTime {
    time: String,
    utc_offset_minutes: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureClock {
    Behind,
    Passed,
    OffsetChanged,
}

fn server_time(rows: &TextRows, at: usize) -> Option<ServerTime> {
    let row = rows.first()?;
    let time = row
        .get(at)
        .cloned()
        .flatten()
        .filter(|time| !time.is_empty())?;
    let utc_offset_minutes = row.get(at + 1).and_then(whole_number)?;
    Some(ServerTime {
        time,
        utc_offset_minutes,
    })
}

fn copy_start(rows: &TextRows) -> Option<ServerTime> {
    server_time(rows, 1)
}

fn capture_clock_query(time: &ServerTime) -> String {
    format!(
        "SELECT CASE WHEN sys.fn_cdc_map_lsn_to_time(sys.fn_cdc_get_max_lsn()) > CONVERT(datetime, {}, 121) \
         THEN N'1' ELSE N'0' END, CAST(DATEPART(TZOFFSET, SYSDATETIMEOFFSET()) AS nvarchar(max))",
        quote_unicode(&time.time)
    )
}

fn capture_clock(time: &ServerTime, check: &TextRows) -> CaptureClock {
    let Some(row) = check.first() else {
        return CaptureClock::OffsetChanged;
    };
    if row.get(1).and_then(whole_number) != Some(time.utc_offset_minutes) {
        return CaptureClock::OffsetChanged;
    }
    if cell(row, 0) == "1" {
        CaptureClock::Passed
    } else {
        CaptureClock::Behind
    }
}

fn copy_start_after_poll(
    start: Option<ServerTime>,
    clock: Option<CaptureClock>,
) -> Option<ServerTime> {
    match clock {
        Some(CaptureClock::Passed | CaptureClock::OffsetChanged) => None,
        Some(CaptureClock::Behind) | None => start,
    }
}

fn first_change_after_copy(next: Lsn, bounds: &TextRows) -> Lsn {
    bounds
        .first()
        .and_then(|row| row.get(2))
        .and_then(parse_lsn)
        .filter(|lsn| lsn.is_valid())
        .map_or(next, |after_copy| next.max(after_copy))
}

fn exact_from_query(time: &ServerTime) -> String {
    format!(
        "SELECT CONVERT(nvarchar(max), sys.fn_cdc_increment_lsn(sys.fn_cdc_map_time_to_lsn(N'largest less than or equal', CONVERT(datetime, {}, 121))), 1)",
        quote_unicode(&time.time)
    )
}

fn exact_from_of(rows: &TextRows) -> Option<Lsn> {
    match rows.first()?.first() {
        Some(Some(text)) => Lsn::parse(text),
        _ => Some(Lsn(0)),
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum SavedPosition {
    Tracking(i64),
    Capture {
        next: Lsn,
        exact_from: Option<Lsn>,
        instance: Option<String>,
    },
}

const NO_LSN: &str = "-";

fn table_position(table: &SourceTable, version: Option<i64>, exact_from: Option<Lsn>) -> String {
    match &table.changes {
        ChangeSource::Tracking => format!("change_tracking {}", version.unwrap_or(0)),
        ChangeSource::Capture { instance, next, .. } => format!(
            "cdc {} {} {instance}",
            next.literal(),
            exact_from.map_or(NO_LSN.to_owned(), Lsn::literal)
        ),
    }
}

fn read_position(text: &str) -> Option<SavedPosition> {
    match text.split_once(' ')? {
        ("change_tracking", version) => version.parse().ok().map(SavedPosition::Tracking),
        ("cdc", rest) => {
            let mut parts = rest.splitn(3, ' ');
            let next = Lsn::parse(parts.next()?)?;
            match (parts.next(), parts.next()) {
                (None, _) => Some(SavedPosition::Capture {
                    next,
                    exact_from: None,
                    instance: None,
                }),
                (Some(exact_from), Some(instance)) if !instance.is_empty() => {
                    Some(SavedPosition::Capture {
                        next,
                        exact_from: if exact_from == NO_LSN {
                            None
                        } else {
                            Some(Lsn::parse(exact_from)?)
                        },
                        instance: Some(instance.to_owned()),
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn column_change(known: &[String], now: &[String]) -> Option<String> {
    if known == now {
        return None;
    }
    if now.is_empty() {
        return Some("the table is gone".to_owned());
    }
    let added: Vec<&str> = now
        .iter()
        .filter(|name| !known.contains(name))
        .map(String::as_str)
        .collect();
    let dropped: Vec<&str> = known
        .iter()
        .filter(|name| !now.contains(name))
        .map(String::as_str)
        .collect();
    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("added {}", added.join(", ")));
    }
    if !dropped.is_empty() {
        parts.push(format!("dropped {}", dropped.join(", ")));
    }
    if parts.is_empty() {
        parts.push("reordered its columns".to_owned());
    }
    Some(parts.join("; "))
}

fn columns_query(tables: &[SourceTable]) -> String {
    let filters: Vec<String> = tables
        .iter()
        .map(|table| {
            format!(
                "(TABLE_SCHEMA = {} AND TABLE_NAME = {})",
                quote_unicode(&table.schema),
                quote_unicode(&table.name)
            )
        })
        .collect();
    format!(
        "SELECT CAST(TABLE_SCHEMA AS nvarchar(max)), CAST(TABLE_NAME AS nvarchar(max)), CAST(COLUMN_NAME AS nvarchar(max)) \
         FROM INFORMATION_SCHEMA.COLUMNS WHERE {} ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION",
        filters.join(" OR ")
    )
}

fn columns_of(rows: &[Vec<Option<String>>], table: &SourceTable) -> Vec<String> {
    rows.iter()
        .filter(|row| cell(row, 0) == table.schema && cell(row, 1) == table.name)
        .map(|row| cell(row, 2))
        .collect()
}

fn schema_changed(target: &str, table: &str, change: &str, file: &str) -> Failure {
    source_failed(format!(
        "cannot follow {target}: the columns of {table} changed on the source ({change}), and follow does not apply SQL Server schema changes. Make the same change to the table in {file} (eggdb sql), then run this follow again to resume; or follow again into a new file"
    ))
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum Notice {
    LeftOut {
        table: String,
        instance: String,
        columns: Vec<String>,
        try_command: String,
    },
    Dropped {
        table: String,
        columns: Vec<String>,
    },
    Switched {
        table: String,
        from: String,
        to: String,
        added: Vec<String>,
        dropped: Vec<String>,
    },
}

fn notice_event(notice: &Notice) -> (JsonObject, String) {
    match notice {
        Notice::LeftOut {
            table,
            instance,
            columns,
            try_command,
        } => (
            JsonObject::new()
                .string("event", "columns_left_out")
                .string("table", table)
                .string("instance", instance)
                .string("columns", &columns.join(", "))
                .string("try", try_command),
            format!(
                "  left out        {table}: {} (CDC capture instance {instance} does not capture it; to follow it, create a second one: {try_command})\n",
                columns.join(", ")
            ),
        ),
        Notice::Dropped { table, columns } => (
            JsonObject::new()
                .string("event", "columns_dropped")
                .string("table", table)
                .string("columns", &columns.join(", ")),
            format!(
                "  dropped column  {table}: {} (dropped on the source)\n",
                columns.join(", ")
            ),
        ),
        Notice::Switched {
            table,
            from,
            to,
            added,
            dropped,
        } => {
            let mut changes = Vec::new();
            if !added.is_empty() {
                changes.push(format!("added {}", added.join(", ")));
            }
            if !dropped.is_empty() {
                changes.push(format!("dropped {}", dropped.join(", ")));
            }
            (
                JsonObject::new()
                    .string("event", "capture_switched")
                    .string("table", table)
                    .string("from", from)
                    .string("to", to)
                    .string("added", &added.join(", "))
                    .string("dropped", &dropped.join(", ")),
                format!(
                    "  switched        {table} from CDC capture instance {from} to {to}{}\n",
                    if changes.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", changes.join("; "))
                    }
                ),
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FollowPosition {
    tables: Vec<SourceTable>,
    version: Option<i64>,
    lsn: Option<Lsn>,
    copy_started: Option<ServerTime>,
    exact_after: Option<ServerTime>,
    exact_from: Option<Lsn>,
}

impl FollowPosition {
    fn shown(&self) -> String {
        position_text(self.version, self.lsn)
    }

    fn saved(&self) -> Vec<String> {
        self.tables
            .iter()
            .map(|table| table_position(table, self.version, self.exact_from))
            .collect()
    }
}

struct PollContext<'a> {
    target: &'a str,
    database: &'a str,
    file: &'a str,
}

#[derive(Default)]
struct PollWork {
    statements: Vec<String>,
    counts: FollowCounts,
    notices: Vec<Notice>,
}

enum PollError {
    Lost(String),
    Failed(Failure),
}

fn poll_error(target: &str, error: &TdsClientError) -> PollError {
    match error {
        TdsClientError::Io(error) => PollError::Lost(error.to_string()),
        other => PollError::Failed(tds_failure(target, other)),
    }
}

fn ask(source: &mut TdsClient, target: &str, sql: &str) -> Result<TextRows, PollError> {
    source
        .batch(sql)
        .map(first_rows)
        .map_err(|error| poll_error(target, &error))
}

fn follow_column_changes(
    context: &PollContext,
    table: &mut SourceTable,
    now: &[String],
    work: &mut PollWork,
) -> Result<(), PollError> {
    let known = table.followed_names();
    let instance = match &table.changes {
        ChangeSource::Capture { instance, .. } if !now.is_empty() => instance.clone(),
        _ => {
            return match column_change(&known, now) {
                Some(change) => Err(PollError::Failed(schema_changed(
                    context.target,
                    &table.shown(),
                    &change,
                    context.file,
                ))),
                None => Ok(()),
            };
        }
    };
    let dropped: Vec<String> = known
        .iter()
        .filter(|name| !now.contains(name))
        .cloned()
        .collect();
    if let Some(key) = dropped.iter().find(|name| table.key.contains(name)) {
        return Err(PollError::Failed(source_failed(key_dropped(
            context.target,
            &table.shown(),
            key,
        ))));
    }
    let added: Vec<String> = now
        .iter()
        .filter(|name| !known.contains(name) && !table.left_out.contains(name))
        .cloned()
        .collect();
    table.left_out.retain(|name| now.contains(name));
    if !dropped.is_empty() {
        for column in &dropped {
            work.statements.push(drop_column_statement(table, column));
        }
        table
            .columns
            .retain(|column| !dropped.contains(&column.name));
        work.counts.schema_changes += dropped.len() as u64;
        work.notices.push(Notice::Dropped {
            table: table.shown(),
            columns: dropped,
        });
    }
    if !added.is_empty() {
        table.left_out.extend(added.iter().cloned());
        work.notices.push(Notice::LeftOut {
            table: table.shown(),
            try_command: second_instance_command(&table.schema, &table.name, &instance),
            instance,
            columns: added,
        });
    }
    Ok(())
}

fn switch_capture(
    source: &mut TdsClient,
    context: &PollContext,
    table: &mut SourceTable,
    successor: &CaptureInstance,
    next: Lsn,
    now: &[String],
    work: &mut PollWork,
) -> Result<(), PollError> {
    let plan = switch_plan(table, successor, now);
    if let Some(key) = plan.dropped.iter().find(|name| table.key.contains(name)) {
        return Err(PollError::Failed(source_failed(key_dropped(
            context.target,
            &table.shown(),
            key,
        ))));
    }
    for column in &plan.dropped {
        work.statements.push(drop_column_statement(table, column));
    }
    table
        .columns
        .retain(|column| !plan.dropped.contains(&column.name));
    if !plan.added.is_empty() {
        let metadata = source_columns(&ask(
            source,
            context.target,
            &table_columns_query(&table.schema, &table.name),
        )?);
        let mut added = Vec::new();
        for name in &plan.added {
            let column = metadata
                .iter()
                .find(|column| &column.name == name)
                .cloned()
                .ok_or_else(|| {
                    PollError::Failed(malformed_change(context.target, &table.shown()))
                })?;
            work.statements.push(add_column_statement(table, &column));
            added.push(column);
        }
        table.columns.extend(added.iter().cloned());
        if !table.key.is_empty() {
            let rows = ask(source, context.target, &backfill_query(table, &added))?;
            for row in &rows {
                work.statements
                    .push(backfill_statement(table, &added, row).ok_or_else(|| {
                        PollError::Failed(malformed_change(context.target, &table.shown()))
                    })?);
            }
        }
    }
    let from = match &table.changes {
        ChangeSource::Capture { instance, .. } => instance.clone(),
        ChangeSource::Tracking => String::new(),
    };
    table.left_out = plan.left_out;
    table.changes = ChangeSource::Capture {
        instance: successor.name.clone(),
        next,
        net_changes: successor.net_changes,
    };
    work.counts.schema_changes += (plan.added.len() + plan.dropped.len()) as u64;
    work.notices.push(Notice::Switched {
        table: table.shown(),
        from,
        to: successor.name.clone(),
        added: plan.added,
        dropped: plan.dropped,
    });
    Ok(())
}

fn poll(
    source: &mut TdsClient,
    context: &PollContext,
    before: &FollowPosition,
) -> Result<(FollowPosition, PollWork), PollError> {
    let target = context.target;
    let mut position = before.clone();
    let mut work = PollWork::default();
    let mut names_now: Vec<Vec<String>> = Vec::new();
    if !position.tables.is_empty() {
        let columns_now = ask(source, target, &columns_query(&position.tables))?;
        for table in &mut position.tables {
            let now = columns_of(&columns_now, table);
            follow_column_changes(context, table, &now, &mut work)?;
            names_now.push(now);
        }
    }
    let current_version = match position.version {
        Some(version) => Some(
            first_value(&ask(source, target, TRACKING_VERSION_QUERY)?, whole_number)
                .unwrap_or(version),
        ),
        None => None,
    };
    let current_lsn = match position.lsn {
        Some(lsn) => Some(
            first_value(&ask(source, target, MAX_LSN_QUERY)?, parse_lsn)
                .unwrap_or(lsn)
                .max(lsn),
        ),
        None => None,
    };
    let clock = match (&position.copy_started, current_lsn) {
        (Some(start), Some(_)) => Some(capture_clock(
            start,
            &ask(source, target, &capture_clock_query(start))?,
        )),
        _ => None,
    };
    let skip_after = position
        .copy_started
        .as_ref()
        .filter(|_| clock == Some(CaptureClock::Passed))
        .map(|start| start.time.clone());
    if let (Some(time), Some(_)) = (position.exact_after.clone(), current_lsn) {
        match capture_clock(&time, &ask(source, target, &capture_clock_query(&time))?) {
            CaptureClock::Passed => {
                if let Some(from) = exact_from_of(&ask(source, target, &exact_from_query(&time))?) {
                    position.exact_from = Some(from);
                    position.exact_after = None;
                }
            }
            CaptureClock::OffsetChanged => {
                if let Some(now) = server_time(&ask(source, target, EXACT_AFTER_QUERY)?, 0) {
                    position.exact_after = Some(now);
                }
            }
            CaptureClock::Behind => {}
        }
    }
    let reads_capture = position
        .tables
        .iter()
        .any(|table| table.changes != ChangeSource::Tracking);
    let instances = if reads_capture && current_lsn.is_some() {
        capture_instances(&ask(source, target, CAPTURE_INSTANCES_QUERY)?)
    } else {
        Vec::new()
    };
    let version = position.version;
    let exact_from = position.exact_from;
    for (index, table) in position.tables.iter_mut().enumerate() {
        match table.changes.clone() {
            ChangeSource::Tracking => {
                let (Some(from), Some(current)) = (version, current_version) else {
                    continue;
                };
                let minimum = first_value(
                    &ask(
                        source,
                        target,
                        &format!(
                            "SELECT CAST(CHANGE_TRACKING_MIN_VALID_VERSION(OBJECT_ID({})) AS nvarchar(max))",
                            quote_unicode(&table.qualified())
                        ),
                    )?,
                    whole_number,
                )
                .unwrap_or(0);
                if minimum > from {
                    return Err(PollError::Failed(source_failed(format!(
                        "cannot follow {target}: the change retention of {} was exceeded while following: changes were lost. Follow again into a new file",
                        context.database
                    ))));
                }
                if table.key.is_empty() || current <= from {
                    continue;
                }
                let changes = ask(
                    source,
                    target,
                    &tracking_changes_query(table, from, current),
                )?;
                let key_length = table.key.len();
                for change in &changes {
                    let key_values = change.get(2..2 + key_length).unwrap_or_default();
                    let row = (cell(change, 1) == "1")
                        .then(|| change.get(2 + key_length..).unwrap_or_default());
                    work.statements
                        .extend(change_statements(table, key_values, row).ok_or_else(|| {
                            PollError::Failed(malformed_change(target, &table.shown()))
                        })?);
                    count_change(&mut work.counts, tracking_operation(&cell(change, 0)));
                }
            }
            ChangeSource::Capture {
                instance,
                next,
                net_changes,
            } => {
                let Some(to) = current_lsn else {
                    continue;
                };
                if clock == Some(CaptureClock::Behind) {
                    continue;
                }
                let now = names_now.get(index).cloned().unwrap_or_default();
                let current_exists = instances
                    .iter()
                    .any(|other| other.name == instance && other.is_of(table));
                let successor = successor_of(&instances, table, &instance).cloned();
                let starts = successor.as_ref().and_then(|successor| successor.minimum);
                if let (Some(successor), Some(starts)) = (&successor, starts)
                    && starts <= next
                {
                    switch_capture(source, context, table, successor, next, &now, &mut work)?;
                    continue;
                }
                if !current_exists && let (Some(successor), Some(starts)) = (&successor, starts) {
                    return Err(PollError::Failed(source_failed(capture_gap(
                        target,
                        &table.shown(),
                        &instance,
                        &successor.name,
                        starts,
                        next,
                    ))));
                }
                let upper = starts.map_or(to, |starts| to.min(starts.before()));
                let bounds = ask(
                    source,
                    target,
                    &capture_bounds_query(&instance, upper, skip_after.as_deref()),
                )?;
                let next = first_change_after_copy(next, &bounds);
                let minimum = first_value(&bounds, parse_lsn).unwrap_or(Lsn(0));
                if !minimum.is_valid() || minimum > next {
                    return Err(PollError::Failed(source_failed(capture_lost(
                        target,
                        &table.shown(),
                        &instance,
                        minimum,
                        next,
                    ))));
                }
                let advanced = if next > upper {
                    next
                } else {
                    let following = bounds
                        .first()
                        .and_then(|row| row.get(1))
                        .and_then(parse_lsn)
                        .ok_or_else(|| {
                            PollError::Failed(malformed_change(target, &table.shown()))
                        })?;
                    let net = net_changes && exact_from.is_some_and(|from| next >= from);
                    let changes = ask(
                        source,
                        target,
                        &capture_changes_query(table, &instance, next, upper, net),
                    )?;
                    for change in &changes {
                        let Some(step) = capture_operation(&cell(change, 0)) else {
                            continue;
                        };
                        let row = change.get(1..).unwrap_or_default();
                        let image = match step {
                            CaptureRow::BeforeUpdate | CaptureRow::Change(RowChange::Delete) => {
                                None
                            }
                            CaptureRow::Change(_) => Some(row),
                        };
                        work.statements.extend(
                            change_statements(table, &capture_key_values(table, row), image)
                                .ok_or_else(|| {
                                    PollError::Failed(malformed_change(target, &table.shown()))
                                })?,
                        );
                        if let CaptureRow::Change(operation) = step {
                            count_change(&mut work.counts, operation);
                        }
                    }
                    following
                };
                match (&successor, starts) {
                    (Some(successor), Some(starts)) if starts.before() <= to => {
                        switch_capture(
                            source,
                            context,
                            table,
                            successor,
                            next.max(starts),
                            &now,
                            &mut work,
                        )?;
                    }
                    _ => {
                        if let ChangeSource::Capture { next, .. } = &mut table.changes {
                            *next = advanced;
                        }
                    }
                }
            }
        }
    }
    position.copy_started = copy_start_after_poll(position.copy_started.take(), clock);
    position.version = position.version.max(current_version);
    position.lsn = position.lsn.max(current_lsn);
    Ok((position, work))
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

    #[allow(clippy::too_many_arguments)]
    fn copy_sql_server(
        &mut self,
        source: &mut TdsClient,
        target: &str,
        tables: &[SourceTable],
        identity: &str,
        version: Option<i64>,
        started: Instant,
        counts: &mut FollowCounts,
        created: &mut Vec<String>,
    ) -> Result<(), Failure> {
        let query = |source: &mut TdsClient, sql: &str| {
            source
                .batch(sql)
                .map_err(|error| tds_failure(target, &error))
        };
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
        let mut schemas: Vec<&str> = tables.iter().map(|table| table.schema.as_str()).collect();
        schemas.dedup();
        for schema in schemas.iter().filter(|schema| **schema != "dbo") {
            run(&format!("CREATE SCHEMA {}", quote_name(schema)))?;
            created.push(format!("DROP SCHEMA {}", quote_name(schema)));
        }
        for table in tables {
            let mut definitions: Vec<String> =
                table.columns.iter().map(SourceColumn::definition).collect();
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
            created.push(format!("DROP TABLE {}", table.qualified()));
        }
        for table in tables {
            if stop_signals::requested() {
                return Err(copy_stopped(&self.file_display));
            }
            let copied_at = Instant::now();
            let selected: Vec<String> = table
                .columns
                .iter()
                .map(|column| text_expression(&quote_name(&column.name), &column.data_type))
                .collect();
            let rows = first_rows(query(
                source,
                &format!("SELECT {} FROM {}", selected.join(", "), table.qualified()),
            )?);
            let names = table.column_names();
            run("BEGIN TRANSACTION")?;
            for chunk in rows.chunks(ROWS_PER_INSERT) {
                if stop_signals::requested() {
                    return Err(copy_stopped(&self.file_display));
                }
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
            let left_out = table.left_out.join(", ");
            self.follow_event(
                JsonObject::new()
                    .string("event", "table_copied")
                    .string("table", &table.shown())
                    .number("rows", rows.len() as u64)
                    .number("elapsed_ms", copied_at.elapsed().as_millis() as u64)
                    .string("changes", table.changes.name())
                    .boolean("net_changes", table.changes.net_changes())
                    .string_if_any(
                        "left_out",
                        Some(left_out.as_str()).filter(|columns| !columns.is_empty()),
                    ),
                &format!(
                    "  copied          {:<28} {:>10} rows   {:.1} s   {}{}\n",
                    table.shown(),
                    rows.len(),
                    copied_at.elapsed().as_secs_f64(),
                    table.changes.label(),
                    if left_out.is_empty() {
                        String::new()
                    } else {
                        format!(", leaves out {left_out} (not in its capture instance)")
                    }
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
        let mut state = SavedFollow::new(SQL_SERVER, identity);
        for table in tables {
            state.set(
                &table_item(&table.shown()),
                &table_position(table, version, None),
            );
        }
        let mut script = String::from("BEGIN;\n");
        for statement in state.create_statements() {
            script.push_str(&statement);
            script.push_str(";\n");
        }
        script.push_str("COMMIT;");
        let mut state_session = store.session(Dialect::PostgreSql);
        let created = state_session.execute(&script, &mut |_| {});
        let _ = state_session.close();
        created.map_err(|error| {
            source_failed(format!(
                "cannot save the follow position in {}: {}",
                self.file_display,
                error.message()
            ))
        })?;
        Ok(())
    }

    fn reconnect_tds(
        &mut self,
        options: &TdsConnectOptions,
        error: String,
        position: &str,
    ) -> Option<TdsClient> {
        let deadline = Instant::now() + self.reconnect_for();
        let mut message = error;
        let mut attempt = 0u32;
        while self.wait_to_reconnect(&mut attempt, &message, deadline) {
            match TdsClient::connect(options) {
                Ok(client) => {
                    self.reconnected_event(attempt, position);
                    return Some(client);
                }
                Err(error) => message = error.to_string(),
            }
        }
        None
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
        let instances = if capture_on {
            capture_instances(&first_rows(query(&mut source, CAPTURE_INSTANCES_QUERY)?))
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
        let mut tables = Vec::new();
        let mut unfollowable = Vec::new();
        for row in &listed {
            let (schema, name) = (cell(row, 0), cell(row, 1));
            let tracked = tracking_on && cell(row, 2) == "1";
            let columns = source_columns(&first_rows(query(
                &mut source,
                &table_columns_query(&schema, &name),
            )?));
            let key: Vec<String> = first_rows(query(&mut source, &key_query(&schema, &name))?)
                .iter()
                .map(|row| cell(row, 0))
                .collect();
            let names: Vec<String> = columns.iter().map(|column| column.name.clone()).collect();
            let theirs = instances_of(&instances, &schema, &name);
            match choose_changes(forced, tracked, &theirs, &names, &key) {
                Ok(changes) => tables.push(SourceTable {
                    schema,
                    name,
                    columns,
                    key,
                    changes,
                    left_out: Vec::new(),
                }),
                Err(key_left_out) => unfollowable.push(Unfollowable {
                    schema,
                    name,
                    key_left_out,
                }),
            }
        }
        if let Some(first) = unfollowable.first() {
            let listed: Vec<String> = unfollowable.iter().map(Unfollowable::shown).collect();
            let capture_command = match &first.key_left_out {
                Some(left_out) => {
                    second_instance_command(&first.schema, &first.name, &left_out.instance)
                }
                None => enable_capture_command(&first.schema, &first.name, None),
            };
            let tracking_command = format!(
                "ALTER TABLE {}.{} ENABLE CHANGE_TRACKING;",
                first.schema, first.name
            );
            let (what, try_command) = match forced {
                Some(SqlServerChanges::ChangeTracking) => ("are not tracked", tracking_command),
                Some(SqlServerChanges::ChangeDataCapture) => (
                    "have no CDC capture instance that includes their primary key",
                    capture_command,
                ),
                None if capture_on && !tracking_on => (
                    "have no CDC capture instance that includes their primary key",
                    capture_command,
                ),
                None if capture_on => (
                    "have neither change tracking nor a CDC capture instance that includes their primary key",
                    tracking_command,
                ),
                None => ("are not tracked", tracking_command),
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
        let mut copy_started: Option<ServerTime> = None;
        let lsn: Option<Lsn> = if uses_capture {
            let start = first_rows(query(&mut source, COPY_START_QUERY)?);
            copy_started = copy_start(&start);
            Some(first_value(&start, parse_lsn).unwrap_or(Lsn(0)))
        } else {
            None
        };
        if let Some(start) = lsn {
            for table in &mut tables {
                let shown = table.shown();
                let ChangeSource::Capture {
                    instance,
                    next,
                    net_changes,
                } = &mut table.changes
                else {
                    continue;
                };
                let bounds = first_rows(query(
                    &mut source,
                    &capture_bounds_query(instance, start, None),
                )?);
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
                let older = (minimum > following)
                    .then(|| {
                        instances_of(&instances, &table.schema, &table.name)
                            .into_iter()
                            .filter(|other| other.name != *instance && other.captures(&table.key))
                            .find(|other| other.minimum.is_some_and(|lsn| lsn <= following))
                    })
                    .flatten();
                match older {
                    Some(older) => {
                        *instance = older.name.clone();
                        *net_changes = older.net_changes;
                        *next = following;
                    }
                    None => *next = minimum.max(following),
                }
            }
        }

        let identity = format!(
            "server {} database {}",
            source
                .batch("SELECT CAST(@@SERVERNAME AS nvarchar(max))")
                .ok()
                .and_then(|results| first_value(&first_rows(results), |value| value.clone()))
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| format!("{}:{}", options.host, options.port)),
            options.database
        );
        let captured_by = |table: &SourceTable| -> Option<Vec<String>> {
            let ChangeSource::Capture { instance, .. } = &table.changes else {
                return None;
            };
            instances
                .iter()
                .find(|other| other.name == *instance && other.is_of(table))
                .map(|other| other.columns.clone())
        };
        let mut carried: Vec<String> = Vec::new();
        let mut exact_from: Option<Lsn> = None;
        let saved = if existed { self.saved_follow()? } else { None };
        if let Some(saved) = &saved {
            let cannot = |reason: String| {
                source_failed(format!(
                    "cannot resume the follow into {}: {reason}",
                    self.file_display
                ))
            };
            if let Some(refusal) = saved.refusal(SQL_SERVER, &identity, &self.file_display) {
                return Err(cannot(refusal));
            }
            let mut resumed_version: Option<i64> = None;
            for table in &mut tables {
                let shown = table.shown();
                match (
                    &mut table.changes,
                    saved.table(&shown).and_then(read_position),
                ) {
                    (ChangeSource::Tracking, Some(SavedPosition::Tracking(saved_version))) => {
                        resumed_version = Some(
                            resumed_version.map_or(saved_version, |least| least.min(saved_version)),
                        );
                    }
                    (
                        ChangeSource::Capture {
                            instance,
                            next,
                            net_changes,
                        },
                        Some(SavedPosition::Capture {
                            next: saved_next,
                            exact_from: saved_exact_from,
                            instance: saved_instance,
                        }),
                    ) => {
                        *next = saved_next;
                        if let Some(saved_instance) = saved_instance {
                            *net_changes = instances
                                .iter()
                                .find(|other| other.name == saved_instance)
                                .map_or(*net_changes, |other| other.net_changes);
                            *instance = saved_instance;
                        }
                        exact_from = exact_from.or(saved_exact_from);
                    }
                    (_, None) => {
                        return Err(cannot(format!(
                            "the source's table {shown} was not part of it. Follow again into a new file with --create"
                        )));
                    }
                    (changes, Some(_)) => {
                        return Err(cannot(format!(
                            "{shown} is now read through {} on the source, not the way the follow saved. Follow again into a new file with --create",
                            changes.label()
                        )));
                    }
                }
            }
            if uses_tracking {
                version = resumed_version.or(version);
            }
            copy_started = None;
            let target_tables = self.target_table_columns()?;
            for table in &mut tables {
                let in_target = target_tables
                    .iter()
                    .find(|(schema, name, _)| {
                        *name == table.name && (*schema == table.schema || table.schema == "dbo")
                    })
                    .map(|(_, _, columns)| columns.clone());
                let captured = captured_by(&*table);
                match (&table.changes, in_target) {
                    (ChangeSource::Tracking, Some(columns)) => {
                        if let Some(change) = column_change(&columns, &table.followed_names()) {
                            return Err(schema_changed(
                                &target,
                                &table.shown(),
                                &change,
                                &self.file_display,
                            ));
                        }
                    }
                    (ChangeSource::Tracking, None) => {}
                    (ChangeSource::Capture { .. }, Some(columns)) => {
                        let resumed = resume_capture_columns(table, &columns, captured.as_deref())
                            .map_err(|change| {
                                schema_changed(&target, &table.shown(), &change, &self.file_display)
                            })?;
                        carried.extend(
                            resumed
                                .dropped
                                .iter()
                                .map(|column| drop_column_statement(table, column)),
                        );
                        table.columns = resumed.followed;
                        table.left_out = resumed.left_out;
                        if !resumed.dropped.is_empty() {
                            let (object, line) = notice_event(&Notice::Dropped {
                                table: table.shown(),
                                columns: resumed.dropped,
                            });
                            self.follow_event(object, &line);
                        }
                    }
                    (ChangeSource::Capture { .. }, None) => {
                        if let Some(captured) = captured {
                            let (followed, left_out) =
                                split_captured(std::mem::take(&mut table.columns), &captured);
                            table.columns = followed;
                            table.left_out = left_out;
                        }
                    }
                }
            }
            let position = position_text(version, lsn);
            self.follow_event(
                JsonObject::new()
                    .string("event", "resumed")
                    .number("tables", tables.len() as u64)
                    .string("position", &position),
                &format!(
                    "  resumed         {} tables from {position}\n",
                    tables.len()
                ),
            );
        } else {
            for table in &mut tables {
                let Some(captured) = captured_by(&*table) else {
                    continue;
                };
                let (followed, left_out) =
                    split_captured(std::mem::take(&mut table.columns), &captured);
                table.columns = followed;
                table.left_out = left_out;
            }
        }

        let _ = stop_signals::watch();
        let mut counts = FollowCounts::default();
        if saved.is_none() {
            let mut created = Vec::new();
            let copied = self.copy_sql_server(
                &mut source,
                &target,
                &tables,
                &identity,
                version,
                started,
                &mut counts,
                &mut created,
            );
            if let Err(failure) = copied {
                if existed {
                    return Err(self.undo_first_copy(Dialect::SqlServer, &created, failure));
                }
                let _ = std::fs::remove_file(&self.request.file);
                return Err(failure);
            }
        }
        let exact_after = if uses_capture && exact_from.is_none() {
            server_time(&first_rows(query(&mut source, EXACT_AFTER_QUERY)?), 0)
        } else {
            None
        };
        let mut position = FollowPosition {
            tables,
            version,
            lsn,
            copy_started,
            exact_after,
            exact_from,
        };
        let db = self.open(Access::Write { create: false }, None)?;
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
        let poll_every = Duration::from_secs(
            self.request
                .follow
                .poll_seconds
                .unwrap_or(DEFAULT_POLL_SECONDS),
        );
        let file = self.file_display.clone();
        let context = PollContext {
            target: &target,
            database: &options.database,
            file: &file,
        };
        let mut polls = 0u64;
        let mut saved_positions = position.saved();
        let reason = loop {
            if let Err(error) = source.batch(LIVENESS_QUERY) {
                if !matches!(error, TdsClientError::Io(_)) {
                    return Err(tds_failure(&target, &error));
                }
                match self.reconnect_tds(&options, error.to_string(), &position.shown()) {
                    Some(client) => source = client,
                    None => {
                        break if stop_signals::requested() {
                            "interrupt"
                        } else {
                            "source_lost"
                        };
                    }
                }
            }
            let (polled, work) = match poll(&mut source, &context, &position) {
                Ok(polled) => polled,
                Err(PollError::Failed(failure)) => return Err(failure),
                Err(PollError::Lost(message)) => {
                    match self.reconnect_tds(&options, message, &position.shown()) {
                        Some(client) => {
                            source = client;
                            continue;
                        }
                        None => {
                            break if stop_signals::requested() {
                                "interrupt"
                            } else {
                                "source_lost"
                            };
                        }
                    }
                }
            };
            polls += 1;
            let changed = !work.statements.is_empty() || !carried.is_empty();
            let positions = polled.saved();
            if changed || positions != saved_positions {
                run("BEGIN TRANSACTION")?;
                for statement in carried.iter().chain(&work.statements) {
                    run(statement)?;
                }
                for (table, saved_position) in polled.tables.iter().zip(&positions) {
                    run(&update_statement(
                        &table_item(&table.shown()),
                        saved_position,
                    ))?;
                }
                run("COMMIT TRANSACTION")?;
                carried.clear();
                if changed {
                    counts.transactions += 1;
                }
                saved_positions = positions;
            }
            counts.inserts += work.counts.inserts;
            counts.updates += work.counts.updates;
            counts.deletes += work.counts.deletes;
            counts.schema_changes += work.counts.schema_changes;
            position = polled;
            for notice in &work.notices {
                let (object, line) = notice_event(notice);
                self.follow_event(object, &line);
            }
            if self.request.follow.until_caught_up && !changed {
                break "caught_up";
            }
            if self.format != Format::Json {
                self.messages.text(&format!(
                    "  following       poll every {} s, {polls} polls, {} changes applied, at {}\n",
                    poll_every.as_secs(),
                    counts.inserts + counts.updates + counts.deletes,
                    position.shown()
                ));
            }
            let slept = Instant::now();
            while slept.elapsed() < poll_every && !stop_signals::requested() {
                std::thread::sleep(STOP_CHECK);
            }
            if stop_signals::requested() {
                break "interrupt";
            }
        };
        drop(run);
        let _ = session.close();
        let (version, lsn) = (position.version, position.lsn);
        let position = position.shown();
        let ok = reason != "source_lost";
        let verdict = if ok {
            format!(
                "{} holds every change SQL Server recorded up to {position}; point applications at it now",
                self.file_display
            )
        } else {
            format!(
                "the connection to the source was lost for longer than --reconnect-for; {} holds the changes up to {position}. Run the same command again to resume",
                self.file_display
            )
        };
        match self.format {
            Format::Json => {
                let summary = JsonObject::new()
                    .string("event", "stopped")
                    .boolean("ok", ok)
                    .string("reason", reason)
                    .number("elapsed_ms", started.elapsed().as_millis() as u64)
                    .number("tables", counts.tables)
                    .number("rows_copied", counts.rows_copied)
                    .number("transactions", counts.transactions)
                    .number("inserts", counts.inserts)
                    .number("updates", counts.updates)
                    .number("deletes", counts.deletes)
                    .number("schema_changes", counts.schema_changes);
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
                "followed {} into {} for {:.0} s ({})\n  tables          {}, {} rows copied\n  changes         {} polls applied inserts {}, updates {}, deletes {}, schema changes {}\n  position        {position}\n{}: {verdict}\n",
                options.shown(),
                self.file_display,
                started.elapsed().as_secs_f64(),
                match reason {
                    "caught_up" => "caught up, --until caught-up",
                    "source_lost" => "the connection to the source was lost",
                    _ => "stopped by Ctrl-C or SIGTERM",
                },
                counts.tables,
                counts.rows_copied,
                polls,
                counts.inserts,
                counts.updates,
                counts.deletes,
                counts.schema_changes,
                if ok { "ok" } else { "stopped" },
            )),
        }
        Ok(if ok { Exit::Done } else { Exit::DatabaseError })
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

fn capture_gap(target: &str, table: &str, old: &str, new: &str, starts: Lsn, next: Lsn) -> String {
    format!(
        "cannot follow {target}: the CDC capture instance {old} of {table} was dropped before the follow reached the start of {new} ({}), so the changes from {} up to it are lost. Follow again into a new file, and drop an old capture instance only after the follow's capture_switched event",
        starts.literal(),
        next.literal()
    )
}

fn key_dropped(target: &str, table: &str, column: &str) -> String {
    format!(
        "cannot follow {target}: primary key column {column} of {table} was dropped on the source, so its CDC changes can no longer be applied by key. Follow again into a new file"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_connection_check_reads_its_answer_as_text_as_every_query_does() {
        assert_eq!(LIVENESS_QUERY, "SELECT CAST(1 AS nvarchar(max))");
    }

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
    fn source_columns_keep_their_type_and_nullability_apart() {
        let rows = vec![
            vec![
                Some("id".to_owned()),
                Some("int".to_owned()),
                None,
                Some("10".to_owned()),
                Some("0".to_owned()),
                None,
                Some("NO".to_owned()),
            ],
            vec![
                Some("note".to_owned()),
                Some("nvarchar".to_owned()),
                Some("40".to_owned()),
                None,
                None,
                None,
                Some("YES".to_owned()),
            ],
        ];
        let columns = source_columns(&rows);
        assert_eq!(columns[0].definition(), "[id] int NOT NULL");
        assert_eq!(columns[1].definition(), "[note] nvarchar(40)");
        assert!(
            table_columns_query("dbo", "it's")
                .contains("TABLE_SCHEMA = N'dbo' AND TABLE_NAME = N'it''s'")
        );
        assert!(
            key_query("dbo", "orders")
                .contains("c.TABLE_SCHEMA = N'dbo' AND c.TABLE_NAME = N'orders'")
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

    fn column(name: &str, data_type: &str) -> SourceColumn {
        SourceColumn {
            name: name.to_owned(),
            data_type: data_type.to_owned(),
            declared: data_type.to_owned(),
            not_null: false,
        }
    }

    fn orders() -> SourceTable {
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
                net_changes: false,
            },
            left_out: Vec::new(),
        }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn instance(name: &str, columns: &[&str], minimum: Option<u128>) -> CaptureInstance {
        CaptureInstance {
            schema: "dbo".to_owned(),
            table: "orders".to_owned(),
            name: name.to_owned(),
            net_changes: false,
            minimum: minimum.map(Lsn),
            columns: names(columns),
        }
    }

    #[test]
    fn each_table_saves_its_position_and_reads_it_back() {
        let mut table = orders();
        let saved = table_position(&table, Some(9), None);
        assert_eq!(saved, "cdc 0x00000000000000000001 - dbo_orders");
        assert_eq!(
            read_position(&saved),
            Some(SavedPosition::Capture {
                next: Lsn(1),
                exact_from: None,
                instance: Some("dbo_orders".to_owned()),
            })
        );
        table.changes = ChangeSource::Tracking;
        assert_eq!(table_position(&table, Some(9), None), "change_tracking 9");
        assert_eq!(
            read_position("change_tracking 9"),
            Some(SavedPosition::Tracking(9))
        );
        assert_eq!(read_position("change_tracking nine"), None);
        assert_eq!(read_position("nothing"), None);
    }

    #[test]
    fn a_cdc_position_carries_its_capture_instance_and_where_net_changes_are_exact() {
        let mut table = orders();
        table.changes = ChangeSource::Capture {
            instance: "sales orders v2".to_owned(),
            next: Lsn(0x2a),
            net_changes: true,
        };
        let saved = table_position(&table, None, Some(Lsn(0x20)));
        assert_eq!(
            saved,
            "cdc 0x0000000000000000002A 0x00000000000000000020 sales orders v2"
        );
        assert_eq!(
            read_position(&saved),
            Some(SavedPosition::Capture {
                next: Lsn(0x2a),
                exact_from: Some(Lsn(0x20)),
                instance: Some("sales orders v2".to_owned()),
            })
        );
        assert_eq!(
            read_position("cdc 0x0000000000000000002A"),
            Some(SavedPosition::Capture {
                next: Lsn(0x2a),
                exact_from: None,
                instance: None,
            })
        );
        assert_eq!(read_position("cdc 0x2A junk dbo_orders"), None);
        assert_eq!(read_position("cdc 0x2A -"), None);
        assert_eq!(read_position("cdc 0x2A - "), None);
    }

    #[test]
    fn changed_columns_are_named() {
        let known = vec!["id".to_owned(), "note".to_owned()];
        assert_eq!(column_change(&known, &known), None);
        assert_eq!(
            column_change(&known, &["id".to_owned(), "amount".to_owned()]).as_deref(),
            Some("added amount; dropped note")
        );
        assert_eq!(
            column_change(&known, &["note".to_owned(), "id".to_owned()]).as_deref(),
            Some("reordered its columns")
        );
        assert_eq!(
            column_change(&known, &[]).as_deref(),
            Some("the table is gone")
        );
    }

    #[test]
    fn one_query_reads_the_columns_of_every_followed_table() {
        let table = orders();
        let query = columns_query(std::slice::from_ref(&table));
        assert!(query.contains("(TABLE_SCHEMA = N'dbo' AND TABLE_NAME = N'orders')"));
        let rows = vec![
            vec![
                Some("dbo".to_owned()),
                Some("orders".to_owned()),
                Some("id".to_owned()),
            ],
            vec![
                Some("dbo".to_owned()),
                Some("other".to_owned()),
                Some("x".to_owned()),
            ],
        ];
        assert_eq!(columns_of(&rows, &table), vec!["id".to_owned()]);
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
        assert_eq!(Lsn(0x2a).before(), Lsn(0x29));
        assert_eq!(Lsn(0).before(), Lsn(0));
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
    fn capture_instances_are_read_one_row_per_captured_column_newest_first() {
        let row = |instance: &str, net: &str, minimum: Option<&str>, column: &str| {
            vec![
                Some("dbo".to_owned()),
                Some("orders".to_owned()),
                Some(instance.to_owned()),
                Some(net.to_owned()),
                minimum.map(str::to_owned),
                Some(column.to_owned()),
            ]
        };
        let rows = vec![
            row("dbo_orders_2", "1", None, "id"),
            row("dbo_orders_2", "1", None, "extra"),
            row("dbo_orders", "0", Some("0x0000002A000001F80003"), "id"),
            row("dbo_orders", "0", Some("0x0000002A000001F80003"), "note"),
        ];
        let read = capture_instances(&rows);
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].name, "dbo_orders_2");
        assert!(read[0].net_changes);
        assert_eq!(read[0].minimum, None);
        assert_eq!(read[0].columns, names(&["id", "extra"]));
        assert_eq!(read[1].minimum, Lsn::parse("0x0000002A000001F80003"));
        assert_eq!(read[1].columns, names(&["id", "note"]));
        assert!(read[1].is_of(&orders()));
        assert_eq!(instances_of(&read, "dbo", "orders").len(), 2);
        assert!(instances_of(&read, "dbo", "other").is_empty());
        assert!(capture_instances(&Vec::new()).is_empty());
        assert!(CAPTURE_INSTANCES_QUERY.contains("cdc.captured_columns"));
        assert!(CAPTURE_INSTANCES_QUERY.contains("i.is_primary_key = 1 AND i.name = c.index_name"));
        assert!(
            CAPTURE_INSTANCES_QUERY
                .ends_with("ORDER BY c.create_date DESC, c.object_id DESC, cc.column_ordinal")
        );
    }

    #[test]
    fn cdc_is_chosen_when_the_table_has_a_capture_instance_unless_forced() {
        let whole = instance("dbo_t", &["id", "note"], Some(1));
        let columns = names(&["id", "note"]);
        let key = names(&["id"]);
        let theirs = [&whole];
        let capture = Ok(capture_source(&whole));
        assert_eq!(choose_changes(None, true, &theirs, &columns, &key), capture);
        assert_eq!(
            choose_changes(None, false, &theirs, &columns, &key),
            capture
        );
        assert_eq!(
            choose_changes(None, true, &[], &columns, &key),
            Ok(ChangeSource::Tracking)
        );
        assert_eq!(choose_changes(None, false, &[], &columns, &key), Err(None));
        let tracking = Some(SqlServerChanges::ChangeTracking);
        assert_eq!(
            choose_changes(tracking, true, &theirs, &columns, &key),
            Ok(ChangeSource::Tracking)
        );
        assert_eq!(
            choose_changes(tracking, false, &theirs, &columns, &key),
            Err(None)
        );
        let cdc = Some(SqlServerChanges::ChangeDataCapture);
        assert_eq!(choose_changes(cdc, true, &theirs, &columns, &key), capture);
        assert_eq!(choose_changes(cdc, true, &[], &columns, &key), Err(None));
    }

    #[test]
    fn a_capture_instance_of_some_columns_is_followed_when_nothing_covers_every_column() {
        let part = instance("dbo_t", &["id"], Some(1));
        let columns = names(&["id", "note"]);
        let key = names(&["id"]);
        let theirs = [&part];
        let capture = Ok(capture_source(&part));
        assert_eq!(
            choose_changes(None, true, &theirs, &columns, &key),
            Ok(ChangeSource::Tracking)
        );
        assert_eq!(
            choose_changes(None, false, &theirs, &columns, &key),
            capture
        );
        let cdc = Some(SqlServerChanges::ChangeDataCapture);
        assert_eq!(choose_changes(cdc, true, &theirs, &columns, &key), capture);
        let whole = instance("dbo_t_2", &["id", "note"], Some(1));
        assert_eq!(
            choose_changes(cdc, true, &[&part, &whole], &columns, &key),
            Ok(capture_source(&whole))
        );
    }

    #[test]
    fn a_capture_instance_without_the_primary_key_is_not_used_and_is_named() {
        let keyless = instance("dbo_t", &["note"], Some(1));
        let columns = names(&["id", "note"]);
        let key = names(&["id"]);
        let left_out = Err(Some(KeyLeftOut {
            instance: "dbo_t".to_owned(),
            columns: names(&["id"]),
        }));
        assert_eq!(
            choose_changes(None, false, &[&keyless], &columns, &key),
            left_out
        );
        assert_eq!(
            choose_changes(
                Some(SqlServerChanges::ChangeDataCapture),
                true,
                &[&keyless],
                &columns,
                &key
            ),
            left_out
        );
        assert_eq!(
            choose_changes(None, true, &[&keyless], &columns, &key),
            Ok(ChangeSource::Tracking)
        );
        let shown = Unfollowable {
            schema: "dbo".to_owned(),
            name: "t".to_owned(),
            key_left_out: Some(KeyLeftOut {
                instance: "dbo_t".to_owned(),
                columns: names(&["id"]),
            }),
        }
        .shown();
        assert_eq!(
            shown,
            "dbo.t (its CDC capture instance dbo_t leaves out primary key column id)"
        );
    }

    #[test]
    fn only_the_captured_columns_are_followed_and_the_rest_are_named() {
        let (followed, left_out) = split_captured(orders().columns, &names(&["id", "note"]));
        let followed: Vec<&str> = followed.iter().map(|column| column.name.as_str()).collect();
        assert_eq!(followed, vec!["note", "id"]);
        assert_eq!(left_out, names(&["tag"]));
    }

    #[test]
    fn net_changes_need_the_primary_key_as_the_instance_row_identifier() {
        let mut net = instance("dbo_t", &["id"], Some(1));
        net.net_changes = true;
        assert!(capture_source(&net).net_changes());
        assert_eq!(capture_source(&net).label(), "CDC, net changes");
        assert!(!capture_source(&instance("dbo_t", &["id"], Some(1))).net_changes());
        assert!(!ChangeSource::Tracking.net_changes());
    }

    #[test]
    fn cdc_queries_read_one_lsn_range_of_the_capture_instance() {
        let from = Lsn::parse("0x0000002A000001F80003").unwrap();
        let to = Lsn::parse("0x0000002B000000100001").unwrap();
        assert_eq!(
            capture_changes_query(&orders(), "dbo_orders", from, to, false),
            "SELECT CAST(C.[__$operation] AS nvarchar(max)), CAST(C.[note] AS nvarchar(max)), \
             CAST(C.[id] AS nvarchar(max)), CONVERT(nvarchar(max), C.[tag], 1) \
             FROM cdc.[fn_cdc_get_all_changes_dbo_orders](0x0000002A000001F80003, 0x0000002B000000100001, N'all update old') AS C \
             ORDER BY C.[__$start_lsn], C.[__$seqval], C.[__$operation]"
        );
        assert_eq!(
            capture_bounds_query("it's", to, None),
            "SELECT CONVERT(nvarchar(max), sys.fn_cdc_get_min_lsn(N'it''s'), 1), \
             CONVERT(nvarchar(max), sys.fn_cdc_increment_lsn(0x0000002B000000100001), 1), CAST(NULL AS nvarchar(max))"
        );
        assert_eq!(
            capture_bounds_query("dbo_orders", to, Some("2026-11-02 11:40:01.123")),
            "SELECT CONVERT(nvarchar(max), sys.fn_cdc_get_min_lsn(N'dbo_orders'), 1), \
             CONVERT(nvarchar(max), sys.fn_cdc_increment_lsn(0x0000002B000000100001), 1), \
             CONVERT(nvarchar(max), sys.fn_cdc_increment_lsn(sys.fn_cdc_map_time_to_lsn(N'largest less than or equal', CONVERT(datetime, N'2026-11-02 11:40:01.123', 121))), 1)"
        );
    }

    #[test]
    fn net_changes_read_the_final_row_of_each_key_with_deletes_first() {
        let from = Lsn::parse("0x0000002A000001F80003").unwrap();
        let to = Lsn::parse("0x0000002B000000100001").unwrap();
        assert_eq!(
            capture_changes_query(&orders(), "dbo_orders", from, to, true),
            "SELECT CAST(C.[__$operation] AS nvarchar(max)), CAST(C.[note] AS nvarchar(max)), \
             CAST(C.[id] AS nvarchar(max)), CONVERT(nvarchar(max), C.[tag], 1) \
             FROM cdc.[fn_cdc_get_net_changes_dbo_orders](0x0000002A000001F80003, 0x0000002B000000100001, N'all') AS C \
             ORDER BY C.[__$operation], C.[__$start_lsn]"
        );
    }

    #[test]
    fn net_changes_start_once_every_change_the_copy_saw_has_been_applied_one_by_one() {
        assert_eq!(
            EXACT_AFTER_QUERY,
            "SELECT CONVERT(nvarchar(max), CAST(DATEADD(second, 1, SYSDATETIME()) AS datetime), 121), \
             CAST(DATEPART(TZOFFSET, SYSDATETIMEOFFSET()) AS nvarchar(max))"
        );
        let after = server_time(
            &vec![vec![
                Some("2026-11-03 01:40:02.000".to_owned()),
                Some("0".to_owned()),
            ]],
            0,
        )
        .unwrap();
        assert_eq!(
            exact_from_query(&after),
            "SELECT CONVERT(nvarchar(max), sys.fn_cdc_increment_lsn(sys.fn_cdc_map_time_to_lsn(N'largest less than or equal', CONVERT(datetime, N'2026-11-03 01:40:02.000', 121))), 1)"
        );
        assert_eq!(
            exact_from_of(&vec![vec![Some("0x0000002A000001F80003".to_owned())]]),
            Lsn::parse("0x0000002A000001F80003")
        );
        assert_eq!(exact_from_of(&vec![vec![None]]), Some(Lsn(0)));
        assert_eq!(exact_from_of(&vec![vec![Some("junk".to_owned())]]), None);
        assert_eq!(exact_from_of(&Vec::new()), None);
    }

    #[test]
    fn changes_committed_before_the_copy_started_are_not_read_again_once_captured() {
        let next = Lsn::parse("0x0000002A000001F80003").unwrap();
        let bounds = |after_copy: Option<&str>| {
            vec![vec![
                Some("0x00000020000000100001".to_owned()),
                Some("0x0000002A000001F80003".to_owned()),
                after_copy.map(str::to_owned),
            ]]
        };
        let captured_late = Lsn::parse("0x0000002E000056D000D1").unwrap();
        assert_eq!(
            first_change_after_copy(next, &bounds(Some("0x0000002E000056D000D1"))),
            captured_late
        );
        assert_eq!(
            first_change_after_copy(captured_late, &bounds(Some("0x0000002A000001F80003"))),
            captured_late
        );
        assert_eq!(first_change_after_copy(next, &bounds(None)), next);
        assert_eq!(
            first_change_after_copy(next, &bounds(Some("0x00000000000000000000"))),
            next
        );
        assert_eq!(first_change_after_copy(next, &bounds(Some("junk"))), next);
        assert_eq!(first_change_after_copy(next, &Vec::new()), next);
    }

    #[test]
    fn the_copy_start_reads_the_capture_position_and_a_time_a_second_earlier() {
        assert_eq!(
            COPY_START_QUERY,
            "SELECT CONVERT(nvarchar(max), sys.fn_cdc_get_max_lsn(), 1), \
             CONVERT(nvarchar(max), CAST(DATEADD(second, -1, SYSDATETIME()) AS datetime), 121), \
             CAST(DATEPART(TZOFFSET, SYSDATETIMEOFFSET()) AS nvarchar(max))"
        );
    }

    fn started_at_utc_plus_one() -> ServerTime {
        ServerTime {
            time: "2026-10-25 02:40:01.123".to_owned(),
            utc_offset_minutes: 60,
        }
    }

    fn check(passed: &str, offset: &str) -> TextRows {
        vec![vec![Some(passed.to_owned()), Some(offset.to_owned())]]
    }

    #[test]
    fn the_copy_start_keeps_its_time_and_utc_offset_and_is_dropped_when_either_is_unreadable() {
        let row = |time: Option<&str>, offset: Option<&str>| {
            vec![vec![
                Some("0x0000002A000001F80003".to_owned()),
                time.map(str::to_owned),
                offset.map(str::to_owned),
            ]]
        };
        assert_eq!(
            copy_start(&row(Some("2026-10-25 02:40:01.123"), Some("60"))),
            Some(started_at_utc_plus_one())
        );
        assert_eq!(copy_start(&row(Some(""), Some("60"))), None);
        assert_eq!(copy_start(&row(None, Some("60"))), None);
        assert_eq!(
            copy_start(&row(Some("2026-10-25 02:40:01.123"), None)),
            None
        );
        assert_eq!(copy_start(&Vec::new()), None);
        assert_eq!(
            capture_clock_query(&started_at_utc_plus_one()),
            "SELECT CASE WHEN sys.fn_cdc_map_lsn_to_time(sys.fn_cdc_get_max_lsn()) > CONVERT(datetime, N'2026-10-25 02:40:01.123', 121) \
             THEN N'1' ELSE N'0' END, CAST(DATEPART(TZOFFSET, SYSDATETIMEOFFSET()) AS nvarchar(max))"
        );
    }

    #[test]
    fn the_copy_start_skip_waits_for_the_capture_job_then_applies_only_in_the_same_utc_offset() {
        let start = started_at_utc_plus_one();
        assert_eq!(
            capture_clock(&start, &check("0", "60")),
            CaptureClock::Behind
        );
        assert_eq!(
            capture_clock(&start, &check("1", "60")),
            CaptureClock::Passed
        );
        assert_eq!(
            capture_clock(&start, &check("0", "0")),
            CaptureClock::OffsetChanged
        );
        assert_eq!(
            capture_clock(&start, &check("1", "0")),
            CaptureClock::OffsetChanged
        );
        assert_eq!(
            capture_clock(&start, &check("1", "junk")),
            CaptureClock::OffsetChanged
        );
        assert_eq!(
            capture_clock(&start, &Vec::new()),
            CaptureClock::OffsetChanged
        );
    }

    #[test]
    fn the_copy_start_skip_is_used_once_and_never_again_after_it_applies_or_is_dropped() {
        let start = Some(started_at_utc_plus_one());
        assert_eq!(
            copy_start_after_poll(start.clone(), Some(CaptureClock::Behind)),
            start
        );
        assert_eq!(
            copy_start_after_poll(start.clone(), Some(CaptureClock::Passed)),
            None
        );
        assert_eq!(
            copy_start_after_poll(start.clone(), Some(CaptureClock::OffsetChanged)),
            None
        );
        assert_eq!(
            copy_start_after_poll(None, Some(CaptureClock::Behind)),
            None
        );
        let mut state = start;
        for passed in ["0", "1", "0"] {
            let clock = state
                .as_ref()
                .map(|started| capture_clock(started, &check(passed, "60")));
            state = copy_start_after_poll(state, clock);
        }
        assert_eq!(state, None);
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

    #[test]
    fn a_newer_capture_instance_with_the_key_and_a_start_is_the_one_to_switch_to() {
        let table = orders();
        let old = instance("dbo_orders", &["id", "note", "tag"], Some(5));
        let new = instance("dbo_orders_2", &["id", "note", "tag", "extra"], Some(9));
        let both = vec![new.clone(), old.clone()];
        assert_eq!(successor_of(&both, &table, "dbo_orders"), Some(&new));
        assert_eq!(successor_of(&both, &table, "dbo_orders_2"), None);
        let not_started = vec![instance("dbo_orders_2", &["id"], None), old.clone()];
        assert_eq!(successor_of(&not_started, &table, "dbo_orders"), None);
        let keyless = vec![instance("dbo_orders_2", &["note"], Some(9)), old.clone()];
        assert_eq!(successor_of(&keyless, &table, "dbo_orders"), None);
        assert_eq!(
            successor_of(&[new.clone()], &table, "dbo_orders"),
            Some(&new)
        );
        let mut elsewhere = new.clone();
        elsewhere.table = "other".to_owned();
        assert_eq!(successor_of(&[elsewhere, old], &table, "dbo_orders"), None);
    }

    #[test]
    fn switching_instances_adds_the_new_captured_columns_and_drops_the_ones_it_leaves_out() {
        let table = orders();
        let new = instance("dbo_orders_2", &["id", "note", "extra"], Some(9));
        let now = names(&["note", "id", "tag", "extra", "secret"]);
        assert_eq!(
            switch_plan(&table, &new, &now),
            SwitchPlan {
                dropped: names(&["tag"]),
                added: names(&["extra"]),
                left_out: names(&["tag", "secret"]),
            }
        );
        let same = instance("dbo_orders_2", &["id", "note", "tag"], Some(9));
        assert_eq!(
            switch_plan(&table, &same, &names(&["note", "id", "tag"])),
            SwitchPlan {
                dropped: Vec::new(),
                added: Vec::new(),
                left_out: Vec::new(),
            }
        );
    }

    #[test]
    fn added_columns_are_filled_from_the_source_rows_by_key() {
        let table = orders();
        let extra = SourceColumn {
            name: "extra".to_owned(),
            data_type: "nvarchar".to_owned(),
            declared: "nvarchar(50)".to_owned(),
            not_null: true,
        };
        assert_eq!(
            add_column_statement(&table, &extra),
            "ALTER TABLE [dbo].[orders] ADD [extra] nvarchar(50)"
        );
        assert_eq!(
            drop_column_statement(&table, "tag"),
            "ALTER TABLE [dbo].[orders] DROP COLUMN [tag]"
        );
        let added = vec![extra];
        assert_eq!(
            backfill_query(&table, &added),
            "SELECT CAST([id] AS nvarchar(max)), CAST([extra] AS nvarchar(max)) FROM [dbo].[orders]"
        );
        assert_eq!(
            backfill_statement(
                &table,
                &added,
                &[Some("7".to_owned()), Some("it's".to_owned())]
            ),
            Some("UPDATE [dbo].[orders] SET [extra] = N'it''s' WHERE [id] = N'7'".to_owned())
        );
        assert_eq!(
            backfill_statement(&table, &added, &[Some("7".to_owned()), None]),
            Some("UPDATE [dbo].[orders] SET [extra] = NULL WHERE [id] = N'7'".to_owned())
        );
        assert_eq!(
            backfill_statement(&table, &added, &[Some("7".to_owned())]),
            None
        );
        assert_eq!(
            backfill_statement(&table, &[], &[Some("7".to_owned())]),
            None
        );
    }

    fn context() -> PollContext<'static> {
        PollContext {
            target: "db:1433/app",
            database: "app",
            file: "app.db",
        }
    }

    #[test]
    fn a_column_dropped_on_the_source_is_dropped_from_a_cdc_table_in_the_file() {
        let mut table = orders();
        let mut work = PollWork::default();
        follow_column_changes(&context(), &mut table, &names(&["note", "id"]), &mut work)
            .ok()
            .unwrap();
        assert_eq!(
            work.statements,
            vec!["ALTER TABLE [dbo].[orders] DROP COLUMN [tag]".to_owned()]
        );
        assert_eq!(table.followed_names(), names(&["note", "id"]));
        assert_eq!(work.counts.schema_changes, 1);
        assert_eq!(
            work.notices,
            vec![Notice::Dropped {
                table: "dbo.orders".to_owned(),
                columns: names(&["tag"]),
            }]
        );
    }

    #[test]
    fn a_column_added_on_the_source_is_named_once_while_the_capture_instance_leaves_it_out() {
        let mut table = orders();
        let mut work = PollWork::default();
        let now = names(&["note", "id", "tag", "extra"]);
        follow_column_changes(&context(), &mut table, &now, &mut work)
            .ok()
            .unwrap();
        assert!(work.statements.is_empty());
        assert_eq!(table.left_out, names(&["extra"]));
        assert_eq!(
            work.notices,
            vec![Notice::LeftOut {
                table: "dbo.orders".to_owned(),
                instance: "dbo_orders".to_owned(),
                columns: names(&["extra"]),
                try_command: "EXEC sys.sp_cdc_enable_table @source_schema = N'dbo', @source_name = N'orders', @role_name = NULL, @capture_instance = N'dbo_orders_2';".to_owned(),
            }]
        );
        let mut again = PollWork::default();
        follow_column_changes(&context(), &mut table, &now, &mut again)
            .ok()
            .unwrap();
        assert!(again.notices.is_empty());
        follow_column_changes(
            &context(),
            &mut table,
            &names(&["note", "id", "tag"]),
            &mut again,
        )
        .ok()
        .unwrap();
        assert!(table.left_out.is_empty());
    }

    #[test]
    fn a_dropped_key_column_or_a_gone_table_stops_the_follow() {
        let mut table = orders();
        let mut work = PollWork::default();
        let dropped_key =
            follow_column_changes(&context(), &mut table, &names(&["note", "tag"]), &mut work);
        assert!(matches!(dropped_key, Err(PollError::Failed(_))));
        let gone = follow_column_changes(&context(), &mut orders(), &[], &mut work);
        assert!(matches!(gone, Err(PollError::Failed(_))));
        let mut tracked = orders();
        tracked.changes = ChangeSource::Tracking;
        let added = follow_column_changes(
            &context(),
            &mut tracked,
            &names(&["note", "id", "tag", "extra"]),
            &mut work,
        );
        assert!(matches!(added, Err(PollError::Failed(_))));
        assert!(work.statements.is_empty());
        assert_eq!(
            key_dropped("db:1433/app", "dbo.orders", "id"),
            "cannot follow db:1433/app: primary key column id of dbo.orders was dropped on the source, so its CDC changes can no longer be applied by key. Follow again into a new file"
        );
    }

    #[test]
    fn a_resumed_cdc_table_follows_the_columns_the_file_and_the_source_share() {
        let table = orders();
        let resumed = resume_capture_columns(
            &table,
            &names(&["note", "id", "gone"]),
            Some(&names(&["id", "note", "tag"])),
        )
        .unwrap();
        let followed: Vec<&str> = resumed
            .followed
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(followed, vec!["note", "id"]);
        assert_eq!(resumed.left_out, names(&["tag"]));
        assert_eq!(resumed.dropped, names(&["gone"]));
        assert_eq!(
            resume_capture_columns(
                &table,
                &names(&["note", "id", "tag"]),
                Some(&names(&["id", "note"]))
            ),
            Err("the file has tag, which its CDC capture instance does not capture".to_owned())
        );
        assert!(resume_capture_columns(&table, &names(&["note", "id", "tag"]), None).is_ok());
        let mut keyless = orders();
        keyless.columns.retain(|column| column.name != "id");
        assert_eq!(
            resume_capture_columns(&keyless, &names(&["note", "id"]), None),
            Err("primary key column id was dropped".to_owned())
        );
    }

    #[test]
    fn a_second_capture_instance_is_suggested_by_a_free_name() {
        assert_eq!(
            second_instance_command("dbo", "orders", "dbo_orders"),
            "EXEC sys.sp_cdc_enable_table @source_schema = N'dbo', @source_name = N'orders', @role_name = NULL, @capture_instance = N'dbo_orders_2';"
        );
        assert_eq!(
            second_instance_command("dbo", "orders", "dbo_orders_2"),
            "EXEC sys.sp_cdc_enable_table @source_schema = N'dbo', @source_name = N'orders', @role_name = NULL, @capture_instance = N'dbo_orders';"
        );
        assert_eq!(
            enable_capture_command("dbo", "orders", None),
            "EXEC sys.sp_cdc_enable_table @source_schema = N'dbo', @source_name = N'orders', @role_name = NULL;"
        );
    }

    #[test]
    fn a_connection_lost_mid_poll_is_retried_and_any_other_error_stops_the_follow() {
        let lost = TdsClientError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        assert!(matches!(poll_error("db", &lost), PollError::Lost(_)));
        let refused = TdsClientError::Server {
            number: 313,
            message: "An insufficient number of arguments".to_owned(),
        };
        assert!(matches!(poll_error("db", &refused), PollError::Failed(_)));
    }

    #[test]
    fn a_poll_only_moves_the_saved_position_when_it_is_committed() {
        let before = FollowPosition {
            tables: vec![orders()],
            version: None,
            lsn: Some(Lsn(1)),
            copy_started: None,
            exact_after: None,
            exact_from: Some(Lsn(1)),
        };
        let mut after = before.clone();
        if let ChangeSource::Capture { next, .. } = &mut after.tables[0].changes {
            *next = Lsn(9);
        }
        assert_eq!(
            before.saved(),
            vec!["cdc 0x00000000000000000001 0x00000000000000000001 dbo_orders".to_owned()]
        );
        assert_ne!(before.saved(), after.saved());
        assert_eq!(before.shown(), "CDC LSN 0x00000000000000000001");
    }

    #[test]
    fn notices_are_one_json_event_and_one_line_each() {
        let (object, line) = notice_event(&Notice::Switched {
            table: "dbo.orders".to_owned(),
            from: "dbo_orders".to_owned(),
            to: "dbo_orders_2".to_owned(),
            added: names(&["extra"]),
            dropped: Vec::new(),
        });
        let json = object.line();
        assert!(json.contains("\"event\":\"capture_switched\""));
        assert!(json.contains("\"to\":\"dbo_orders_2\""));
        assert_eq!(
            line,
            "  switched        dbo.orders from CDC capture instance dbo_orders to dbo_orders_2 (added extra)\n"
        );
        let (object, line) = notice_event(&Notice::Dropped {
            table: "dbo.orders".to_owned(),
            columns: names(&["tag"]),
        });
        assert!(object.line().contains("\"event\":\"columns_dropped\""));
        assert_eq!(
            line,
            "  dropped column  dbo.orders: tag (dropped on the source)\n"
        );
    }

    #[test]
    fn an_old_capture_instance_dropped_too_early_names_the_lost_range() {
        assert_eq!(
            capture_gap(
                "db:1433/app",
                "dbo.orders",
                "dbo_orders",
                "dbo_orders_2",
                Lsn(9),
                Lsn(5)
            ),
            "cannot follow db:1433/app: the CDC capture instance dbo_orders of dbo.orders was dropped before the follow reached the start of dbo_orders_2 (0x00000000000000000009), so the changes from 0x00000000000000000005 up to it are lost. Follow again into a new file, and drop an old capture instance only after the follow's capture_switched event"
        );
    }
}
