//! EggDB's offline-first sync (D228). A [`Replica`] keeps its rows and an append-only change log
//! in EggDB's key-value transactions, so an embedded database on a phone or laptop takes writes
//! while it is disconnected and keeps them across restarts. [`Replica::changes_after`] reads the
//! log from a cursor, [`Replica::apply`] takes in another replica's changes, and [`sync`] does
//! both ways between a client and a server. Every change carries a hybrid-clock [`Stamp`]; for
//! each key the change with the highest stamp wins on every replica, a delete included, so
//! replicas that have exchanged all their changes hold the same rows whatever order they synced
//! in. Changes travel as bytes ([`encode`], [`decode`]) for any transport.

use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};

const ROW: u8 = b'r';
const LOG: u8 = b'l';
const META: &[u8] = b"m";
const MAX_CHANGES: usize = 1 << 20;

/// A hybrid logical clock stamp: physical milliseconds, a counter, and the replica that wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Stamp {
    /// Milliseconds since 1970, never behind a stamp the replica has seen.
    pub millis: u64,
    /// Events within one millisecond.
    pub counter: u32,
    /// The replica that made it.
    pub replica: u64,
}

impl Stamp {
    fn encode(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.millis.to_be_bytes());
        out.extend_from_slice(&self.counter.to_be_bytes());
        out.extend_from_slice(&self.replica.to_be_bytes());
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        Some(Self {
            millis: u64::from_be_bytes(bytes.get(..8)?.try_into().ok()?),
            counter: u32::from_be_bytes(bytes.get(8..12)?.try_into().ok()?),
            replica: u64::from_be_bytes(bytes.get(12..20)?.try_into().ok()?),
        })
    }
}

/// One write: the key, its new value (`None` deletes it) and its stamp.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Change {
    /// The key.
    pub key: Vec<u8>,
    /// The value, or `None` for a deletion.
    pub value: Option<Vec<u8>>,
    /// When and where it was written.
    pub stamp: Stamp,
}

/// Why sync failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// The key-value store failed.
    Storage(String),
    /// Bytes received are not a valid list of changes.
    Malformed(&'static str),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Storage(message) => f.write_str(message),
            SyncError::Malformed(what) => write!(f, "the changes received are damaged: {what}"),
        }
    }
}

impl std::error::Error for SyncError {}

fn storage(message: String) -> SyncError {
    SyncError::Storage(message)
}

/// Appends `changes` to `out` as bytes: a count, then each change's stamp, key and value.
pub fn encode(changes: &[Change], out: &mut Vec<u8>) {
    out.extend_from_slice(&u32::try_from(changes.len()).unwrap_or(u32::MAX).to_be_bytes());
    for change in changes {
        change.stamp.encode(out);
        out.extend_from_slice(&u32::try_from(change.key.len()).unwrap_or(u32::MAX).to_be_bytes());
        out.extend_from_slice(&change.key);
        match &change.value {
            Some(value) => {
                out.push(1);
                out.extend_from_slice(&u32::try_from(value.len()).unwrap_or(u32::MAX).to_be_bytes());
                out.extend_from_slice(value);
            }
            None => out.push(0),
        }
    }
}

struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn take(&mut self, count: usize) -> Result<&'b [u8], SyncError> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or(SyncError::Malformed("cut short"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn length(&mut self) -> Result<usize, SyncError> {
        let raw = self.take(4)?;
        usize::try_from(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]])).map_err(|_| SyncError::Malformed("a length does not fit"))
    }
}

/// The changes in `bytes`, as [`encode`] wrote them.
pub fn decode(bytes: &[u8]) -> Result<Vec<Change>, SyncError> {
    let mut reader = Reader { bytes, at: 0 };
    let count = reader.length()?;
    if count > MAX_CHANGES || count > bytes.len() {
        return Err(SyncError::Malformed("too many changes"));
    }
    let mut changes = Vec::with_capacity(count);
    for _ in 0..count {
        let stamp = Stamp::decode(reader.take(20)?).ok_or(SyncError::Malformed("a stamp is cut short"))?;
        let key_length = reader.length()?;
        let key = reader.take(key_length)?.to_vec();
        let value = match reader.take(1)?[0] {
            0 => None,
            1 => {
                let length = reader.length()?;
                Some(reader.take(length)?.to_vec())
            }
            _ => return Err(SyncError::Malformed("a value marker is neither 0 nor 1")),
        };
        changes.push(Change { key, value, stamp });
    }
    if reader.at != bytes.len() {
        return Err(SyncError::Malformed("bytes follow the last change"));
    }
    Ok(changes)
}

fn row_key(key: &[u8]) -> Vec<u8> {
    let mut stored = vec![ROW];
    stored.extend_from_slice(key);
    stored
}

fn log_key(position: u64) -> Vec<u8> {
    let mut stored = vec![LOG];
    stored.extend_from_slice(&position.to_be_bytes());
    stored
}

fn row_value(change: &Change) -> Vec<u8> {
    let mut out = Vec::new();
    change.stamp.encode(&mut out);
    if let Some(value) = &change.value {
        out.push(1);
        out.extend_from_slice(value);
    } else {
        out.push(0);
    }
    out
}

fn decode_row(key: &[u8], bytes: &[u8]) -> Result<Change, SyncError> {
    let stamp = Stamp::decode(bytes).ok_or_else(|| storage("a stored row is damaged".to_owned()))?;
    let value = match bytes.get(20) {
        Some(1) => Some(bytes[21..].to_vec()),
        Some(0) => None,
        _ => return Err(storage("a stored row is damaged".to_owned())),
    };
    Ok(Change { key: key.to_vec(), value, stamp })
}

#[derive(Clone, Copy, Default)]
struct Meta {
    clock: Stamp,
    next_position: u64,
}

impl Meta {
    fn read(keys: &dyn KeysRead) -> Result<Self, SyncError> {
        let Some(bytes) = keys.get(META).map_err(storage)? else {
            return Ok(Self::default());
        };
        let clock = Stamp::decode(&bytes).ok_or_else(|| storage("the sync state is damaged".to_owned()))?;
        let next_position = u64::from_be_bytes(bytes.get(20..28).and_then(|raw| raw.try_into().ok()).ok_or_else(|| storage("the sync state is damaged".to_owned()))?);
        Ok(Self { clock, next_position })
    }

    fn write(self, keys: &mut dyn KeysWrite) -> Result<(), SyncError> {
        let mut bytes = Vec::with_capacity(28);
        self.clock.encode(&mut bytes);
        bytes.extend_from_slice(&self.next_position.to_be_bytes());
        keys.put(META, &bytes).map_err(storage)
    }
}

/// One replica's synced rows and change log, kept in database `db` of a keyspace.
pub struct Replica<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
    db: u32,
    id: u64,
}

impl std::fmt::Debug for Replica<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replica").field("id", &self.id).field("db", &self.db).finish_non_exhaustive()
    }
}

fn store_if_newer(keys: &mut dyn KeysWrite, meta: &mut Meta, change: &Change) -> Result<bool, SyncError> {
    if let Some(held) = keys.get(&row_key(&change.key)).map_err(storage)? {
        if decode_row(&change.key, &held)?.stamp >= change.stamp {
            return Ok(false);
        }
    }
    keys.put(&row_key(&change.key), &row_value(change)).map_err(storage)?;
    let mut entry = Vec::new();
    encode(std::slice::from_ref(change), &mut entry);
    keys.put(&log_key(meta.next_position), &entry).map_err(storage)?;
    meta.next_position += 1;
    if change.stamp > meta.clock {
        meta.clock = Stamp { replica: meta.clock.replica, ..change.stamp };
    }
    Ok(true)
}

impl<'k> Replica<'k> {
    /// The replica `id` (unique among those that sync together) stored in database `db` of
    /// `keyspace`.
    pub fn new(keyspace: &'k (dyn Keyspace + Sync), db: u32, id: u64) -> Self {
        Self { keyspace, db, id }
    }

    fn transact<T>(&self, body: impl FnOnce(&mut dyn KeysWrite) -> Result<T, SyncError>) -> Result<T, SyncError> {
        let mut body = Some(body);
        let mut out = None;
        let result = self.keyspace.write(self.db, &mut |keys| {
            let body = body.take().ok_or("the write ran twice")?;
            let outcome = body(keys);
            let failed = outcome.is_err();
            out = Some(outcome);
            if failed { Err("the sync write was rolled back".to_owned()) } else { Ok(()) }
        });
        match (result, out) {
            (_, Some(Err(error))) => Err(error),
            (Err(message), _) => Err(storage(message)),
            (Ok(()), Some(outcome)) => outcome,
            (Ok(()), None) => Err(storage("the write did not run".to_owned())),
        }
    }

    fn read<T>(&self, body: impl FnOnce(&dyn KeysRead) -> Result<T, SyncError>) -> Result<T, SyncError> {
        let mut body = Some(body);
        let mut out = None;
        self.keyspace
            .read(self.db, &mut |keys| {
                if let Some(body) = body.take() {
                    out = Some(body(keys));
                }
                Ok(())
            })
            .map_err(storage)?;
        out.unwrap_or_else(|| Err(storage("the read did not run".to_owned())))
    }

    /// Writes `value` (`None` deletes) to `key` locally at `now_ms`, connected or not.
    pub fn write(&self, key: &[u8], value: Option<&[u8]>, now_ms: u64) -> Result<Stamp, SyncError> {
        let id = self.id;
        self.transact(|keys| {
            let mut meta = Meta::read(&*keys)?;
            let stamp = if now_ms > meta.clock.millis {
                Stamp { millis: now_ms, counter: 0, replica: id }
            } else {
                Stamp { millis: meta.clock.millis, counter: meta.clock.counter.saturating_add(1), replica: id }
            };
            meta.clock = stamp;
            let change = Change { key: key.to_vec(), value: value.map(<[u8]>::to_vec), stamp };
            store_if_newer(keys, &mut meta, &change)?;
            meta.write(keys)?;
            Ok(stamp)
        })
    }

    /// The value of `key`, `None` when it was never written or was deleted.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, SyncError> {
        self.read(|keys| match keys.get(&row_key(key)).map_err(storage)? {
            Some(bytes) => Ok(decode_row(key, &bytes)?.value),
            None => Ok(None),
        })
    }

    /// Every live key and value, in key order.
    pub fn rows(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>, SyncError> {
        self.read(|keys| {
            let mut rows = Vec::new();
            for (stored, bytes) in keys.scan(&[ROW]).map_err(storage)? {
                if let Some(value) = decode_row(&stored[1..], &bytes)?.value {
                    rows.push((stored[1..].to_vec(), value));
                }
            }
            Ok(rows)
        })
    }

    /// The changes logged at or after `cursor`, at most `limit`, and the cursor after them.
    pub fn changes_after(&self, cursor: u64, limit: usize) -> Result<(Vec<Change>, u64), SyncError> {
        self.read(|keys| {
            let meta = Meta::read(keys)?;
            let mut changes = Vec::new();
            let mut position = cursor;
            while position < meta.next_position && changes.len() < limit {
                let entry = keys.get(&log_key(position)).map_err(storage)?.ok_or_else(|| storage("the change log has a gap".to_owned()))?;
                changes.extend(decode(&entry)?);
                position += 1;
            }
            Ok((changes, position))
        })
    }

    /// Takes in another replica's changes in one transaction; returns how many changed a row.
    pub fn apply(&self, changes: &[Change]) -> Result<usize, SyncError> {
        self.transact(|keys| {
            let mut meta = Meta::read(&*keys)?;
            let mut applied = 0;
            for change in changes {
                if store_if_newer(keys, &mut meta, change)? {
                    applied += 1;
                }
            }
            meta.write(keys)?;
            Ok(applied)
        })
    }
}

/// Where a client stands with one server: how far it has sent its own log, and how far it has
/// received the server's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursors {
    /// The client's log position up to which the server has its changes.
    pub sent: u64,
    /// The server's log position up to which the client has its changes.
    pub received: u64,
}

/// Exchanges changes both ways between `client` and `server`, at most `batch` per direction,
/// moving `cursors` past what each side took in; returns how many rows changed on each side.
pub fn sync(client: &Replica<'_>, server: &Replica<'_>, cursors: &mut Cursors, batch: usize) -> Result<(usize, usize), SyncError> {
    let (outgoing, sent) = client.changes_after(cursors.sent, batch)?;
    let mut wire = Vec::new();
    encode(&outgoing, &mut wire);
    let on_server = server.apply(&decode(&wire)?)?;
    cursors.sent = sent;
    let (incoming, received) = server.changes_after(cursors.received, batch)?;
    wire.clear();
    encode(&incoming, &mut wire);
    let on_client = client.apply(&decode(&wire)?)?;
    cursors.received = received;
    Ok((on_server, on_client))
}

#[cfg(test)]
mod tests;
