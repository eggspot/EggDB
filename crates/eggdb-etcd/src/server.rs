use crate::proto::{Read, Write};
use crate::store::{
    Compare, CompareOperand, EtcdError, Event, KeyRange, KeyValue, Operation, Outcome, RangeOptions,
    Store, Transaction,
};
use eggdb_http2::{GrpcStatus, Handler, Http2Request, Http2Response, grpc_message, grpc_messages};
use eggdb_keyspace::Keyspace;
use std::net::TcpListener;
use std::time::Duration;

const CLUSTER_ID: i64 = 0x0E99_DB00_0000_0001;
const MEMBER_ID: i64 = 0x0E99_DB00_0000_0002;
const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_TIMEOUT: Duration = Duration::from_secs(300);
const MOST_NESTED_TXNS: usize = 8;

/// How the etcd server runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// The most connections served at once.
    pub max_connections: usize,
    /// The version `Maintenance/Status` reports.
    pub version: String,
    /// The client URL `Cluster/MemberList` reports.
    pub client_url: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { max_connections: 1_000, version: "3.5.0".to_owned(), client_url: String::new() }
    }
}

fn header(revision: i64) -> Vec<u8> {
    Write::new().int(1, CLUSTER_ID).int(2, MEMBER_ID).int(3, revision).int(4, 1).finish()
}

fn key_value(kv: &KeyValue) -> Vec<u8> {
    Write::new()
        .bytes(1, &kv.key)
        .int(2, kv.create_revision)
        .int(3, kv.mod_revision)
        .int(4, kv.version)
        .bytes_if_any(5, &kv.value)
        .int(6, kv.lease)
        .finish()
}

fn key_range(message: &Read<'_>) -> KeyRange {
    KeyRange { key: message.bytes(1).to_vec(), end: message.bytes(2).to_vec() }
}

fn range_request(bytes: &[u8]) -> Result<Operation, String> {
    let message = Read::new(bytes)?;
    Ok(Operation::Range(
        key_range(&message),
        RangeOptions {
            limit: message.int(3),
            revision: message.int(4),
            keys_only: message.flag(8),
            count_only: message.flag(9),
        },
    ))
}

fn put_request(bytes: &[u8]) -> Result<Operation, String> {
    let message = Read::new(bytes)?;
    Ok(Operation::Put {
        key: message.bytes(1).to_vec(),
        value: message.bytes(2).to_vec(),
        lease: message.int(3),
        prev_kv: message.flag(4),
        ignore_value: message.flag(5),
        ignore_lease: message.flag(6),
    })
}

fn delete_request(bytes: &[u8]) -> Result<Operation, String> {
    let message = Read::new(bytes)?;
    Ok(Operation::Delete { range: key_range(&message), prev_kv: message.flag(3) })
}

fn compare(bytes: &[u8]) -> Result<Compare, String> {
    let message = Read::new(bytes)?;
    let target = message.int(2);
    let operand = match target {
        3 => CompareOperand::Value(message.bytes(7).to_vec()),
        0 => CompareOperand::Number(message.int(4)),
        1 => CompareOperand::Number(message.int(5)),
        2 => CompareOperand::Number(message.int(6)),
        _ => CompareOperand::Number(message.int(8)),
    };
    Ok(Compare {
        result: message.int(1),
        target,
        key: message.bytes(3).to_vec(),
        range_end: message.bytes(64).to_vec(),
        operand,
    })
}

fn request_op(bytes: &[u8], depth: usize) -> Result<Operation, String> {
    let message = Read::new(bytes)?;
    if message.has(1) {
        range_request(message.bytes(1))
    } else if message.has(2) {
        put_request(message.bytes(2))
    } else if message.has(3) {
        delete_request(message.bytes(3))
    } else if message.has(4) {
        txn_request(message.bytes(4), depth + 1).map(|transaction| Operation::Txn(Box::new(transaction)))
    } else {
        Err("a transaction holds an empty operation".to_owned())
    }
}

fn txn_request(bytes: &[u8], depth: usize) -> Result<Transaction, String> {
    if depth > MOST_NESTED_TXNS {
        return Err("transactions are nested too deeply".to_owned());
    }
    let message = Read::new(bytes)?;
    Ok(Transaction {
        compares: message.repeated(1).into_iter().map(compare).collect::<Result<_, _>>()?,
        success: message.repeated(2).into_iter().map(|op| request_op(op, depth)).collect::<Result<_, _>>()?,
        failure: message.repeated(3).into_iter().map(|op| request_op(op, depth)).collect::<Result<_, _>>()?,
    })
}

fn outcome_body(outcome: &Outcome, revision: i64) -> Vec<u8> {
    match outcome {
        Outcome::Range(result) => {
            let mut message = Write::new().bytes(1, &header(revision));
            for kv in &result.kvs {
                message = message.bytes(2, &key_value(kv));
            }
            message.flag(3, result.more).int(4, result.count).finish()
        }
        Outcome::Put(previous) => {
            let message = Write::new().bytes(1, &header(revision));
            match previous {
                Some(kv) => message.bytes(2, &key_value(kv)).finish(),
                None => message.finish(),
            }
        }
        Outcome::Delete(deleted, previous) => {
            let mut message = Write::new().bytes(1, &header(revision)).int(2, *deleted);
            for kv in previous {
                message = message.bytes(3, &key_value(kv));
            }
            message.finish()
        }
        Outcome::Txn(succeeded, outcomes) => {
            let mut message = Write::new().bytes(1, &header(revision)).flag(2, *succeeded);
            for inner in outcomes {
                let field = match inner {
                    Outcome::Range(_) => 1,
                    Outcome::Put(_) => 2,
                    Outcome::Delete(..) => 3,
                    Outcome::Txn(..) => 4,
                };
                message = message.bytes(3, &Write::new().bytes(field, &outcome_body(inner, revision)).finish());
            }
            message.finish()
        }
    }
}

fn event(event: &Event) -> Vec<u8> {
    Write::new().int(1, i64::from(event.deleted)).bytes(2, &key_value(&event.kv)).finish()
}

fn reply(messages: Vec<Vec<u8>>) -> Http2Response {
    Http2Response {
        headers: vec![
            (":status".to_owned(), "200".to_owned()),
            ("content-type".to_owned(), "application/grpc".to_owned()),
        ],
        body: messages.iter().map(|message| grpc_message(message)).collect(),
        trailers: vec![("grpc-status".to_owned(), GrpcStatus::Ok.code())],
    }
}

fn failure(status: GrpcStatus, message: &str) -> Http2Response {
    let mut shown = String::with_capacity(message.len());
    for byte in message.bytes() {
        if (0x20..0x7F).contains(&byte) && byte != b'%' {
            shown.push(char::from(byte));
        } else {
            shown.push_str(&format!("%{byte:02X}"));
        }
    }
    Http2Response {
        headers: vec![
            (":status".to_owned(), "200".to_owned()),
            ("content-type".to_owned(), "application/grpc".to_owned()),
            ("grpc-status".to_owned(), status.code()),
            ("grpc-message".to_owned(), shown),
        ],
        body: Vec::new(),
        trailers: Vec::new(),
    }
}

fn status_of(error: &EtcdError) -> GrpcStatus {
    match error {
        EtcdError::Invalid(_) => GrpcStatus::InvalidArgument,
        EtcdError::Compacted(_) | EtcdError::FutureRevision => GrpcStatus::OutOfRange,
        EtcdError::LeaseMissing => GrpcStatus::NotFound,
        EtcdError::Storage(_) => GrpcStatus::Internal,
    }
}

/// The etcd v3 gRPC services over a store.
pub(crate) struct Service<'k> {
    store: Store<'k>,
    config: ServerConfig,
}

impl<'k> Service<'k> {
    pub(crate) fn new(keyspace: &'k (dyn Keyspace + Sync), config: ServerConfig) -> Self {
        Self { store: Store::new(keyspace), config }
    }

    fn answer(&self, path: &str, message: &[u8]) -> Result<Vec<u8>, (GrpcStatus, String)> {
        let invalid = |reason: String| (GrpcStatus::InvalidArgument, reason);
        let failed = |error: EtcdError| (status_of(&error), error.message());
        match path {
            "/etcdserverpb.KV/Range" => {
                let Operation::Range(range, options) = range_request(message).map_err(invalid)? else {
                    return Err(invalid("not a range".to_owned()));
                };
                let (result, revision) = self.store.range(&range, options).map_err(failed)?;
                Ok(outcome_body(&Outcome::Range(result), revision))
            }
            "/etcdserverpb.KV/Put" => {
                let (outcome, revision) = self.store.run(&put_request(message).map_err(invalid)?).map_err(failed)?;
                Ok(outcome_body(&outcome, revision))
            }
            "/etcdserverpb.KV/DeleteRange" => {
                let (outcome, revision) = self.store.run(&delete_request(message).map_err(invalid)?).map_err(failed)?;
                Ok(outcome_body(&outcome, revision))
            }
            "/etcdserverpb.KV/Txn" => {
                let transaction = txn_request(message, 0).map_err(invalid)?;
                let (outcome, revision) = self.store.run(&Operation::Txn(Box::new(transaction))).map_err(failed)?;
                Ok(outcome_body(&outcome, revision))
            }
            "/etcdserverpb.KV/Compact" => {
                let request = Read::new(message).map_err(invalid)?;
                let revision = self.store.compact(request.int(1)).map_err(failed)?;
                Ok(Write::new().bytes(1, &header(revision)).finish())
            }
            "/etcdserverpb.Lease/LeaseGrant" => {
                let request = Read::new(message).map_err(invalid)?;
                let ttl = request.int(1);
                let (id, revision) = self.store.grant(ttl, request.int(2)).map_err(failed)?;
                Ok(Write::new().bytes(1, &header(revision)).int(2, id).int(3, ttl).finish())
            }
            "/etcdserverpb.Lease/LeaseRevoke" => {
                let request = Read::new(message).map_err(invalid)?;
                let revision = self.store.revoke(request.int(1)).map_err(failed)?;
                Ok(Write::new().bytes(1, &header(revision)).finish())
            }
            "/etcdserverpb.Lease/LeaseKeepAlive" => {
                let request = Read::new(message).map_err(invalid)?;
                let id = request.int(1);
                let (ttl, revision) = self.store.keep_alive(id).map_err(failed)?;
                Ok(Write::new().bytes(1, &header(revision)).int(2, id).int(3, ttl).finish())
            }
            "/etcdserverpb.Lease/LeaseTimeToLive" => {
                let request = Read::new(message).map_err(invalid)?;
                let id = request.int(1);
                let (state, revision) = self.store.time_to_live(id, request.flag(2)).map_err(failed)?;
                let mut reply = Write::new().bytes(1, &header(revision)).int(2, id);
                match state {
                    Some(state) => {
                        reply = reply.int(3, state.ttl_seconds).int(4, state.granted_seconds);
                        for key in &state.keys {
                            reply = reply.bytes(5, key);
                        }
                    }
                    None => reply = reply.int(3, -1),
                }
                Ok(reply.finish())
            }
            "/etcdserverpb.Lease/LeaseLeases" => {
                let (leases, revision) = self.store.leases().map_err(failed)?;
                let mut reply = Write::new().bytes(1, &header(revision));
                for id in leases {
                    reply = reply.bytes(2, &Write::new().int(1, id).finish());
                }
                Ok(reply.finish())
            }
            "/etcdserverpb.Maintenance/Status" => {
                let revision = self.store.revision().map_err(failed)?;
                Ok(Write::new()
                    .bytes(1, &header(revision))
                    .bytes(2, self.config.version.as_bytes())
                    .int(4, MEMBER_ID)
                    .int(5, revision)
                    .int(6, 1)
                    .finish())
            }
            "/etcdserverpb.Cluster/MemberList" => {
                let revision = self.store.revision().map_err(failed)?;
                let member = Write::new()
                    .int(1, MEMBER_ID)
                    .bytes(2, b"eggdb")
                    .bytes_if_any(4, self.config.client_url.as_bytes())
                    .finish();
                Ok(Write::new().bytes(1, &header(revision)).bytes(2, &member).finish())
            }
            _ => Err((GrpcStatus::Unimplemented, format!("EggDB does not serve {path}"))),
        }
    }

    fn watch(&self, messages: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, (GrpcStatus, String)> {
        let mut replies = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            let request = Read::new(message).map_err(|reason| (GrpcStatus::InvalidArgument, reason))?;
            if !request.has(1) {
                continue;
            }
            let create = Read::new(request.bytes(1)).map_err(|reason| (GrpcStatus::InvalidArgument, reason))?;
            let range = key_range(&create);
            let watch_id = if create.int(7) != 0 { create.int(7) } else { index as i64 };
            match self.store.events_since(&range, create.int(3)) {
                Ok((events, revision)) => {
                    replies.push(Write::new().bytes(1, &header(revision)).int(2, watch_id).flag(3, true).finish());
                    if !events.is_empty() && create.int(3) > 0 {
                        let mut reply = Write::new().bytes(1, &header(revision)).int(2, watch_id);
                        for change in &events {
                            reply = reply.bytes(11, &event(change));
                        }
                        replies.push(reply.finish());
                    }
                }
                Err(EtcdError::Compacted(compacted)) => {
                    replies.push(
                        Write::new()
                            .bytes(1, &header(compacted))
                            .int(2, watch_id)
                            .flag(3, true)
                            .flag(4, true)
                            .int(5, compacted)
                            .bytes(6, b"etcdserver: mvcc: required revision has been compacted")
                            .finish(),
                    );
                }
                Err(error) => return Err((status_of(&error), error.message())),
            }
        }
        Ok(replies)
    }
}

impl Handler for Service<'_> {
    fn handle(&self, request: &Http2Request) -> Http2Response {
        let messages = match grpc_messages(&request.body) {
            Ok(messages) => messages,
            Err(error) => return failure(GrpcStatus::InvalidArgument, &error.to_string()),
        };
        let path = request.path();
        if path == "/etcdserverpb.Watch/Watch" {
            return match self.watch(&messages) {
                Ok(replies) => reply(replies),
                Err((status, message)) => failure(status, &message),
            };
        }
        let mut replies = Vec::with_capacity(messages.len());
        for message in &messages {
            match self.answer(path, message) {
                Ok(reply) => replies.push(reply),
                Err((status, message)) => return failure(status, &message),
            }
        }
        if messages.is_empty() {
            match self.answer(path, &[]) {
                Ok(reply) => replies.push(reply),
                Err((status, message)) => return failure(status, &message),
            }
        }
        reply(replies)
    }
}

/// Serves etcd's v3 gRPC API on `listener` from `keyspace` until `stop` turns true, a thread
/// per connection.
pub fn serve(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let service = Service::new(keyspace, config.clone());
    let connections = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !stop() {
            let mut stream = match eggdb_platform::accept_tcp(listener) {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if connections.load(std::sync::atomic::Ordering::Acquire) >= config.max_connections
                || stream.set_nonblocking(false).is_err()
                || stream.set_read_timeout(Some(READ_TIMEOUT)).is_err()
            {
                continue;
            }
            connections.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let (service, connections) = (&service, &connections);
            let _ = std::thread::Builder::new().name("eggdb-etcd".to_owned()).spawn_scoped(scope, move || {
                let _ = eggdb_http2::serve_connection(&mut stream, service);
                connections.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            });
        }
    });
    Ok(())
}
