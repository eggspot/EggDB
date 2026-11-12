use crate::jute::{Reader, Writer};
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};

const NODE: u8 = b'n';
const CHILD: u8 = b'c';
const EPHEMERAL: u8 = b'e';
const ZXID: &[u8] = b"\x00zxid";
const MOST_PATH_BYTES: usize = 4096;
const MOST_DATA_BYTES: usize = 1 << 20;
const MOST_CHILDREN: usize = 1 << 20;
const ROLLED_BACK: &str = "a ZooKeeper operation failed, so the transaction rolled back";

/// ZooKeeper's error codes, as replies carry them.
pub(crate) mod code {
    pub(crate) const OK: i32 = 0;
    pub(crate) const RUNTIME_INCONSISTENCY: i32 = -2;
    pub(crate) const MARSHALLING_ERROR: i32 = -5;
    pub(crate) const UNIMPLEMENTED: i32 = -6;
    pub(crate) const BAD_ARGUMENTS: i32 = -8;
    pub(crate) const NO_NODE: i32 = -101;
    pub(crate) const BAD_VERSION: i32 = -103;
    pub(crate) const NO_CHILDREN_FOR_EPHEMERALS: i32 = -108;
    pub(crate) const NODE_EXISTS: i32 = -110;
    pub(crate) const NOT_EMPTY: i32 = -111;
}

/// A znode's metadata, as `Stat` gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Stat {
    pub(crate) czxid: i64,
    pub(crate) mzxid: i64,
    pub(crate) ctime: i64,
    pub(crate) mtime: i64,
    pub(crate) version: i32,
    pub(crate) cversion: i32,
    pub(crate) aversion: i32,
    pub(crate) ephemeral_owner: i64,
    pub(crate) data_length: i32,
    pub(crate) children: i32,
    pub(crate) pzxid: i64,
}

impl Stat {
    pub(crate) fn write(&self, out: &mut Writer) {
        out.long(self.czxid)
            .long(self.mzxid)
            .long(self.ctime)
            .long(self.mtime)
            .int(self.version)
            .int(self.cversion)
            .int(self.aversion)
            .long(self.ephemeral_owner)
            .int(self.data_length)
            .int(self.children)
            .long(self.pzxid);
    }
}

/// A stored znode: its data, its ACL as the client sent it, and its metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Znode {
    pub(crate) data: Vec<u8>,
    pub(crate) acl: Vec<u8>,
    pub(crate) stat: Stat,
}

impl Znode {
    fn encode(&self) -> Vec<u8> {
        let mut out = Writer::default();
        self.stat.write(&mut out);
        out.buffer(Some(&self.acl)).buffer(Some(&self.data));
        out.out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader::new(bytes);
        let stat = Stat {
            czxid: reader.long()?,
            mzxid: reader.long()?,
            ctime: reader.long()?,
            mtime: reader.long()?,
            version: reader.int()?,
            cversion: reader.int()?,
            aversion: reader.int()?,
            ephemeral_owner: reader.long()?,
            data_length: reader.int()?,
            children: reader.int()?,
            pzxid: reader.long()?,
        };
        let acl = reader.buffer()?.unwrap_or_default();
        let data = reader.buffer()?.unwrap_or_default();
        Some(Self { data, acl, stat })
    }
}

fn node_key(path: &str) -> Vec<u8> {
    let mut key = vec![NODE];
    key.extend_from_slice(path.as_bytes());
    key
}

fn child_prefix(parent: &str) -> Vec<u8> {
    let mut key = vec![CHILD];
    key.extend_from_slice(parent.as_bytes());
    key.push(0);
    key
}

fn ephemeral_prefix(owner: i64) -> Vec<u8> {
    let mut key = vec![EPHEMERAL];
    key.extend_from_slice(&owner.to_be_bytes());
    key
}

fn ephemeral_key(owner: i64, path: &str) -> Vec<u8> {
    let mut key = ephemeral_prefix(owner);
    key.extend_from_slice(path.as_bytes());
    key
}

/// The paths of the ephemeral znodes `owner` holds.
pub(crate) fn ephemerals(keys: &dyn KeysRead, owner: i64) -> Result<Vec<String>, String> {
    let prefix = ephemeral_prefix(owner);
    Ok(keys
        .scan(&prefix)?
        .into_iter()
        .filter_map(|(key, _)| String::from_utf8(key.get(prefix.len()..)?.to_vec()).ok())
        .collect())
}

/// The owners of every ephemeral znode, for clearing those a previous server process left.
pub(crate) fn ephemeral_owners(keys: &dyn KeysRead) -> Result<Vec<i64>, String> {
    let mut owners: Vec<i64> = keys
        .scan(&[EPHEMERAL])?
        .into_iter()
        .filter_map(|(key, _)| Some(i64::from_be_bytes(key.get(1..9)?.try_into().ok()?)))
        .collect();
    owners.dedup();
    Ok(owners)
}

fn child_key(parent: &str, name: &str) -> Vec<u8> {
    let mut key = child_prefix(parent);
    key.extend_from_slice(name.as_bytes());
    key
}

/// Whether `path` is a valid znode path: absolute, no empty, `.` or `..` parts, no trailing
/// slash except the root, and no control characters.
pub(crate) fn valid_path(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    path.starts_with('/')
        && !path.ends_with('/')
        && path.len() <= MOST_PATH_BYTES
        && path[1..].split('/').all(|part| !part.is_empty() && part != "." && part != "..")
        && !path.chars().any(|character| character.is_control())
}

/// The parent of `path` and the last part's name; `None` for the root.
pub(crate) fn split(path: &str) -> Option<(&str, &str)> {
    if path == "/" {
        return None;
    }
    let at = path.rfind('/')?;
    Some((if at == 0 { "/" } else { &path[..at] }, &path[at + 1..]))
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

/// What a write changed, for the watches it fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Change {
    Created(String),
    Deleted(String),
    DataChanged(String),
    ChildrenChanged(String),
}

/// The znode tree in a keyspace.
pub(crate) struct Tree<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
}

pub(crate) fn read<T>(keyspace: &dyn Keyspace, body: impl FnOnce(&dyn KeysRead) -> Result<T, String>) -> Result<T, String> {
    let mut body = Some(body);
    let mut out = None;
    keyspace.read(0, &mut |keys| {
        if let Some(body) = body.take() {
            out = Some(body(keys)?);
        }
        Ok(())
    })?;
    out.ok_or_else(|| "the read did not run".to_owned())
}

pub(crate) fn write<T>(keyspace: &dyn Keyspace, body: impl FnOnce(&mut dyn KeysWrite) -> Result<T, String>) -> Result<T, String> {
    let mut body = Some(body);
    let mut out = None;
    keyspace.write(0, &mut |keys| {
        if let Some(body) = body.take() {
            out = Some(body(keys)?);
        }
        Ok(())
    })?;
    out.ok_or_else(|| "the write did not run".to_owned())
}

/// One write of a `multi` or a single request, run inside a write transaction.
pub(crate) enum Operation {
    Create { path: String, data: Vec<u8>, acl: Vec<u8>, ephemeral_owner: i64, sequential: bool },
    Delete { path: String, version: i32 },
    SetData { path: String, data: Vec<u8>, version: i32 },
    SetAcl { path: String, acl: Vec<u8>, version: i32 },
    Check { path: String, version: i32 },
}

/// What one operation gives back: the created path, a `Stat`, or nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Created(String, Stat),
    Stat(Stat),
    Done,
}

pub(crate) fn get(keys: &dyn KeysRead, path: &str) -> Result<Option<Znode>, String> {
    if path == "/" {
        let mut root = keys.get(&node_key("/"))?.and_then(|raw| Znode::decode(&raw)).unwrap_or_default();
        root.stat.children = i32::try_from(children(keys, "/")?.len()).unwrap_or(i32::MAX);
        return Ok(Some(root));
    }
    Ok(keys.get(&node_key(path))?.and_then(|raw| Znode::decode(&raw)))
}

pub(crate) fn children(keys: &dyn KeysRead, path: &str) -> Result<Vec<String>, String> {
    let prefix = child_prefix(path);
    Ok(keys
        .scan(&prefix)?
        .into_iter()
        .take(MOST_CHILDREN)
        .filter_map(|(key, _)| String::from_utf8(key.get(prefix.len()..)?.to_vec()).ok())
        .collect())
}

fn next_zxid(keys: &mut dyn KeysWrite) -> Result<i64, String> {
    let current = keys.get(ZXID)?.and_then(|raw| raw.try_into().ok()).map_or(0, i64::from_be_bytes);
    keys.put(ZXID, &(current + 1).to_be_bytes())?;
    Ok(current + 1)
}

/// The last transaction id written, for replies.
pub(crate) fn last_zxid(keys: &dyn KeysRead) -> Result<i64, String> {
    Ok(keys.get(ZXID)?.and_then(|raw| raw.try_into().ok()).map_or(0, i64::from_be_bytes))
}

fn put(keys: &mut dyn KeysWrite, path: &str, node: &Znode) -> Result<(), String> {
    if path == "/" {
        let mut stored = node.clone();
        stored.stat.children = 0;
        return keys.put(&node_key("/"), &stored.encode());
    }
    keys.put(&node_key(path), &node.encode())
}

fn version_matches(expected: i32, actual: i32) -> bool {
    expected == -1 || expected == actual
}

/// Applies one operation; `Err(code)` is a ZooKeeper error that leaves the transaction to be
/// rolled back by the caller when it is part of a `multi`.
pub(crate) fn apply(keys: &mut dyn KeysWrite, operation: &Operation, changes: &mut Vec<Change>) -> Result<Result<Outcome, i32>, String> {
    Ok(match operation {
        Operation::Create { path, data, acl, ephemeral_owner, sequential } => {
            if data.len() > MOST_DATA_BYTES {
                return Ok(Err(code::BAD_ARGUMENTS));
            }
            let Some((parent_path, _)) = split(path) else {
                return Ok(Err(code::NODE_EXISTS));
            };
            let Some(mut parent) = get(&*keys, parent_path)? else {
                return Ok(Err(code::NO_NODE));
            };
            if parent.stat.ephemeral_owner != 0 {
                return Ok(Err(code::NO_CHILDREN_FOR_EPHEMERALS));
            }
            let path = if *sequential { format!("{path}{:010}", parent.stat.cversion.max(0)) } else { path.clone() };
            if !valid_path(&path) {
                return Ok(Err(code::BAD_ARGUMENTS));
            }
            if get(&*keys, &path)?.is_some() {
                return Ok(Err(code::NODE_EXISTS));
            }
            let zxid = next_zxid(keys)?;
            let now = now_millis();
            let node = Znode {
                data: data.clone(),
                acl: acl.clone(),
                stat: Stat {
                    czxid: zxid,
                    mzxid: zxid,
                    ctime: now,
                    mtime: now,
                    ephemeral_owner: *ephemeral_owner,
                    data_length: i32::try_from(data.len()).unwrap_or(i32::MAX),
                    pzxid: zxid,
                    ..Stat::default()
                },
            };
            put(keys, &path, &node)?;
            if *ephemeral_owner != 0 {
                keys.put(&ephemeral_key(*ephemeral_owner, &path), &[])?;
            }
            let name = split(&path).map_or("", |(_, name)| name).to_owned();
            keys.put(&child_key(parent_path, &name), &[])?;
            parent.stat.cversion += 1;
            parent.stat.pzxid = zxid;
            parent.stat.children += 1;
            put(keys, parent_path, &parent)?;
            changes.push(Change::Created(path.clone()));
            changes.push(Change::ChildrenChanged(parent_path.to_owned()));
            Outcome::Created(path, node.stat)
        }
        Operation::Delete { path, version } => {
            let Some((parent_path, name)) = split(path) else {
                return Ok(Err(code::BAD_ARGUMENTS));
            };
            let Some(node) = get(&*keys, path)? else {
                return Ok(Err(code::NO_NODE));
            };
            if !version_matches(*version, node.stat.version) {
                return Ok(Err(code::BAD_VERSION));
            }
            if node.stat.children > 0 {
                return Ok(Err(code::NOT_EMPTY));
            }
            let zxid = next_zxid(keys)?;
            keys.delete(&node_key(path))?;
            keys.delete(&child_key(parent_path, name))?;
            if node.stat.ephemeral_owner != 0 {
                keys.delete(&ephemeral_key(node.stat.ephemeral_owner, path))?;
            }
            if let Some(mut parent) = get(&*keys, parent_path)? {
                parent.stat.cversion += 1;
                parent.stat.pzxid = zxid;
                parent.stat.children = (parent.stat.children - 1).max(0);
                put(keys, parent_path, &parent)?;
            }
            changes.push(Change::Deleted(path.clone()));
            changes.push(Change::ChildrenChanged(parent_path.to_owned()));
            Outcome::Done
        }
        Operation::SetData { path, data, version } => {
            if data.len() > MOST_DATA_BYTES {
                return Ok(Err(code::BAD_ARGUMENTS));
            }
            let Some(mut node) = get(&*keys, path)? else {
                return Ok(Err(code::NO_NODE));
            };
            if !version_matches(*version, node.stat.version) {
                return Ok(Err(code::BAD_VERSION));
            }
            node.stat.mzxid = next_zxid(keys)?;
            node.stat.mtime = now_millis();
            node.stat.version += 1;
            node.stat.data_length = i32::try_from(data.len()).unwrap_or(i32::MAX);
            node.data = data.clone();
            put(keys, path, &node)?;
            changes.push(Change::DataChanged(path.clone()));
            Outcome::Stat(node.stat)
        }
        Operation::SetAcl { path, acl, version } => {
            let Some(mut node) = get(&*keys, path)? else {
                return Ok(Err(code::NO_NODE));
            };
            if !version_matches(*version, node.stat.aversion) {
                return Ok(Err(code::BAD_VERSION));
            }
            node.stat.aversion += 1;
            node.acl = acl.clone();
            put(keys, path, &node)?;
            Outcome::Stat(node.stat)
        }
        Operation::Check { path, version } => {
            let Some(node) = get(&*keys, path)? else {
                return Ok(Err(code::NO_NODE));
            };
            if !version_matches(*version, node.stat.version) {
                return Ok(Err(code::BAD_VERSION));
            }
            Outcome::Done
        }
    })
}

impl<'k> Tree<'k> {
    pub(crate) fn new(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self { keyspace }
    }

    pub(crate) fn keyspace(&self) -> &'k (dyn Keyspace + Sync) {
        self.keyspace
    }

    /// Runs operations as one transaction: all of them, or (on the first ZooKeeper error) none.
    /// When one fails, it reports its error, those before it `OK` and those after it
    /// `RUNTIMEINCONSISTENCY`, as ZooKeeper's `multi` does, and nothing changes.
    pub(crate) fn run(&self, operations: &[Operation]) -> Result<(Vec<Result<Outcome, i32>>, Vec<Change>), String> {
        let mut failure: Option<(usize, i32)> = None;
        let attempt = write(self.keyspace, |keys| {
            let mut outcomes = Vec::with_capacity(operations.len());
            let mut changes = Vec::new();
            for (index, operation) in operations.iter().enumerate() {
                match apply(keys, operation, &mut changes)? {
                    Ok(outcome) => outcomes.push(Ok(outcome)),
                    Err(error) => {
                        failure = Some((index, error));
                        return Err(ROLLED_BACK.to_owned());
                    }
                }
            }
            Ok((outcomes, changes))
        });
        match (attempt, failure) {
            (Ok(done), _) => Ok(done),
            (Err(_), Some((failed, error))) => Ok((
                (0..operations.len())
                    .map(|index| match index.cmp(&failed) {
                        std::cmp::Ordering::Less => Err(code::OK),
                        std::cmp::Ordering::Equal => Err(error),
                        std::cmp::Ordering::Greater => Err(code::RUNTIME_INCONSISTENCY),
                    })
                    .collect(),
                Vec::new(),
            )),
            (Err(message), None) => Err(message),
        }
    }
}
