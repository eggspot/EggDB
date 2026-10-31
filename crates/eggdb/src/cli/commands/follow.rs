use super::{Access, Session};
use crate::cli::args::{CommandName, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject};
use eggdb::sql::Store as DatabaseStore;
use eggdb_pgwire::client::{ClientError, ConnectOptions, PgClient, QueryRows, parse_postgres_url};
use eggdb_pgwire::replication::{
    Change, ChangeApplier, Lsn, StreamMessage, format_lsn, parse_logical_message, parse_lsn,
    parse_stream_message, quote_identifier, quote_literal, standby_status_update,
};
use eggdb_platform::stop_signals;
use eggdb_sql::{CopyChannel, CopyStart, Dialect, SqlError, SqlResult, SqlState};
use std::time::{Duration, Instant};

const DEFAULT_PUBLICATION: &str = "eggdb_follow";
const STATUS_INTERVAL: Duration = Duration::from_secs(10);
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
const POLL_TIMEOUT: Duration = Duration::from_millis(500);
const PASSWORD_VARIABLE: &str = "PGPASSWORD";

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
    SqlError::new(SqlState::CONNECTION_FAILURE, format!("reading from the source failed: {error}"))
}

impl CopyChannel for CopyBridge<'_> {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        self.client.start_copy_out(&self.source_sql).map_err(|error| sql_failure(&error))
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        match self.client.read_copy_out().map_err(|error| sql_failure(&error))? {
            Some(piece) => {
                self.rows += piece.iter().filter(|byte| **byte == b'\n').count() as u64;
                data.extend_from_slice(&piece);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Err(SqlError::new(SqlState::FEATURE_NOT_SUPPORTED, "follow copies into EggDB only"))
    }

    fn write_copy_data(&mut self, _data: &[u8]) -> SqlResult<()> {
        Err(SqlError::new(SqlState::FEATURE_NOT_SUPPORTED, "follow copies into EggDB only"))
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
            Self::SourceClosed => "the source closed the connection",
        }
    }
}

impl Session<'_> {
    pub(super) fn follow_event(&mut self, object: JsonObject, text: &str) {
        match self.format {
            Format::Json => self.out.text(&object.line()),
            _ => self.messages.text(text),
        }
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
                std::io::BufRead::read_line(&mut std::io::BufReader::new(&mut *self.stdin), &mut line).map_err(|error| {
                    Failure::usage(format!("cannot read the password from stdin: {error}"), Some(CommandName::Follow))
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
            return Err(usage("--poll is for SQL Server sources; PostgreSQL streams its changes".to_owned()));
        }
        let mut options = parse_postgres_url(&source).map_err(usage)?;
        options.password = self.source_password(PASSWORD_VARIABLE)?;
        self.follow_postgres(options)
    }

    fn follow_postgres(&mut self, options: ConnectOptions) -> Result<Exit, Failure> {
        let target = format!("{}:{}/{}", options.host, options.port, options.database);
        let existed = self.request.file.exists();
        let started = Instant::now();
        let mut catalog = PgClient::connect(&options).map_err(|error| client_failure(&target, &error))?;
        let tls = catalog.tls_version().map_or("no TLS", |version| {
            if version == eggdb_tls::cipher_suite::version::TLS13 { "TLS 1.3" } else { "TLS 1.2" }
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
        let wal_level = rows_of(catalog.simple_query("SHOW wal_level").map_err(|error| client_failure(&target, &error))?);
        let wal_level = wal_level.first().and_then(|row| row.first()).map(text).unwrap_or_default();
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

        let (publication, created_publication) = self.prepare_publication(&mut catalog, &target)?;
        let mut replication_options = options.clone();
        replication_options.replication = true;
        let mut stream = PgClient::connect(&replication_options).map_err(|error| client_failure(&target, &error))?;
        let slot_name = format!("eggdb_follow_{}", std::process::id());
        let slot = stream
            .simple_query(&format!(
                "CREATE_REPLICATION_SLOT {} TEMPORARY LOGICAL pgoutput EXPORT_SNAPSHOT",
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
                _ => client_failure(&target, &error),
            })?;
        let slot_row = rows_of(slot).into_iter().next().unwrap_or_default();
        let consistent_point = slot_row.get(1).map(text).and_then(|lsn| parse_lsn(&lsn)).ok_or_else(|| {
            source_failed(format!("cannot follow {target}: the replication slot reported no position"))
        })?;
        let snapshot = slot_row.get(2).map(text).unwrap_or_default();
        self.follow_event(
            JsonObject::new()
                .string("event", "slot")
                .string("name", &slot_name)
                .boolean("temporary", true)
                .string("snapshot_lsn", &format_lsn(consistent_point)),
            &format!(
                "  publication     {publication}{}\n  slot            {slot_name} (temporary), snapshot at {}\n",
                if created_publication { " (created, FOR ALL TABLES)" } else { "" },
                format_lsn(consistent_point)
            ),
        );

        let mut counts = FollowCounts::default();
        let copy_outcome = self.copy_snapshot(&mut catalog, &target, &snapshot, &publication, &mut counts);
        if let Err(failure) = copy_outcome {
            if created_publication {
                let _ = catalog.simple_query(&format!("DROP PUBLICATION IF EXISTS {}", quote_identifier(&publication)));
            }
            stream.terminate();
            catalog.terminate();
            if !existed {
                let _ = std::fs::remove_file(&self.request.file);
            }
            return Err(failure);
        }
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

        let _ = stop_signals::watch();
        let followed = self.stream_changes(&mut stream, &slot_name, &publication, consistent_point, &mut counts);
        let (reason, applied) = match followed {
            Ok(outcome) => outcome,
            Err(failure) => {
                if created_publication {
                    let _ = catalog.simple_query(&format!("DROP PUBLICATION IF EXISTS {}", quote_identifier(&publication)));
                }
                return Err(failure);
            }
        };
        stream.terminate();
        counts.sequences = self.copy_sequence_values(&mut catalog)?;
        let publication_dropped = created_publication
            && catalog
                .simple_query(&format!("DROP PUBLICATION IF EXISTS {}", quote_identifier(&publication)))
                .is_ok();
        catalog.terminate();
        let ok = !matches!(reason, StopReason::SourceClosed);
        let verdict = if ok {
            format!(
                "{} holds every source transaction committed up to {}; point applications at it now",
                self.file_display,
                format_lsn(applied)
            )
        } else {
            format!(
                "the source closed the connection; {} is consistent at {} but changes after it are not in it. The slot was temporary and is gone: follow again into a new file",
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
                    .string("lsn", &format_lsn(applied))
                    .boolean("slot_dropped", true)
                    .boolean("publication_dropped", publication_dropped)
                    .string("verdict", &verdict)
                    .line(),
            ),
            _ => self.out.text(&format!(
                "followed {} into {} for {:.0} s ({})\n  tables          {}, {} rows copied\n  transactions    {} applied (inserts {}, updates {}, deletes {}, truncates {})\n  sequences       {} copied at stop\n  position        {}\n  cleaned up      slot dropped{}\n{}: {verdict}\n",
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
                counts.sequences,
                format_lsn(applied),
                if publication_dropped { format!(", publication {publication} dropped") } else { String::new() },
                if ok { "ok" } else { "stopped" },
            )),
        }
        Ok(if ok { Exit::Done } else { Exit::DatabaseError })
    }

    fn prepare_publication(&mut self, catalog: &mut PgClient, target: &str) -> Result<(String, bool), Failure> {
        let wanted = self.request.follow.publication.clone();
        let name = wanted.clone().unwrap_or_else(|| DEFAULT_PUBLICATION.to_owned());
        let existing = rows_of(
            catalog
                .simple_query(&format!("SELECT 1 FROM pg_catalog.pg_publication WHERE pubname = {}", quote_literal(&name)))
                .map_err(|error| client_failure(target, &error))?,
        );
        if !existing.is_empty() {
            return Ok((name, false));
        }
        if wanted.is_some() {
            return Err(source_failed(format!("cannot follow {target}: publication '{name}' does not exist")));
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
            "n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_%'".to_owned()
        } else {
            let names: Vec<String> = self.request.follow.schemas.iter().map(|name| quote_literal(name)).collect();
            format!("n.nspname IN ({})", names.join(", "))
        }
    }

    fn copy_snapshot(
        &mut self,
        catalog: &mut PgClient,
        target: &str,
        snapshot: &str,
        publication: &str,
        counts: &mut FollowCounts,
    ) -> Result<(), Failure> {
        let failed = |error: ClientError| client_failure(target, &error);
        catalog.simple_query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY").map_err(failed)?;
        catalog
            .simple_query(&format!("SET TRANSACTION SNAPSHOT {}", quote_literal(snapshot)))
            .map_err(failed)?;
        let filter = self.schema_filter();
        let tables: Vec<SourceTable> = rows_of(
            catalog
                .simple_query(&format!(
                    "SELECT c.oid::text, n.nspname, c.relname, c.relreplident::text, \
                     EXISTS (SELECT 1 FROM pg_catalog.pg_constraint k WHERE k.conrelid = c.oid AND k.contype = 'p')::text \
                     FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     WHERE c.relkind IN ('r', 'p') AND NOT c.relispartition AND {filter} ORDER BY n.nspname, c.relname"
                ))
                .map_err(failed)?,
        )
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
        let published: Vec<String> = rows_of(
            catalog
                .simple_query(&format!(
                    "SELECT schemaname || '.' || tablename FROM pg_catalog.pg_publication_tables WHERE pubname = {}",
                    quote_literal(publication)
                ))
                .map_err(failed)?,
        )
        .iter()
        .map(|row| text(&row[0]))
        .collect();
        let unpublished: Vec<String> = tables.iter().map(SourceTable::shown).filter(|name| !published.contains(name)).collect();
        if !unpublished.is_empty() {
            return Err(source_failed(format!(
                "cannot follow {target}: publication '{publication}' does not publish {} of the tables: {}; their changes would never arrive. Add them to it, or leave them out with --schema",
                unpublished.len(),
                unpublished.join(", ")
            )));
        }

        let db = self.open(Access::Write { create: self.request.create }, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::PostgreSql);
        let mut run = |sql: &str| -> Result<(), Failure> {
            session
                .execute(sql, &mut |_| {})
                .map_err(|error| source_failed(format!("cannot create in {}: {} ({sql})", self.file_display, error.message())))
        };
        let schemas: Vec<String> = {
            let mut names: Vec<String> = tables.iter().map(|table| table.schema.clone()).collect();
            names.dedup();
            names
        };
        for schema in &schemas {
            if schema != "public" {
                run(&format!("CREATE SCHEMA IF NOT EXISTS {}", quote_identifier(schema)))?;
            }
        }
        let sequences = rows_of(
            catalog
                .simple_query(&format!(
                    "SELECT s.schemaname, s.sequencename, s.data_type::text, s.start_value::text, s.min_value::text, \
                     s.max_value::text, s.increment_by::text, s.cycle::text FROM pg_catalog.pg_sequences s \
                     JOIN pg_catalog.pg_namespace n ON n.nspname = s.schemaname WHERE {filter}"
                ))
                .map_err(failed)?,
        );
        for sequence in &sequences {
            run(&format!(
                "CREATE SEQUENCE {} AS {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {}{}",
                qualified(&text(&sequence[0]), &text(&sequence[1])),
                text(&sequence[2]),
                text(&sequence[6]),
                text(&sequence[4]),
                text(&sequence[5]),
                text(&sequence[3]),
                if text(&sequence[7]) == "true" { " CYCLE" } else { "" }
            ))?;
        }
        let mut deferred = Vec::new();
        for table in &tables {
            let columns = rows_of(
                catalog
                    .simple_query(&format!(
                        "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull::text, \
                         pg_catalog.pg_get_expr(d.adbin, d.adrelid), a.attidentity::text \
                         FROM pg_catalog.pg_attribute a LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                         WHERE a.attrelid = {} AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
                        table.oid
                    ))
                    .map_err(failed)?,
            );
            let constraints = rows_of(
                catalog
                    .simple_query(&format!(
                        "SELECT conname, contype::text, pg_catalog.pg_get_constraintdef(oid) FROM pg_catalog.pg_constraint \
                         WHERE conrelid = {} AND contype IN ('p', 'u', 'c', 'f') ORDER BY contype DESC, conname",
                        table.oid
                    ))
                    .map_err(failed)?,
            );
            let mut definitions: Vec<String> = columns
                .iter()
                .map(|column| {
                    let mut definition = format!("{} {}", quote_identifier(&text(&column[0])), text(&column[1]));
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
                let clause = format!("CONSTRAINT {} {}", quote_identifier(&text(&constraint[0])), text(&constraint[2]));
                if text(&constraint[1]) == "f" {
                    deferred.push(format!("ALTER TABLE {} ADD {clause}", table.qualified()));
                } else {
                    definitions.push(clause);
                }
            }
            run(&format!("CREATE TABLE {} ({})", table.qualified(), definitions.join(", ")))
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
        }
        drop(run);
        self.follow_event(
            JsonObject::new()
                .string("event", "schema")
                .raw("schemas", &format!("[{}]", schemas.iter().map(|name| render::json_string(name)).collect::<Vec<_>>().join(",")))
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
            let copied_at = Instant::now();
            let mut bridge = CopyBridge {
                client: &mut *catalog,
                source_sql: format!("COPY {} TO STDOUT", table.qualified()),
                rows: 0,
            };
            session
                .execute_with_copy(&format!("COPY {} FROM STDIN", table.qualified()), &mut |_| true, &mut bridge)
                .map_err(|error| source_failed(format!("cannot copy {}: {}", table.shown(), error.message())))?;
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
        let indexes = rows_of(
            catalog
                .simple_query(&format!(
                    "SELECT i.indexdef FROM pg_catalog.pg_indexes i JOIN pg_catalog.pg_namespace n ON n.nspname = i.schemaname \
                     WHERE {filter} AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class x ON x.oid = k.conindid \
                     WHERE x.relname = i.indexname AND k.connamespace = n.oid)"
                ))
                .map_err(failed)?,
        );
        for statement in indexes.iter().map(|row| text(&row[0])).chain(deferred) {
            session
                .execute(&statement, &mut |_| {})
                .map_err(|error| source_failed(format!("cannot create in {}: {} ({statement})", self.file_display, error.message())))?;
        }
        let _ = session.close();
        catalog.simple_query("COMMIT").map_err(failed)?;
        Ok(())
    }

    fn stream_changes(
        &mut self,
        stream: &mut PgClient,
        slot_name: &str,
        publication: &str,
        start: Lsn,
        counts: &mut FollowCounts,
    ) -> Result<(StopReason, Lsn), Failure> {
        let failed = |error: ClientError| source_failed(format!("the replication stream failed: {error}"));
        stream
            .start_copy_both(&format!(
                "START_REPLICATION SLOT {} LOGICAL {} (proto_version '1', publication_names {})",
                quote_identifier(slot_name),
                format_lsn(start),
                quote_literal(&quote_identifier(publication))
            ))
            .map_err(failed)?;
        stream.set_read_timeout(Some(POLL_TIMEOUT)).map_err(failed)?;
        let db = self.open(Access::Write { create: false }, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::PostgreSql);
        let mut applier = ChangeApplier::new();
        let mut applied = start;
        let mut pending: Vec<String> = Vec::new();
        let mut in_transaction = false;
        let mut last_status = Instant::now();
        let mut last_progress = Instant::now();
        let mut server_end = start;
        let until_caught_up = self.request.follow.until_caught_up;
        loop {
            if stop_signals::requested() && !in_transaction {
                return Ok((StopReason::Interrupt, applied));
            }
            let message = match stream.read_copy_both() {
                Ok(Some(message)) => message,
                Ok(None) => return Ok((StopReason::SourceClosed, applied)),
                Err(ClientError::TimedOut) => {
                    if last_status.elapsed() >= STATUS_INTERVAL || until_caught_up {
                        stream
                            .send_copy_data(&standby_status_update(applied, now_unix_micros(), until_caught_up))
                            .map_err(failed)?;
                        last_status = Instant::now();
                    }
                    continue;
                }
                Err(ClientError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Ok((StopReason::SourceClosed, applied));
                }
                Err(error) => return Err(failed(error)),
            };
            match parse_stream_message(&message).map_err(|error| source_failed(error.to_string()))? {
                StreamMessage::Keepalive { wal_end, reply_requested } => {
                    server_end = server_end.max(wal_end);
                    if !in_transaction {
                        applied = applied.max(wal_end);
                        if until_caught_up {
                            stream
                                .send_copy_data(&standby_status_update(applied, now_unix_micros(), false))
                                .map_err(failed)?;
                            return Ok((StopReason::CaughtUp, applied));
                        }
                    }
                    if reply_requested {
                        stream
                            .send_copy_data(&standby_status_update(applied, now_unix_micros(), false))
                            .map_err(failed)?;
                        last_status = Instant::now();
                    }
                }
                StreamMessage::XLogData { wal_end, data, .. } => {
                    server_end = server_end.max(wal_end);
                    let logical = parse_logical_message(&data).map_err(|error| source_failed(error.to_string()))?;
                    match applier.apply(logical).map_err(|error| source_failed(error.to_string()))? {
                        Change::Begin => {
                            in_transaction = true;
                            pending.clear();
                        }
                        Change::Statement(statement) => pending.push(statement),
                        Change::Commit { end_lsn, .. } => {
                            let mut script = String::from("BEGIN;\n");
                            for statement in &pending {
                                counts.count(statement);
                                script.push_str(statement);
                                script.push_str(";\n");
                            }
                            script.push_str("COMMIT;");
                            session.execute(&script, &mut |_| {}).map_err(|error| {
                                let _ = session.execute("ROLLBACK", &mut |_| {});
                                source_failed(format!(
                                    "a source transaction could not be applied: {}; {} is consistent at {}",
                                    error.message(),
                                    self.file_display,
                                    format_lsn(applied)
                                ))
                            })?;
                            counts.transactions += 1;
                            pending.clear();
                            in_transaction = false;
                            applied = end_lsn;
                            server_end = server_end.max(end_lsn);
                        }
                        Change::Nothing => {}
                    }
                }
            }
            if last_progress.elapsed() >= PROGRESS_INTERVAL && self.format != Format::Json {
                last_progress = Instant::now();
                let lag = server_end.saturating_sub(applied);
                self.messages.text(&format!(
                    "  following       lag {lag} bytes, {} transactions applied, at {}\n",
                    counts.transactions,
                    format_lsn(applied)
                ));
            }
        }
    }

    fn copy_sequence_values(&mut self, catalog: &mut PgClient) -> Result<u64, Failure> {
        let filter = self.schema_filter();
        let values = rows_of(
            catalog
                .simple_query(&format!(
                    "SELECT s.schemaname, s.sequencename, s.last_value::text FROM pg_catalog.pg_sequences s \
                     JOIN pg_catalog.pg_namespace n ON n.nspname = s.schemaname WHERE {filter} AND s.last_value IS NOT NULL"
                ))
                .map_err(|error| source_failed(format!("cannot read the sequences at stop: {error}")))?,
        );
        let db = self.open(Access::Write { create: false }, None)?;
        let store = DatabaseStore(&db);
        let mut session = store.session(Dialect::PostgreSql);
        for row in &values {
            let name = qualified(&text(&row[0]), &text(&row[1]));
            session
                .execute(&format!("SELECT pg_catalog.setval({}, {}, true)", quote_literal(&name), text(&row[2])), &mut |_| {})
                .map_err(|error| source_failed(format!("cannot set the sequence {name}: {}", error.message())))?;
        }
        let _ = session.close();
        Ok(values.len() as u64)
    }
}
