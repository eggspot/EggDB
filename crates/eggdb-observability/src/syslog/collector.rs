use super::framing::Framer;
use super::{Message, parse};
use crate::store::{literal, run, timestamp_literal};
use eggdb_platform::{DatagramSocket, ErrorKind};
use eggdb_sql::{Dialect, KeyValueStore, Session};
use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The longest syslog message the collector takes, in bytes: longer datagrams are dropped and
/// a TCP connection that sends a longer frame is closed. RFC 5424 asks receivers to take at
/// least 2,048 bytes; this leaves room for the 8 KiB that common senders allow by default.
pub const MAX_SYSLOG_MESSAGE_BYTES: usize = 16 * 1024;

const TABLE: &str = "syslog";
const ROWS_PER_INSERT: usize = 500;
const RECEIVE_WAIT: Duration = Duration::from_millis(50);
const ACCEPT_POLL: Duration = Duration::from_millis(50);
const READ_CHUNK_BYTES: usize = 16 * 1024;

type Received = (i64, Message);

/// Takes syslog messages (RFC 5424, and RFC 3164 as senders write it) over UDP, one message
/// per datagram, and over TCP, framed by octet counting or by line ends (RFC 6587), and
/// stores each one as a row of the `syslog` table, created when missing: `received_at`,
/// `facility`, `severity`, `timestamp` (the sender's, `NULL` when it gave none), `hostname`,
/// `app_name`, `procid`, `msgid`, `structured_data` (RFC 5424's elements as sent) and
/// `message`. Messages that arrive together are written in one statement.
pub struct SyslogCollector<'s, S: KeyValueStore> {
    store: &'s S,
    user: String,
    database: String,
    max_connections: usize,
}

impl<S: KeyValueStore> std::fmt::Debug for SyslogCollector<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyslogCollector")
            .field("user", &self.user)
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_micros()).unwrap_or(i64::MAX)
        })
}

fn optional(value: Option<&str>) -> String {
    value.map_or_else(|| "NULL".to_owned(), literal)
}

fn row((received_at, message): &Received) -> String {
    let cells = [
        timestamp_literal(*received_at),
        message.facility.to_string(),
        message.severity.to_string(),
        message
            .timestamp_micros
            .map_or_else(|| "NULL".to_owned(), timestamp_literal),
        optional(message.hostname.as_deref()),
        optional(message.app_name.as_deref()),
        optional(message.procid.as_deref()),
        optional(message.msgid.as_deref()),
        optional(message.structured_data.as_deref()),
        literal(&message.message),
    ];
    format!("({})", cells.join(", "))
}

fn create_table(session: &mut Session<'_>) -> Result<(), String> {
    run(
        session,
        &format!(
            "CREATE TABLE IF NOT EXISTS {TABLE} (received_at timestamptz NOT NULL, facility smallint NOT NULL, severity smallint NOT NULL, \"timestamp\" timestamptz, hostname text, app_name text, procid text, msgid text, structured_data text, message text NOT NULL)"
        ),
    )
    .map(|_| ())
}

fn insert(session: &mut Session<'_>, batch: &[Received]) -> Result<(), String> {
    for rows in batch.chunks(ROWS_PER_INSERT) {
        let values: Vec<String> = rows.iter().map(row).collect();
        let statement = format!(
            "INSERT INTO {TABLE} (received_at, facility, severity, \"timestamp\", hostname, app_name, procid, msgid, structured_data, message) VALUES {}",
            values.join(", ")
        );
        if run(session, &statement).is_err() {
            create_table(session)?;
            run(session, &statement)?;
        }
    }
    Ok(())
}

fn store_batch(session: &mut Session<'_>, batch: &mut Vec<Received>) {
    if !batch.is_empty() {
        let _ = insert(session, batch);
        batch.clear();
    }
}

impl<'s, S: KeyValueStore + Sync> SyslogCollector<'s, S> {
    /// A collector that writes to `store` as `user` in `database`, taking at most
    /// `max_connections` TCP connections at once.
    pub fn new(store: &'s S, user: &str, database: &str, max_connections: usize) -> Self {
        Self {
            store,
            user: user.to_owned(),
            database: database.to_owned(),
            max_connections,
        }
    }

    fn session(&self) -> Session<'s> {
        let mut session = Session::with_dialect(self.store, Dialect::PostgreSql);
        session.identify(&self.user, &self.database);
        session.set_protocol("syslog");
        session
    }

    /// Creates the `syslog` table when it is missing, so it can be queried before the first
    /// message arrives.
    pub fn prepare(&self) -> Result<(), String> {
        create_table(&mut self.session())
    }

    /// Takes messages from `datagrams` and connections to `listener`, each when given, until
    /// `stop` answers true, then writes what it holds and returns.
    pub fn serve<D: DatagramSocket>(
        &self,
        datagrams: Option<&D>,
        listener: Option<&TcpListener>,
        stop: &(dyn Fn() -> bool + Sync),
    ) -> io::Result<()> {
        if let Some(listener) = listener {
            listener.set_nonblocking(true)?;
        }
        std::thread::scope(|scope| {
            if let Some(socket) = datagrams {
                scope.spawn(move || self.serve_datagrams(socket, stop));
            }
            if let Some(listener) = listener {
                self.accept(listener, stop);
            }
        });
        Ok(())
    }

    fn serve_datagrams<D: DatagramSocket>(&self, socket: &D, stop: &(dyn Fn() -> bool + Sync)) {
        let mut session = self.session();
        let mut buffer = vec![0; MAX_SYSLOG_MESSAGE_BYTES + 1];
        let mut batch: Vec<Received> = Vec::new();
        let mut batch_started = Instant::now();
        while !stop() {
            match socket.receive_from(&mut buffer, RECEIVE_WAIT) {
                Ok((length, _)) if length <= MAX_SYSLOG_MESSAGE_BYTES => {
                    if let Some(datagram) = buffer.get(..length).filter(|bytes| !bytes.is_empty()) {
                        if batch.is_empty() {
                            batch_started = Instant::now();
                        }
                        let received_at = now_micros();
                        batch.push((received_at, parse(datagram, received_at)));
                    }
                }
                Ok(_) => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::InvalidInput) => {}
                Err(_) => std::thread::sleep(RECEIVE_WAIT),
            }
            if batch.len() >= ROWS_PER_INSERT || batch_started.elapsed() >= RECEIVE_WAIT {
                store_batch(&mut session, &mut batch);
            }
        }
        store_batch(&mut session, &mut batch);
    }

    fn accept(&self, listener: &TcpListener, stop: &(dyn Fn() -> bool + Sync)) {
        let active = AtomicUsize::new(0);
        std::thread::scope(|connections| {
            while !stop() {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(_) => {
                        std::thread::sleep(ACCEPT_POLL);
                        continue;
                    }
                };
                if stream.set_nonblocking(false).is_err()
                    || stream.set_read_timeout(Some(RECEIVE_WAIT)).is_err()
                {
                    continue;
                }
                if active.fetch_add(1, Ordering::AcqRel) >= self.max_connections {
                    active.fetch_sub(1, Ordering::AcqRel);
                    continue;
                }
                let active = &active;
                let spawned = std::thread::Builder::new()
                    .name("eggdb-syslog".to_owned())
                    .spawn_scoped(connections, move || {
                        self.serve_stream(stream, stop);
                        active.fetch_sub(1, Ordering::AcqRel);
                    });
                if spawned.is_err() {
                    active.fetch_sub(1, Ordering::AcqRel);
                }
            }
        });
    }

    fn serve_stream(&self, mut stream: TcpStream, stop: &(dyn Fn() -> bool + Sync)) {
        let mut session = self.session();
        let mut framer = Framer::new(MAX_SYSLOG_MESSAGE_BYTES);
        let mut chunk = vec![0; READ_CHUNK_BYTES];
        let mut batch: Vec<Received> = Vec::new();
        while !stop() {
            let read = match stream.read(&mut chunk) {
                Ok(0) => {
                    if let Some(last) = framer.finish() {
                        let received_at = now_micros();
                        batch.push((received_at, parse(&last, received_at)));
                    }
                    break;
                }
                Ok(read) => read,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(_) => break,
            };
            framer.push(chunk.get(..read).unwrap_or_default());
            let received_at = now_micros();
            let framed = loop {
                match framer.next_frame() {
                    Ok(Some(frame)) => batch.push((received_at, parse(&frame, received_at))),
                    Ok(None) => break true,
                    Err(_) => break false,
                }
            };
            store_batch(&mut session, &mut batch);
            if !framed {
                return;
            }
        }
        store_batch(&mut session, &mut batch);
    }
}
