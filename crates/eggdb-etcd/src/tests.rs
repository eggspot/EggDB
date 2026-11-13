use crate::proto::{Read, Write};
use crate::server::{ServerConfig, Service};
use eggdb_http2::{Handler, Http2Request, grpc_message, grpc_messages};
use eggdb_keyspace::MemoryKeyspace;

fn call(service: &Service<'_>, path: &str, messages: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, String> {
    let request = Http2Request {
        headers: vec![(":path".to_owned(), path.to_owned())],
        body: messages.iter().flat_map(|message| grpc_message(message)).collect(),
    };
    let response = service.handle(&request);
    if let Some((_, status)) = response.headers.iter().find(|(name, _)| name == "grpc-status") {
        let message = response
            .headers
            .iter()
            .find(|(name, _)| name == "grpc-message")
            .map(|(_, message)| message.clone())
            .unwrap_or_default();
        return Err(format!("{status}: {message}"));
    }
    let body: Vec<u8> = response.body.concat();
    grpc_messages(&body).map_err(|error| error.to_string())
}

fn one(service: &Service<'_>, path: &str, message: Vec<u8>) -> Vec<u8> {
    call(service, path, &[message]).unwrap().remove(0)
}

fn put(key: &str, value: &str) -> Vec<u8> {
    Write::new().bytes(1, key.as_bytes()).bytes(2, value.as_bytes()).finish()
}

fn revision_of(reply: &[u8]) -> i64 {
    Read::new(Read::new(reply).unwrap().bytes(1)).unwrap().int(3)
}

fn values(reply: &[u8]) -> Vec<(String, String)> {
    Read::new(reply)
        .unwrap()
        .repeated(2)
        .into_iter()
        .map(|kv| {
            let kv = Read::new(kv).unwrap();
            (String::from_utf8(kv.bytes(1).to_vec()).unwrap(), String::from_utf8(kv.bytes(5).to_vec()).unwrap())
        })
        .collect()
}

#[test]
fn puts_ranges_and_past_revisions_follow_etcd() {
    let keyspace = MemoryKeyspace::default();
    let service = Service::new(&keyspace, ServerConfig::default());
    let first = one(&service, "/etcdserverpb.KV/Put", put("/app/a", "1"));
    assert_eq!(revision_of(&first), 2);
    one(&service, "/etcdserverpb.KV/Put", put("/app/b", "2"));
    one(&service, "/etcdserverpb.KV/Put", put("/app/a", "3"));
    let prefix = Write::new().bytes(1, b"/app/").bytes(2, b"/app0").finish();
    let now = one(&service, "/etcdserverpb.KV/Range", prefix.clone());
    assert_eq!(values(&now), vec![("/app/a".to_owned(), "3".to_owned()), ("/app/b".to_owned(), "2".to_owned())]);
    assert_eq!(Read::new(&now).unwrap().int(4), 2);
    let a = Read::new(Read::new(&now).unwrap().repeated(2)[0]).unwrap();
    assert_eq!((a.int(2), a.int(3), a.int(4)), (2, 4, 2));
    let past = Write::new().bytes(1, b"/app/").bytes(2, b"/app0").int(4, 2).finish();
    assert_eq!(values(&one(&service, "/etcdserverpb.KV/Range", past)), vec![("/app/a".to_owned(), "1".to_owned())]);
    let limited = Write::new().bytes(1, b"/app/").bytes(2, b"/app0").int(3, 1).finish();
    let reply = one(&service, "/etcdserverpb.KV/Range", limited);
    assert_eq!(values(&reply).len(), 1);
    assert!(Read::new(&reply).unwrap().flag(3));
    let deleted = one(&service, "/etcdserverpb.KV/DeleteRange", Write::new().bytes(1, b"/app/b").int(3, 1).finish());
    assert_eq!(Read::new(&deleted).unwrap().int(2), 1);
    let compact = Write::new().int(1, 4).finish();
    one(&service, "/etcdserverpb.KV/Compact", compact);
    let too_old = Write::new().bytes(1, b"/app/a").int(4, 2).finish();
    assert!(call(&service, "/etcdserverpb.KV/Range", &[too_old]).unwrap_err().starts_with("11:"));
    let still = Write::new().bytes(1, b"/app/").bytes(2, b"/app0").int(4, 4).finish();
    assert_eq!(
        values(&one(&service, "/etcdserverpb.KV/Range", still)),
        vec![("/app/a".to_owned(), "3".to_owned()), ("/app/b".to_owned(), "2".to_owned())]
    );
}

#[test]
fn a_transaction_compares_then_runs_one_branch_at_one_revision() {
    let keyspace = MemoryKeyspace::default();
    let service = Service::new(&keyspace, ServerConfig::default());
    one(&service, "/etcdserverpb.KV/Put", put("lock", "free"));
    let compare = Write::new().int(1, 0).int(2, 3).bytes(3, b"lock").bytes(7, b"free").finish();
    let take = Write::new().bytes(2, &put("lock", "taken")).finish();
    let also = Write::new().bytes(2, &put("owner", "ada")).finish();
    let read = Write::new().bytes(1, &Write::new().bytes(1, b"lock").finish()).finish();
    let txn = Write::new().bytes(1, &compare).bytes(2, &take).bytes(2, &also).bytes(3, &read).finish();
    let first = one(&service, "/etcdserverpb.KV/Txn", txn.clone());
    assert!(Read::new(&first).unwrap().flag(2));
    assert_eq!(revision_of(&first), 3);
    let second = one(&service, "/etcdserverpb.KV/Txn", txn);
    assert!(!Read::new(&second).unwrap().flag(2));
    let response_op = Read::new(Read::new(&second).unwrap().repeated(3)[0]).unwrap();
    assert_eq!(values(response_op.bytes(1)), vec![("lock".to_owned(), "taken".to_owned())]);
    assert_eq!(revision_of(&second), 3);
}

#[test]
fn leases_carry_their_keys_and_watches_replay_history() {
    let keyspace = MemoryKeyspace::default();
    let service = Service::new(&keyspace, ServerConfig::default());
    let granted = one(&service, "/etcdserverpb.Lease/LeaseGrant", Write::new().int(1, 60).finish());
    let id = Read::new(&granted).unwrap().int(2);
    assert_ne!(id, 0);
    let leased = Write::new().bytes(1, b"/svc/x").bytes(2, b"up").int(3, id).finish();
    one(&service, "/etcdserverpb.KV/Put", leased);
    let alive = one(&service, "/etcdserverpb.Lease/LeaseKeepAlive", Write::new().int(1, id).finish());
    assert_eq!(Read::new(&alive).unwrap().int(3), 60);
    let ttl = one(&service, "/etcdserverpb.Lease/LeaseTimeToLive", Write::new().int(1, id).int(2, 1).finish());
    assert_eq!(Read::new(&ttl).unwrap().repeated(5), vec![&b"/svc/x"[..]]);
    one(&service, "/etcdserverpb.Lease/LeaseRevoke", Write::new().int(1, id).finish());
    let gone = one(&service, "/etcdserverpb.KV/Range", Write::new().bytes(1, b"/svc/x").finish());
    assert!(values(&gone).is_empty());
    let missing = Write::new().bytes(1, b"/svc/y").bytes(2, b"up").int(3, 999).finish();
    assert!(call(&service, "/etcdserverpb.KV/Put", &[missing]).unwrap_err().starts_with("5:"));
    let create = Write::new().bytes(1, b"/svc/").bytes(2, b"/svc0").int(3, 1).finish();
    let watch = Write::new().bytes(1, &create).finish();
    let replies = call(&service, "/etcdserverpb.Watch/Watch", &[watch]).unwrap();
    assert!(Read::new(&replies[0]).unwrap().flag(3));
    let events = Read::new(&replies[1]).unwrap().repeated(11);
    assert_eq!(events.len(), 2);
    assert_eq!(Read::new(events[1]).unwrap().int(1), 1);
    let status = one(&service, "/etcdserverpb.Maintenance/Status", Vec::new());
    assert_eq!(Read::new(&status).unwrap().bytes(2), b"3.5.0");
    assert!(call(&service, "/etcdserverpb.Auth/Authenticate", &[Vec::new()]).unwrap_err().starts_with("12:"));
}

#[test]
fn malformed_messages_are_errors_not_panics() {
    let keyspace = MemoryKeyspace::default();
    let service = Service::new(&keyspace, ServerConfig::default());
    let mut state = 0x1234_5678_9abc_def1u64;
    for path in ["/etcdserverpb.KV/Range", "/etcdserverpb.KV/Put", "/etcdserverpb.KV/Txn", "/etcdserverpb.Watch/Watch"] {
        for _ in 0..200 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bytes: Vec<u8> = state.to_le_bytes().iter().cycle().take((state % 40) as usize).copied().collect();
            let _ = call(&service, path, &[bytes]);
        }
    }
    let deep = (0..20).fold(Vec::new(), |inner, _| {
        Write::new().bytes(2, &Write::new().bytes(4, &inner).finish()).finish()
    });
    assert!(call(&service, "/etcdserverpb.KV/Txn", &[deep]).is_err());
}
