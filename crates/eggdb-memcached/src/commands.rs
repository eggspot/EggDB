use crate::item::{Item, expiry_of};
use eggdb_resp::{KeysRead, KeysWrite, Keyspace};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_KEY_BYTES: usize = 250;
pub(crate) const MAX_ITEM_BYTES: usize = 1 << 20;

/// What the server shares between connections.
pub struct Shared<'k> {
    /// Where items live (database 0 of the keyspace).
    pub keyspace: &'k (dyn Keyspace + Sync),
    /// The next CAS value.
    pub next_cas: AtomicU64,
}

impl std::fmt::Debug for Shared<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl Shared<'_> {
    fn cas(&self) -> u64 {
        self.next_cas.fetch_add(1, Ordering::Relaxed)
    }
}

fn valid_key(key: &[u8]) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY_BYTES && key.iter().all(|byte| *byte > b' ' && *byte != 0x7f)
}

fn load(keys: &dyn KeysRead, key: &[u8], now: u64) -> Result<Option<Item>, String> {
    Ok(keys
        .get(key)?
        .and_then(|bytes| Item::decode(&bytes))
        .filter(|item| !item.expired(now)))
}

fn store(keys: &mut dyn KeysWrite, key: &[u8], item: &Item) -> Result<(), String> {
    keys.put(key, &item.encode())
}

fn read<T>(shared: &Shared<'_>, body: impl FnOnce(&dyn KeysRead) -> Result<T, String>) -> Result<T, String> {
    let mut body = Some(body);
    let mut out = None;
    shared.keyspace.read(0, &mut |keys| {
        if let Some(body) = body.take() {
            out = Some(body(keys)?);
        }
        Ok(())
    })?;
    out.ok_or_else(|| "the read did not run".to_owned())
}

fn write<T>(shared: &Shared<'_>, body: impl FnOnce(&mut dyn KeysWrite) -> Result<T, String>) -> Result<T, String> {
    let mut body = Some(body);
    let mut out = None;
    shared.keyspace.write(0, &mut |keys| {
        if let Some(body) = body.take() {
            out = Some(body(keys)?);
        }
        Ok(())
    })?;
    out.ok_or_else(|| "the write did not run".to_owned())
}

/// A storage command's mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `set`.
    Set,
    /// `add`: only when absent.
    Add,
    /// `replace`: only when present.
    Replace,
    /// `append`.
    Append,
    /// `prepend`.
    Prepend,
    /// `cas` with the value the client read.
    Cas(u64),
}

/// The outcome of a storage command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// `STORED`.
    Stored,
    /// `NOT_STORED`.
    NotStored,
    /// `EXISTS`: the CAS value differs.
    Exists,
    /// `NOT_FOUND`.
    NotFound,
}

/// Runs `set`, `add`, `replace`, `append`, `prepend` or `cas`.
pub fn storage(shared: &Shared<'_>, mode: Mode, key: &[u8], flags: u32, exptime: i64, data: Vec<u8>, now: u64) -> Result<Stored, String> {
    let cas = shared.cas();
    write(shared, |keys| {
        let existing = load(keys, key, now)?;
        let item = match (mode, existing) {
            (Mode::Add, Some(_)) | (Mode::Replace | Mode::Append | Mode::Prepend, None) => {
                return Ok(Stored::NotStored);
            }
            (Mode::Cas(_), None) => return Ok(Stored::NotFound),
            (Mode::Cas(expected), Some(existing)) if existing.cas != expected => return Ok(Stored::Exists),
            (Mode::Append, Some(mut existing)) => {
                existing.data.extend_from_slice(&data);
                existing.cas = cas;
                existing
            }
            (Mode::Prepend, Some(mut existing)) => {
                let mut joined = data;
                joined.extend_from_slice(&existing.data);
                existing.data = joined;
                existing.cas = cas;
                existing
            }
            _ => Item { data, flags, cas, expires_at_ms: expiry_of(exptime, now) },
        };
        if item.data.len() > MAX_ITEM_BYTES {
            return Err("SERVER_ERROR object too large for cache".to_owned());
        }
        if item.expired(now) {
            keys.delete(key)?;
        } else {
            store(keys, key, &item)?;
        }
        Ok(Stored::Stored)
    })
}

/// The items of `keys` that exist, with their keys.
pub fn get(shared: &Shared<'_>, keys_wanted: &[Vec<u8>], touch: Option<i64>, now: u64) -> Result<Vec<(Vec<u8>, Item)>, String> {
    let fetch = |keys: &dyn KeysRead| -> Result<Vec<(Vec<u8>, Item)>, String> {
        let mut found = Vec::new();
        for key in keys_wanted {
            if let Some(item) = load(keys, key, now)? {
                found.push((key.clone(), item));
            }
        }
        Ok(found)
    };
    match touch {
        None => read(shared, fetch),
        Some(exptime) => write(shared, |keys| {
            let mut found = fetch(keys)?;
            for (key, item) in &mut found {
                item.expires_at_ms = expiry_of(exptime, now);
                store(keys, key, item)?;
            }
            Ok(found)
        }),
    }
}

/// Deletes `key`; returns whether it was there.
pub fn delete(shared: &Shared<'_>, key: &[u8], now: u64) -> Result<bool, String> {
    write(shared, |keys| {
        let found = load(keys, key, now)?.is_some();
        if found {
            keys.delete(key)?;
        }
        Ok(found)
    })
}

/// Adds or subtracts `delta` from a decimal item; `None` when it does not exist.
pub fn arithmetic(shared: &Shared<'_>, key: &[u8], delta: u64, increment: bool, now: u64) -> Result<Option<u64>, String> {
    let cas = shared.cas();
    write(shared, |keys| {
        let Some(mut item) = load(keys, key, now)? else {
            return Ok(None);
        };
        let current: u64 = std::str::from_utf8(&item.data)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .ok_or_else(|| "CLIENT_ERROR cannot increment or decrement non-numeric value".to_owned())?;
        let next = if increment { current.wrapping_add(delta) } else { current.saturating_sub(delta) };
        item.data = next.to_string().into_bytes();
        item.cas = cas;
        store(keys, key, &item)?;
        Ok(Some(next))
    })
}

/// Sets a new expiry on `key`; returns whether it exists.
pub fn touch(shared: &Shared<'_>, key: &[u8], exptime: i64, now: u64) -> Result<bool, String> {
    write(shared, |keys| {
        let Some(mut item) = load(keys, key, now)? else {
            return Ok(false);
        };
        item.expires_at_ms = expiry_of(exptime, now);
        store(keys, key, &item)?;
        Ok(true)
    })
}

/// Removes every item.
pub fn flush_all(shared: &Shared<'_>) -> Result<(), String> {
    write(shared, |keys| keys.clear())
}

pub(crate) fn check_key(key: &[u8]) -> Result<(), String> {
    if valid_key(key) {
        Ok(())
    } else {
        Err("CLIENT_ERROR bad command line format".to_owned())
    }
}
