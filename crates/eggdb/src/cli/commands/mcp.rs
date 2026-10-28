use super::{Access, DEFAULT_CACHE_BYTES, open_failure, without_open_prefix};
use crate::cli::Console;
use crate::cli::args::Request;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render;
use eggdb::sql::OwnedStore;
use eggdb::{Database, ErrorKind, Options};
use eggdb_mcp::audit::{AuditLog, audit_path_for};
use eggdb_mcp::{DatabaseOpener, OpenRefusal, Server, ServerConfig, StartFailure};
use std::path::PathBuf;
use std::time::Duration;

const DEFAULT_TIME_LIMIT_SECONDS: u64 = 5;

struct ReadOnlyOpener {
    path: PathBuf,
    file_display: String,
    cache_bytes: usize,
}

impl ReadOnlyOpener {
    fn open(&self) -> eggdb::Result<Database> {
        Database::open_with(
            &self.path,
            Options::new()
                .read_only(true)
                .create_if_missing(false)
                .cache_bytes(self.cache_bytes),
        )
    }
}

impl DatabaseOpener for ReadOnlyOpener {
    type Store = OwnedStore;

    fn open_read_only(&self) -> Result<OwnedStore, OpenRefusal> {
        self.open().map(OwnedStore).map_err(|error| {
            let message = without_open_prefix(&error.to_string(), &self.file_display);
            match error.kind() {
                ErrorKind::Locked => OpenRefusal::Busy,
                ErrorKind::NotFound => OpenRefusal::Missing,
                ErrorKind::Corrupt => OpenRefusal::Damaged(message),
                _ => OpenRefusal::Failed(message),
            }
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
    let opener = ReadOnlyOpener {
        path: request.file.clone(),
        file_display: file_display.clone(),
        cache_bytes: request.cache_bytes.unwrap_or(DEFAULT_CACHE_BYTES),
    };
    let busy_at_start = match opener.open() {
        Ok(db) => {
            let _ = db.close();
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
            });
        }
        Err(error) => {
            return Err(open_failure(&file, &file_display, &error, Access::ReadOnly));
        }
    };
    let audit_path = audit_path_for(&request.file);
    let audit_shown = render::text(audit_path.display().to_string().as_bytes());
    let time_limit_seconds = request.time_limit.unwrap_or(DEFAULT_TIME_LIMIT_SECONDS);
    let config = ServerConfig {
        database_name: file.clone(),
        server_version: eggdb::VERSION.to_owned(),
        time_limit: Duration::from_secs(time_limit_seconds),
        audit: AuditLog::new(audit_path),
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
    })?;
    let mut banner = if console.terminal.stderr {
        format!(
            "eggdb mcp {file}: a read-only MCP server on stdin and stdout, for an AI agent's client to start; nothing to type here.\n  audit        {audit_shown} (eggdb audit {file} shows it)\n  time limit   {time_limit_seconds} s per call\n  add it       claude mcp add mydb -- eggdb mcp {file}\n  stop         Ctrl-C, or close stdin\n"
        )
    } else {
        format!(
            "eggdb {} mcp: serving {file} read-only; audit {audit_shown}; time limit {time_limit_seconds} s\n",
            eggdb::VERSION
        )
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
