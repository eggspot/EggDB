use super::{Access, Session};
use crate::cli::args::{CommandName, ExportFile, Format};
use crate::cli::failure::{Exit, Failure};
use crate::cli::render::{self, JsonObject, TimeStyle};
use eggdb::sql::Store as DatabaseStore;
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
        let listener = self.listener(
            "--listen",
            "PostgreSQL",
            address,
            eggdb_pgwire::listen_on_loopback(address).map_err(|error| match error {
                ListenError::NotLoopback(_) => None,
                ListenError::Io(error) => Some(error),
            }),
        )?;
        let tds_listener = match options.tds_listen {
            Some(address) => Some(self.listener(
                "--tds-listen",
                "SQL Server",
                address,
                eggdb_tdswire::listen_on_loopback(address).map_err(|error| match error {
                    eggdb_tdswire::ListenError::NotLoopback(_) => None,
                    eggdb_tdswire::ListenError::Io(error) => Some(error),
                }),
            )?),
            None => None,
        };
        let mysql_listener = match options.mysql_listen {
            Some(address) => Some(self.listener(
                "--mysql-listen",
                "MySQL",
                address,
                eggdb_mysqlwire::listen_on_loopback(address).map_err(|error| match error {
                    eggdb_mysqlwire::ListenError::NotLoopback(_) => None,
                    eggdb_mysqlwire::ListenError::Io(error) => Some(error),
                }),
            )?),
            None => None,
        };
        let bound = listener.local_addr().map_err(|error| {
            Failure::database(format!("cannot listen: {error}"), Exit::DatabaseError)
        })?;
        let mysql_bound = match &mysql_listener {
            Some(listener) => Some(listener.local_addr().map_err(|error| {
                Failure::database(format!("cannot listen: {error}"), Exit::DatabaseError)
            })?),
            None => None,
        };
        let tds_bound = match &tds_listener {
            Some(listener) => Some(listener.local_addr().map_err(|error| {
                Failure::database(format!("cannot listen: {error}"), Exit::DatabaseError)
            })?),
            None => None,
        };
        let mut salt = [0u8; SCRAM_SALT_BYTES];
        secure_random::fill(&mut salt).map_err(|error| {
            Failure::database(
                format!("cannot get random bytes from the operating system: {error}"),
                Exit::DatabaseError,
            )
        })?;
        let verifier = ScramVerifier::derive(password.as_bytes(), &salt, MIN_SCRAM_ITERATIONS);
        let mysql_credentials = mysql_listener
            .as_ref()
            .map(|_| eggdb_mysqlwire::MySqlCredentials::derive(password.as_bytes()));
        let tds_credentials = match &tds_listener {
            Some(_) => Some(
                eggdb_tdswire::TdsCredentials::derive(
                    password.as_bytes(),
                    eggdb_tdswire::DEFAULT_ITERATIONS,
                )
                .map_err(|error| {
                    Failure::database(
                        format!("cannot get random bytes from the operating system: {error}"),
                        Exit::DatabaseError,
                    )
                })?,
            ),
            None => None,
        };
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
        let mysql_config = mysql_credentials.map(|credentials| eggdb_mysqlwire::ServerConfig {
            user: config.user.clone(),
            credentials,
            database: config.database.clone(),
            max_connections: config.max_connections,
            authentication_timeout: eggdb_mysqlwire::DEFAULT_AUTHENTICATION_TIMEOUT,
        });
        let tds_config = tds_credentials.map(|credentials| eggdb_tdswire::ServerConfig {
            user: config.user.clone(),
            credentials,
            database: config.database.clone(),
            max_connections: config.max_connections,
            authentication_timeout: eggdb_tdswire::DEFAULT_AUTHENTICATION_TIMEOUT,
            log_batches: options.log_batches,
        });
        self.print_listening(bound, mysql_bound, tds_bound, &config, read_only);
        self.out.finish()?;
        let _ = stop_signals::watch();
        let store = DatabaseStore(&db);
        let (events, received) = std::sync::mpsc::channel::<String>();
        let mysql_events = events.clone();
        let tds_events = events.clone();
        let served = std::thread::scope(|scope| {
            let server = scope.spawn(|| {
                let log = move |line: &str| {
                    let _ = events.send(stamped(line));
                };
                eggdb_pgwire::serve(&store, &listener, &config, &stop_signals::requested, &log)
            });
            let mysql_server = scope.spawn(|| match (&mysql_listener, &mysql_config) {
                (Some(listener), Some(config)) => {
                    let log = move |line: &str| {
                        let _ = mysql_events.send(stamped(&format!("mysql {line}")));
                    };
                    eggdb_mysqlwire::serve(&store, listener, config, &stop_signals::requested, &log)
                }
                _ => {
                    drop(mysql_events);
                    Ok(0)
                }
            });
            let tds_server = scope.spawn(|| match (&tds_listener, &tds_config) {
                (Some(listener), Some(config)) => {
                    let log = move |line: &str| {
                        let _ = tds_events.send(stamped(&format!("tds {line}")));
                    };
                    eggdb_tdswire::serve(&store, listener, config, &stop_signals::requested, &log)
                }
                _ => {
                    drop(tds_events);
                    Ok(0)
                }
            });
            for line in received {
                self.messages.text(&line);
                let _ = self.messages.finish();
            }
            let unexpected = || {
                Err(std::io::Error::other(
                    "the server thread stopped unexpectedly",
                ))
            };
            let postgresql = server.join().unwrap_or_else(|_| unexpected());
            let mysql = mysql_server.join().unwrap_or_else(|_| unexpected());
            let tds = tds_server.join().unwrap_or_else(|_| unexpected());
            postgresql
                .and_then(|open| mysql.map(|mysql_open| open + mysql_open))
                .and_then(|open| tds.map(|tds_open| open + tds_open))
        });
        drop(listener);
        drop(mysql_listener);
        drop(tds_listener);
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

    fn listener(
        &self,
        option: &str,
        protocol: &str,
        address: SocketAddr,
        opened: Result<std::net::TcpListener, Option<std::io::Error>>,
    ) -> Result<std::net::TcpListener, Failure> {
        match opened {
            Ok(listener) => Ok(listener),
            Err(None) => Err(Failure::Database {
                message: format!(
                    "cannot listen on {address}: it is not a loopback address. Until EggDB speaks TLS, serve listens only on this machine (127.0.0.1, ::1 or another 127.x address), so no password crosses a network in the clear."
                ),
                try_command: Some(format!(
                    "eggdb serve {} {option} 127.0.0.1:{}",
                    self.file,
                    address.port()
                )),
                exit: Exit::Usage,
                code: crate::cli::failure::USAGE_CODE,
            }),
            Err(Some(error)) => {
                let in_use = error.kind() == std::io::ErrorKind::AddrInUse;
                Err(Failure::Database {
                    message: if in_use {
                        format!(
                            "cannot listen on {address}: address already in use. Another server is listening there (often {protocol} itself); pick another port, or port 0 for any free one."
                        )
                    } else {
                        format!("cannot listen on {address}: {error}")
                    },
                    try_command: in_use.then(|| {
                        format!(
                            "eggdb serve {} {option} {}",
                            self.file,
                            SocketAddr::new(address.ip(), address.port().wrapping_add(1))
                        )
                    }),
                    exit: Exit::DatabaseError,
                    code: if in_use { "database_busy" } else { "database_failed" },
                })
            }
        }
    }

    fn print_listening(
        &mut self,
        bound: SocketAddr,
        mysql_bound: Option<SocketAddr>,
        tds_bound: Option<SocketAddr>,
        config: &ServerConfig,
        read_only: bool,
    ) {
        let host = bound.ip().to_string();
        let url_host = match bound.ip() {
            IpAddr::V6(_) => format!("[{host}]"),
            IpAddr::V4(_) => host.clone(),
        };
        let mysql_line = mysql_bound.map_or_else(String::new, |mysql| {
            let mysql_host = mysql.ip().to_string();
            format!(
                "  mysql        listening on {mysql} (MySQL protocol): mysql -h {mysql_host} -P {} -u {} -p {}\n",
                mysql.port(),
                config.user,
                config.database
            )
        });
        let tds_line = tds_bound.map_or_else(String::new, |tds| {
            format!(
                "  sqlcmd       listening on {tds} (SQL Server's TDS protocol, no encryption): sqlcmd -S {},{} -U {} -d {} (password in SQLCMDPASSWORD or -P; go-sqlcmd's default encryption or -N disable)\n",
                tds.ip(),
                tds.port(),
                config.user,
                config.database
            )
        });
        match self.format {
            Format::Json => {
                let mut object = JsonObject::new()
                    .string("listening", &bound.to_string())
                    .string("host", &host)
                    .number("port", u64::from(bound.port()));
                if let Some(mysql) = mysql_bound {
                    object = object
                        .string("mysql_listening", &mysql.to_string())
                        .number("mysql_port", u64::from(mysql.port()));
                }
                if let Some(tds) = tds_bound {
                    object = object
                        .string("tds_listening", &tds.to_string())
                        .number("tds_port", u64::from(tds.port()));
                }
                self.out.text(
                    &object
                    .string("file", &self.file_display)
                    .string("database", &config.database)
                    .string("user", &config.user)
                    .number("max_connections", config.max_connections as u64)
                    .boolean("read_only", read_only)
                    .line(),
                );
            }
            _ => self.out.text(&format!(
                "listening on {bound} (PostgreSQL protocol; loopback only until EggDB has TLS)\n  database     {} ({}, open {})\n  user         {} (SCRAM-SHA-256), up to {} connections\n  connect      psql postgresql://{}@{url_host}:{}/{}\n{mysql_line}{tds_line}  stop         Ctrl-C\n",
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
