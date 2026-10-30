use super::{Access, DEFAULT_CACHE_BYTES, open_failure, without_open_prefix};
use crate::cli::Console;
use crate::cli::args::Request;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render;
use eggdb::sql::OwnedStore;
use eggdb::{Database, ErrorKind, Options};
use eggdb_mcp::audit::{AuditLog, audit_path_for};
use eggdb_mcp::{DatabaseOpener, OpenRefusal, Server, ServerConfig, StartFailure};
use eggdb_sql::{KeyValueStore, UnknownTableName, VisibleTables};
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_TIME_LIMIT_SECONDS: u64 = 5;
const DEFAULT_CALL_GAP_MS: u64 = 100;

struct FileOpener {
    path: PathBuf,
    file_display: String,
    cache_bytes: usize,
}

impl FileOpener {
    fn open(&self, read_only: bool) -> eggdb::Result<Database> {
        Database::open_with(
            &self.path,
            Options::new()
                .read_only(read_only)
                .create_if_missing(false)
                .cache_bytes(self.cache_bytes),
        )
    }

    fn refusal(&self, error: &eggdb::Error) -> OpenRefusal {
        let message = without_open_prefix(&error.to_string(), &self.file_display);
        match error.kind() {
            ErrorKind::Locked => OpenRefusal::Busy,
            ErrorKind::NotFound => OpenRefusal::Missing,
            ErrorKind::Corrupt => OpenRefusal::Damaged(message),
            _ => OpenRefusal::Failed(message),
        }
    }
}

impl DatabaseOpener for FileOpener {
    type Store = OwnedStore;

    fn open_read_only(&self) -> Result<OwnedStore, OpenRefusal> {
        self.open(true)
            .map(OwnedStore)
            .map_err(|error| self.refusal(&error))
    }

    fn open_read_write(&self) -> Result<OwnedStore, OpenRefusal> {
        self.open(false)
            .map(OwnedStore)
            .map_err(|error| match error.kind() {
                ErrorKind::PermissionDenied | ErrorKind::ReadOnly => {
                    OpenRefusal::NotWritable("permission denied".to_owned())
                }
                _ => self.refusal(&error),
            })
    }

    fn close(&self, store: OwnedStore) -> Result<(), OpenRefusal> {
        store
            .close()
            .map_err(|error| OpenRefusal::Failed(error.to_string()))
    }
}

pub(super) fn run(request: Request, console: &mut Console<'_>) -> Result<Exit, Failure> {
    let file_display = request.file.display().to_string();
    let file = render::text(file_display.as_bytes());
    let opener = FileOpener {
        path: request.file.clone(),
        file_display: file_display.clone(),
        cache_bytes: request.cache_bytes.unwrap_or(DEFAULT_CACHE_BYTES),
    };
    let tables = (!request.tables.is_empty())
        .then(|| VisibleTables::new(request.tables.iter().map(String::as_str)));
    let busy_at_start = match opener.open(true) {
        Ok(db) => {
            let store = OwnedStore(db);
            let unknown = tables
                .as_ref()
                .map(|tables| unknown_names(&store, tables))
                .transpose();
            let _ = store.close();
            let unknown = unknown.map_err(|message| {
                Failure::database(format!("cannot read {file}: {message}"), Exit::DatabaseError)
            })?;
            if let Some(unknown) = unknown
                && !unknown.is_empty()
            {
                return Err(unknown_tables_failure(&file, &unknown));
            }
            false
        }
        Err(error) if error.kind() == ErrorKind::Locked => true,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(Failure::Database {
                message: format!(
                    "cannot open {file}: no such file. eggdb mcp never creates a database: check the path, or create it with `eggdb create {file}`"
                ),
                try_command: Some(format!("eggdb create {file}")),
                exit: Exit::DatabaseError,
                code: "not_found",
            });
        }
        Err(error) => {
            return Err(open_failure(&file, &file_display, &error, Access::ReadOnly));
        }
    };
    let audit_path = audit_path_for(&request.file);
    let audit_shown = render::text(audit_path.display().to_string().as_bytes());
    let time_limit_seconds = request.time_limit.unwrap_or(DEFAULT_TIME_LIMIT_SECONDS);
    let call_gap_ms = request.call_gap_ms.unwrap_or(DEFAULT_CALL_GAP_MS);
    let shown_tables: Vec<String> = tables
        .iter()
        .flat_map(VisibleTables::names)
        .map(|name| render::text(name.as_bytes()))
        .collect();
    let config = ServerConfig {
        database_name: file.clone(),
        server_version: eggdb::VERSION.to_owned(),
        time_limit: Duration::from_secs(time_limit_seconds),
        audit: AuditLog::new(audit_path),
        allow_write: request.allow_write,
        tables,
        call_gap: Duration::from_millis(call_gap_ms),
        exit_process: Some(std::process::exit),
    };
    let mut server = Server::new(&opener, config).map_err(|failure| match failure {
        StartFailure::Audit(failure) => {
            let advice = if failure.reason.starts_with("it ")
                || failure.reason.starts_with("another file")
            {
                "Move it away: eggdb mcp writes its audit only to a plain file of its own, which it creates."
            } else {
                "Make the database's directory writable, or copy the database somewhere that is."
            };
            Failure::database(
                format!(
                    "cannot write {audit_shown}: {failure}. Every MCP call is recorded there, so eggdb mcp does not start without it. {advice}"
                ),
                Exit::DatabaseError,
            )
        }
        StartFailure::Watchdog(error) => Failure::database(
            format!("cannot start eggdb mcp's watchdog thread: {error}"),
            Exit::DatabaseError,
        ),
        StartFailure::TablesWithWrites => Failure::usage(
            "--tables goes with a read-only server in this release: drop --allow-write, or drop --tables to let the agent write every table".to_owned(),
            Some(request.command),
        ),
    })?;
    let mut add_options = String::new();
    if request.allow_write {
        add_options.push_str(" --allow-write");
    }
    if !shown_tables.is_empty() {
        add_options.push_str(&format!(" --tables {}", shown_tables.join(",")));
    }
    if time_limit_seconds != DEFAULT_TIME_LIMIT_SECONDS {
        add_options.push_str(&format!(" --time-limit {time_limit_seconds}"));
    }
    if call_gap_ms != DEFAULT_CALL_GAP_MS {
        add_options.push_str(&format!(" --call-gap {call_gap_ms}"));
    }
    let gap_line = if call_gap_ms == 0 {
        "  call gap     off (--call-gap 0): calls may follow back to back, and your writers are refused meanwhile\n".to_owned()
    } else {
        format!(
            "  call gap     {call_gap_ms} ms after each call before the next, so your programs can write in between\n"
        )
    };
    let gap_words = if call_gap_ms == 0 {
        "call gap off".to_owned()
    } else {
        format!("call gap {call_gap_ms} ms")
    };
    let tables_line = if shown_tables.is_empty() {
        String::new()
    } else {
        format!(
            "  tables       {} only (--tables); other tables do not exist for the agent, and kv_get and kv_scan are off\n",
            shown_tables.join(", ")
        )
    };
    let tables_words = if shown_tables.is_empty() {
        String::new()
    } else {
        format!(", tables {}", shown_tables.join(", "))
    };
    let mut banner = match (console.terminal.stderr, request.allow_write) {
        (true, false) => format!(
            "eggdb mcp {file}: a read-only MCP server on stdin and stdout, for an AI agent's client to start; nothing to type here.\n{tables_line}  audit        {audit_shown} (eggdb audit {file} shows it)\n  time limit   {time_limit_seconds} s per call\n{gap_line}  add it       claude mcp add mydb -- eggdb mcp {file}{add_options}\n  stop         Ctrl-C, or close stdin\n"
        ),
        (true, true) => format!(
            "eggdb mcp {file}: a read-write MCP server on stdin and stdout, for an AI agent's client to start; nothing to type here.\n  writes       allowed (--allow-write): write_sql runs one statement per call, in its own transaction\n  confirm      DROP, TRUNCATE, a DELETE or UPDATE of a whole table, dropping or retyping a column: only after you say yes\n  audit        {audit_shown} (eggdb audit {file} shows it)\n  time limit   {time_limit_seconds} s per call\n{gap_line}  add it       claude mcp add mydb -- eggdb mcp {file}{add_options}\n  stop         Ctrl-C, or close stdin\n"
        ),
        (false, false) => format!(
            "eggdb {} mcp: serving {file} read-only{tables_words}; audit {audit_shown}; time limit {time_limit_seconds} s; {gap_words}\n",
            eggdb::VERSION
        ),
        (false, true) => format!(
            "eggdb {} mcp: serving {file} read-write (destructive statements need the user's confirmation); audit {audit_shown}; time limit {time_limit_seconds} s; {gap_words}\n",
            eggdb::VERSION
        ),
    };
    if busy_at_start {
        banner.push_str(&format!(
            "  {file} is open for writing in another program now; calls tell the agent so until it is closed\n"
        ));
    }
    let _ = console.stderr.write_all(banner.as_bytes());
    let _ = console.stderr.flush();
    server
        .serve(&mut *console.stdin, &mut *console.stdout)
        .map_err(|error| {
            Failure::database(
                format!("the MCP connection on stdin and stdout failed: {error}"),
                Exit::DatabaseError,
            )
        })?;
    Ok(Exit::Done)
}

fn unknown_names(
    store: &OwnedStore,
    tables: &VisibleTables,
) -> Result<Vec<UnknownTableName>, String> {
    let read = store.begin_read().map_err(|error| error.to_string())?;
    tables
        .unknown_names(&read)
        .map_err(|error| error.to_string())
}

fn quoted_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [first @ .., last] => format!("{} and {last}", first.join(", ")),
    }
}

fn unknown_tables_failure(file: &str, unknown: &[UnknownTableName]) -> Failure {
    let names: Vec<String> = unknown
        .iter()
        .map(|name| {
            let shown = render::text(name.name.as_bytes());
            match &name.stored_as {
                Some(stored) => format!(
                    "'{shown}' (stored as '{}')",
                    render::text(stored.as_bytes())
                ),
                None => format!("'{shown}'"),
            }
        })
        .collect();
    let message = if unknown.len() == 1 {
        format!(
            "--tables names {}: no table, view or sequence in {file} has that name. Names match as stored; fix it, or create the table first.",
            quoted_list(&names)
        )
    } else {
        format!(
            "--tables names {}: no table, view or sequence in {file} has these names. Names match as stored; fix them, or create the tables first.",
            quoted_list(&names)
        )
    };
    Failure::Database {
        message,
        try_command: Some(format!("eggdb table list {file}")),
        exit: Exit::NotFound,
        code: "table_not_found",
    }
}
