use crate::jute::{Reader, Writer};
use crate::tree::{self, Change, Operation, Outcome, Tree, code};
use eggdb_keyspace::Keyspace;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);
const WRITE_LIMIT: Duration = Duration::from_secs(5);
const EXPIRY_TICK: Duration = Duration::from_millis(500);
const MOST_FRAME_BYTES: usize = (1 << 20) + 64 * 1024;
const PASSWORD_BYTES: usize = 16;
const PING_XID: i32 = -2;
const AUTH_XID: i32 = -4;
const WATCH_XID: i32 = -1;
const SET_WATCHES_XID: i32 = -8;
const SYNC_CONNECTED: i32 = 3;
const MULTI_END: i32 = -1;

mod op {
    pub(super) const CREATE: i32 = 1;
    pub(super) const DELETE: i32 = 2;
    pub(super) const EXISTS: i32 = 3;
    pub(super) const GET_DATA: i32 = 4;
    pub(super) const SET_DATA: i32 = 5;
    pub(super) const GET_ACL: i32 = 6;
    pub(super) const SET_ACL: i32 = 7;
    pub(super) const GET_CHILDREN: i32 = 8;
    pub(super) const SYNC: i32 = 9;
    pub(super) const PING: i32 = 11;
    pub(super) const GET_CHILDREN2: i32 = 12;
    pub(super) const CHECK: i32 = 13;
    pub(super) const MULTI: i32 = 14;
    pub(super) const CREATE2: i32 = 15;
    pub(super) const CREATE_CONTAINER: i32 = 19;
    pub(super) const CREATE_TTL: i32 = 21;
    pub(super) const AUTH: i32 = 100;
    pub(super) const SET_WATCHES: i32 = 101;
    pub(super) const GET_EPHEMERALS: i32 = 103;
    pub(super) const GET_ALL_CHILDREN_NUMBER: i32 = 104;
    pub(super) const CLOSE_SESSION: i32 = -11;
}

mod event {
    pub(super) const CREATED: i32 = 1;
    pub(super) const DELETED: i32 = 2;
    pub(super) const DATA_CHANGED: i32 = 3;
    pub(super) const CHILDREN_CHANGED: i32 = 4;
}

/// How the ZooKeeper server runs.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The most connections served at once.
    pub max_connections: usize,
    /// The shortest session timeout granted, in milliseconds.
    pub least_session_timeout_ms: i32,
    /// The longest session timeout granted, in milliseconds.
    pub most_session_timeout_ms: i32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { max_connections: 1_000, least_session_timeout_ms: 4_000, most_session_timeout_ms: 40_000 }
    }
}

type SharedWriter = Arc<Mutex<TcpStream>>;

struct Session {
    timeout: Duration,
    password: Vec<u8>,
    last_seen: Instant,
    writer: Option<SharedWriter>,
}

#[derive(Default)]
struct Watches {
    data: HashMap<String, HashSet<i64>>,
    children: HashMap<String, HashSet<i64>>,
}

struct Shared<'k> {
    tree: Tree<'k>,
    config: ServerConfig,
    sessions: Mutex<HashMap<i64, Session>>,
    watches: Mutex<Watches>,
    next_session: AtomicI64,
}

fn send(writer: &SharedWriter, bytes: &[u8]) -> io::Result<()> {
    writer.lock().unwrap_or_else(PoisonError::into_inner).write_all(bytes)
}

fn random_bytes(count: usize, seed: i64) -> Vec<u8> {
    use std::hash::{BuildHasher, RandomState};
    let state = RandomState::new();
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    (0..count.div_ceil(8))
        .flat_map(|index| state.hash_one((seed, time, index)).to_le_bytes())
        .take(count)
        .collect()
}

fn acl_bytes(reader: &mut Reader<'_>) -> Option<Vec<u8>> {
    let count = reader.count()?;
    let mut out = Writer::default();
    out.int(i32::try_from(count).unwrap_or(i32::MAX));
    for _ in 0..count {
        out.int(reader.int()?);
        out.string(&reader.string()?);
        out.string(&reader.string()?);
    }
    Some(out.out)
}

fn open_acl() -> Vec<u8> {
    let mut out = Writer::default();
    out.int(1).int(31).string("world").string("anyone");
    out.out
}

impl<'k> Shared<'k> {
    fn zxid(&self) -> i64 {
        tree::read(self.tree.keyspace(), |keys| tree::last_zxid(keys)).unwrap_or(0)
    }

    fn reply(&self, xid: i32, error: i32, body: Option<&Writer>) -> Vec<u8> {
        let mut out = Writer::default();
        out.int(xid).long(self.zxid()).int(error);
        if error == code::OK
            && let Some(body) = body
        {
            out.out.extend_from_slice(&body.out);
        }
        out.framed()
    }

    fn watch(&self, children: bool, path: &str, session: i64) {
        let mut watches = self.watches.lock().unwrap_or_else(PoisonError::into_inner);
        let table = if children { &mut watches.children } else { &mut watches.data };
        table.entry(path.to_owned()).or_default().insert(session);
    }

    fn fire(&self, changes: &[Change]) {
        let mut events: Vec<(i64, i32, String)> = Vec::new();
        {
            let mut watches = self.watches.lock().unwrap_or_else(PoisonError::into_inner);
            for change in changes {
                let (path, kind, data, children) = match change {
                    Change::Created(path) => (path, event::CREATED, true, false),
                    Change::Deleted(path) => (path, event::DELETED, true, true),
                    Change::DataChanged(path) => (path, event::DATA_CHANGED, true, false),
                    Change::ChildrenChanged(path) => (path, event::CHILDREN_CHANGED, false, true),
                };
                let mut sessions = HashSet::new();
                if data {
                    sessions.extend(watches.data.remove(path).unwrap_or_default());
                }
                if children {
                    sessions.extend(watches.children.remove(path).unwrap_or_default());
                }
                events.extend(sessions.into_iter().map(|session| (session, kind, path.clone())));
            }
        }
        let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        for (session, kind, path) in events {
            if let Some(writer) = sessions.get(&session).and_then(|session| session.writer.clone()) {
                let mut out = Writer::default();
                out.int(WATCH_XID).long(-1).int(0).int(kind).int(SYNC_CONNECTED).string(&path);
                let _ = send(&writer, &out.framed());
            }
        }
    }

    fn end_session(&self, session: i64) {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner).remove(&session);
        {
            let mut watches = self.watches.lock().unwrap_or_else(PoisonError::into_inner);
            for held in watches.data.values_mut().chain(watches.children.values_mut()) {
                held.remove(&session);
            }
        }
        self.drop_ephemerals(session);
    }

    fn drop_ephemerals(&self, owner: i64) {
        let paths = tree::read(self.tree.keyspace(), |keys| tree::ephemerals(keys, owner)).unwrap_or_default();
        let mut changes = Vec::new();
        for path in paths {
            if let Ok((_, changed)) = self.tree.run(&[Operation::Delete { path, version: -1 }]) {
                changes.extend(changed);
            }
        }
        self.fire(&changes);
    }

    fn expire_idle(&self) {
        let now = Instant::now();
        let expired: Vec<i64> = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, session)| now.duration_since(session.last_seen) > session.timeout)
            .map(|(id, _)| *id)
            .collect();
        for session in expired {
            self.end_session(session);
        }
    }

    fn alive(&self, session: i64) -> bool {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner).contains_key(&session)
    }

    fn touch(&self, session: i64) {
        if let Some(session) = self.sessions.lock().unwrap_or_else(PoisonError::into_inner).get_mut(&session) {
            session.last_seen = Instant::now();
        }
    }

    fn connect(&self, frame: &[u8], writer: &SharedWriter) -> Option<i64> {
        let mut reader = Reader::new(frame);
        let _protocol = reader.int()?;
        let _last_zxid = reader.long()?;
        let requested = reader.int()?;
        let session_id = reader.long()?;
        let password = reader.buffer()?.unwrap_or_default();
        let timeout = requested.clamp(self.config.least_session_timeout_ms, self.config.most_session_timeout_ms);
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let (id, password) = if session_id != 0 {
            match sessions.get_mut(&session_id) {
                Some(session) if session.password == password => {
                    session.writer = Some(Arc::clone(writer));
                    session.last_seen = Instant::now();
                    (session_id, session.password.clone())
                }
                _ => {
                    drop(sessions);
                    let mut out = Writer::default();
                    out.int(0).int(0).long(0).buffer(Some(&[0u8; PASSWORD_BYTES])).boolean(false);
                    let _ = send(writer, &out.framed());
                    return None;
                }
            }
        } else {
            let id = self.next_session.fetch_add(1, Ordering::Relaxed);
            let password = random_bytes(PASSWORD_BYTES, id);
            sessions.insert(
                id,
                Session {
                    timeout: Duration::from_millis(timeout as u64),
                    password: password.clone(),
                    last_seen: Instant::now(),
                    writer: Some(Arc::clone(writer)),
                },
            );
            (id, password)
        };
        drop(sessions);
        let mut out = Writer::default();
        out.int(0).int(timeout).long(id).buffer(Some(&password)).boolean(false);
        send(writer, &out.framed()).ok()?;
        Some(id)
    }

    fn create(&self, reader: &mut Reader<'_>, kind: i32, session: i64) -> Option<(i32, Writer, Vec<Change>)> {
        let path = reader.string()?;
        let data = reader.buffer()?.unwrap_or_default();
        let acl = acl_bytes(reader)?;
        let flags = reader.int()?;
        if kind == op::CREATE_TTL {
            let _ttl = reader.long()?;
        }
        if !tree::valid_path(&path) && !(flags & 2 == 2 && tree::valid_path(&format!("{path}0"))) {
            return Some((code::BAD_ARGUMENTS, Writer::default(), Vec::new()));
        }
        let ephemeral = matches!(flags, 1 | 3);
        let sequential = matches!(flags, 2 | 3 | 6);
        let operation = Operation::Create {
            path,
            data,
            acl,
            ephemeral_owner: if ephemeral { session } else { 0 },
            sequential,
        };
        let (mut outcomes, changes) = self.tree.run(std::slice::from_ref(&operation)).ok()?;
        let mut body = Writer::default();
        Some(match outcomes.pop()? {
            Ok(Outcome::Created(created, stat)) => {
                body.string(&created);
                if kind != op::CREATE {
                    stat.write(&mut body);
                }
                (code::OK, body, changes)
            }
            Ok(_) => (code::RUNTIME_INCONSISTENCY, body, changes),
            Err(error) => (error, body, Vec::new()),
        })
    }

    fn multi(&self, reader: &mut Reader<'_>, session: i64) -> Option<(Writer, Vec<Change>)> {
        let mut operations = Vec::new();
        let mut kinds = Vec::new();
        loop {
            let kind = reader.int()?;
            let done = reader.boolean()?;
            let _error = reader.int()?;
            if done {
                break;
            }
            let operation = match kind {
                op::CREATE | op::CREATE2 | op::CREATE_CONTAINER | op::CREATE_TTL => {
                    let path = reader.string()?;
                    let data = reader.buffer()?.unwrap_or_default();
                    let acl = acl_bytes(reader)?;
                    let flags = reader.int()?;
                    if kind == op::CREATE_TTL {
                        let _ttl = reader.long()?;
                    }
                    Operation::Create {
                        path,
                        data,
                        acl,
                        ephemeral_owner: if matches!(flags, 1 | 3) { session } else { 0 },
                        sequential: matches!(flags, 2 | 3 | 6),
                    }
                }
                op::DELETE => Operation::Delete { path: reader.string()?, version: reader.int()? },
                op::SET_DATA => Operation::SetData {
                    path: reader.string()?,
                    data: reader.buffer()?.unwrap_or_default(),
                    version: reader.int()?,
                },
                op::CHECK => Operation::Check { path: reader.string()?, version: reader.int()? },
                _ => return None,
            };
            operations.push(operation);
            kinds.push(kind);
        }
        let (outcomes, changes) = self.tree.run(&operations).ok()?;
        let mut body = Writer::default();
        for (kind, outcome) in kinds.into_iter().zip(outcomes) {
            match outcome {
                Ok(Outcome::Created(path, stat)) => {
                    body.int(kind).boolean(false).int(code::OK).string(&path);
                    if kind != op::CREATE {
                        stat.write(&mut body);
                    }
                }
                Ok(Outcome::Stat(stat)) => {
                    body.int(kind).boolean(false).int(code::OK);
                    stat.write(&mut body);
                }
                Ok(Outcome::Done) => {
                    body.int(kind).boolean(false).int(code::OK);
                }
                Err(error) => {
                    body.int(MULTI_END).boolean(false).int(error).int(error);
                }
            }
        }
        body.int(MULTI_END).boolean(true).int(MULTI_END);
        Some((body, changes))
    }

    fn request(&self, frame: &[u8], session: i64, writer: &SharedWriter) -> Option<bool> {
        let mut reader = Reader::new(frame);
        let xid = reader.int()?;
        let kind = reader.int()?;
        let keyspace = self.tree.keyspace();
        let mut body = Writer::default();
        let mut changes = Vec::new();
        let error = match kind {
            op::PING => {
                send(writer, &self.reply(PING_XID, code::OK, None)).ok()?;
                return Some(true);
            }
            op::CLOSE_SESSION => {
                self.end_session(session);
                send(writer, &self.reply(xid, code::OK, None)).ok()?;
                return Some(false);
            }
            op::AUTH => {
                send(writer, &self.reply(AUTH_XID, code::OK, None)).ok()?;
                return Some(true);
            }
            op::SET_WATCHES => {
                let _relative_zxid = reader.long()?;
                for children in [false, false, true] {
                    for _ in 0..reader.count()? {
                        let path = reader.string()?;
                        self.watch(children, &path, session);
                    }
                }
                send(writer, &self.reply(SET_WATCHES_XID, code::OK, None)).ok()?;
                return Some(true);
            }
            op::CREATE | op::CREATE2 | op::CREATE_CONTAINER | op::CREATE_TTL => {
                let (error, created, changed) = self.create(&mut reader, kind, session)?;
                body = created;
                changes = changed;
                error
            }
            op::DELETE => {
                let path = reader.string()?;
                let version = reader.int()?;
                let (mut outcomes, changed) = self.tree.run(&[Operation::Delete { path, version }]).ok()?;
                changes = changed;
                outcomes.pop()?.err().unwrap_or(code::OK)
            }
            op::SET_DATA => {
                let path = reader.string()?;
                let data = reader.buffer()?.unwrap_or_default();
                let version = reader.int()?;
                let (mut outcomes, changed) = self.tree.run(&[Operation::SetData { path, data, version }]).ok()?;
                changes = changed;
                match outcomes.pop()? {
                    Ok(Outcome::Stat(stat)) => {
                        stat.write(&mut body);
                        code::OK
                    }
                    Ok(_) => code::RUNTIME_INCONSISTENCY,
                    Err(error) => error,
                }
            }
            op::SET_ACL => {
                let path = reader.string()?;
                let acl = acl_bytes(&mut reader)?;
                let version = reader.int()?;
                let (mut outcomes, _) = self.tree.run(&[Operation::SetAcl { path, acl, version }]).ok()?;
                match outcomes.pop()? {
                    Ok(Outcome::Stat(stat)) => {
                        stat.write(&mut body);
                        code::OK
                    }
                    Ok(_) => code::RUNTIME_INCONSISTENCY,
                    Err(error) => error,
                }
            }
            op::EXISTS | op::GET_DATA | op::GET_ACL | op::GET_CHILDREN | op::GET_CHILDREN2 | op::GET_ALL_CHILDREN_NUMBER => {
                let path = reader.string()?;
                let watch = if matches!(kind, op::GET_ACL | op::GET_ALL_CHILDREN_NUMBER) { false } else { reader.boolean()? };
                let found = tree::read(keyspace, |keys| {
                    let node = tree::get(keys, &path)?;
                    let children = if node.is_some() && matches!(kind, op::GET_CHILDREN | op::GET_CHILDREN2 | op::GET_ALL_CHILDREN_NUMBER) {
                        tree::children(keys, &path)?
                    } else {
                        Vec::new()
                    };
                    Ok((node, children))
                })
                .ok()?;
                match found {
                    (None, _) => {
                        if watch && kind == op::EXISTS {
                            self.watch(false, &path, session);
                        }
                        code::NO_NODE
                    }
                    (Some(node), children) => {
                        if watch {
                            self.watch(matches!(kind, op::GET_CHILDREN | op::GET_CHILDREN2), &path, session);
                        }
                        match kind {
                            op::EXISTS => node.stat.write(&mut body),
                            op::GET_DATA => {
                                body.buffer(Some(&node.data));
                                node.stat.write(&mut body);
                            }
                            op::GET_ACL => {
                                if node.acl.is_empty() {
                                    body.out.extend_from_slice(&open_acl());
                                } else {
                                    body.out.extend_from_slice(&node.acl);
                                }
                                node.stat.write(&mut body);
                            }
                            op::GET_ALL_CHILDREN_NUMBER => {
                                body.int(i32::try_from(children.len()).unwrap_or(i32::MAX));
                            }
                            _ => {
                                body.int(i32::try_from(children.len()).unwrap_or(i32::MAX));
                                for child in &children {
                                    body.string(child);
                                }
                                if kind == op::GET_CHILDREN2 {
                                    node.stat.write(&mut body);
                                }
                            }
                        }
                        code::OK
                    }
                }
            }
            op::GET_EPHEMERALS => {
                let prefix = reader.string()?;
                let paths = tree::read(keyspace, |keys| tree::ephemerals(keys, session)).ok()?;
                let kept: Vec<&String> = paths.iter().filter(|path| path.starts_with(&prefix)).collect();
                body.int(i32::try_from(kept.len()).unwrap_or(i32::MAX));
                for path in kept {
                    body.string(path);
                }
                code::OK
            }
            op::SYNC => {
                let path = reader.string()?;
                body.string(&path);
                code::OK
            }
            op::CHECK => {
                let path = reader.string()?;
                let version = reader.int()?;
                let (mut outcomes, _) = self.tree.run(&[Operation::Check { path, version }]).ok()?;
                outcomes.pop()?.err().unwrap_or(code::OK)
            }
            op::MULTI => {
                let (written, changed) = self.multi(&mut reader, session)?;
                body = written;
                changes = changed;
                code::OK
            }
            _ => code::UNIMPLEMENTED,
        };
        send(writer, &self.reply(xid, error, Some(&body))).ok()?;
        self.fire(&changes);
        Some(true)
    }
}

fn read_frame(stream: &mut TcpStream, buffer: &mut Vec<u8>, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<Option<Vec<u8>>> {
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if buffer.len() >= 4 {
            let length = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
            if length > MOST_FRAME_BYTES {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "a frame is larger than the jute limit"));
            }
            if buffer.len() >= 4 + length {
                let frame = buffer[4..4 + length].to_vec();
                buffer.drain(..4 + length);
                return Ok(Some(frame));
            }
        }
        if stop() {
            return Ok(None);
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(Some(Vec::new())),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn serve_connection(shared: &Shared<'_>, mut stream: TcpStream, stop: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    let writer: SharedWriter = Arc::new(Mutex::new(stream.try_clone()?));
    let mut buffer = Vec::new();
    let mut session = None;
    let started = Instant::now();
    loop {
        let Some(frame) = read_frame(&mut stream, &mut buffer, stop)? else {
            break;
        };
        if frame.is_empty() {
            if let Some(id) = session
                && !shared.alive(id)
            {
                break;
            }
            if session.is_none() && started.elapsed() > Duration::from_millis(shared.config.most_session_timeout_ms as u64) {
                break;
            }
            continue;
        }
        match session {
            None => match shared.connect(&frame, &writer) {
                Some(id) => session = Some(id),
                None => break,
            },
            Some(id) => {
                if !shared.alive(id) {
                    break;
                }
                shared.touch(id);
                match shared.request(&frame, id, &writer) {
                    Some(true) => {}
                    Some(false) | None => break,
                }
            }
        }
    }
    if let Some(id) = session
        && let Some(held) = shared.sessions.lock().unwrap_or_else(PoisonError::into_inner).get_mut(&id)
        && held.writer.as_ref().is_some_and(|held| Arc::ptr_eq(held, &writer))
    {
        held.writer = None;
    }
    Ok(())
}

/// Serves ZooKeeper clients on `listener` until `stop` turns true, keeping znodes in
/// `keyspace`. Ephemeral znodes a previous server process left are removed first, as their
/// sessions did not survive it.
pub fn serve(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let first_session = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |since| i64::try_from(since.as_millis()).unwrap_or(1))
        << 16;
    let shared = Shared {
        tree: Tree::new(keyspace),
        config: config.clone(),
        sessions: Mutex::new(HashMap::new()),
        watches: Mutex::new(Watches::default()),
        next_session: AtomicI64::new(first_session),
    };
    for owner in tree::read(keyspace, |keys| tree::ephemeral_owners(keys)).unwrap_or_default() {
        shared.drop_ephemerals(owner);
    }
    let active = AtomicUsize::new(0);
    let shared = &shared;
    std::thread::scope(|scope| {
        let _ = std::thread::Builder::new().name("eggdb-zookeeper-sessions".to_owned()).spawn_scoped(scope, move || {
            while !stop() {
                std::thread::sleep(EXPIRY_TICK);
                shared.expire_idle();
            }
        });
        while !stop() {
            let stream = match eggdb_platform::accept_tcp(listener) {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err()
                || stream.set_read_timeout(Some(READ_POLL)).is_err()
                || stream.set_write_timeout(Some(WRITE_LIMIT)).is_err()
                || stream.set_nodelay(true).is_err()
            {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let active = &active;
            let spawned = std::thread::Builder::new().name("eggdb-zookeeper".to_owned()).spawn_scoped(scope, move || {
                let _ = serve_connection(shared, stream, stop);
                active.fetch_sub(1, Ordering::AcqRel);
            });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(())
}
