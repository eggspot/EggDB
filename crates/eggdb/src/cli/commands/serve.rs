use super::{Access, Session};
use crate::cli::args::{CommandName, ExportFile, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject, TimeStyle};
use crate::cli::sql_store::DatabaseStore;
use eggdb_pgwire::{
    DEFAULT_AUTHENTICATION_TIMEOUT, DEFAULT_MAX_CONNECTIONS, ListenError, MIN_SCRAM_ITERATIONS,
    SCRAM_SALT_BYTES, ScramVerifier, ServerConfig,
};
use eggdb_platform::{secure_random, stop_signals};
use std::io::BufRead;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;

const DEFAULT_USER: &str = "eggdb";
const DEFAULT_PORT: u16 = 5432;
const PASSWORD_VARIABLE: &str = "EGGDB_PASSWORD";

fn stamped(line: &str) -> String {
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_micros() as u64);
    let stamp = render::utc_time(micros - micros % 1_000_000, TimeStyle::Readable);
    let one_line: String = line
        .chars()
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect();
    format!("{}  {one_line}\n", stamp.replace(".000000", ""))
}

fn database_name_of(file: &Path) -> String {
    file.file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("eggdb")
        .to_owned()
}

impl Session<'_> {
    pub(super) fn serve(&mut self) -> Result<Exit, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Serve));
        let options = self.request.serve.clone();
        let user = options.user.unwrap_or_else(|| DEFAULT_USER.to_owned());
        let password = self.serve_password(&user)?;
        if !password.is_ascii() {
            return Err(usage(
                "the password must be ASCII in this release: SASLprep, which normalizes other characters for SCRAM, is not implemented yet".to_owned(),
            ));
        }
        let address = options.listen.unwrap_or(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            DEFAULT_PORT,
        ));
        let listener = match eggdb_pgwire::listen_on_loopback(address) {
            Ok(listener) => listener,
            Err(ListenError::NotLoopback(address)) => {
                return Err(Failure::Database {
                    message: format!(
                        "cannot listen on {address}: it is not a loopback address. Until EggDB speaks TLS, serve listens only on this machine (127.0.0.1, ::1 or another 127.x address), so no password crosses a network in the clear."
                    ),
                    try_command: Some(format!(
                        "eggdb serve {} --listen 127.0.0.1:{}",
                        self.file,
                        address.port()
                    )),
                    exit: Exit::Usage,
                });
            }
            Err(ListenError::Io(error)) => {
                let in_use = error.kind() == std::io::ErrorKind::AddrInUse;
                return Err(Failure::Database {
                    message: if in_use {
                        format!(
                            "cannot listen on {address}: address already in use. Another server is listening there (often PostgreSQL itself); pick another port, or port 0 for any free one."
                        )
                    } else {
                        format!("cannot listen on {address}: {error}")
                    },
                    try_command: in_use.then(|| {
                        format!(
                            "eggdb serve {} --listen {}",
                            self.file,
                            SocketAddr::new(address.ip(), address.port().wrapping_add(1))
                        )
                    }),
                    exit: Exit::DatabaseError,
                });
            }
        };
        let bound = listener.local_addr().map_err(|error| {
            Failure::database(format!("cannot listen: {error}"), Exit::DatabaseError)
        })?;
        let mut salt = [0u8; SCRAM_SALT_BYTES];
        secure_random::fill(&mut salt).map_err(|error| {
            Failure::database(
                format!("cannot get random bytes from the operating system: {error}"),
                Exit::DatabaseError,
            )
        })?;
        let verifier = ScramVerifier::derive(password.as_bytes(), &salt, MIN_SCRAM_ITERATIONS);
        drop(password);
        let read_only = self.request.read_only;
        let access = if read_only {
            Access::ReadOnly
        } else {
            Access::Write {
                create: self.request.create,
            }
        };
        let db = self.open(access, None)?;
        let config = ServerConfig {
            user,
            verifier,
            database: options
                .database
                .unwrap_or_else(|| database_name_of(&self.request.file)),
            max_connections: options.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS),
            authentication_timeout: DEFAULT_AUTHENTICATION_TIMEOUT,
        };
        self.print_listening(bound, &config, read_only);
        self.out.finish()?;
        let _ = stop_signals::watch();
        let store = DatabaseStore(&db);
        let (events, received) = std::sync::mpsc::channel::<String>();
        let served = std::thread::scope(|scope| {
            let server = scope.spawn(|| {
                let log = move |line: &str| {
                    let _ = events.send(stamped(line));
                };
                eggdb_pgwire::serve(&store, &listener, &config, &stop_signals::requested, &log)
            });
            for line in received {
                self.messages.text(&line);
                let _ = self.messages.finish();
            }
            server.join().unwrap_or_else(|_| {
                Err(std::io::Error::other(
                    "the server thread stopped unexpectedly",
                ))
            })
        });
        drop(listener);
        let open = served.map_err(|error| {
            Failure::database(format!("the server failed: {error}"), Exit::DatabaseError)
        })?;
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .boolean("stopped", true)
                    .number("connections_closed", open as u64)
                    .line(),
            ),
            _ => {
                if open > 0 {
                    self.out.text(&format!(
                        "stopping: {open} connection{} open; finished their statements and rolled back open transactions\n",
                        if open == 1 { " was" } else { "s were" }
                    ));
                }
                self.out.text("stopped\n");
            }
        }
        self.close(db)?;
        Ok(Exit::Done)
    }

    fn print_listening(&mut self, bound: SocketAddr, config: &ServerConfig, read_only: bool) {
        let host = bound.ip().to_string();
        let url_host = match bound.ip() {
            IpAddr::V6(_) => format!("[{host}]"),
            IpAddr::V4(_) => host.clone(),
        };
        match self.format {
            Format::Json => self.out.text(
                &JsonObject::new()
                    .string("listening", &bound.to_string())
                    .string("host", &host)
                    .number("port", u64::from(bound.port()))
                    .string("file", &self.file_display)
                    .string("database", &config.database)
                    .string("user", &config.user)
                    .number("max_connections", config.max_connections as u64)
                    .boolean("read_only", read_only)
                    .line(),
            ),
            _ => self.out.text(&format!(
                "listening on {bound} (PostgreSQL protocol; loopback only until EggDB has TLS)\n  database     {} ({}, open {})\n  user         {} (SCRAM-SHA-256), up to {} connections\n  connect      psql postgresql://{}@{url_host}:{}/{}\n  stop         Ctrl-C\n",
                config.database,
                self.file,
                if read_only { "read-only" } else { "for writing" },
                config.user,
                config.max_connections,
                config.user,
                bound.port(),
                config.database,
            )),
        }
    }

    fn serve_password(&mut self, user: &str) -> Result<String, Failure> {
        let usage = |message: String| Failure::usage(message, Some(CommandName::Serve));
        match self.request.serve.password_file.clone() {
            Some(source) => {
                let (shown, first_line) = match &source {
                    ExportFile::Standard => {
                        self.stdin_taken = true;
                        let mut line = String::new();
                        std::io::BufReader::new(&mut *self.stdin)
                            .read_line(&mut line)
                            .map_err(|error| {
                                usage(format!("cannot read the password from stdin: {error}"))
                            })?;
                        ("-".to_owned(), line)
                    }
                    ExportFile::Path(path) => {
                        let shown = render::text(path.display().to_string().as_bytes());
                        let text = std::fs::read(path).map_err(|error| {
                            usage(format!("cannot read --password-file {shown}: {error}"))
                        })?;
                        let text = String::from_utf8(text).map_err(|_| {
                            usage(format!("--password-file {shown} is not UTF-8 text"))
                        })?;
                        let line = text.lines().next().unwrap_or("").to_owned();
                        (shown, line)
                    }
                };
                let password = first_line.trim_end_matches(['\r', '\n']).to_owned();
                if password.is_empty() {
                    return Err(usage(format!(
                        "--password-file {shown} holds no password on its first line"
                    )));
                }
                Ok(password)
            }
            None => match std::env::var(PASSWORD_VARIABLE) {
                Ok(password) if !password.is_empty() => Ok(password),
                Ok(_) => Err(usage(format!("{PASSWORD_VARIABLE} is set but empty"))),
                Err(std::env::VarError::NotUnicode(_)) => {
                    Err(usage(format!("{PASSWORD_VARIABLE} is not UTF-8 text")))
                }
                Err(std::env::VarError::NotPresent) => Err(usage(format!(
                    "serve needs a password for user {user}: set {PASSWORD_VARIABLE}, or pass --password-file <file> (its first line, or - for stdin). It is never taken on the command line, where every user of this machine can read it."
                ))),
            },
        }
    }
}
