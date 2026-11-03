use super::follow_state::{
    DEFAULT_RECONNECT_SECONDS, IDENTITY, LSN, POSTGRES, PUBLICATION, PUBLICATION_CREATED,
    READ_STATEMENT, SLOT, SavedFollow, reconnect_delay, update_statement,
};
use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject};
use eggdb::sql::Store as DatabaseStore;
use eggdb_pgwire::client::{ClientError, ConnectOptions, PgClient, QueryRows, parse_postgres_url};
use eggdb_pgwire::replication::{
    Change, ChangeApplier, LogicalMessage, Lsn, Relation, RelationColumn, ReplicationError,
    ReshapeStep, SpooledChanges, StreamMessage, StreamOptions, StreamedMessage,
    StreamedTransactions, builtin_type_name, decodes_binary, format_lsn, parse_logical_message,
    parse_lsn, parse_stream_message, parse_streamed_message, quote_identifier, quote_literal,
    server_major_version, standby_status_update,
};
use eggdb_platform::stop_signals;
use eggdb_sql::{
    CopyChannel, CopyStart, Dialect, RelationKind, SqlError, SqlResult, SqlState, Value,
};
use std::time::{Duration, Instant};

const DEFAULT_PUBLICATION: &str = "eggdb_follow";
const STATUS_INTERVAL: Duration = Duration::from_secs(10);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
const POLL_TIMEOUT: Duration = Duration::from_millis(500);
const PASSWORD_VARIABLE: &str = "PGPASSWORD";
const STOP_CHECK: Duration = Duration::from_millis(200);
const SLOT_DROP_ATTEMPTS: u32 = 25;
const UNDEFINED_OBJECT: &str = "42704";

fn connection_lost(error: &ClientError) -> bool {
    match error {
        ClientError::Io(_) => true,
        ClientError::Server { code, .. } => code.starts_with("57P") || code.starts_with("08"),
        _ => false,
    }
}

fn start_replication(
    stream: &mut PgClient,
    slot_name: &str,
    publication: &str,
    at: Lsn,
    options: StreamOptions,
) -> Result<(), ClientError> {
    stream.start_copy_both(&options.start_replication_command(slot_name, at, publication))?;
    stream.set_read_timeout(Some(POLL_TIMEOUT))
}

fn drop_slot(catalog: &mut PgClient, slot_name: &str) -> bool {
    for _ in 0..SLOT_DROP_ATTEMPTS {
        match catalog.simple_query(&format!(
            "SELECT pg_catalog.pg_drop_replication_slot({})",
            quote_literal(slot_name)
        )) {
            Ok(_) => return true,
            Err(ClientError::Server { code, .. }) if code == UNDEFINED_OBJECT => return true,
            Err(ClientError::Server { code, .. }) if code == "55006" => {
                std::thread::sleep(STOP_CHECK);
            }
            Err(_) => return false,
        }
    }
    false
}

fn slot_kept_note(slot_name: &str) -> String {
    format!(
        "The replication slot {slot_name} is kept, so running the same command again resumes; it holds the source's WAL until then (SELECT pg_drop_replication_slot('{slot_name}'); gives up)"
    )
}

fn with_slot_note(failure: Failure, slot_name: &str) -> Failure {
    match failure {
        Failure::Database {
            message,
            try_command,
            exit,
            code,
        } => Failure::Database {
            message: format!("{message}. {}", slot_kept_note(slot_name)),
            try_command,
            exit,
            code,
        },
        other => other,
    }
}

fn orphan_note(slot_name: &str, pending_path: &std::path::Path) -> String {
    format!(
        "The replication slot {slot_name} could not be dropped and holds the source's WAL; running the same command again drops it (it is named in {}), or drop it on the source: {}",
        pending_path.display(),
        drop_slot_command(slot_name)
    )
}

fn with_orphan_note(failure: Failure, slot_name: &str, pending_path: &std::path::Path) -> Failure {
    match failure {
        Failure::Database {
            message,
            try_command,
            exit,
            code,
        } => Failure::Database {
            message: format!("{message}. {}", orphan_note(slot_name, pending_path)),
            try_command: try_command.or_else(|| Some(drop_slot_command(slot_name))),
            exit,
            code,
        },
        other => other,
    }
}

fn applier_for_target(
    session: &mut eggdb_sql::Session<'_>,
    schemas: &[String],
    file_display: &str,
) -> Result<ChangeApplier, Failure> {
    let mut applier = ChangeApplier::new();
    let relations = session.relations().map_err(|error| {
        source_failed(format!(
            "cannot read the tables of {file_display}: {}",
            error.message()
        ))
    })?;
    for relation in relations {
        if let RelationKind::Table(table) = relation.kind
            && relation.schema != "eggdb_follow"
        {
            applier.know_target_table(
                &relation.schema,
                &relation.name,
                table
                    .columns
                    .into_iter()
                    .map(|column| column.name)
                    .collect(),
            );
        }
    }
    applier.only_schemas(schemas.to_vec());
    applier.track_new_tables();
    Ok(applier)
}

fn added_column_type(
    step: &ReshapeStep,
    source_type: Option<(String, Option<String>)>,
) -> Option<String> {
    let (type_oid, type_modifier, adding) = match step {
        ReshapeStep::AddColumn {
            type_oid,
            type_modifier,
            ..
        } => (*type_oid, *type_modifier, true),
        ReshapeStep::ChangeType {
            type_oid,
            type_modifier,
            ..
        } => (*type_oid, *type_modifier, false),
        _ => return None,
    };
    match source_type {
        Some((spelled, default)) => Some(match default {
            Some(default) if adding && !default.contains("nextval(") => {
                format!("{spelled} DEFAULT {default}")
            }
            _ => spelled,
        }),
        None => builtin_type_name(type_oid, type_modifier),
    }
}

pub(super) fn source_failed(message: String) -> Failure {
    Failure::Database {
        message,
        try_command: None,
        exit: Exit::DatabaseError,
        code: "source_failed",
    }
}

fn client_failure(target: &str, error: &ClientError) -> Failure {
    source_failed(match error {
        ClientError::Io(error) => format!(
            "cannot connect to {target}: {error}. Check the host and port, and that PostgreSQL listens there (listen_addresses, pg_hba.conf)"
        ),
        ClientError::Tls(message) => format!("cannot connect to {target} over TLS: {message}"),
        ClientError::Authentication(message) => format!(
            "cannot connect to {target}: {message}. Set PGPASSWORD, or pass --password-file"
        ),
        other => format!("cannot follow {target}: {other}"),
    })
}

fn now_unix_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_micros()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn rows_of(results: Vec<QueryRows>) -> Vec<Vec<Option<String>>> {
    results.into_iter().flat_map(|result| result.rows).collect()
}

fn catalog_rows<const COLUMNS: usize>(
    results: Vec<QueryRows>,
    source: &str,
) -> Result<Vec<[Option<String>; COLUMNS]>, Failure> {
    rows_of(results)
        .into_iter()
        .map(|mut row| {
            row.truncate(COLUMNS);
            <[Option<String>; COLUMNS]>::try_from(row).map_err(|short| {
                source_failed(format!(
                    "cannot follow {source}: it answered a catalog query with {} columns where {COLUMNS} were asked for",
                    short.len()
                ))
            })
        })
        .collect()
}

pub(super) fn copy_stopped(file_display: &str) -> Failure {
    source_failed(format!(
        "stopped during the first copy, before {file_display} held a consistent copy of the source; run the same command again to start the copy over"
    ))
}

pub(super) fn drop_what_the_copy_created(
    store: &DatabaseStore<'_>,
    dialect: Dialect,
    undo: &[String],
) -> Vec<String> {
    let mut session = store.session(dialect);
    let mut left: Vec<String> = undo.iter().rev().cloned().collect();
    loop {
        let before = left.len();
        left.retain(|statement| session.execute(statement, &mut |_| {}).is_err());
        if left.is_empty() || left.len() == before {
            break;
        }
    }
    let _ = session.close();
    left
}

fn with_leftovers_note(failure: Failure, file_display: &str, left: &[String]) -> Failure {
    let Some(first) = left.first() else {
        return failure;
    };
    match failure {
        Failure::Database {
            message,
            try_command,
            exit,
            code,
        } => Failure::Database {
            message: format!(
                "{message}. The tables the copy created in {file_display} could not all be dropped; drop them before running the same command again: {}",
                left.join("; ")
            ),
            try_command: try_command.or_else(|| Some(format!("{first};"))),
            exit,
            code,
        },
        other => other,
    }
}

impl Session<'_> {
    pub(super) fn undo_first_copy(
        &self,
        dialect: Dialect,
        undo: &[String],
        failure: Failure,
    ) -> Failure {
        if undo.is_empty() {
            return failure;
        }
        let left = match self.open(Access::Write { create: false }, None) {
            Ok(db) => drop_what_the_copy_created(&DatabaseStore(&db), dialect, undo),
            Err(_) => undo.iter().rev().cloned().collect(),
        };
        with_leftovers_note(failure, &self.file_display, &left)
    }
}

fn orphaned_slot_left(pending: &PendingSlot, path: &std::path::Path) -> String {
    format!(
        "  orphaned slot   {} of a follow stopped during its copy is still on {}; drop it there: {} Then delete {}, which keeps this note coming until it is gone\n",
        pending.slot_name,
        pending.identity,
        drop_slot_command(&pending.slot_name),
        path.display()
    )
}

fn drop_slot_command(slot_name: &str) -> String {
    format!(
        "SELECT pg_catalog.pg_drop_replication_slot({});",
        quote_literal(slot_name)
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingSlot {
    identity: String,
    slot_name: String,
    publication: String,
    created_publication: bool,
}

const PENDING_SLOT_SUFFIX: &str = ".follow-slot";

fn pending_slot_path(file: &std::path::Path) -> std::path::PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(PENDING_SLOT_SUFFIX);
    std::path::PathBuf::from(name)
}

fn escaped_line(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unescaped_line(line: &str) -> Option<String> {
    let mut value = String::with_capacity(line.len());
    let mut characters = line.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            value.push(character);
            continue;
        }
        value.push(match characters.next()? {
            '\\' => '\\',
            'n' => '\n',
            'r' => '\r',
            _ => return None,
        });
    }
    Some(value)
}

impl PendingSlot {
    fn to_text(&self) -> String {
        format!(
            "{SOURCE_KIND_LINE}\n{IDENTITY} {}\n{SLOT} {}\n{PUBLICATION} {}\n{PUBLICATION_CREATED} {}\n",
            escaped_line(&self.identity),
            escaped_line(&self.slot_name),
            escaped_line(&self.publication),
            self.created_publication
        )
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != SOURCE_KIND_LINE {
            return None;
        }
        let mut value = |item: &str| {
            lines
                .next()?
                .strip_prefix(item)?
                .strip_prefix(' ')
                .and_then(unescaped_line)
        };
        let identity = value(IDENTITY)?;
        let slot_name = value(SLOT)?;
        let publication = value(PUBLICATION)?;
        let created_publication = match value(PUBLICATION_CREATED)?.as_str() {
            "true" => true,
            "false" => false,
            _ => return None,
        };
        (!slot_name.is_empty()).then_some(Self {
            identity,
            slot_name,
            publication,
            created_publication,
        })
    }
}

const SOURCE_KIND_LINE: &str = "eggdb follow slot postgres";

fn text(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}

fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(name))
}

struct SourceTable {
    oid: String,
    schema: String,
    name: String,
    replica_identity: String,
    has_primary_key: bool,
}

impl SourceTable {
    fn shown(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    fn qualified(&self) -> String {
        qualified(&self.schema, &self.name)
    }
}

struct CopyBridge<'c> {
    client: &'c mut PgClient,
    source_sql: String,
    rows: u64,
}

fn sql_failure(error: &ClientError) -> SqlError {
    SqlError::new(
        SqlState::CONNECTION_FAILURE,
        format!("reading from the source failed: {error}"),
    )
}

impl CopyChannel for CopyBridge<'_> {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        self.client
            .start_copy_out(&self.source_sql)
            .map_err(|error| sql_failure(&error))
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        if stop_signals::requested() {
            return Err(SqlError::new(
                SqlState::QUERY_CANCELED,
                "the copy was stopped",
            ));
        }
        match self
            .client
            .read_copy_out()
            .map_err(|error| sql_failure(&error))?
        {
            Some(piece) => {
                self.rows += piece.iter().filter(|byte| **byte == b'\n').count() as u64;
                data.extend_from_slice(&piece);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "follow copies into EggDB only",
        ))
    }

    fn write_copy_data(&mut self, _data: &[u8]) -> SqlResult<()> {
        Err(SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "follow copies into EggDB only",
        ))
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct FollowCounts {
    pub(super) tables: u64,
    pub(super) rows_copied: u64,
    pub(super) transactions: u64,
    pub(super) inserts: u64,
    pub(super) updates: u64,
    pub(super) deletes: u64,
    pub(super) truncates: u64,
    pub(super) sequences: u64,
    pub(super) schema_changes: u64,
}

impl FollowCounts {
    fn count(&mut self, statement: &str) {
        match statement.split(' ').next() {
            Some("INSERT") => self.inserts += 1,
            Some("UPDATE") => self.updates += 1,
            Some("DELETE") => self.deletes += 1,
            Some("TRUNCATE") => self.truncates += 1,
            _ => {}
        }
    }
}

enum StopReason {
    Interrupt,
    CaughtUp,
    SourceClosed,
}

struct Resumed {
    slot_name: String,
    publication: String,
    created_publication: bool,
    start: Lsn,
}

impl StopReason {
    fn name(&self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::CaughtUp => "caught_up",
            Self::SourceClosed => "source_closed",
        }
    }

    fn shown(&self) -> &'static str {
        match self {
            Self::Interrupt => "stopped by Ctrl-C or SIGTERM",
            Self::CaughtUp => "caught up, --until caught-up",
            Self::SourceClosed => "the connection to the source was lost",
        }
    }
}

impl Session<'_> {
    pub(super) fn follow_event(&mut self, object: JsonObject, text: &str) {
        let (shown, line) = match self.format {
            Format::Json => (&mut self.out, object.line()),
            _ => (&mut self.messages, text.to_owned()),
        };
        shown.text(&line);
        let _ = shown.finish();
    }

    pub(super) fn saved_follow(&mut self) -> Result<Option<SavedFollow>, Failure> {
        let db = self.open(Access::Write { create: false }, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::PostgreSql);
        let mut rows = None;
        let read = session.execute(READ_STATEMENT, &mut |result| rows = Some(result.rows));
        let _ = session.close();
        Ok(match (read, rows) {
            (Ok(()), Some(rows)) => Some(SavedFollow::from_rows(rows)),
            _ => None,
        })
    }

    pub(super) fn target_table_columns(
        &mut self,
    ) -> Result<Vec<(String, String, Vec<String>)>, Failure> {
        let db = self.open(Access::Write { create: false }, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::PostgreSql);
        let relations = session.relations();
        let _ = session.close();
        let relations = relations.map_err(|error| {
            source_failed(format!(
                "cannot read the tables of {}: {}",
                self.file_display,
                error.message()
            ))
        })?;
        Ok(relations
            .into_iter()
            .filter_map(|relation| match relation.kind {
                RelationKind::Table(table) if relation.schema != "eggdb_follow" => Some((
                    relation.schema,
                    relation.name,
                    table
                        .columns
                        .into_iter()
                        .map(|column| column.name)
                        .collect(),
                )),
                _ => None,
            })
            .collect())
    }

    pub(super) fn save_follow_position(&mut self, statements: &[String]) -> Result<(), Failure> {
        let db = self.open(Access::Write { create: false }, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::PostgreSql);
        let mut script = String::from("BEGIN;\n");
        for statement in statements {
            script.push_str(statement);
            script.push_str(";\n");
        }
        script.push_str("COMMIT;");
        let saved = session.execute(&script, &mut |_| {});
        let _ = session.close();
        saved.map_err(|error| {
            source_failed(format!(
                "cannot save the follow position in {}: {}",
                self.file_display,
                error.message()
            ))
        })
    }

    pub(super) fn reconnect_for(&self) -> Duration {
        Duration::from_secs(
            self.request
                .follow
                .reconnect_seconds
                .unwrap_or(DEFAULT_RECONNECT_SECONDS),
        )
    }

    pub(super) fn wait_to_reconnect(
        &mut self,
        attempt: &mut u32,
        error: &str,
        deadline: Instant,
    ) -> bool {
        *attempt += 1;
        let delay = reconnect_delay(*attempt);
        if stop_signals::requested() || Instant::now() + delay > deadline {
            return false;
        }
        self.follow_event(
            JsonObject::new()
                .string("event", "reconnecting")
                .number("attempt", u64::from(*attempt))
                .number("wait_ms", delay.as_millis() as u64)
                .string("error", error),
            &format!(
                "  reconnecting    attempt {} in {} s: {error}\n",
                attempt,
                delay.as_secs()
            ),
        );
        let waited = Instant::now();
        while waited.elapsed() < delay {
            if stop_signals::requested() {
                return false;
            }
            std::thread::sleep(STOP_CHECK);
        }
        true
    }

    pub(super) fn reconnected_event(&mut self, attempt: u32, position: &str) {
        self.follow_event(
            JsonObject::new()
                .string("event", "reconnected")
                .number("attempt", u64::from(attempt))
                .string("position", position),
            &format!("  reconnected     after {attempt} attempts, resuming at {position}\n"),
        );
    }

    pub(super) fn source_password(&mut self, variable: &str) -> Result<Option<String>, Failure> {
        match self.request.serve.password_file.clone() {
            Some(crate::cli::args::ExportFile::Path(path)) => {
                let text = std::fs::read_to_string(&path).map_err(|error| {
                    Failure::usage(
                        format!("cannot read --password-file {}: {error}", path.display()),
                        Some(CommandName::Follow),
                    )
                })?;
                Ok(Some(text.lines().next().unwrap_or_default().to_owned()))
            }
            Some(crate::cli::args::ExportFile::Standard) => {
                let mut line = String::new();
                std::io::BufRead::read_line(
                    &mut std::io::BufReader::new(&mut *self.stdin),
                    &mut line,
                )
                .map_err(|error| {
                    Failure::usage(
                        format!("cannot read the password from stdin: {error}"),
                        Some(CommandName::Follow),
                    )
                })?;
                self.stdin_taken = true;
                Ok(Some(line.trim_end_matches(['\r', '\n']).to_owned()))
            }
            None => Ok(std::env::var(variable).ok()),
        }
    }

    pub(super) fn follow(&mut self) -> Result<Exit, Failure> {
        let source = self.request.follow.source.clone();
        if source.starts_with("sqlserver://") {
            return self.follow_sql_server();
        }
        let usage = |message: String| Failure::usage(message, Some(CommandName::Follow));
        if !source.starts_with("postgres://") && !source.starts_with("postgresql://") {
            let scheme = source.split("://").next().unwrap_or(&source);
            return Err(usage(format!(
                "the source must be postgres:// or sqlserver://, not '{scheme}://'"
            )));
        }
        if self.request.follow.poll_seconds.is_some() {
            return Err(usage(
                "--poll is for SQL Server sources; PostgreSQL streams its changes".to_owned(),
            ));
        }
        if self.request.follow.sql_server_changes.is_some() {
            return Err(usage(
                "--changes is for SQL Server sources; PostgreSQL streams its changes".to_owned(),
            ));
        }
        let mut options = parse_postgres_url(&source).map_err(usage)?;
        options.password = self.source_password(PASSWORD_VARIABLE)?;
        self.follow_postgres(options)
    }

    fn follow_postgres(&mut self, options: ConnectOptions) -> Result<Exit, Failure> {
        let target = format!("{}:{}/{}", options.host, options.port, options.database);
        let existed = self.request.file.exists();
        let started = Instant::now();
        let mut catalog =
            PgClient::connect(&options).map_err(|error| client_failure(&target, &error))?;
        let tls = catalog.tls_version().map_or("no TLS", |version| {
            if version == eggdb_tls::cipher_suite::version::TLS13 {
                "TLS 1.3"
            } else {
                "TLS 1.2"
            }
        });
        let server = catalog.server_version().to_owned();
        self.follow_event(
            JsonObject::new()
                .string("event", "connected")
                .string("source", &options.shown())
                .string("server", &server)
                .string("tls", tls)
                .string("file", &self.file_display)
                .boolean("created", !existed),
            &format!(
                "following {} into {}{}\n  connected       PostgreSQL {server}, {tls}, sslmode={}\n",
                options.shown(),
                self.file_display,
                if existed { "" } else { " (created)" },
                options.ssl_mode.name()
            ),
        );
        let wal_level = rows_of(
            catalog
                .simple_query("SHOW wal_level")
                .map_err(|error| client_failure(&target, &error))?,
        );
        let wal_level = wal_level
            .first()
            .and_then(|row| row.first())
            .map(text)
            .unwrap_or_default();
        if wal_level != "logical" {
            return Err(Failure::Database {
                message: format!(
                    "cannot follow {target}: wal_level is '{wal_level}'; a logical stream needs 'logical'. Set it and restart PostgreSQL (a reload is not enough)"
                ),
                try_command: Some("ALTER SYSTEM SET wal_level = logical;".to_owned()),
                exit: Exit::DatabaseError,
                code: "source_failed",
            });
        }

        let mut replication_options = options.clone();
        replication_options.replication = true;
        let mut stream = PgClient::connect(&replication_options)
            .map_err(|error| client_failure(&target, &error))?;
        let system = rows_of(
            stream
                .simple_query("IDENTIFY_SYSTEM")
                .map_err(|error| client_failure(&target, &error))?,
        );
        let system_row = system.into_iter().next().unwrap_or_default();
        let identity = format!(
            "system {} database {}",
            system_row.first().map(text).unwrap_or_default(),
            system_row.get(3).map(text).unwrap_or_default()
        );
        let saved = if existed { self.saved_follow()? } else { None };
        self.clear_pending_slot(&mut catalog, &identity, saved.as_ref());
        let _ = stop_signals::watch();
        let mut counts = FollowCounts::default();
        let resumed = match &saved {
            Some(saved) => {
                let resumed = self.resume_postgres(&mut catalog, &target, &identity, saved)?;
                self.follow_event(
                    JsonObject::new()
                        .string("event", "resumed")
                        .string("slot", &resumed.slot_name)
                        .string("publication", &resumed.publication)
                        .string("lsn", &format_lsn(resumed.start)),
                    &format!(
                        "  resumed         slot {}, publication {}, from {}\n",
                        resumed.slot_name,
                        resumed.publication,
                        format_lsn(resumed.start)
                    ),
                );
                resumed
            }
            None => self.start_postgres(
                &mut catalog,
                &mut stream,
                &options,
                &target,
                &identity,
                existed,
                started,
                &mut counts,
            )?,
        };
        let Resumed {
            slot_name,
            publication,
            created_publication,
            start,
        } = resumed;

        let followed = self.stream_changes(
            &mut stream,
            &mut catalog,
            &replication_options,
            &slot_name,
            &publication,
            start,
            &mut counts,
        );
        let (reason, applied) = followed.map_err(|failure| with_slot_note(failure, &slot_name))?;
        stream.terminate();
        let ok = !matches!(reason, StopReason::SourceClosed);
        counts.sequences = match self.copy_sequence_values(&mut catalog) {
            Ok(sequences) => sequences,
            Err(_) if !ok => 0,
            Err(failure) => return Err(with_slot_note(failure, &slot_name)),
        };
        self.save_follow_position(&[update_statement(LSN, &format_lsn(applied))])?;
        let finished = matches!(reason, StopReason::CaughtUp);
        let slot_dropped = finished && drop_slot(&mut catalog, &slot_name);
        let publication_dropped = finished
            && created_publication
            && catalog
                .simple_query(&format!(
                    "DROP PUBLICATION IF EXISTS {}",
                    quote_identifier(&publication)
                ))
                .is_ok();
        catalog.terminate();
        let kept = if slot_dropped {
            String::new()
        } else {
            format!(" {}", slot_kept_note(&slot_name))
        };
        let verdict = if ok {
            format!(
                "{} holds every source transaction committed up to {}; point applications at it now.{kept}",
                self.file_display,
                format_lsn(applied)
            )
        } else {
            format!(
                "the connection to the source was lost for longer than --reconnect-for; {} is consistent at {} but changes after it are not in it.{kept}",
                self.file_display,
                format_lsn(applied)
            )
        };
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("event", "stopped")
                    .boolean("ok", ok)
                    .string("reason", reason.name())
                    .number("elapsed_ms", started.elapsed().as_millis() as u64)
                    .number("tables", counts.tables)
                    .number("rows_copied", counts.rows_copied)
                    .number("transactions", counts.transactions)
                    .number("inserts", counts.inserts)
                    .number("updates", counts.updates)
                    .number("deletes", counts.deletes)
                    .number("truncates", counts.truncates)
                    .number("sequences", counts.sequences)
                    .number("schema_changes", counts.schema_changes)
                    .string("lsn", &format_lsn(applied))
                    .string("slot", &slot_name)
                    .boolean("slot_dropped", slot_dropped)
                    .boolean("publication_dropped", publication_dropped)
                    .string("verdict", &verdict)
                    .line(),
            ),
            _ => self.out.text(&format!(
                "followed {} into {} for {:.0} s ({})\n  tables          {}, {} rows copied\n  transactions    {} applied (inserts {}, updates {}, deletes {}, truncates {}, schema changes {})\n  sequences       {} copied at stop\n  position        {}\n  slot            {slot_name} {}{}\n{}: {verdict}\n",
                options.shown(),
                self.file_display,
                started.elapsed().as_secs_f64(),
                reason.shown(),
                counts.tables,
                counts.rows_copied,
                counts.transactions,
                counts.inserts,
                counts.updates,
                counts.deletes,
                counts.truncates,
                counts.schema_changes,
                counts.sequences,
                format_lsn(applied),
                if slot_dropped { "dropped" } else { "kept for resuming" },
                if publication_dropped { format!(", publication {publication} dropped") } else { String::new() },
                if ok { "ok" } else { "stopped" },
            )),
        }
        Ok(if ok { Exit::Done } else { Exit::DatabaseError })
    }

    fn clear_pending_slot(
        &mut self,
        catalog: &mut PgClient,
        identity: &str,
        saved: Option<&SavedFollow>,
    ) {
        let path = pending_slot_path(&self.request.file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Some(pending) = PendingSlot::parse(&text) else {
            self.follow_event(
                JsonObject::new()
                    .string("event", "orphaned_slot")
                    .string("file", &path.display().to_string())
                    .boolean("dropped", false),
                &format!(
                    "  orphaned slot   {0} is not readable; a replication slot it named may still hold the source's WAL (see pg_replication_slots). Drop that slot on the source, then delete {0}\n",
                    path.display()
                ),
            );
            return;
        };
        if saved.and_then(|saved| saved.get(SLOT)) == Some(pending.slot_name.as_str()) {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let dropped = pending.identity == identity && drop_slot(catalog, &pending.slot_name);
        if dropped && pending.created_publication {
            let _ = catalog.simple_query(&format!(
                "DROP PUBLICATION IF EXISTS {}",
                quote_identifier(&pending.publication)
            ));
        }
        if dropped {
            let _ = std::fs::remove_file(&path);
        }
        self.follow_event(
            JsonObject::new()
                .string("event", "orphaned_slot")
                .string("slot", &pending.slot_name)
                .string("file", &path.display().to_string())
                .boolean("dropped", dropped),
            &if dropped {
                format!(
                    "  orphaned slot   {} of a follow stopped during its copy, dropped\n",
                    pending.slot_name
                )
            } else {
                orphaned_slot_left(&pending, &path)
            },
        );
    }

    fn resume_postgres(
        &mut self,
        catalog: &mut PgClient,
        target: &str,
        identity: &str,
        saved: &SavedFollow,
    ) -> Result<Resumed, Failure> {
        let cannot = |reason: String| {
            source_failed(format!(
                "cannot resume the follow into {}: {reason}",
                self.file_display
            ))
        };
        if let Some(refusal) = saved.refusal(POSTGRES, identity, &self.file_display) {
            return Err(cannot(refusal));
        }
        let slot_name = saved.get(SLOT).unwrap_or_default().to_owned();
        let publication = saved.get(PUBLICATION).unwrap_or_default().to_owned();
        let start = saved
            .get(LSN)
            .and_then(parse_lsn)
            .ok_or_else(|| cannot("its saved position is not readable".to_owned()))?;
        let slot = rows_of(
            catalog
                .simple_query(&format!(
                    "SELECT 1 FROM pg_catalog.pg_replication_slots WHERE slot_name = {}",
                    quote_literal(&slot_name)
                ))
                .map_err(|error| client_failure(target, &error))?,
        );
        if slot.is_empty() {
            return Err(cannot(format!(
                "its replication slot {slot_name} is gone from {target}, so the changes after {} are lost. Follow again into a new file with --create",
                format_lsn(start)
            )));
        }
        Ok(Resumed {
            slot_name,
            publication,
            created_publication: saved.get(PUBLICATION_CREATED) == Some("true"),
            start,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn start_postgres(
        &mut self,
        catalog: &mut PgClient,
        stream: &mut PgClient,
        options: &ConnectOptions,
        target: &str,
        identity: &str,
        existed: bool,
        started: Instant,
        counts: &mut FollowCounts,
    ) -> Result<Resumed, Failure> {
        let (publication, created_publication) = self.prepare_publication(catalog, target)?;
        let slot_name = format!(
            "eggdb_follow_{}_{}",
            now_unix_micros() / 1_000_000,
            std::process::id()
        );
        let pending = PendingSlot {
            identity: identity.to_owned(),
            slot_name: slot_name.clone(),
            publication: publication.clone(),
            created_publication,
        };
        let pending_path = pending_slot_path(&self.request.file);
        std::fs::write(&pending_path, pending.to_text()).map_err(|error| {
            source_failed(format!(
                "cannot write {} before creating the replication slot: {error}",
                pending_path.display()
            ))
        })?;
        if stop_signals::requested() {
            let _ = std::fs::remove_file(&pending_path);
            return Err(copy_stopped(&self.file_display));
        }
        let slot = stream
            .simple_query(&format!(
                "CREATE_REPLICATION_SLOT {} LOGICAL pgoutput EXPORT_SNAPSHOT",
                quote_identifier(&slot_name)
            ))
            .map_err(|error| match &error {
                ClientError::Server { code, .. } if code == "42501" => Failure::Database {
                    message: format!(
                        "cannot follow {target}: {} may not create a replication slot (needs REPLICATION or superuser)",
                        options.user
                    ),
                    try_command: Some(format!("ALTER ROLE {} REPLICATION;", quote_identifier(&options.user))),
                    exit: Exit::DatabaseError,
                    code: "source_failed",
                },
                ClientError::Server { code, .. } if code == "53400" => source_failed(format!(
                    "cannot follow {target}: every replication slot is in use (max_replication_slots). Drop one you no longer need, or raise it and restart"
                )),
                _ => client_failure(target, &error),
            });
        let slot = match slot {
            Ok(slot) => slot,
            Err(failure) => {
                let _ = std::fs::remove_file(&pending_path);
                return Err(failure);
            }
        };
        let slot_row = rows_of(slot).into_iter().next().unwrap_or_default();
        let consistent_point = slot_row
            .get(1)
            .map(text)
            .and_then(|lsn| parse_lsn(&lsn))
            .ok_or_else(|| {
                source_failed(format!(
                    "cannot follow {target}: the replication slot reported no position"
                ))
            })?;
        let snapshot = slot_row.get(2).map(text).unwrap_or_default();
        self.follow_event(
            JsonObject::new()
                .string("event", "slot")
                .string("name", &slot_name)
                .boolean("temporary", false)
                .string("snapshot_lsn", &format_lsn(consistent_point)),
            &format!(
                "  publication     {publication}{}\n  slot            {slot_name}, snapshot at {}\n",
                if created_publication { " (created, FOR ALL TABLES)" } else { "" },
                format_lsn(consistent_point)
            ),
        );
        let mut saved = SavedFollow::new(POSTGRES, identity);
        saved.set(SLOT, &slot_name);
        saved.set(PUBLICATION, &publication);
        saved.set(
            PUBLICATION_CREATED,
            if created_publication { "true" } else { "false" },
        );
        saved.set(LSN, &format_lsn(consistent_point));
        let mut created = Vec::new();
        let copy_outcome = self.copy_snapshot(
            catalog,
            target,
            &snapshot,
            &publication,
            &saved,
            counts,
            &mut created,
        );
        if let Err(failure) = copy_outcome {
            let slot_dropped = stream
                .simple_query(&format!(
                    "DROP_REPLICATION_SLOT {}",
                    quote_identifier(&slot_name)
                ))
                .is_ok();
            let publication_dropped = !created_publication
                || catalog
                    .simple_query(&format!(
                        "DROP PUBLICATION IF EXISTS {}",
                        quote_identifier(&publication)
                    ))
                    .is_ok();
            let failure = if existed {
                self.undo_first_copy(Dialect::PostgreSql, &created, failure)
            } else {
                let _ = std::fs::remove_file(&self.request.file);
                failure
            };
            if slot_dropped && publication_dropped {
                let _ = std::fs::remove_file(&pending_path);
                return Err(failure);
            }
            return Err(with_orphan_note(failure, &slot_name, &pending_path));
        }
        let _ = std::fs::remove_file(&pending_path);
        self.follow_event(
            JsonObject::new()
                .string("event", "copied")
                .number("tables", counts.tables)
                .number("rows", counts.rows_copied)
                .number("elapsed_ms", started.elapsed().as_millis() as u64),
            &format!(
                "  copied          {} tables, {} rows in {:.1} s; now following\n",
                counts.tables,
                counts.rows_copied,
                started.elapsed().as_secs_f64()
            ),
        );
        Ok(Resumed {
            slot_name,
            publication,
            created_publication,
            start: consistent_point,
        })
    }

    fn prepare_publication(
        &mut self,
        catalog: &mut PgClient,
        target: &str,
    ) -> Result<(String, bool), Failure> {
        let wanted = self.request.follow.publication.clone();
        let name = wanted
            .clone()
            .unwrap_or_else(|| DEFAULT_PUBLICATION.to_owned());
        let existing = rows_of(
            catalog
                .simple_query(&format!(
                    "SELECT 1 FROM pg_catalog.pg_publication WHERE pubname = {}",
                    quote_literal(&name)
                ))
                .map_err(|error| client_failure(target, &error))?,
        );
        if !existing.is_empty() {
            return Ok((name, false));
        }
        if wanted.is_some() {
            return Err(source_failed(format!(
                "cannot follow {target}: publication '{name}' does not exist"
            )));
        }
        catalog
            .simple_query(&format!("CREATE PUBLICATION {} FOR ALL TABLES", quote_identifier(&name)))
            .map_err(|error| match &error {
                ClientError::Server { code, .. } if code == "42501" => Failure::Database {
                    message: format!(
                        "cannot follow {target}: the user may not create a publication (needs to own every table, or superuser). Have the owner create one and pass --publication"
                    ),
                    try_command: Some(format!("CREATE PUBLICATION {name} FOR ALL TABLES;")),
                    exit: Exit::DatabaseError,
                    code: "source_failed",
                },
                _ => client_failure(target, &error),
            })?;
        Ok((name, true))
    }

    fn schema_filter(&self) -> String {
        if self.request.follow.schemas.is_empty() {
            "n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_%'"
                .to_owned()
        } else {
            let names: Vec<String> = self
                .request
                .follow
                .schemas
                .iter()
                .map(|name| quote_literal(name))
                .collect();
            format!("n.nspname IN ({})", names.join(", "))
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_snapshot(
        &mut self,
        catalog: &mut PgClient,
        target: &str,
        snapshot: &str,
        publication: &str,
        saved: &SavedFollow,
        counts: &mut FollowCounts,
        created: &mut Vec<String>,
    ) -> Result<(), Failure> {
        let failed = |error: ClientError| client_failure(target, &error);
        catalog
            .simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .map_err(failed)?;
        catalog
            .simple_query(&format!(
                "SET TRANSACTION SNAPSHOT {}",
                quote_literal(snapshot)
            ))
            .map_err(failed)?;
        let filter = self.schema_filter();
        let tables: Vec<SourceTable> = catalog_rows::<5>(
            catalog
                .simple_query(&format!(
                    "SELECT c.oid::text, n.nspname, c.relname, c.relreplident::text, \
                     EXISTS (SELECT 1 FROM pg_catalog.pg_constraint k WHERE k.conrelid = c.oid AND k.contype = 'p')::text \
                     FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     WHERE c.relkind IN ('r', 'p') AND NOT c.relispartition AND {filter} ORDER BY n.nspname, c.relname"
                ))
                .map_err(failed)?,
            target,
        )?
        .into_iter()
        .map(|row| SourceTable {
            oid: text(&row[0]),
            schema: text(&row[1]),
            name: text(&row[2]),
            replica_identity: text(&row[3]),
            has_primary_key: text(&row[4]) == "true",
        })
        .collect();

        let unkeyed: Vec<String> = tables
            .iter()
            .filter(|table| match table.replica_identity.as_str() {
                "f" | "i" => false,
                "d" => !table.has_primary_key,
                _ => true,
            })
            .map(SourceTable::shown)
            .collect();
        if !unkeyed.is_empty() {
            return Err(Failure::Database {
                message: format!(
                    "cannot follow {target}: {} table{} no primary key or replica identity, so their updates and deletes could not be applied: {}. Give each a primary key or REPLICA IDENTITY FULL, or leave their schema out with --schema",
                    unkeyed.len(),
                    if unkeyed.len() == 1 { " has" } else { "s have" },
                    unkeyed.join(", ")
                ),
                try_command: Some(format!("ALTER TABLE {} REPLICA IDENTITY FULL;", unkeyed[0])),
                exit: Exit::DatabaseError,
                code: "source_failed",
            });
        }
        let published: Vec<String> = catalog_rows::<1>(
            catalog
                .simple_query(&format!(
                    "SELECT schemaname || '.' || tablename FROM pg_catalog.pg_publication_tables WHERE pubname = {}",
                    quote_literal(publication)
                ))
                .map_err(failed)?,
            target,
        )?
        .iter()
        .map(|row| text(&row[0]))
        .collect();
        let unpublished: Vec<String> = tables
            .iter()
            .map(SourceTable::shown)
            .filter(|name| !published.contains(name))
            .collect();
        if !unpublished.is_empty() {
            return Err(source_failed(format!(
                "cannot follow {target}: publication '{publication}' does not publish {} of the tables: {}; their changes would never arrive. Add them to it, or leave them out with --schema",
                unpublished.len(),
                unpublished.join(", ")
            )));
        }

        let db = self.open(
            Access::Write {
                create: self.request.create,
            },
            None,
        )?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::PostgreSql);
        let mut run = |sql: &str| -> Result<(), Failure> {
            session.execute(sql, &mut |_| {}).map_err(|error| {
                source_failed(format!(
                    "cannot create in {}: {} ({sql})",
                    self.file_display,
                    error.message()
                ))
            })
        };
        let schemas: Vec<String> = {
            let mut names: Vec<String> = tables.iter().map(|table| table.schema.clone()).collect();
            names.dedup();
            names
        };
        for schema in &schemas {
            if schema != "public" {
                run(&format!(
                    "CREATE SCHEMA IF NOT EXISTS {}",
                    quote_identifier(schema)
                ))?;
            }
        }
        let sequences = catalog_rows::<8>(
            catalog
                .simple_query(&format!(
                    "SELECT s.schemaname, s.sequencename, s.data_type::text, s.start_value::text, s.min_value::text, \
                     s.max_value::text, s.increment_by::text, s.cycle::text FROM pg_catalog.pg_sequences s \
                     JOIN pg_catalog.pg_namespace n ON n.nspname = s.schemaname WHERE {filter}"
                ))
                .map_err(failed)?,
            target,
        )?;
        for sequence in &sequences {
            let sequence_name = qualified(&text(&sequence[0]), &text(&sequence[1]));
            run(&format!(
                "CREATE SEQUENCE {} AS {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {}{}",
                sequence_name,
                text(&sequence[2]),
                text(&sequence[6]),
                text(&sequence[4]),
                text(&sequence[5]),
                text(&sequence[3]),
                if text(&sequence[7]) == "true" {
                    " CYCLE"
                } else {
                    ""
                }
            ))?;
            created.push(format!("DROP SEQUENCE {sequence_name}"));
        }
        let mut deferred = Vec::new();
        for table in &tables {
            let definition = table_definition(catalog, &table.oid, &table.qualified(), target)?;
            deferred.extend(
                definition
                    .foreign_keys
                    .into_iter()
                    .map(|(_, statement)| statement),
            );
            run(&definition.create)
                .map_err(|failure| match failure {
                    Failure::Database { message, .. } if message.contains("already exists") => Failure::Database {
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
        drop(run);
        self.follow_event(
            JsonObject::new()
                .string("event", "schema")
                .raw(
                    "schemas",
                    &format!(
                        "[{}]",
                        schemas
                            .iter()
                            .map(|name| render::json_string(name))
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                )
                .number("tables", tables.len() as u64)
                .number("sequences", sequences.len() as u64),
            &format!(
                "  schema          {}: {} tables, {} sequences\n",
                schemas.join(", "),
                tables.len(),
                sequences.len()
            ),
        );
        for table in &tables {
            if stop_signals::requested() {
                return Err(copy_stopped(&self.file_display));
            }
            let copied_at = Instant::now();
            let mut bridge = CopyBridge {
                client: &mut *catalog,
                source_sql: format!("COPY {} TO STDOUT", table.qualified()),
                rows: 0,
            };
            session
                .execute_with_copy(
                    &format!("COPY {} FROM STDIN", table.qualified()),
                    &mut |_| true,
                    &mut bridge,
                )
                .map_err(|error| {
                    if stop_signals::requested() {
                        copy_stopped(&self.file_display)
                    } else {
                        source_failed(format!(
                            "cannot copy {}: {}",
                            table.shown(),
                            error.message()
                        ))
                    }
                })?;
            let rows = bridge.rows;
            counts.tables += 1;
            counts.rows_copied += rows;
            self.follow_event(
                JsonObject::new()
                    .string("event", "table_copied")
                    .string("table", &table.shown())
                    .number("rows", rows)
                    .number("elapsed_ms", copied_at.elapsed().as_millis() as u64),
                &format!(
                    "  copied          {:<28} {rows:>10} rows   {:.1} s\n",
                    table.shown(),
                    copied_at.elapsed().as_secs_f64()
                ),
            );
        }
        let indexes = catalog_rows::<1>(
            catalog
                .simple_query(&format!(
                    "SELECT i.indexdef FROM pg_catalog.pg_indexes i JOIN pg_catalog.pg_namespace n ON n.nspname = i.schemaname \
                     WHERE {filter} AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class x ON x.oid = k.conindid \
                     WHERE x.relname = i.indexname AND k.connamespace = n.oid)"
                ))
                .map_err(failed)?,
            target,
        )?;
        for statement in indexes
            .iter()
            .map(|row| text(&row[0]))
            .chain(deferred)
            .chain(saved.create_statements())
        {
            session.execute(&statement, &mut |_| {}).map_err(|error| {
                source_failed(format!(
                    "cannot create in {}: {} ({statement})",
                    self.file_display,
                    error.message()
                ))
            })?;
        }
        let _ = session.close();
        catalog.simple_query("COMMIT").map_err(failed)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn stream_changes(
        &mut self,
        stream: &mut PgClient,
        catalog: &mut PgClient,
        replication_options: &ConnectOptions,
        slot_name: &str,
        publication: &str,
        start: Lsn,
        counts: &mut FollowCounts,
    ) -> Result<(StopReason, Lsn), Failure> {
        let failed =
            |error: ClientError| source_failed(format!("the replication stream failed: {error}"));
        let major_version = server_major_version(catalog.server_version());
        let mut options = StreamOptions::for_server(
            major_version,
            self.followed_types_decode_binary(catalog, publication),
        );
        start_replication(stream, slot_name, publication, start, options).map_err(failed)?;
        self.stream_options_event(options);
        let db = self.open(Access::Write { create: false }, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::PostgreSql);
        let schemas = self.request.follow.schemas.clone();
        let (copied, foreign_keys) = self.copied_tables(&mut session)?;
        let mut applying = Applying {
            applier: applier_for_target(&mut session, &schemas, &self.file_display)?,
            session,
            pending: Vec::new(),
            in_transaction: false,
            applied: start,
            server_end: start,
            copied,
            foreign_keys,
        };
        applying.restore_copied();
        let mut streamed = StreamedTransactions::new(
            spill_directory(&self.request.file),
            spill_prefix(&self.request.file),
            STREAM_MEMORY_LIMIT,
        );
        let mut in_stream: Option<u32> = None;
        let mut last_status = Instant::now();
        let mut last_progress = Instant::now();
        let until_caught_up = self.request.follow.until_caught_up;
        loop {
            if stop_signals::requested() && !applying.in_transaction {
                return Ok((StopReason::Interrupt, applying.applied));
            }
            let read = stream.read_copy_both();
            let lost = match &read {
                Ok(None) => Some("the source ended the stream".to_owned()),
                Err(error) if connection_lost(error) => Some(error.to_string()),
                _ => None,
            };
            if let Some(lost) = lost {
                streamed.clear();
                in_stream = None;
                applying.start_over(&schemas, &self.file_display)?;
                match self.reconnect_stream(
                    stream,
                    catalog,
                    replication_options,
                    slot_name,
                    publication,
                    applying.applied,
                    options,
                    &lost,
                ) {
                    Some(reason) => return Ok((reason, applying.applied)),
                    None => continue,
                }
            }
            let message = match read {
                Ok(Some(message)) => message,
                Ok(None) => continue,
                Err(ClientError::TimedOut) => {
                    if last_status.elapsed() >= STATUS_INTERVAL || until_caught_up {
                        let _ = stream.send_copy_data(&standby_status_update(
                            applying.applied,
                            now_unix_micros(),
                            until_caught_up,
                        ));
                        last_status = Instant::now();
                    }
                    continue;
                }
                Err(error) => return Err(failed(error)),
            };
            match parse_stream_message(&message)
                .map_err(|error| source_failed(error.to_string()))?
            {
                StreamMessage::Keepalive {
                    wal_end,
                    reply_requested,
                } => {
                    applying.server_end = applying.server_end.max(wal_end);
                    if !applying.in_transaction && in_stream.is_none() {
                        applying.applied = applying.applied.max(wal_end);
                        if until_caught_up {
                            stream
                                .send_copy_data(&standby_status_update(
                                    applying.applied,
                                    now_unix_micros(),
                                    false,
                                ))
                                .map_err(failed)?;
                            return Ok((StopReason::CaughtUp, applying.applied));
                        }
                    }
                    if reply_requested {
                        let _ = stream.send_copy_data(&standby_status_update(
                            applying.applied,
                            now_unix_micros(),
                            false,
                        ));
                        last_status = Instant::now();
                    }
                }
                StreamMessage::XLogData { wal_end, data, .. } => {
                    applying.server_end = applying.server_end.max(wal_end);
                    let unreadable = |error: ReplicationError| source_failed(error.to_string());
                    let outcome = if let Some(xid) = in_stream {
                        match parse_streamed_message(&data).map_err(unreadable)? {
                            StreamedMessage::Change {
                                xid: subtransaction,
                                message,
                            } => streamed
                                .push(xid, subtransaction, message)
                                .map_err(unreadable)?,
                            StreamedMessage::Control(LogicalMessage::StreamStop) => {
                                in_stream = None
                            }
                            StreamedMessage::Control(_) => {}
                        }
                        Applied::Done
                    } else {
                        match parse_logical_message(&data).map_err(unreadable)? {
                            LogicalMessage::StreamStart { xid, first_segment } => {
                                streamed.start(xid, first_segment);
                                in_stream = Some(xid);
                                Applied::Done
                            }
                            LogicalMessage::StreamStop => Applied::Done,
                            LogicalMessage::StreamAbort {
                                xid,
                                subtransaction,
                            } => {
                                streamed.abort(xid, subtransaction).map_err(unreadable)?;
                                Applied::Done
                            }
                            LogicalMessage::StreamCommit {
                                xid,
                                commit_lsn,
                                end_lsn,
                                commit_unix_micros,
                            } => {
                                let changes = streamed.commit(xid).map_err(unreadable)?;
                                self.apply_streamed(
                                    &mut applying,
                                    catalog,
                                    replication_options,
                                    counts,
                                    options,
                                    xid,
                                    changes,
                                    LogicalMessage::Commit {
                                        commit_lsn,
                                        end_lsn,
                                        commit_unix_micros,
                                    },
                                )?
                            }
                            message => self.apply_message(
                                &mut applying,
                                catalog,
                                replication_options,
                                counts,
                                options,
                                message,
                            )?,
                        }
                    };
                    if outcome == Applied::ReadAsText {
                        options = options.without_binary();
                        streamed.clear();
                        in_stream = None;
                        applying.start_over(&schemas, &self.file_display)?;
                        self.restart_stream(
                            stream,
                            replication_options,
                            slot_name,
                            publication,
                            applying.applied,
                            options,
                        )?;
                        self.stream_options_event(options);
                    }
                }
            }
            if last_progress.elapsed() >= PROGRESS_INTERVAL && self.format != Format::Json {
                last_progress = Instant::now();
                let lag = applying.server_end.saturating_sub(applying.applied);
                self.messages.text(&format!(
                    "  following       lag {lag} bytes, {} transactions applied, at {}\n",
                    counts.transactions,
                    format_lsn(applying.applied)
                ));
            }
        }
    }

    fn followed_types_decode_binary(&self, catalog: &mut PgClient, publication: &str) -> bool {
        let filter = self.schema_filter();
        catalog
            .simple_query(&format!(
                "SELECT DISTINCT a.atttypid::text FROM pg_catalog.pg_publication_tables p \
                 JOIN pg_catalog.pg_namespace n ON n.nspname = p.schemaname \
                 JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = p.tablename \
                 JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid \
                 WHERE p.pubname = {} AND a.attnum > 0 AND NOT a.attisdropped AND {filter}",
                quote_literal(publication)
            ))
            .map(|results| {
                rows_of(results).iter().all(|row| {
                    row.first()
                        .map(text)
                        .and_then(|oid| oid.parse::<u32>().ok())
                        .is_some_and(decodes_binary)
                })
            })
            .unwrap_or(false)
    }

    fn stream_options_event(&mut self, options: StreamOptions) {
        self.follow_event(
            JsonObject::new()
                .string("event", "stream")
                .number("protocol", u64::from(options.protocol_version))
                .boolean("streaming", options.streaming)
                .boolean("binary", options.binary),
            &format!(
                "  stream          pgoutput protocol {}, {}, {} tuples\n",
                options.protocol_version,
                if options.streaming {
                    "large transactions streamed"
                } else {
                    "whole transactions"
                },
                if options.binary { "binary" } else { "text" }
            ),
        );
    }

    fn copied_tables(
        &mut self,
        session: &mut eggdb_sql::Session<'_>,
    ) -> Result<(Vec<(u32, Lsn)>, Vec<PendingForeignKey>), Failure> {
        let mut rows = Vec::new();
        session
            .execute(COPIED_TABLES_STATEMENT, &mut |result| rows = result.rows)
            .map_err(|error| {
                source_failed(format!(
                    "cannot read the follow state of {}: {}",
                    self.file_display,
                    error.message()
                ))
            })?;
        let items: Vec<(String, String)> = rows
            .into_iter()
            .filter_map(|row| {
                let mut values = row.into_iter();
                match (values.next(), values.next()) {
                    (Some(Value::Text(item)), Some(Value::Text(value))) => Some((item, value)),
                    _ => None,
                }
            })
            .collect();
        Ok(copied_tables_from(&items))
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_streamed(
        &mut self,
        applying: &mut Applying<'_>,
        catalog: &mut PgClient,
        replication_options: &ConnectOptions,
        counts: &mut FollowCounts,
        options: StreamOptions,
        xid: u32,
        changes: SpooledChanges,
        commit: LogicalMessage,
    ) -> Result<Applied, Failure> {
        let final_lsn = match &commit {
            LogicalMessage::Commit { commit_lsn, .. } => *commit_lsn,
            _ => applying.applied,
        };
        self.apply_message(
            applying,
            catalog,
            replication_options,
            counts,
            options,
            LogicalMessage::Begin { final_lsn, xid },
        )?;
        for change in changes {
            let message = change
                .and_then(|bytes| parse_logical_message(&bytes))
                .map_err(|error| source_failed(error.to_string()))?;
            if self.apply_message(
                applying,
                catalog,
                replication_options,
                counts,
                options,
                message,
            )? == Applied::ReadAsText
            {
                return Ok(Applied::ReadAsText);
            }
        }
        self.apply_message(
            applying,
            catalog,
            replication_options,
            counts,
            options,
            commit,
        )
    }

    fn apply_message(
        &mut self,
        applying: &mut Applying<'_>,
        catalog: &mut PgClient,
        replication_options: &ConnectOptions,
        counts: &mut FollowCounts,
        options: StreamOptions,
        message: LogicalMessage,
    ) -> Result<Applied, Failure> {
        if let LogicalMessage::Relation(relation) = &message
            && options.binary
            && relation
                .columns
                .iter()
                .any(|column| !decodes_binary(column.type_oid))
        {
            return Ok(Applied::ReadAsText);
        }
        let applied = applying.applied;
        let change = applying.applier.apply(message).map_err(|error| {
            source_failed(format!(
                "{error}. {} is consistent at {}; follow again into a new file to take the source's new shape",
                self.file_display,
                format_lsn(applied)
            ))
        })?;
        match change {
            Change::Begin => {
                applying.in_transaction = true;
                applying.pending.clear();
            }
            Change::Statement(statement) => applying.pending.push(statement),
            Change::Reshape {
                relation,
                namespace,
                name,
                steps,
            } => {
                for step in &steps {
                    let statement =
                        self.reshape_statement(catalog, relation, &namespace, &name, step)?;
                    self.follow_event(
                        JsonObject::new()
                            .string("event", "schema_changed")
                            .string("table", &format!("{namespace}.{name}"))
                            .string("statement", &statement),
                        &format!("  schema change   {namespace}.{name}: {statement}\n"),
                    );
                    counts.schema_changes += 1;
                    if applying.in_transaction {
                        applying.pending.push(statement);
                    } else {
                        applying.session.execute(&statement, &mut |_| {}).map_err(|error| {
                            source_failed(format!(
                                "a change to the columns of {namespace}.{name} could not be applied: {} ({statement})",
                                error.message()
                            ))
                        })?;
                    }
                }
            }
            Change::NewTable(relation) => {
                self.add_new_table(applying, catalog, replication_options, counts, &relation)?
            }
            Change::Commit { end_lsn, .. } => {
                let mut script = String::from("BEGIN;\n");
                for statement in &applying.pending {
                    counts.count(statement);
                    script.push_str(statement);
                    script.push_str(";\n");
                }
                let (ready, waiting): (Vec<PendingForeignKey>, Vec<PendingForeignKey>) =
                    std::mem::take(&mut applying.foreign_keys)
                        .into_iter()
                        .partition(|key| end_lsn >= key.snapshot_lsn);
                for key in &ready {
                    script.push_str(&key.statement);
                    script.push_str(";\n");
                    script.push_str(&delete_state_statement(&key.item));
                    script.push_str(";\n");
                }
                script.push_str(&update_statement(LSN, &format_lsn(end_lsn)));
                script.push_str(";\nCOMMIT;");
                let outcome = applying.session.execute(&script, &mut |_| {});
                if let Err(error) = outcome {
                    let _ = applying.session.execute("ROLLBACK", &mut |_| {});
                    return Err(source_failed(format!(
                        "a source transaction could not be applied: {}; {} is consistent at {}",
                        error.message(),
                        self.file_display,
                        format_lsn(applying.applied)
                    )));
                }
                applying.foreign_keys = waiting;
                counts.transactions += 1;
                applying.pending.clear();
                applying.in_transaction = false;
                applying.applied = end_lsn;
                applying.server_end = applying.server_end.max(end_lsn);
            }
            Change::Nothing => {}
        }
        Ok(Applied::Done)
    }

    fn add_new_table(
        &mut self,
        applying: &mut Applying<'_>,
        catalog: &mut PgClient,
        replication_options: &ConnectOptions,
        counts: &mut FollowCounts,
        relation: &Relation,
    ) -> Result<(), Failure> {
        let shown = format!("{}.{}", relation.namespace, relation.name);
        let still_there = !rows_of(
            catalog
                .simple_query(&format!(
                    "SELECT 1 FROM pg_catalog.pg_class WHERE oid = {}",
                    relation.id
                ))
                .map_err(|error| source_failed(format!("cannot read the new table {shown}: {error}")))?,
        )
        .is_empty();
        let started = Instant::now();
        let (rows, snapshot_lsn) = if still_there {
            let copy = self.copy_new_table(applying, catalog, replication_options, relation)?;
            applying.applier.copied(relation.id, Some(copy.columns), copy.snapshot_lsn);
            applying.copied.push((relation.id, copy.snapshot_lsn));
            applying.foreign_keys.extend(copy.foreign_keys);
            (copy.rows, Some(copy.snapshot_lsn))
        } else {
            let create = new_table_from_relation(relation).map_err(source_failed)?;
            for statement in create {
                applying.session.execute(&statement, &mut |_| {}).map_err(|error| {
                    source_failed(format!(
                        "cannot create the new table {shown} in {}: {} ({statement})",
                        self.file_display,
                        error.message()
                    ))
                })?;
            }
            (0, None)
        };
        counts.tables += 1;
        counts.rows_copied += rows;
        let mut event = JsonObject::new()
            .string("event", "table_added")
            .string("table", &shown)
            .boolean("copied", snapshot_lsn.is_some())
            .number("rows", rows)
            .number("elapsed_ms", started.elapsed().as_millis() as u64);
        if let Some(lsn) = snapshot_lsn {
            event = event.string("snapshot_lsn", &format_lsn(lsn));
        }
        self.follow_event(
            event,
            &match snapshot_lsn {
                Some(lsn) => format!(
                    "  table added     {shown}: created and copied, {rows} rows at {}\n",
                    format_lsn(lsn)
                ),
                None => format!(
                    "  table added     {shown}: created from the stream (the source no longer has it)\n"
                ),
            },
        );
        Ok(())
    }

    fn copy_new_table(
        &mut self,
        applying: &mut Applying<'_>,
        catalog: &mut PgClient,
        replication_options: &ConnectOptions,
        relation: &Relation,
    ) -> Result<NewTableCopy, Failure> {
        let shown = format!("{}.{}", relation.namespace, relation.name);
        let cannot = |reason: String| source_failed(format!("cannot copy the new table {shown}: {reason}"));
        let mut slot_client = PgClient::connect(replication_options)
            .map_err(|error| cannot(error.to_string()))?;
        let slot = slot_client
            .simple_query(&format!(
                "CREATE_REPLICATION_SLOT {} TEMPORARY LOGICAL pgoutput EXPORT_SNAPSHOT",
                quote_identifier(&format!(
                    "eggdb_follow_{}_{}_{}",
                    now_unix_micros() / 1_000_000,
                    std::process::id(),
                    relation.id
                ))
            ))
            .map_err(|error| cannot(error.to_string()))?;
        let slot_row = rows_of(slot).into_iter().next().unwrap_or_default();
        let snapshot_lsn = slot_row
            .get(1)
            .map(text)
            .and_then(|lsn| parse_lsn(&lsn))
            .ok_or_else(|| cannot("the replication slot reported no position".to_owned()))?;
        let snapshot = slot_row.get(2).map(text).unwrap_or_default();
        let copied = self.copy_new_table_at(applying, catalog, relation, &snapshot, snapshot_lsn);
        let _ = catalog.simple_query("COMMIT");
        slot_client.terminate();
        copied.map_err(|failure| match failure {
            Failure::Database { message, .. } => cannot(message),
            other => other,
        })
    }

    fn copy_new_table_at(
        &mut self,
        applying: &mut Applying<'_>,
        catalog: &mut PgClient,
        relation: &Relation,
        snapshot: &str,
        snapshot_lsn: Lsn,
    ) -> Result<NewTableCopy, Failure> {
        let target = format!("{}.{}", relation.namespace, relation.name);
        let failed = |error: ClientError| source_failed(error.to_string());
        catalog
            .simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .map_err(failed)?;
        catalog
            .simple_query(&format!("SET TRANSACTION SNAPSHOT {}", quote_literal(snapshot)))
            .map_err(failed)?;
        let qualified_name = qualified(&relation.namespace, &relation.name);
        let definition =
            table_definition(catalog, &relation.id.to_string(), &qualified_name, &target)?;
        let indexes = catalog_rows::<1>(
            catalog
                .simple_query(&format!(
                    "SELECT i.indexdef FROM pg_catalog.pg_indexes i JOIN pg_catalog.pg_namespace n ON n.nspname = i.schemaname \
                     WHERE i.schemaname = {} AND i.tablename = {} AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint k \
                     JOIN pg_catalog.pg_class x ON x.oid = k.conindid WHERE x.relname = i.indexname AND k.connamespace = n.oid)",
                    quote_literal(&relation.namespace),
                    quote_literal(&relation.name)
                ))
                .map_err(failed)?,
            &target,
        )?;
        let foreign_keys: Vec<PendingForeignKey> = definition
            .foreign_keys
            .into_iter()
            .map(|(name, statement)| PendingForeignKey {
                item: foreign_key_item(relation.id, &name),
                statement,
                snapshot_lsn,
            })
            .collect();
        let mut statements = Vec::new();
        if relation.namespace != "public" {
            statements.push(format!(
                "CREATE SCHEMA IF NOT EXISTS {}",
                quote_identifier(&relation.namespace)
            ));
        }
        statements.push(definition.create);
        let mut after_copy: Vec<String> = indexes.iter().map(|row| text(&row[0])).collect();
        after_copy.push(insert_state_statement(
            &copied_item(relation.id),
            &format_lsn(snapshot_lsn),
        ));
        for key in &foreign_keys {
            after_copy.push(delete_state_statement(&key.item));
            after_copy.push(insert_state_statement(&key.item, &key.statement));
        }
        run_in(&mut applying.session, "BEGIN", &self.file_display)?;
        let rows = match fill_new_table(
            &mut applying.session,
            catalog,
            &qualified_name,
            &statements,
            &after_copy,
            &self.file_display,
        ) {
            Ok(rows) => rows,
            Err(failure) => {
                let _ = applying.session.execute("ROLLBACK", &mut |_| {});
                return Err(failure);
            }
        };
        Ok(NewTableCopy {
            columns: definition.columns,
            foreign_keys,
            rows,
            snapshot_lsn,
        })
    }

    fn restart_stream(
        &mut self,
        stream: &mut PgClient,
        replication_options: &ConnectOptions,
        slot_name: &str,
        publication: &str,
        at: Lsn,
        options: StreamOptions,
    ) -> Result<(), Failure> {
        let failed = |error: ClientError| {
            source_failed(format!(
                "cannot restart the replication stream with text tuples: {error}"
            ))
        };
        let fresh = PgClient::connect(replication_options).map_err(failed)?;
        std::mem::replace(stream, fresh).terminate();
        let mut attempt = 0;
        loop {
            match start_replication(stream, slot_name, publication, at, options) {
                Ok(()) => return Ok(()),
                Err(ClientError::Server { code, .. })
                    if code == SLOT_IN_USE && attempt < SLOT_DROP_ATTEMPTS =>
                {
                    attempt += 1;
                    std::thread::sleep(STOP_CHECK);
                    let fresh = PgClient::connect(replication_options).map_err(failed)?;
                    std::mem::replace(stream, fresh).terminate();
                }
                Err(error) => return Err(failed(error)),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn reconnect_stream(
        &mut self,
        stream: &mut PgClient,
        catalog: &mut PgClient,
        replication_options: &ConnectOptions,
        slot_name: &str,
        publication: &str,
        applied: Lsn,
        options: StreamOptions,
        lost: &str,
    ) -> Option<StopReason> {
        let deadline = Instant::now() + self.reconnect_for();
        let mut error = lost.to_owned();
        let mut attempt = 0u32;
        while self.wait_to_reconnect(&mut attempt, &error, deadline) {
            let connected = PgClient::connect(replication_options).and_then(|mut client| {
                start_replication(&mut client, slot_name, publication, applied, options)?;
                Ok(client)
            });
            match connected {
                Ok(client) => {
                    let _ = std::mem::replace(stream, client);
                    let mut catalog_options = replication_options.clone();
                    catalog_options.replication = false;
                    if let Ok(fresh) = PgClient::connect(&catalog_options) {
                        let _ = std::mem::replace(catalog, fresh);
                    }
                    self.reconnected_event(attempt, &format_lsn(applied));
                    return None;
                }
                Err(failure) => error = failure.to_string(),
            }
        }
        Some(if stop_signals::requested() {
            StopReason::Interrupt
        } else {
            StopReason::SourceClosed
        })
    }

    fn reshape_statement(
        &self,
        catalog: &mut PgClient,
        relation: u32,
        namespace: &str,
        name: &str,
        step: &ReshapeStep,
    ) -> Result<String, Failure> {
        let column = match step {
            ReshapeStep::AddColumn { name, .. } | ReshapeStep::ChangeType { name, .. } => {
                Some(name.as_str())
            }
            _ => None,
        };
        let source_type = column.and_then(|column| {
            catalog
                .simple_query(&format!(
                    "SELECT pg_catalog.format_type(a.atttypid, a.atttypmod), pg_catalog.pg_get_expr(d.adbin, d.adrelid) \
                     FROM pg_catalog.pg_attribute a LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                     WHERE a.attrelid = {relation} AND a.attname = {} AND NOT a.attisdropped",
                    quote_literal(column)
                ))
                .ok()
                .and_then(|results| rows_of(results).into_iter().next())
                .map(|row| {
                    (
                        row.first().map(text).unwrap_or_default(),
                        row.get(1).cloned().flatten(),
                    )
                })
                .filter(|(spelled, _)| !spelled.is_empty())
        });
        step.sql(namespace, name, added_column_type(step, source_type).as_deref())
            .map_err(|error| {
                source_failed(format!(
                    "cannot follow a change to the columns of {namespace}.{name} on the source: {error}. Apply the same change to the table in {} (eggdb sql), then run this follow again to resume",
                    self.file_display
                ))
            })
    }

    fn copy_sequence_values(&mut self, catalog: &mut PgClient) -> Result<u64, Failure> {
        let filter = self.schema_filter();
        let values = catalog_rows::<3>(
            catalog
                .simple_query(&format!(
                    "SELECT s.schemaname, s.sequencename, s.last_value::text FROM pg_catalog.pg_sequences s \
                     JOIN pg_catalog.pg_namespace n ON n.nspname = s.schemaname WHERE {filter} AND s.last_value IS NOT NULL"
                ))
                .map_err(|error| source_failed(format!("cannot read the sequences at stop: {error}")))?,
            "the source",
        )?;
        let db = self.open(Access::Write { create: false }, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::PostgreSql);
        for row in &values {
            let name = qualified(&text(&row[0]), &text(&row[1]));
            session
                .execute(
                    &format!(
                        "SELECT pg_catalog.setval({}, {}, true)",
                        quote_literal(&name),
                        text(&row[2])
                    ),
                    &mut |_| {},
                )
                .map_err(|error| {
                    source_failed(format!(
                        "cannot set the sequence {name}: {}",
                        error.message()
                    ))
                })?;
        }
        let _ = session.close();
        Ok(values.len() as u64)
    }
}

const STREAM_MEMORY_LIMIT: usize = 64 << 20;
const SLOT_IN_USE: &str = "55006";
const COPIED_ITEM_PREFIX: &str = "copied ";
const FOREIGN_KEY_ITEM_PREFIX: &str = "foreign key ";
const COPIED_TABLES_STATEMENT: &str = "SELECT item, value FROM eggdb_follow.state WHERE item LIKE 'copied %' OR item LIKE 'foreign key %'";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Applied {
    Done,
    ReadAsText,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingForeignKey {
    item: String,
    statement: String,
    snapshot_lsn: Lsn,
}

struct NewTableCopy {
    columns: Vec<RelationColumn>,
    foreign_keys: Vec<PendingForeignKey>,
    rows: u64,
    snapshot_lsn: Lsn,
}

struct TableDefinition {
    create: String,
    foreign_keys: Vec<(String, String)>,
    columns: Vec<RelationColumn>,
}

struct Applying<'d> {
    session: eggdb_sql::Session<'d>,
    applier: ChangeApplier,
    pending: Vec<String>,
    in_transaction: bool,
    applied: Lsn,
    server_end: Lsn,
    copied: Vec<(u32, Lsn)>,
    foreign_keys: Vec<PendingForeignKey>,
}

impl Applying<'_> {
    fn restore_copied(&mut self) {
        for (relation, snapshot_lsn) in &self.copied {
            self.applier.copied(*relation, None, *snapshot_lsn);
        }
    }

    fn start_over(&mut self, schemas: &[String], file_display: &str) -> Result<(), Failure> {
        self.pending.clear();
        self.in_transaction = false;
        self.applier = applier_for_target(&mut self.session, schemas, file_display)?;
        self.restore_copied();
        Ok(())
    }
}

fn spill_directory(file: &std::path::Path) -> std::path::PathBuf {
    file.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| std::path::PathBuf::from("."), std::path::Path::to_path_buf)
}

fn spill_prefix(file: &std::path::Path) -> String {
    format!(
        "{}.follow-",
        file.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    )
}

fn copied_item(relation: u32) -> String {
    format!("{COPIED_ITEM_PREFIX}{relation}")
}

fn foreign_key_item(relation: u32, name: &str) -> String {
    format!("{FOREIGN_KEY_ITEM_PREFIX}{relation} {name}")
}

fn insert_state_statement(item: &str, value: &str) -> String {
    format!(
        "INSERT INTO eggdb_follow.state (item, value) VALUES ({}, {})",
        quote_literal(item),
        quote_literal(value)
    )
}

fn delete_state_statement(item: &str) -> String {
    format!(
        "DELETE FROM eggdb_follow.state WHERE item = {}",
        quote_literal(item)
    )
}

fn copied_tables_from(items: &[(String, String)]) -> (Vec<(u32, Lsn)>, Vec<PendingForeignKey>) {
    let copied: Vec<(u32, Lsn)> = items
        .iter()
        .filter_map(|(item, value)| {
            Some((
                item.strip_prefix(COPIED_ITEM_PREFIX)?.parse().ok()?,
                parse_lsn(value)?,
            ))
        })
        .collect();
    let foreign_keys = items
        .iter()
        .filter_map(|(item, value)| {
            let (relation, _) = item.strip_prefix(FOREIGN_KEY_ITEM_PREFIX)?.split_once(' ')?;
            let relation: u32 = relation.parse().ok()?;
            let (_, snapshot_lsn) = copied.iter().find(|(id, _)| *id == relation)?;
            Some(PendingForeignKey {
                item: item.clone(),
                statement: value.clone(),
                snapshot_lsn: *snapshot_lsn,
            })
        })
        .collect();
    (copied, foreign_keys)
}

fn new_table_from_relation(relation: &Relation) -> Result<Vec<String>, String> {
    let shown = format!("{}.{}", relation.namespace, relation.name);
    let mut definitions = Vec::with_capacity(relation.columns.len() + 1);
    for column in &relation.columns {
        let spelled = builtin_type_name(column.type_oid, column.type_modifier).ok_or_else(|| {
            format!(
                "the source created the table {shown} and dropped it again, and its column {} has type {}, which follow cannot spell without the source's catalog. Create {shown} in the file (eggdb sql), then run this follow again to resume",
                column.name, column.type_oid
            )
        })?;
        definitions.push(format!("{} {spelled}", quote_identifier(&column.name)));
    }
    let keys: Vec<String> = relation
        .columns
        .iter()
        .filter(|column| column.key)
        .map(|column| quote_identifier(&column.name))
        .collect();
    if relation.replica_identity == b'd' && !keys.is_empty() {
        definitions.push(format!("PRIMARY KEY ({})", keys.join(", ")));
    }
    let mut statements = Vec::new();
    if relation.namespace != "public" {
        statements.push(format!(
            "CREATE SCHEMA IF NOT EXISTS {}",
            quote_identifier(&relation.namespace)
        ));
    }
    statements.push(format!(
        "CREATE TABLE {} ({})",
        qualified(&relation.namespace, &relation.name),
        definitions.join(", ")
    ));
    Ok(statements)
}

fn run_in(
    session: &mut eggdb_sql::Session<'_>,
    sql: &str,
    file_display: &str,
) -> Result<(), Failure> {
    session.execute(sql, &mut |_| {}).map_err(|error| {
        source_failed(format!(
            "cannot create it in {file_display}: {} ({sql})",
            error.message()
        ))
    })
}

fn fill_new_table(
    session: &mut eggdb_sql::Session<'_>,
    catalog: &mut PgClient,
    qualified_name: &str,
    before_copy: &[String],
    after_copy: &[String],
    file_display: &str,
) -> Result<u64, Failure> {
    for statement in before_copy {
        run_in(session, statement, file_display)?;
    }
    let mut bridge = CopyBridge {
        client: catalog,
        source_sql: format!("COPY {qualified_name} TO STDOUT"),
        rows: 0,
    };
    session
        .execute_with_copy(
            &format!("COPY {qualified_name} FROM STDIN"),
            &mut |_| true,
            &mut bridge,
        )
        .map_err(|error| source_failed(error.message().to_owned()))?;
    let rows = bridge.rows;
    for statement in after_copy {
        run_in(session, statement, file_display)?;
    }
    run_in(session, "COMMIT", file_display)?;
    Ok(rows)
}

fn table_definition(
    catalog: &mut PgClient,
    oid: &str,
    qualified_name: &str,
    target: &str,
) -> Result<TableDefinition, Failure> {
    let failed = |error: ClientError| client_failure(target, &error);
    let columns = catalog_rows::<7>(
        catalog
            .simple_query(&format!(
                "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull::text, \
                 pg_catalog.pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, a.atttypid::text, a.atttypmod::text \
                 FROM pg_catalog.pg_attribute a LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = {oid} AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
            ))
            .map_err(failed)?,
        target,
    )?;
    let constraints = catalog_rows::<3>(
        catalog
            .simple_query(&format!(
                "SELECT conname, contype::text, pg_catalog.pg_get_constraintdef(oid) FROM pg_catalog.pg_constraint \
                 WHERE conrelid = {oid} AND contype IN ('p', 'u', 'c', 'f') ORDER BY contype DESC, conname"
            ))
            .map_err(failed)?,
        target,
    )?;
    let mut foreign_keys = Vec::new();
    let mut definitions: Vec<String> = columns
        .iter()
        .map(|column| {
            let mut definition = format!(
                "{} {}",
                quote_identifier(&text(&column[0])),
                text(&column[1])
            );
            match text(&column[4]).as_str() {
                "a" => definition.push_str(" GENERATED ALWAYS AS IDENTITY"),
                "d" => definition.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                _ => {
                    if let Some(default) = &column[3] {
                        definition.push_str(&format!(" DEFAULT {default}"));
                    }
                }
            }
            if text(&column[2]) == "true" {
                definition.push_str(" NOT NULL");
            }
            definition
        })
        .collect();
    for constraint in &constraints {
        let clause = format!(
            "CONSTRAINT {} {}",
            quote_identifier(&text(&constraint[0])),
            text(&constraint[2])
        );
        if text(&constraint[1]) == "f" {
            foreign_keys.push((
                text(&constraint[0]),
                format!("ALTER TABLE {qualified_name} ADD {clause}"),
            ));
        } else {
            definitions.push(clause);
        }
    }
    Ok(TableDefinition {
        create: format!("CREATE TABLE {qualified_name} ({})", definitions.join(", ")),
        foreign_keys,
        columns: columns
            .iter()
            .map(|column| RelationColumn {
                key: false,
                name: text(&column[0]),
                type_oid: text(&column[5]).parse().unwrap_or(0),
                type_modifier: text(&column[6]).parse().unwrap_or(-1),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(rows: Vec<Vec<Option<String>>>) -> Vec<QueryRows> {
        vec![QueryRows {
            rows,
            ..QueryRows::default()
        }]
    }

    #[test]
    fn a_catalog_answer_with_too_few_columns_is_an_error_not_a_panic() {
        let short = answer(vec![vec![Some("a".to_owned())]]);
        assert!(catalog_rows::<3>(short, "db:5432/app").is_err());
        let wide = answer(vec![vec![
            Some("a".to_owned()),
            None,
            Some("c".to_owned()),
            Some("extra".to_owned()),
        ]]);
        let rows = catalog_rows::<3>(wide, "db:5432/app").unwrap();
        assert_eq!(
            rows,
            vec![[Some("a".to_owned()), None, Some("c".to_owned())]]
        );
        assert!(catalog_rows::<1>(answer(vec![Vec::new()]), "db").is_err());
    }

    #[test]
    fn the_pending_slot_is_written_before_the_copy_and_read_back_by_the_next_run() {
        let pending = PendingSlot {
            identity: "system 7 database a\nb\\c".to_owned(),
            slot_name: "eggdb_follow_1_2".to_owned(),
            publication: "eggdb_follow".to_owned(),
            created_publication: true,
        };
        let text = pending.to_text();
        assert_eq!(text.lines().count(), 5);
        assert_eq!(PendingSlot::parse(&text), Some(pending));
        assert_eq!(PendingSlot::parse(""), None);
        assert_eq!(PendingSlot::parse("eggdb follow slot postgres\n"), None);
        assert_eq!(
            PendingSlot::parse(
                "eggdb follow slot postgres\nidentity x\nslot \npublication p\npublication_created false\n"
            ),
            None
        );
        assert_eq!(
            PendingSlot::parse(
                "eggdb follow slot postgres\nidentity x\\q\nslot s\npublication p\npublication_created false\n"
            ),
            None
        );
    }

    #[test]
    fn an_orphaned_slot_of_another_source_names_the_sidecar_to_delete() {
        let pending = PendingSlot {
            identity: "system 7 database shop".to_owned(),
            slot_name: "eggdb_follow_1_2".to_owned(),
            publication: "eggdb_follow".to_owned(),
            created_publication: false,
        };
        let note = orphaned_slot_left(&pending, std::path::Path::new("dir/app.eggdb.follow-slot"));
        assert!(note.contains("system 7 database shop"), "{note}");
        assert!(
            note.contains("SELECT pg_catalog.pg_drop_replication_slot('eggdb_follow_1_2');"),
            "{note}"
        );
        assert!(
            note.contains("Then delete dir/app.eggdb.follow-slot"),
            "{note}"
        );
    }

    fn database_with(statements: &[&str]) -> eggdb::Database {
        let db = eggdb::Database::in_memory().expect("in memory");
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::SqlServer);
        for statement in statements {
            session.execute(statement, &mut |_| {}).expect(statement);
        }
        let _ = session.close();
        db
    }

    fn tables_of(db: &eggdb::Database) -> Vec<String> {
        let store = DatabaseStore(db);
        let mut session = store.session(Dialect::SqlServer);
        let mut names = Vec::new();
        session
            .execute(
                "SELECT table_schema + '.' + table_name FROM information_schema.tables \
                 WHERE table_schema <> 'information_schema' ORDER BY 1",
                &mut |result| {
                    for row in &result.rows {
                        names.extend(row.iter().filter_map(|value| value.to_text()));
                    }
                },
            )
            .expect("tables");
        let _ = session.close();
        names
    }

    #[test]
    fn a_failed_first_copy_into_an_existing_file_drops_only_what_it_created() {
        let db = database_with(&[
            "CREATE TABLE dbo.kept (id int PRIMARY KEY)",
            "CREATE SCHEMA sales",
            "CREATE TABLE sales.orders (id int PRIMARY KEY)",
            "CREATE TABLE sales.lines (id int PRIMARY KEY, order_id int REFERENCES sales.orders (id))",
        ]);
        let left = drop_what_the_copy_created(
            &DatabaseStore(&db),
            Dialect::SqlServer,
            &[
                "DROP SCHEMA [sales]".to_owned(),
                "DROP TABLE [sales].[lines]".to_owned(),
                "DROP TABLE [sales].[orders]".to_owned(),
            ],
        );
        assert_eq!(left, Vec::<String>::new());
        assert_eq!(tables_of(&db), ["dbo.kept"]);
    }

    #[test]
    fn what_the_copy_could_not_drop_is_named_with_the_statements_to_run() {
        let failure = with_leftovers_note(
            copy_stopped("app.eggdb"),
            "app.eggdb",
            &["DROP TABLE [dbo].[orders]".to_owned()],
        );
        let Failure::Database {
            message,
            try_command,
            ..
        } = failure
        else {
            panic!("a database failure");
        };
        assert!(message.contains("could not all be dropped"), "{message}");
        assert!(message.contains("DROP TABLE [dbo].[orders]"), "{message}");
        assert_eq!(try_command.as_deref(), Some("DROP TABLE [dbo].[orders];"));
        let untouched = with_leftovers_note(copy_stopped("app.eggdb"), "app.eggdb", &[]);
        let Failure::Database { message, .. } = untouched else {
            panic!("a database failure");
        };
        assert!(!message.contains("could not all be dropped"), "{message}");
    }

    #[test]
    fn the_pending_slot_sits_next_to_the_file() {
        assert_eq!(
            pending_slot_path(std::path::Path::new("dir/app.eggdb")),
            std::path::PathBuf::from("dir/app.eggdb.follow-slot")
        );
    }

    #[test]
    fn an_orphaned_slot_names_its_exact_drop_command() {
        let failure = with_orphan_note(
            copy_stopped("app.eggdb"),
            "eggdb_follow_1_2",
            std::path::Path::new("app.eggdb.follow-slot"),
        );
        let Failure::Database {
            message,
            try_command,
            ..
        } = failure
        else {
            panic!("a database failure");
        };
        assert!(message.contains("stopped during the first copy"));
        assert!(message.contains("app.eggdb.follow-slot"));
        assert_eq!(
            try_command.as_deref(),
            Some("SELECT pg_catalog.pg_drop_replication_slot('eggdb_follow_1_2');")
        );
    }

    #[test]
    fn copied_tables_and_their_waiting_foreign_keys_are_read_from_the_follow_state() {
        let items = vec![
            (copied_item(16_400), "0/3000060".to_owned()),
            (copied_item(16_401), "not a position".to_owned()),
            (
                foreign_key_item(16_400, "late_customer_fkey"),
                "ALTER TABLE \"public\".\"late\" ADD CONSTRAINT x FOREIGN KEY (c) REFERENCES customers(id)".to_owned(),
            ),
            (foreign_key_item(16_402, "orphan"), "ALTER TABLE y".to_owned()),
            ("lsn".to_owned(), "0/10".to_owned()),
        ];
        let (copied, foreign_keys) = copied_tables_from(&items);
        assert_eq!(copied, vec![(16_400, 0x0300_0060)]);
        assert_eq!(foreign_keys.len(), 1);
        assert_eq!(foreign_keys[0].item, "foreign key 16400 late_customer_fkey");
        assert_eq!(foreign_keys[0].snapshot_lsn, 0x0300_0060);
        assert!(insert_state_statement("copied 1", "0/1").starts_with("INSERT INTO eggdb_follow.state"));
        assert_eq!(
            delete_state_statement("it's"),
            "DELETE FROM eggdb_follow.state WHERE item = 'it''s'"
        );
    }

    fn late_relation(type_oid: u32) -> Relation {
        Relation {
            id: 16_500,
            namespace: "sales".to_owned(),
            name: "late".to_owned(),
            replica_identity: b'd',
            columns: vec![
                RelationColumn {
                    key: true,
                    name: "id".to_owned(),
                    type_oid: 23,
                    type_modifier: -1,
                },
                RelationColumn {
                    key: false,
                    name: "label".to_owned(),
                    type_oid,
                    type_modifier: 24,
                },
            ],
        }
    }

    #[test]
    fn a_new_table_the_source_no_longer_has_is_created_from_its_relation_message() {
        assert_eq!(
            new_table_from_relation(&late_relation(1043)).unwrap(),
            vec![
                "CREATE SCHEMA IF NOT EXISTS \"sales\"".to_owned(),
                "CREATE TABLE \"sales\".\"late\" (\"id\" integer, \"label\" character varying(20), PRIMARY KEY (\"id\"))".to_owned(),
            ]
        );
        let refused = new_table_from_relation(&late_relation(600)).unwrap_err();
        assert!(refused.contains("sales.late"));
        assert!(refused.contains("label"));
    }

    #[test]
    fn streamed_transactions_spill_next_to_the_file() {
        let file = std::path::Path::new("/data/app.eggdb");
        assert_eq!(spill_directory(file), std::path::PathBuf::from("/data"));
        assert_eq!(spill_prefix(file), "app.eggdb.follow-");
        assert_eq!(
            spill_directory(std::path::Path::new("app.eggdb")),
            std::path::PathBuf::from(".")
        );
    }
}
