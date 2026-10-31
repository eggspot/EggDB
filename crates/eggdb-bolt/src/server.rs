use crate::packstream::{Packed, Reader, write_integer, write_list_header, write_map_header, write_string, write_value};
use eggdb_graph::{QueryResult, Value};
use eggdb_keyspace::Keyspace;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

const MAGIC: [u8; 4] = [0x60, 0x60, 0xb0, 0x17];
const MAX_MESSAGE_BYTES: usize = 16 << 20;
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);
const FAILED_LOGIN_PAUSE: Duration = Duration::from_millis(250);
const SERVER_AGENT: &str = "Neo4j/5.0.0 (EggDB)";

const HELLO: u8 = 0x01;
const GOODBYE: u8 = 0x02;
const RESET: u8 = 0x0f;
const RUN: u8 = 0x10;
const BEGIN: u8 = 0x11;
const COMMIT: u8 = 0x12;
const ROLLBACK: u8 = 0x13;
const DISCARD: u8 = 0x2f;
const PULL: u8 = 0x3f;
const TELEMETRY: u8 = 0x54;

const SUCCESS: u8 = 0x70;
const RECORD: u8 = 0x71;
const IGNORED: u8 = 0x7e;
const FAILURE: u8 = 0x7f;

/// How the Bolt server runs.
#[derive(Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// The one user drivers log in as, with basic authentication.
    pub user: String,
    /// The SHA-256 of that user's password (the password is not kept).
    pub password_sha256: [u8; 32],
    /// The most connections served at once.
    pub max_connections: usize,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig").field("user", &self.user).field("max_connections", &self.max_connections).finish_non_exhaustive()
    }
}

struct Connection<'s> {
    keyspace: &'s dyn Keyspace,
    config: &'s ServerConfig,
    id: u64,
    element_ids: bool,
    authenticated: bool,
    failed: bool,
    pending: Option<QueryResult>,
    in_transaction: bool,
    wrote_in_transaction: bool,
    read_only_transaction: bool,
}

fn choose_version(proposals: &[u8; 16]) -> Option<(u8, u8)> {
    proposals.chunks(4).find_map(|proposal| {
        let (range, minor, major) = (proposal[1], proposal[2], proposal[3]);
        match major {
            5 if minor.saturating_sub(range) == 0 => Some((5, 0)),
            4 if minor >= 4 && minor.saturating_sub(range) <= 4 => Some((4, 4)),
            _ => None,
        }
    })
}

fn message(signature: u8, body: impl FnOnce(&mut Vec<u8>), fields: u8) -> Vec<u8> {
    let mut out = vec![0xb0 | fields, signature];
    body(&mut out);
    out
}

fn metadata(entries: &[(&str, Value)]) -> Vec<u8> {
    message(
        SUCCESS,
        |out| {
            write_map_header(out, entries.len());
            for (name, value) in entries {
                write_string(out, name);
                write_value(out, value, false);
            }
        },
        1,
    )
}

fn failure(code: &str, text: &str) -> Vec<u8> {
    message(
        FAILURE,
        |out| {
            write_map_header(out, 2);
            write_string(out, "code");
            write_string(out, code);
            write_string(out, "message");
            write_string(out, text);
        },
        1,
    )
}

fn write_chunked(writer: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    for chunk in payload.chunks(65_535) {
        writer.write_all(&(chunk.len() as u16).to_be_bytes())?;
        writer.write_all(chunk)?;
    }
    writer.write_all(&[0, 0])
}

fn read_message(reader: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut payload = Vec::new();
    loop {
        let mut size = [0u8; 2];
        match reader.read_exact(&mut size) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && payload.is_empty() => return Ok(None),
            Err(error) => return Err(error),
        }
        let size = usize::from(u16::from_be_bytes(size));
        if size == 0 {
            if payload.is_empty() {
                continue;
            }
            return Ok(Some(payload));
        }
        if payload.len() + size > MAX_MESSAGE_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a Bolt message is larger than 16 MiB"));
        }
        let start = payload.len();
        payload.resize(start + size, 0);
        reader.read_exact(&mut payload[start..])?;
    }
}

impl Connection<'_> {
    fn authenticate(&self, extra: &Packed) -> bool {
        let principal = extra.get("principal").and_then(Packed::as_str);
        let credentials = extra.get("credentials").and_then(Packed::as_str);
        match (extra.get("scheme").and_then(Packed::as_str), principal, credentials) {
            (Some("basic"), Some(user), Some(password)) => {
                user == self.config.user && eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(password.as_bytes()), &self.config.password_sha256)
            }
            _ => false,
        }
    }

    fn summary(&self) -> Vec<u8> {
        metadata(&[("has_more", Value::Boolean(false)), ("type", Value::String("rw".to_owned())), ("t_last", Value::Integer(0)), ("db", Value::String("neo4j".to_owned()))])
    }

    fn pull(&mut self, extra: Option<&Packed>, replies: &mut Vec<Vec<u8>>) {
        let wanted = match extra.and_then(|extra| extra.get("n")) {
            Some(Packed::Integer(count)) if *count > 0 => *count as usize,
            _ => usize::MAX,
        };
        let Some(mut result) = self.pending.take() else {
            replies.push(self.summary());
            return;
        };
        let taken = wanted.min(result.rows.len());
        for row in result.rows.drain(..taken) {
            replies.push(message(
                RECORD,
                |out| {
                    write_list_header(out, row.len());
                    for value in &row {
                        write_value(out, value, self.element_ids);
                    }
                },
                1,
            ));
        }
        if result.rows.is_empty() {
            replies.push(self.summary());
        } else {
            self.pending = Some(result);
            replies.push(metadata(&[("has_more", Value::Boolean(true))]));
        }
    }

    fn handle(&mut self, signature: u8, fields: &[Packed], replies: &mut Vec<Vec<u8>>) -> bool {
        if signature == GOODBYE {
            return false;
        }
        if !self.authenticated {
            if signature != HELLO {
                replies.push(failure("Neo.ClientError.Request.Invalid", "HELLO must come first"));
                return false;
            }
            let extra = fields.first().cloned().unwrap_or(Packed::Map(Vec::new()));
            if !self.authenticate(&extra) {
                std::thread::sleep(FAILED_LOGIN_PAUSE);
                replies.push(failure("Neo.ClientError.Security.Unauthorized", "The client is unauthorized due to authentication failure."));
                return false;
            }
            self.authenticated = true;
            replies.push(metadata(&[
                ("server", Value::String(SERVER_AGENT.to_owned())),
                ("connection_id", Value::String(format!("bolt-{}", self.id))),
            ]));
            return true;
        }
        if signature == RESET {
            self.failed = false;
            self.pending = None;
            self.in_transaction = false;
            self.wrote_in_transaction = false;
            replies.push(metadata(&[]));
            return true;
        }
        if self.failed {
            replies.push(vec![0xb0, IGNORED]);
            return true;
        }
        match signature {
            RUN => {
                let query = fields.first().and_then(Packed::as_str).unwrap_or_default().to_owned();
                let parameters = match fields.get(1) {
                    Some(Packed::Map(entries)) => entries.iter().map(|(name, value)| Ok((name.clone(), value.to_value()?))).collect::<Result<Vec<_>, String>>(),
                    _ => Ok(Vec::new()),
                };
                let read_only = if self.in_transaction {
                    self.read_only_transaction
                } else {
                    fields.get(2).and_then(|extra| extra.get("mode")).and_then(Packed::as_str) == Some("r")
                };
                let outcome = parameters
                    .map_err(|message| eggdb_graph::GraphError { code: "Neo.ClientError.Statement.TypeError".to_owned(), message })
                    .and_then(|parameters| eggdb_graph::run(self.keyspace, &query, &parameters, read_only));
                match outcome {
                    Ok(result) => {
                        if self.in_transaction && !read_only {
                            self.wrote_in_transaction = true;
                        }
                        let columns = Value::List(result.columns.iter().cloned().map(Value::String).collect());
                        let mut entries = vec![("fields", columns), ("t_first", Value::Integer(0))];
                        if self.in_transaction {
                            entries.push(("qid", Value::Integer(0)));
                        }
                        replies.push(metadata(&entries));
                        self.pending = Some(result);
                    }
                    Err(error) => {
                        self.failed = true;
                        replies.push(failure(&error.code, &error.message));
                    }
                }
            }
            PULL => self.pull(fields.first(), replies),
            DISCARD => {
                self.pending = None;
                replies.push(self.summary());
            }
            BEGIN => {
                self.in_transaction = true;
                self.wrote_in_transaction = false;
                self.read_only_transaction = fields.first().and_then(|extra| extra.get("mode")).and_then(Packed::as_str) == Some("r");
                replies.push(metadata(&[]));
            }
            COMMIT => {
                self.in_transaction = false;
                replies.push(metadata(&[("bookmark", Value::String("eggdb:committed".to_owned()))]));
            }
            ROLLBACK => {
                self.in_transaction = false;
                if self.wrote_in_transaction {
                    self.failed = true;
                    replies.push(failure(
                        "Neo.ClientError.Transaction.TransactionRollbackFailed",
                        "the statements of this transaction were each committed when they ran; EggDB cannot roll back an explicit Bolt transaction yet",
                    ));
                } else {
                    replies.push(metadata(&[]));
                }
            }
            TELEMETRY => replies.push(metadata(&[])),
            _ => {
                self.failed = true;
                replies.push(failure("Neo.ClientError.Request.Invalid", &format!("message {signature:#04x} is not supported; connect with bolt:// rather than neo4j://")));
            }
        }
        true
    }
}

pub(crate) fn serve_stream(
    keyspace: &dyn Keyspace,
    config: &ServerConfig,
    id: u64,
    mut reader: impl Read,
    writer: &mut impl Write,
) -> io::Result<()> {
    let mut magic = [0u8; 4];
    reader.read_exact(&mut magic)?;
    if magic != MAGIC {
        return Ok(());
    }
    let mut proposals = [0u8; 16];
    reader.read_exact(&mut proposals)?;
    let Some((major, minor)) = choose_version(&proposals) else {
        writer.write_all(&[0; 4])?;
        return Ok(());
    };
    writer.write_all(&[0, 0, minor, major])?;
    let mut connection = Connection {
        keyspace,
        config,
        id,
        element_ids: major >= 5,
        authenticated: false,
        failed: false,
        pending: None,
        in_transaction: false,
        wrote_in_transaction: false,
        read_only_transaction: false,
    };
    while let Some(payload) = read_message(&mut reader)? {
        let mut parser = Reader::new(&payload);
        let (signature, fields) = match parser.value(0) {
            Ok(Packed::Struct(signature, fields)) if parser.finished() => (signature, fields),
            _ => {
                write_chunked(writer, &failure("Neo.ClientError.Request.Invalid", "the message is not a valid PackStream structure"))?;
                return Ok(());
            }
        };
        let mut replies = Vec::new();
        let keep_going = connection.handle(signature, &fields, &mut replies);
        for reply in replies {
            write_chunked(writer, &reply)?;
        }
        if !keep_going {
            return Ok(());
        }
    }
    Ok(())
}

struct StopAware<'s> {
    stream: TcpStream,
    stop: &'s (dyn Fn() -> bool + Sync),
}

impl Read for StopAware<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if (self.stop)() {
                return Ok(0);
            }
            match self.stream.read(buffer) {
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                other => return other,
            }
        }
    }
}

/// Serves Bolt clients on `listener` from the graph in `keyspace`, a thread per connection,
/// until `stop` turns true.
pub fn serve(keyspace: &(dyn Keyspace + Sync), listener: &TcpListener, config: &ServerConfig, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    let next_id = AtomicU64::new(1);
    std::thread::scope(|scope| {
        while !stop() {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() || stream.set_read_timeout(Some(READ_POLL)).is_err() || stream.set_nodelay(true).is_err() {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let active = &active;
            let spawned = std::thread::Builder::new().name("eggdb-bolt".to_owned()).spawn_scoped(scope, move || {
                if let Ok(mut writer) = stream.try_clone() {
                    let _ = serve_stream(keyspace, config, id, StopAware { stream, stop }, &mut writer);
                }
                active.fetch_sub(1, Ordering::AcqRel);
            });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(())
}

#[cfg(test)]
pub(crate) fn encode_request(signature: u8, fields: &[Packed]) -> Vec<u8> {
    fn put(out: &mut Vec<u8>, value: &Packed) {
        match value {
            Packed::Null => out.push(0xc0),
            Packed::Boolean(flag) => out.push(if *flag { 0xc3 } else { 0xc2 }),
            Packed::Integer(number) => write_integer(out, *number),
            Packed::Float(number) => {
                out.push(0xc1);
                out.extend_from_slice(&number.to_be_bytes());
            }
            Packed::String(text) => write_string(out, text),
            Packed::Bytes(bytes) => {
                out.push(0xcc);
                out.push(bytes.len() as u8);
                out.extend_from_slice(bytes);
            }
            Packed::List(items) => {
                write_list_header(out, items.len());
                for item in items {
                    put(out, item);
                }
            }
            Packed::Map(entries) => {
                write_map_header(out, entries.len());
                for (name, item) in entries {
                    write_string(out, name);
                    put(out, item);
                }
            }
            Packed::Struct(signature, items) => {
                out.push(0xb0 | items.len() as u8);
                out.push(*signature);
                for item in items {
                    put(out, item);
                }
            }
        }
    }
    let mut payload = Vec::new();
    put(&mut payload, &Packed::Struct(signature, fields.to_vec()));
    let mut framed = Vec::new();
    let _ = write_chunked(&mut framed, &payload);
    framed
}
