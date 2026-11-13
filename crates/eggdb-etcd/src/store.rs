//! etcd's data model over an EggDB keyspace: every write raises one store-wide revision, each
//! key keeps its create and modify revisions and version, every change is logged by revision
//! for reads at a past revision and for watches until it is compacted, and leases expire with
//! their keys. Every request runs as one keyspace transaction.

use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace, keys_after_prefix};
use std::collections::BTreeMap;

const DATABASE: u32 = 0;
const REVISION_KEY: &[u8] = b"\x00revision";
const COMPACTED_KEY: &[u8] = b"\x00compacted";
const NEXT_LEASE_KEY: &[u8] = b"\x00next-lease";
const CURRENT: u8 = b'k';
const HISTORY: u8 = b'h';
const LEASE: u8 = b'l';
const LEASE_KEY: u8 = b'm';
const PUT: u8 = 0;
const DELETE: u8 = 1;
const MOST_KEY_BYTES: usize = 1 << 16;
const MOST_VALUE_BYTES: usize = 3 << 20;

/// Why a request failed, with the gRPC status it maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EtcdError {
    Invalid(String),
    Compacted(i64),
    FutureRevision,
    LeaseMissing,
    Storage(String),
}

impl EtcdError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::Invalid(message) | Self::Storage(message) => message.clone(),
            Self::Compacted(_) => "etcdserver: mvcc: required revision has been compacted".to_owned(),
            Self::FutureRevision => "etcdserver: mvcc: required revision is a future revision".to_owned(),
            Self::LeaseMissing => "etcdserver: requested lease not found".to_owned(),
        }
    }
}

/// A key with its value and revisions, as `mvccpb.KeyValue` carries it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct KeyValue {
    pub(crate) key: Vec<u8>,
    pub(crate) create_revision: i64,
    pub(crate) mod_revision: i64,
    pub(crate) version: i64,
    pub(crate) value: Vec<u8>,
    pub(crate) lease: i64,
}

/// One change, as a watch reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Event {
    pub(crate) deleted: bool,
    pub(crate) kv: KeyValue,
}

/// Which keys a request names: one key, the keys from `key` up to `end`, or every key from
/// `key` on when `end` is the single byte 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyRange {
    pub(crate) key: Vec<u8>,
    pub(crate) end: Vec<u8>,
}

impl KeyRange {
    fn contains(&self, key: &[u8]) -> bool {
        if self.end.is_empty() {
            return key == self.key.as_slice();
        }
        key >= self.key.as_slice() && (self.end == [0] || key < self.end.as_slice())
    }
}

fn number(bytes: &[u8]) -> i64 {
    bytes.get(..8).and_then(|bytes| bytes.try_into().ok()).map_or(0, i64::from_be_bytes)
}

fn encode_record(kv: &KeyValue) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + kv.value.len());
    for value in [kv.create_revision, kv.mod_revision, kv.version, kv.lease] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.extend_from_slice(&kv.value);
    out
}

fn decode_record(key: &[u8], bytes: &[u8]) -> Result<KeyValue, EtcdError> {
    if bytes.len() < 32 {
        return Err(EtcdError::Storage("a stored etcd key is damaged".to_owned()));
    }
    Ok(KeyValue {
        key: key.to_vec(),
        create_revision: number(&bytes[0..8]),
        mod_revision: number(&bytes[8..16]),
        version: number(&bytes[16..24]),
        lease: number(&bytes[24..32]),
        value: bytes[32..].to_vec(),
    })
}

fn current_key(key: &[u8]) -> Vec<u8> {
    let mut out = vec![CURRENT];
    out.extend_from_slice(key);
    out
}

fn history_key(revision: i64, sub: u32) -> Vec<u8> {
    let mut out = vec![HISTORY];
    out.extend_from_slice(&revision.to_be_bytes());
    out.extend_from_slice(&sub.to_be_bytes());
    out
}

fn encode_event(event: &Event) -> Vec<u8> {
    let mut out = vec![if event.deleted { DELETE } else { PUT }];
    out.extend_from_slice(&(event.kv.key.len() as u32).to_be_bytes());
    out.extend_from_slice(&event.kv.key);
    out.extend_from_slice(&encode_record(&event.kv));
    out
}

fn decode_event(bytes: &[u8]) -> Result<Event, EtcdError> {
    let damaged = || EtcdError::Storage("a logged etcd change is damaged".to_owned());
    let (&kind, rest) = bytes.split_first().ok_or_else(damaged)?;
    let (length, rest) = rest.split_first_chunk::<4>().ok_or_else(damaged)?;
    let (key, record) = rest.split_at_checked(u32::from_be_bytes(*length) as usize).ok_or_else(damaged)?;
    Ok(Event { deleted: kind == DELETE, kv: decode_record(key, record)? })
}

fn lease_key(id: i64) -> Vec<u8> {
    let mut out = vec![LEASE];
    out.extend_from_slice(&id.to_be_bytes());
    out
}

fn lease_member_key(id: i64, key: &[u8]) -> Vec<u8> {
    let mut out = vec![LEASE_KEY];
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(key);
    out
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

fn storage(message: String) -> EtcdError {
    EtcdError::Storage(message)
}

/// What a range read found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct RangeResult {
    pub(crate) kvs: Vec<KeyValue>,
    pub(crate) more: bool,
    pub(crate) count: i64,
}

/// A range read's options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RangeOptions {
    pub(crate) limit: i64,
    pub(crate) revision: i64,
    pub(crate) keys_only: bool,
    pub(crate) count_only: bool,
}

/// A comparison of a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Compare {
    pub(crate) result: i64,
    pub(crate) target: i64,
    pub(crate) key: Vec<u8>,
    pub(crate) range_end: Vec<u8>,
    pub(crate) operand: CompareOperand,
}

/// What a comparison compares against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompareOperand {
    Number(i64),
    Value(Vec<u8>),
}

/// One operation of a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Operation {
    Range(KeyRange, RangeOptions),
    Put { key: Vec<u8>, value: Vec<u8>, lease: i64, prev_kv: bool, ignore_value: bool, ignore_lease: bool },
    Delete { range: KeyRange, prev_kv: bool },
    Txn(Box<Transaction>),
}

/// A transaction: the comparisons, and the operations for when they all hold or not.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Transaction {
    pub(crate) compares: Vec<Compare>,
    pub(crate) success: Vec<Operation>,
    pub(crate) failure: Vec<Operation>,
}

/// What one operation gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Range(RangeResult),
    Put(Option<KeyValue>),
    Delete(i64, Vec<KeyValue>),
    Txn(bool, Vec<Outcome>),
}

struct Writer<'w> {
    keys: &'w mut dyn KeysWrite,
    revision: i64,
    wrote: bool,
    sub: u32,
}

impl Writer<'_> {
    fn next_revision(&mut self) -> i64 {
        if !self.wrote {
            self.wrote = true;
            self.revision += 1;
        }
        self.revision
    }

    fn log(&mut self, event: &Event) -> Result<(), EtcdError> {
        let key = history_key(self.revision, self.sub);
        self.sub += 1;
        self.keys.put(&key, &encode_event(event)).map_err(storage)
    }
}

fn current(read: &dyn KeysRead, key: &[u8]) -> Result<Option<KeyValue>, EtcdError> {
    read.get(&current_key(key))
        .map_err(storage)?
        .map(|bytes| decode_record(key, &bytes))
        .transpose()
}

fn current_range(read: &dyn KeysRead, range: &KeyRange) -> Result<Vec<KeyValue>, EtcdError> {
    if range.end.is_empty() {
        return Ok(current(read, &range.key)?.into_iter().collect());
    }
    let start = current_key(&range.key);
    let end = if range.end == [0] { keys_after_prefix(&[CURRENT]) } else { Some(current_key(&range.end)) };
    let mut found = Vec::new();
    let mut failure = None;
    read.visit_range(&start, end.as_deref(), &mut |key, value| {
        match decode_record(&key[1..], value) {
            Ok(kv) => found.push(kv),
            Err(error) => {
                failure = Some(error);
                return Ok(false);
            }
        }
        Ok(true)
    })
    .map_err(storage)?;
    match failure {
        Some(error) => Err(error),
        None => Ok(found),
    }
}

fn stored_number(read: &dyn KeysRead, key: &[u8], default: i64) -> Result<i64, EtcdError> {
    Ok(read.get(key).map_err(storage)?.map_or(default, |bytes| number(&bytes)))
}

fn past_range(read: &dyn KeysRead, range: &KeyRange, revision: i64) -> Result<Vec<KeyValue>, EtcdError> {
    let mut state: BTreeMap<Vec<u8>, KeyValue> = BTreeMap::new();
    let mut failure = None;
    let end = history_key(revision.saturating_add(1), 0);
    read.visit_range(&[HISTORY], Some(&end), &mut |_, value| {
        match decode_event(value) {
            Ok(event) if range.contains(&event.kv.key) => {
                if event.deleted {
                    state.remove(&event.kv.key);
                } else {
                    state.insert(event.kv.key.clone(), event.kv);
                }
            }
            Ok(_) => {}
            Err(error) => {
                failure = Some(error);
                return Ok(false);
            }
        }
        Ok(true)
    })
    .map_err(storage)?;
    match failure {
        Some(error) => Err(error),
        None => Ok(state.into_values().collect()),
    }
}

fn range(read: &dyn KeysRead, range: &KeyRange, options: RangeOptions) -> Result<RangeResult, EtcdError> {
    let revision = stored_number(read, REVISION_KEY, 1)?;
    let compacted = stored_number(read, COMPACTED_KEY, 0)?;
    let mut kvs = if options.revision > 0 && options.revision < revision {
        if options.revision < compacted {
            return Err(EtcdError::Compacted(compacted));
        }
        past_range(read, range, options.revision)?
    } else if options.revision > revision {
        return Err(EtcdError::FutureRevision);
    } else {
        current_range(read, range)?
    };
    let count = kvs.len() as i64;
    let mut more = false;
    if options.limit > 0 && kvs.len() as i64 > options.limit {
        kvs.truncate(options.limit as usize);
        more = true;
    }
    if options.count_only {
        kvs.clear();
    } else if options.keys_only {
        for kv in &mut kvs {
            kv.value.clear();
        }
    }
    Ok(RangeResult { kvs, more, count })
}

fn check_key(key: &[u8]) -> Result<(), EtcdError> {
    if key.is_empty() {
        return Err(EtcdError::Invalid("etcdserver: key is not provided".to_owned()));
    }
    if key.len() > MOST_KEY_BYTES {
        return Err(EtcdError::Invalid("etcdserver: key is too large".to_owned()));
    }
    Ok(())
}

fn compare_holds(read: &dyn KeysRead, compare: &Compare) -> Result<bool, EtcdError> {
    let range = KeyRange { key: compare.key.clone(), end: compare.range_end.clone() };
    let kvs = current_range(read, &range)?;
    let targets: Vec<Option<KeyValue>> = if kvs.is_empty() { vec![None] } else { kvs.into_iter().map(Some).collect() };
    for kv in targets {
        let ordering = match (&compare.operand, compare.target) {
            (CompareOperand::Value(expected), 3) => match &kv {
                Some(kv) => kv.value.as_slice().cmp(expected.as_slice()),
                None => return Ok(false),
            },
            (CompareOperand::Number(expected), target) => {
                let actual = kv.as_ref().map_or(0, |kv| match target {
                    0 => kv.version,
                    1 => kv.create_revision,
                    2 => kv.mod_revision,
                    _ => kv.lease,
                });
                actual.cmp(expected)
            }
            _ => return Err(EtcdError::Invalid("etcdserver: a comparison's target and value do not match".to_owned())),
        };
        let holds = match compare.result {
            0 => ordering.is_eq(),
            1 => ordering.is_gt(),
            2 => ordering.is_lt(),
            3 => ordering.is_ne(),
            _ => return Err(EtcdError::Invalid("etcdserver: unknown comparison".to_owned())),
        };
        if !holds {
            return Ok(false);
        }
    }
    Ok(true)
}

fn put(writer: &mut Writer<'_>, key: &[u8], value: &[u8], lease: i64, ignore_value: bool, ignore_lease: bool) -> Result<Option<KeyValue>, EtcdError> {
    check_key(key)?;
    if value.len() > MOST_VALUE_BYTES {
        return Err(EtcdError::Invalid("etcdserver: request is too large".to_owned()));
    }
    let previous = current(&*writer.keys, key)?;
    if (ignore_value || ignore_lease) && previous.is_none() {
        return Err(EtcdError::Invalid("etcdserver: key not found".to_owned()));
    }
    let lease = if ignore_lease { previous.as_ref().map_or(0, |kv| kv.lease) } else { lease };
    if lease != 0 && writer.keys.get(&lease_key(lease)).map_err(storage)?.is_none() {
        return Err(EtcdError::LeaseMissing);
    }
    let revision = writer.next_revision();
    let kv = KeyValue {
        key: key.to_vec(),
        create_revision: previous.as_ref().map_or(revision, |kv| kv.create_revision),
        mod_revision: revision,
        version: previous.as_ref().map_or(1, |kv| kv.version + 1),
        value: if ignore_value { previous.as_ref().map(|kv| kv.value.clone()).unwrap_or_default() } else { value.to_vec() },
        lease,
    };
    if let Some(old) = previous.as_ref().filter(|old| old.lease != 0 && old.lease != lease) {
        writer.keys.delete(&lease_member_key(old.lease, key)).map_err(storage)?;
    }
    if lease != 0 {
        writer.keys.put(&lease_member_key(lease, key), b"").map_err(storage)?;
    }
    writer.keys.put(&current_key(key), &encode_record(&kv)).map_err(storage)?;
    writer.log(&Event { deleted: false, kv })?;
    Ok(previous)
}

fn delete(writer: &mut Writer<'_>, range: &KeyRange) -> Result<Vec<KeyValue>, EtcdError> {
    let doomed = current_range(&*writer.keys, range)?;
    if doomed.is_empty() {
        return Ok(doomed);
    }
    let revision = writer.next_revision();
    for kv in &doomed {
        writer.keys.delete(&current_key(&kv.key)).map_err(storage)?;
        if kv.lease != 0 {
            writer.keys.delete(&lease_member_key(kv.lease, &kv.key)).map_err(storage)?;
        }
        let gone = KeyValue { key: kv.key.clone(), mod_revision: revision, ..KeyValue::default() };
        writer.log(&Event { deleted: true, kv: gone })?;
    }
    Ok(doomed)
}

fn run(writer: &mut Writer<'_>, operation: &Operation) -> Result<Outcome, EtcdError> {
    Ok(match operation {
        Operation::Range(key_range, options) => Outcome::Range(range(&*writer.keys, key_range, *options)?),
        Operation::Put { key, value, lease, prev_kv, ignore_value, ignore_lease } => {
            let previous = put(writer, key, value, *lease, *ignore_value, *ignore_lease)?;
            Outcome::Put(previous.filter(|_| *prev_kv))
        }
        Operation::Delete { range, prev_kv } => {
            let deleted = delete(writer, range)?;
            Outcome::Delete(deleted.len() as i64, if *prev_kv { deleted } else { Vec::new() })
        }
        Operation::Txn(transaction) => {
            let succeeded = transaction
                .compares
                .iter()
                .try_fold(true, |holds, compare| Ok::<bool, EtcdError>(holds && compare_holds(&*writer.keys, compare)?))?;
            let branch = if succeeded { &transaction.success } else { &transaction.failure };
            let mut outcomes = Vec::with_capacity(branch.len());
            for operation in branch {
                outcomes.push(run(writer, operation)?);
            }
            Outcome::Txn(succeeded, outcomes)
        }
    })
}

fn expire_leases(writer: &mut Writer<'_>, now: i64) -> Result<(), EtcdError> {
    let mut expired = Vec::new();
    writer
        .keys
        .visit_prefix(&[LEASE], &mut |key, value| {
            if value.len() >= 16 && number(&value[8..16]) <= now {
                expired.push(number(&key[1..]));
            }
            Ok(true)
        })
        .map_err(storage)?;
    for id in expired {
        revoke_in(writer, id)?;
    }
    Ok(())
}

fn revoke_in(writer: &mut Writer<'_>, id: i64) -> Result<(), EtcdError> {
    let mut prefix = vec![LEASE_KEY];
    prefix.extend_from_slice(&id.to_be_bytes());
    let keys: Vec<Vec<u8>> = writer
        .keys
        .scan(&prefix)
        .map_err(storage)?
        .into_iter()
        .map(|(key, _)| key[prefix.len()..].to_vec())
        .collect();
    for key in keys {
        delete(writer, &KeyRange { key, end: Vec::new() })?;
    }
    writer.keys.delete(&lease_key(id)).map_err(storage)?;
    Ok(())
}

/// The etcd store over one keyspace.
pub(crate) struct Store<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
}

/// A lease's state, as `LeaseTimeToLive` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LeaseState {
    pub(crate) ttl_seconds: i64,
    pub(crate) granted_seconds: i64,
    pub(crate) keys: Vec<Vec<u8>>,
}

impl<'k> Store<'k> {
    pub(crate) fn new(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self { keyspace }
    }

    /// The store's current revision.
    pub(crate) fn revision(&self) -> Result<i64, EtcdError> {
        let mut revision = 1;
        self.keyspace
            .read(DATABASE, &mut |read| {
                revision = read.get(REVISION_KEY)?.map_or(1, |bytes| number(&bytes));
                Ok(())
            })
            .map_err(storage)?;
        Ok(revision)
    }

    /// Runs one write request: leases that have expired go first, then `body`, all in one
    /// transaction that raises the revision once if anything was written. Returns the body's
    /// answer and the revision after it.
    fn write<T>(&self, body: &mut dyn FnMut(&mut Writer<'_>) -> Result<T, EtcdError>) -> Result<(T, i64), EtcdError> {
        let mut answer = None;
        let mut failure = None;
        let mut final_revision = 0;
        let written = self.keyspace
            .write(DATABASE, &mut |keys| {
                let revision = keys.get(REVISION_KEY)?.map_or(1, |bytes| number(&bytes));
                let mut writer = Writer { keys, revision, wrote: false, sub: 0 };
                let outcome = expire_leases(&mut writer, now_ms()).and_then(|()| body(&mut writer));
                match outcome {
                    Ok(value) => {
                        if writer.wrote {
                            writer.keys.put(REVISION_KEY, &writer.revision.to_be_bytes())?;
                        }
                        final_revision = writer.revision;
                        answer = Some(value);
                        Ok(())
                    }
                    Err(error) => {
                        failure = Some(error);
                        Err("the etcd request failed".to_owned())
                    }
                }
            });
        if let Err(message) = written {
            return Err(failure.unwrap_or(EtcdError::Storage(message)));
        }
        match (answer, failure) {
            (Some(answer), _) => Ok((answer, final_revision)),
            (None, Some(error)) => Err(error),
            (None, None) => Err(EtcdError::Storage("the etcd request gave no answer".to_owned())),
        }
    }

    pub(crate) fn range(&self, key_range: &KeyRange, options: RangeOptions) -> Result<(RangeResult, i64), EtcdError> {
        let mut answer = None;
        let mut failure = None;
        let mut revision = 1;
        self.keyspace
            .read(DATABASE, &mut |read| {
                revision = read.get(REVISION_KEY)?.map_or(1, |bytes| number(&bytes));
                match range(read, key_range, options) {
                    Ok(result) => answer = Some(result),
                    Err(error) => failure = Some(error),
                }
                Ok(())
            })
            .map_err(storage)?;
        match (answer, failure) {
            (Some(result), _) => Ok((result, revision)),
            (_, Some(error)) => Err(error),
            _ => Err(EtcdError::Storage("the range gave no answer".to_owned())),
        }
    }

    pub(crate) fn run(&self, operation: &Operation) -> Result<(Outcome, i64), EtcdError> {
        self.write(&mut |writer| run(writer, operation))
    }

    pub(crate) fn compact(&self, revision: i64) -> Result<i64, EtcdError> {
        self.write(&mut |writer| {
            let compacted = stored_number(&*writer.keys, COMPACTED_KEY, 0)?;
            if revision <= compacted {
                return Err(EtcdError::Compacted(compacted));
            }
            if revision > writer.revision {
                return Err(EtcdError::FutureRevision);
            }
            let end = history_key(revision, 0);
            let mut doomed = Vec::new();
            writer
                .keys
                .visit_range(&[HISTORY], Some(&end), &mut |key, _| {
                    doomed.push(key.to_vec());
                    Ok(true)
                })
                .map_err(storage)?;
            let kept = past_range(&*writer.keys, &KeyRange { key: vec![0], end: vec![0] }, revision - 1)?;
            for key in doomed {
                writer.keys.delete(&key).map_err(storage)?;
            }
            for (sub, kv) in kept.into_iter().enumerate() {
                let key = history_key(kv.mod_revision.min(revision - 1).max(0), u32::MAX - sub as u32);
                writer.keys.put(&key, &encode_event(&Event { deleted: false, kv })).map_err(storage)?;
            }
            writer.keys.put(COMPACTED_KEY, &revision.to_be_bytes()).map_err(storage)?;
            Ok(())
        })
        .map(|(_, revision)| revision)
    }

    pub(crate) fn events_since(&self, key_range: &KeyRange, start: i64) -> Result<(Vec<Event>, i64), EtcdError> {
        let mut events = Vec::new();
        let mut failure = None;
        let mut revision = 1;
        let mut compacted = 0;
        self.keyspace
            .read(DATABASE, &mut |read| {
                revision = read.get(REVISION_KEY)?.map_or(1, |bytes| number(&bytes));
                compacted = read.get(COMPACTED_KEY)?.map_or(0, |bytes| number(&bytes));
                let from = history_key(start.max(compacted), 0);
                read.visit_range(&from, keys_after_prefix(&[HISTORY]).as_deref(), &mut |key, value| {
                    if number(&key[1..]) < start {
                        return Ok(true);
                    }
                    match decode_event(value) {
                        Ok(event) if key_range.contains(&event.kv.key) => events.push(event),
                        Ok(_) => {}
                        Err(error) => {
                            failure = Some(error);
                            return Ok(false);
                        }
                    }
                    Ok(true)
                })
            })
            .map_err(storage)?;
        if start > 0 && start < compacted {
            return Err(EtcdError::Compacted(compacted));
        }
        match failure {
            Some(error) => Err(error),
            None => Ok((events, revision)),
        }
    }

    pub(crate) fn grant(&self, ttl_seconds: i64, wanted: i64) -> Result<(i64, i64), EtcdError> {
        if ttl_seconds <= 0 {
            return Err(EtcdError::Invalid("etcdserver: lease TTL must be positive".to_owned()));
        }
        self.write(&mut |writer| {
            let id = if wanted != 0 {
                if writer.keys.get(&lease_key(wanted)).map_err(storage)?.is_some() {
                    return Err(EtcdError::Invalid("etcdserver: lease already exists".to_owned()));
                }
                wanted
            } else {
                let next = stored_number(&*writer.keys, NEXT_LEASE_KEY, 0x7E00_0000_0000_0000)? + 1;
                writer.keys.put(NEXT_LEASE_KEY, &next.to_be_bytes()).map_err(storage)?;
                next
            };
            let mut record = ttl_seconds.to_be_bytes().to_vec();
            record.extend_from_slice(&now_ms().saturating_add(ttl_seconds.saturating_mul(1000)).to_be_bytes());
            writer.keys.put(&lease_key(id), &record).map_err(storage)?;
            Ok(id)
        })
    }

    pub(crate) fn revoke(&self, id: i64) -> Result<i64, EtcdError> {
        self.write(&mut |writer| {
            if writer.keys.get(&lease_key(id)).map_err(storage)?.is_none() {
                return Err(EtcdError::LeaseMissing);
            }
            revoke_in(writer, id)
        })
        .map(|(_, revision)| revision)
    }

    pub(crate) fn keep_alive(&self, id: i64) -> Result<(i64, i64), EtcdError> {
        self.write(&mut |writer| {
            let Some(record) = writer.keys.get(&lease_key(id)).map_err(storage)? else {
                return Ok(0);
            };
            let ttl = number(&record);
            let mut renewed = ttl.to_be_bytes().to_vec();
            renewed.extend_from_slice(&now_ms().saturating_add(ttl.saturating_mul(1000)).to_be_bytes());
            writer.keys.put(&lease_key(id), &renewed).map_err(storage)?;
            Ok(ttl)
        })
    }

    pub(crate) fn time_to_live(&self, id: i64, with_keys: bool) -> Result<(Option<LeaseState>, i64), EtcdError> {
        self.write(&mut |writer| {
            let Some(record) = writer.keys.get(&lease_key(id)).map_err(storage)? else {
                return Ok(None);
            };
            let mut state = LeaseState {
                granted_seconds: number(&record),
                ttl_seconds: (number(record.get(8..).unwrap_or_default()) - now_ms()).max(0) / 1000,
                keys: Vec::new(),
            };
            if with_keys {
                let mut prefix = vec![LEASE_KEY];
                prefix.extend_from_slice(&id.to_be_bytes());
                state.keys = writer.keys.scan(&prefix).map_err(storage)?.into_iter().map(|(key, _)| key[prefix.len()..].to_vec()).collect();
            }
            Ok(Some(state))
        })
    }

    pub(crate) fn leases(&self) -> Result<(Vec<i64>, i64), EtcdError> {
        self.write(&mut |writer| {
            Ok(writer.keys.scan(&[LEASE]).map_err(storage)?.into_iter().map(|(key, _)| number(&key[1..])).collect())
        })
    }
}
