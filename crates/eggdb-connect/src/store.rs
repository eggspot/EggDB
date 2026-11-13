//! Connectors as the worker keeps them, in one keyspace: each one's configuration, state and
//! position, so a restarted server runs every connector on from where it stopped.

use eggdb_keyspace::Keyspace;
use std::collections::BTreeMap;

const DATABASE: u32 = 0;
const CONFIG: u8 = b'c';
const STATE: u8 = b's';
const POSITION: u8 = b'p';

/// A connector's state, as the status endpoint reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum State {
    Running,
    Paused,
    Failed(String),
}

impl State {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Running => "RUNNING",
            Self::Paused => "PAUSED",
            Self::Failed(_) => "FAILED",
        }
    }

    fn encode(&self) -> Vec<u8> {
        match self {
            Self::Running => b"R".to_vec(),
            Self::Paused => b"P".to_vec(),
            Self::Failed(trace) => [b"F".as_slice(), trace.as_bytes()].concat(),
        }
    }

    fn decode(bytes: &[u8]) -> Self {
        match bytes.split_first() {
            Some((b'P', _)) => Self::Paused,
            Some((b'F', trace)) => Self::Failed(String::from_utf8_lossy(trace).into_owned()),
            _ => Self::Running,
        }
    }
}

fn key(kind: u8, name: &str) -> Vec<u8> {
    let mut key = vec![kind];
    key.extend_from_slice(name.as_bytes());
    key
}

fn encode_config(config: &BTreeMap<String, String>) -> Vec<u8> {
    let mut out = Vec::new();
    for (field, value) in config {
        for part in [field, value] {
            out.extend_from_slice(&(part.len() as u32).to_be_bytes());
            out.extend_from_slice(part.as_bytes());
        }
    }
    out
}

fn decode_config(bytes: &[u8]) -> Option<BTreeMap<String, String>> {
    let mut config = BTreeMap::new();
    let mut rest = bytes;
    let part = |rest: &mut &[u8]| -> Option<String> {
        let (length, tail) = rest.split_first_chunk::<4>()?;
        let (text, tail) = tail.split_at_checked(u32::from_be_bytes(*length) as usize)?;
        *rest = tail;
        String::from_utf8(text.to_vec()).ok()
    };
    while !rest.is_empty() {
        let field = part(&mut rest)?;
        let value = part(&mut rest)?;
        config.insert(field, value);
    }
    Some(config)
}

/// The connectors of one worker.
pub(crate) struct Connectors<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
}

/// One connector as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stored {
    pub(crate) name: String,
    pub(crate) config: BTreeMap<String, String>,
    pub(crate) state: State,
    pub(crate) position: i64,
}

impl<'k> Connectors<'k> {
    pub(crate) fn new(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self { keyspace }
    }

    pub(crate) fn names(&self) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        self.keyspace.read(DATABASE, &mut |keys| {
            names = keys.scan(&[CONFIG])?.into_iter().map(|(key, _)| String::from_utf8_lossy(&key[1..]).into_owned()).collect();
            Ok(())
        })?;
        Ok(names)
    }

    pub(crate) fn get(&self, name: &str) -> Result<Option<Stored>, String> {
        let mut found = None;
        self.keyspace.read(DATABASE, &mut |keys| {
            let Some(config) = keys.get(&key(CONFIG, name))? else {
                return Ok(());
            };
            let config = decode_config(&config).ok_or("a stored connector configuration is damaged")?;
            let state = keys.get(&key(STATE, name))?.map_or(State::Running, |bytes| State::decode(&bytes));
            let position = keys
                .get(&key(POSITION, name))?
                .and_then(|bytes| bytes.as_slice().try_into().ok())
                .map_or(0, i64::from_be_bytes);
            found = Some(Stored { name: name.to_owned(), config, state, position });
            Ok(())
        })?;
        Ok(found)
    }

    /// Stores `config` for `name`; returns whether the connector is new. A new connector runs
    /// from the start; a changed one keeps its position and state.
    pub(crate) fn put(&self, name: &str, config: &BTreeMap<String, String>) -> Result<bool, String> {
        let mut created = false;
        self.keyspace.write(DATABASE, &mut |keys| {
            created = keys.get(&key(CONFIG, name))?.is_none();
            keys.put(&key(CONFIG, name), &encode_config(config))?;
            if created {
                keys.put(&key(STATE, name), &State::Running.encode())?;
                keys.put(&key(POSITION, name), &0i64.to_be_bytes())?;
            }
            Ok(())
        })?;
        Ok(created)
    }

    pub(crate) fn set_state(&self, name: &str, state: &State) -> Result<(), String> {
        self.keyspace.write(DATABASE, &mut |keys| keys.put(&key(STATE, name), &state.encode()))
    }

    pub(crate) fn set_position(&self, name: &str, position: i64) -> Result<(), String> {
        self.keyspace.write(DATABASE, &mut |keys| keys.put(&key(POSITION, name), &position.to_be_bytes()))
    }

    pub(crate) fn delete(&self, name: &str) -> Result<bool, String> {
        let mut existed = false;
        self.keyspace.write(DATABASE, &mut |keys| {
            existed = keys.delete(&key(CONFIG, name))?;
            keys.delete(&key(STATE, name))?;
            keys.delete(&key(POSITION, name))?;
            Ok(())
        })?;
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configurations_round_trip_and_damage_is_refused() {
        let config: BTreeMap<String, String> = [("file".to_owned(), "/tmp/in".to_owned()), ("topic".to_owned(), "lines".to_owned())].into_iter().collect();
        assert_eq!(decode_config(&encode_config(&config)), Some(config));
        assert_eq!(decode_config(&[0, 0, 0, 9, b'x']), None);
        assert_eq!(State::decode(&State::Failed("boom".to_owned()).encode()), State::Failed("boom".to_owned()));
    }
}
