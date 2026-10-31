use crate::commands::{Shared, run};
use crate::cursors::Cursors;
use crate::message::{HEADER_BYTES, OP_MSG, OP_QUERY, Request, message_reply, query_reply, read_header, read_message, read_query};
use eggdb_auth::scram::{ScramExchange, ScramVerifier};
use eggdb_bson::{Bson, Document};
use eggdb_documents::Store;
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
    /// Random bytes that make an unknown user's exchange look like a real one's.
    pub unknown_user_secret: [u8; 32],
    /// The most connections served at once.
    pub max_connections: usize,
    /// Fills a buffer with unpredictable bytes, for SCRAM nonces.
    pub random: fn(&mut [u8]) -> io::Result<()>,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("user", &self.user)
            .field("max_connections", &self.max_connections)
            .finish_non_exhaustive()
    }
}

pub(crate) enum Conversation {
    AwaitingFinal { exchange: ScramExchange, skip_empty: bool, user: String },
    AwaitingEmpty { user: String },
}

/// One connection's state.
pub(crate) struct Session {
    pub(crate) id: i32,
    pub(crate) peer: String,
    pub(crate) authenticated_user: Option<String>,
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
                Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {}
                other => return other,
            }
        }
    }
}

fn database_of(body: &Document) -> String {
    body.get_str("$db").unwrap_or("admin").to_owned()
}

fn without_generic_fields(body: &Document) -> Document {
    Document::from_entries(
        body.iter()
            .filter(|(name, _)| !matches!(name.as_str(), "$db" | "lsid" | "$clusterTime" | "$readPreference" | "apiVersion" | "apiStrict" | "apiDeprecationErrors"))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    )
}

/// Serves one connection: reads requests from `reader` and writes replies to `writer` until
/// the client closes it or sends something that is not a valid message.
pub(crate) fn serve_stream(shared: &Shared<'_>, session: &mut Session, mut reader: impl Read, writer: &mut impl Write) -> io::Result<()> {
    let mut next_reply_id = 1i32;
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
        let mut body = vec![0u8; header.length - HEADER_BYTES];
        reader.read_exact(&mut body)?;
        let request = match header.op_code {
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
                let reply = run(shared, session, &database, &without_generic_fields(&body));
                if more_to_come {
                    None
                } else {
                    Some(message_reply(next_reply_id, header.request_id, &reply))
                }
            }
            Request::Query { namespace, query } => {
                let (database, collection) = namespace.split_once('.').unwrap_or((namespace.as_str(), ""));
                if collection != "$cmd" {
                    let refusal = Document::from_entries(vec![
                        ("$err".to_owned(), Bson::String("OP_QUERY is only supported for commands".to_owned())),
                        ("code".to_owned(), Bson::Int32(352)),
                    ]);
                    Some(query_reply(next_reply_id, header.request_id, &refusal, true))
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
            writer.write_all(&reply)?;
        }
    }
}

/// Serves MongoDB clients on `listener` from `store`, a thread per connection, until `stop`
/// turns true.
pub fn serve(store: &Store<'_>, listener: &TcpListener, config: &ServerConfig, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let mut seed = [0u8; 8];
    (config.random)(&mut seed)?;
    let shared = Shared {
        store,
        config,
        cursors: Mutex::new(Cursors::new(u64::from_le_bytes(seed))),
        started: Instant::now(),
    };
    let active = AtomicUsize::new(0);
    let next_id = AtomicI32::new(1);
    std::thread::scope(|scope| {
        while !stop() {
            let (stream, peer) = match listener.accept() {
                Ok(accepted) => accepted,
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
            let (shared, active) = (&shared, &active);
            let spawned = std::thread::Builder::new().name("eggdb-mongo".to_owned()).spawn_scoped(scope, move || {
                if let Ok(mut writer) = stream.try_clone() {
                    let mut session = Session {
                        id,
                        peer: peer.to_string(),
                        authenticated_user: None,
                        conversation: None,
                        failed_logins: 0,
                    };
                    let _ = serve_stream(shared, &mut session, StopAware { stream, stop }, &mut writer);
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
