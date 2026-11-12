use crate::commands::{Shared, exceeded_memory_limit, run};
use crate::compression::{OP_COMPRESSED, compress_reply, decompress_body};
use crate::cursors::Cursors;
use crate::message::{
    HEADER_BYTES, OP_MSG, OP_QUERY, Request, message_reply, query_reply, read_header, read_message,
    read_query,
};
use eggdb_auth::scram::{ScramExchange, ScramFailure, ScramVerifier};
use eggdb_auth::scram_sha1::{ScramSha1Exchange, ScramSha1Verifier};
use eggdb_bson::{Bson, Document};
use eggdb_documents::Store;
use eggdb_platform::connection_limits::MemoryBudget;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);
const FAILED_LOGIN_PAUSE: Duration = Duration::from_millis(250);

/// How the MongoDB-compatible server runs.
pub struct ServerConfig {
    /// The one user clients authenticate as, with SCRAM-SHA-256.
    pub user: String,
    /// That user's SCRAM-SHA-256 verifier (the password is not kept).
    pub verifier: ScramVerifier,
    /// That user's SCRAM-SHA-1 verifier, for drivers that ask for SCRAM-SHA-1; `None` offers
    /// SCRAM-SHA-256 only.
    pub sha1_verifier: Option<ScramSha1Verifier>,
    /// Random bytes that make an unknown user's exchange look like a real one's.
    pub unknown_user_secret: [u8; 32],
    /// The most connections served at once.
    pub max_connections: usize,
    /// Fills a buffer with unpredictable bytes, for SCRAM nonces.
    pub random: fn(&mut [u8]) -> io::Result<()>,
    /// The memory every connection's messages and open cursors hold together,
    /// shared with the server's other listeners.
    pub memory_budget: std::sync::Arc<MemoryBudget>,
    /// The replica set name `hello` gives, making the server a one-member replica set so
    /// drivers allow transactions; `None` answers as a standalone server.
    pub replica_set: Option<String>,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("user", &self.user)
            .field("max_connections", &self.max_connections)
            .finish_non_exhaustive()
    }
}

pub(crate) enum Exchange {
    Sha256(ScramExchange),
    Sha1(ScramSha1Exchange),
}

impl Exchange {
    pub(crate) fn client_first(&mut self, message: &[u8]) -> Result<String, ScramFailure> {
        match self {
            Self::Sha256(exchange) => exchange.client_first(message),
            Self::Sha1(exchange) => exchange.client_first(message),
        }
    }

    pub(crate) fn client_final(&mut self, message: &[u8]) -> Result<String, ScramFailure> {
        match self {
            Self::Sha256(exchange) => exchange.client_final(message),
            Self::Sha1(exchange) => exchange.client_final(message),
        }
    }
}

pub(crate) enum Conversation {
    AwaitingFinal {
        exchange: Box<Exchange>,
        skip_empty: bool,
        user: String,
    },
    AwaitingEmpty {
        user: String,
    },
}

/// One connection's state.
pub(crate) struct Session {
    pub(crate) id: i32,
    pub(crate) peer: String,
    pub(crate) local: String,
    pub(crate) transaction: Option<crate::transactions::Transaction>,
    pub(crate) committed_transaction: Option<i64>,
    pub(crate) authenticated_user: Option<String>,
    pub(crate) authentication_database: String,
    pub(crate) roles: Option<Vec<(String, String)>>,
    pub(crate) conversation: Option<Conversation>,
    pub(crate) failed_logins: u32,
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
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                other => return other,
            }
        }
    }
}

fn database_of(body: &Document) -> String {
    body.get_str("$db").unwrap_or("admin").to_owned()
}

pub(crate) fn without_generic_fields(body: &Document) -> Document {
    Document::from_entries(
        body.iter()
            .filter(|(name, _)| {
                !matches!(
                    name.as_str(),
                    "$db"
                        | "lsid"
                        | "txnNumber"
                        | "autocommit"
                        | "startTransaction"
                        | "$clusterTime"
                        | "$readPreference"
                        | "apiVersion"
                        | "apiStrict"
                        | "apiDeprecationErrors"
                )
            })
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    )
}

fn refusal_reply(op_code: i32, reply_id: i32, request_id: i32, refusal: &Document) -> Vec<u8> {
    if op_code == OP_QUERY {
        query_reply(reply_id, request_id, refusal, false)
    } else {
        message_reply(reply_id, request_id, refusal)
    }
}

/// Serves one connection: reads requests from `reader` and writes replies to `writer` until
/// the client closes it or sends something that is not a valid message.
pub(crate) fn serve_stream(
    shared: &Shared<'_>,
    session: &mut Session,
    mut reader: impl Read,
    writer: &mut impl Write,
) -> io::Result<()> {
    let mut next_reply_id = 1i32;
    let mut held = shared.config.memory_budget.empty_reservation();
    loop {
        let mut header_bytes = [0u8; HEADER_BYTES];
        match reader.read_exact(&mut header_bytes) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        }
        let Ok(header) = read_header(&header_bytes) else {
            return Ok(());
        };
        if !held.hold_if_large(header.length) {
            let refusal = exceeded_memory_limit(&format!("a message of {} bytes", header.length));
            writer.write_all(&refusal_reply(
                header.op_code,
                next_reply_id,
                header.request_id,
                &refusal,
            ))?;
            return Ok(());
        }
        let mut body = vec![0u8; header.length - HEADER_BYTES];
        reader.read_exact(&mut body)?;
        let (op_code, compressor, body) = if header.op_code == OP_COMPRESSED {
            let Ok((op_code, compressor, restored)) = decompress_body(&body) else {
                return Ok(());
            };
            if !held.hold_if_large(restored.len() + HEADER_BYTES) {
                let refusal = exceeded_memory_limit(&format!(
                    "a message of {} bytes",
                    restored.len() + HEADER_BYTES
                ));
                writer.write_all(&refusal_reply(
                    op_code,
                    next_reply_id,
                    header.request_id,
                    &refusal,
                ))?;
                return Ok(());
            }
            (op_code, Some(compressor), restored)
        } else {
            (header.op_code, None, body)
        };
        let request = match op_code {
            OP_MSG => read_message(&body),
            OP_QUERY => read_query(&body),
            _ => return Ok(()),
        };
        let Ok(request) = request else {
            return Ok(());
        };
        let failed_before = session.failed_logins;
        let reply = match request {
            Request::Message { body, more_to_come } => {
                let database = database_of(&body);
                let reply = crate::transactions::run_command(shared, session, &database, &body);
                if more_to_come {
                    None
                } else {
                    Some(message_reply(next_reply_id, header.request_id, &reply))
                }
            }
            Request::Query { namespace, query } => {
                let (database, collection) = namespace
                    .split_once('.')
                    .unwrap_or((namespace.as_str(), ""));
                if collection != "$cmd" {
                    let refusal = Document::from_entries(vec![
                        (
                            "$err".to_owned(),
                            Bson::String("OP_QUERY is only supported for commands".to_owned()),
                        ),
                        ("code".to_owned(), Bson::Int32(352)),
                    ]);
                    Some(query_reply(
                        next_reply_id,
                        header.request_id,
                        &refusal,
                        true,
                    ))
                } else {
                    let reply = run(shared, session, database, &without_generic_fields(&query));
                    Some(query_reply(next_reply_id, header.request_id, &reply, false))
                }
            }
        };
        if session.failed_logins > failed_before {
            std::thread::sleep(FAILED_LOGIN_PAUSE);
        }
        next_reply_id = next_reply_id.wrapping_add(1);
        if let Some(reply) = reply {
            match compressor {
                Some(compressor) => writer.write_all(&compress_reply(&reply, compressor))?,
                None => writer.write_all(&reply)?,
            }
        }
        held.hold_if_large(0);
    }
}

/// Serves MongoDB clients on `listener` from `store`, a thread per connection, until `stop`
/// turns true.
pub fn serve(
    store: &Store<'_>,
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let mut seed = [0u8; 8];
    (config.random)(&mut seed)?;
    let shared = Shared {
        store,
        config,
        cursors: Mutex::new(Cursors::new(
            u64::from_le_bytes(seed),
            &config.memory_budget,
        )),
        started: Instant::now(),
    };
    let active = AtomicUsize::new(0);
    let next_id = AtomicI32::new(1);
    std::thread::scope(|scope| {
        while !stop() {
            let (stream, peer) = match eggdb_platform::accept_tcp(listener) {
                Ok(accepted) => accepted,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err()
                || stream.set_read_timeout(Some(READ_POLL)).is_err()
                || stream.set_nodelay(true).is_err()
            {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let id = next_id.fetch_add(1, Ordering::Relaxed);
            let (shared, active) = (&shared, &active);
            let spawned = std::thread::Builder::new()
                .name("eggdb-mongo".to_owned())
                .spawn_scoped(scope, move || {
                    if let Ok(mut writer) = stream.try_clone() {
                        let local = stream
                            .local_addr()
                            .map(|address| address.to_string())
                            .unwrap_or_default();
                        let mut session = Session {
                            id,
                            peer: peer.to_string(),
                            local,
                            transaction: None,
                            committed_transaction: None,
                            authenticated_user: None,
                            authentication_database: "admin".to_owned(),
                            roles: None,
                            conversation: None,
                            failed_logins: 0,
                        };
                        let _ = serve_stream(
                            shared,
                            &mut session,
                            StopAware { stream, stop },
                            &mut writer,
                        );
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
