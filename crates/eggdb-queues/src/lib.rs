//! Durable message queues for EggDB's messaging servers (D214), on EggDB's key-value
//! transactions: [`Queues::send`] appends a message, [`Queues::receive`] leases the oldest
//! visible ones for a while (SQS's visibility timeout; AMQP and STOMP use long leases released
//! when a consumer goes away), [`Queues::ack`] removes a leased message, and a lease that runs
//! out makes the message visible again, so every message is delivered at least once. Each call
//! is one transaction.

use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};

const QUEUE: u8 = 1;
const MESSAGE: u8 = 2;
const MAX_QUEUE_NAME: usize = 255;
const MAX_MESSAGE_BYTES: usize = 16 << 20;

/// A leased message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Its id within the queue, increasing in send order.
    pub id: u64,
    /// Its body.
    pub body: Vec<u8>,
    /// How many times it has been received, this time included.
    pub receive_count: u32,
    /// When it was sent, in milliseconds since 1970.
    pub sent_ms: u64,
}

/// How many messages a queue holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Depth {
    /// Visible now.
    pub ready: u64,
    /// Leased or delayed.
    pub waiting: u64,
}

/// Why a queue operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueError {
    /// The queue does not exist.
    NoQueue(String),
    /// The name is empty, too long or has a NUL.
    BadName(String),
    /// The message is larger than 16 MiB.
    TooLarge,
    /// The storage failed.
    Storage(String),
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueueError::NoQueue(name) => write!(f, "queue {name} does not exist"),
            QueueError::BadName(name) => write!(f, "{name:?} is not a valid queue name"),
            QueueError::TooLarge => write!(f, "the message is larger than 16 MiB"),
            QueueError::Storage(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for QueueError {}

type Result<T> = std::result::Result<T, QueueError>;

fn queue_key(name: &str) -> Vec<u8> {
    let mut key = vec![QUEUE];
    key.extend_from_slice(name.as_bytes());
    key
}

fn messages_prefix(name: &str) -> Vec<u8> {
    let mut key = vec![MESSAGE];
    key.extend_from_slice(name.as_bytes());
    key.push(0);
    key
}

fn message_key(name: &str, id: u64) -> Vec<u8> {
    let mut key = messages_prefix(name);
    key.extend_from_slice(&id.to_be_bytes());
    key
}

struct Stored {
    visible_ms: u64,
    receive_count: u32,
    sent_ms: u64,
    body: Vec<u8>,
}

impl Stored {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(20 + self.body.len());
        out.extend_from_slice(&self.visible_ms.to_be_bytes());
        out.extend_from_slice(&self.receive_count.to_be_bytes());
        out.extend_from_slice(&self.sent_ms.to_be_bytes());
        out.extend_from_slice(&self.body);
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let damaged = || QueueError::Storage("a stored message is damaged".to_owned());
        Ok(Self {
            visible_ms: u64::from_be_bytes(bytes.get(..8).ok_or_else(damaged)?.try_into().map_err(|_| damaged())?),
            receive_count: u32::from_be_bytes(bytes.get(8..12).ok_or_else(damaged)?.try_into().map_err(|_| damaged())?),
            sent_ms: u64::from_be_bytes(bytes.get(12..20).ok_or_else(damaged)?.try_into().map_err(|_| damaged())?),
            body: bytes[20..].to_vec(),
        })
    }
}

fn append(keys: &mut dyn KeysWrite, name: &str, body: &[u8], delay_ms: u64, now_ms: u64) -> Result<u64> {
    let Some(counter) = keys.get(&queue_key(name)).map_err(storage)? else {
        return Err(QueueError::NoQueue(name.to_owned()));
    };
    let id = u64::from_be_bytes(counter.get(..8).and_then(|raw| raw.try_into().ok()).unwrap_or_default()) + 1;
    keys.put(&queue_key(name), &id.to_be_bytes()).map_err(storage)?;
    let stored = Stored { visible_ms: now_ms.saturating_add(delay_ms), receive_count: 0, sent_ms: now_ms, body: body.to_vec() };
    keys.put(&message_key(name, id), &stored.encode()).map_err(storage)?;
    Ok(id)
}

/// The queues of one keyspace (its database 0).
pub struct Queues<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
}

impl std::fmt::Debug for Queues<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Queues").finish_non_exhaustive()
    }
}

fn storage(message: String) -> QueueError {
    QueueError::Storage(message)
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_QUEUE_NAME || name.contains('\0') {
        return Err(QueueError::BadName(name.to_owned()));
    }
    Ok(())
}

impl<'k> Queues<'k> {
    /// The queues stored in `keyspace`.
    pub fn new(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self { keyspace }
    }

    fn read<T>(&self, body: impl FnOnce(&dyn KeysRead) -> Result<T>) -> Result<T> {
        let mut body = Some(body);
        let mut out = None;
        self.keyspace
            .read(0, &mut |keys| {
                if let Some(body) = body.take() {
                    out = Some(body(keys));
                }
                Ok(())
            })
            .map_err(storage)?;
        out.unwrap_or_else(|| Err(storage("the read did not run".to_owned())))
    }

    fn write<T>(&self, body: impl FnOnce(&mut dyn KeysWrite) -> Result<T>) -> Result<T> {
        let mut body = Some(body);
        let mut out = None;
        let result = self.keyspace.write(0, &mut |keys| {
            let Some(body) = body.take() else {
                return Err("the write ran twice".to_owned());
            };
            let outcome = body(keys);
            let failed = outcome.is_err();
            out = Some(outcome);
            if failed { Err("the queue operation was rolled back".to_owned()) } else { Ok(()) }
        });
        match (result, out) {
            (_, Some(Err(error))) => Err(error),
            (Err(message), _) => Err(storage(message)),
            (Ok(()), Some(outcome)) => outcome,
            (Ok(()), None) => Err(storage("the write did not run".to_owned())),
        }
    }

    /// Creates `name` when missing; returns whether it was created.
    pub fn declare(&self, name: &str) -> Result<bool> {
        check_name(name)?;
        self.write(|keys| {
            if keys.get(&queue_key(name)).map_err(storage)?.is_some() {
                return Ok(false);
            }
            keys.put(&queue_key(name), &0u64.to_be_bytes()).map_err(storage)?;
            Ok(true)
        })
    }

    /// Whether `name` exists.
    pub fn exists(&self, name: &str) -> Result<bool> {
        self.read(|keys| Ok(keys.get(&queue_key(name)).map_err(storage)?.is_some()))
    }

    /// The queue names, in order.
    pub fn names(&self) -> Result<Vec<String>> {
        self.read(|keys| {
            Ok(keys.scan(&[QUEUE]).map_err(storage)?.into_iter().filter_map(|(key, _)| String::from_utf8(key[1..].to_vec()).ok()).collect())
        })
    }

    /// Deletes `name` and its messages; returns how many messages it held.
    pub fn delete(&self, name: &str) -> Result<u64> {
        self.write(|keys| {
            if keys.get(&queue_key(name)).map_err(storage)?.is_none() {
                return Err(QueueError::NoQueue(name.to_owned()));
            }
            let messages = keys.scan(&messages_prefix(name)).map_err(storage)?;
            for (key, _) in &messages {
                keys.delete(key).map_err(storage)?;
            }
            keys.delete(&queue_key(name)).map_err(storage)?;
            Ok(messages.len() as u64)
        })
    }

    /// Removes every message of `name`; returns how many.
    pub fn purge(&self, name: &str) -> Result<u64> {
        self.write(|keys| {
            if keys.get(&queue_key(name)).map_err(storage)?.is_none() {
                return Err(QueueError::NoQueue(name.to_owned()));
            }
            let messages = keys.scan(&messages_prefix(name)).map_err(storage)?;
            for (key, _) in &messages {
                keys.delete(key).map_err(storage)?;
            }
            Ok(messages.len() as u64)
        })
    }

    /// Appends a message, visible after `delay_ms`; returns its id.
    pub fn send(&self, name: &str, body: &[u8], delay_ms: u64, now_ms: u64) -> Result<u64> {
        if body.len() > MAX_MESSAGE_BYTES {
            return Err(QueueError::TooLarge);
        }
        self.write(|keys| append(keys, name, body, delay_ms, now_ms))
    }

    /// Appends every `(queue, body)` message in one transaction, all or none; returns their ids.
    pub fn send_all(&self, messages: &[(&str, &[u8])], now_ms: u64) -> Result<Vec<u64>> {
        if messages.iter().any(|(_, body)| body.len() > MAX_MESSAGE_BYTES) {
            return Err(QueueError::TooLarge);
        }
        self.write(|keys| messages.iter().map(|(name, body)| append(keys, name, body, 0, now_ms)).collect())
    }

    /// Leases up to `max` of the oldest messages visible at `now_ms` for `lease_ms`.
    pub fn receive(&self, name: &str, max: usize, lease_ms: u64, now_ms: u64) -> Result<Vec<Message>> {
        let any_visible = self.read(|keys| {
            if keys.get(&queue_key(name)).map_err(storage)?.is_none() {
                return Err(QueueError::NoQueue(name.to_owned()));
            }
            for (_, value) in keys.scan(&messages_prefix(name)).map_err(storage)? {
                if Stored::decode(&value)?.visible_ms <= now_ms {
                    return Ok(true);
                }
            }
            Ok(false)
        })?;
        if !any_visible || max == 0 {
            return Ok(Vec::new());
        }
        self.write(|keys| {
            if keys.get(&queue_key(name)).map_err(storage)?.is_none() {
                return Err(QueueError::NoQueue(name.to_owned()));
            }
            let mut out = Vec::new();
            for (key, value) in keys.scan(&messages_prefix(name)).map_err(storage)? {
                if out.len() >= max {
                    break;
                }
                let mut stored = Stored::decode(&value)?;
                if stored.visible_ms > now_ms {
                    continue;
                }
                stored.visible_ms = now_ms.saturating_add(lease_ms.max(1));
                stored.receive_count = stored.receive_count.saturating_add(1);
                keys.put(&key, &stored.encode()).map_err(storage)?;
                let id = u64::from_be_bytes(key[key.len() - 8..].try_into().unwrap_or_default());
                out.push(Message { id, body: stored.body, receive_count: stored.receive_count, sent_ms: stored.sent_ms });
            }
            Ok(out)
        })
    }

    /// Removes a received message; returns whether it was there.
    pub fn ack(&self, name: &str, id: u64) -> Result<bool> {
        self.write(|keys| keys.delete(&message_key(name, id)).map_err(storage))
    }

    /// Makes a leased message visible again at `now_ms` (a negative acknowledgement, or its
    /// consumer went away).
    pub fn release(&self, name: &str, id: u64, now_ms: u64) -> Result<bool> {
        self.write(|keys| {
            let key = message_key(name, id);
            let Some(value) = keys.get(&key).map_err(storage)? else {
                return Ok(false);
            };
            let mut stored = Stored::decode(&value)?;
            stored.visible_ms = now_ms;
            keys.put(&key, &stored.encode()).map_err(storage)?;
            Ok(true)
        })
    }

    /// How many messages are ready and waiting at `now_ms`.
    pub fn depth(&self, name: &str, now_ms: u64) -> Result<Depth> {
        self.read(|keys| {
            if keys.get(&queue_key(name)).map_err(storage)?.is_none() {
                return Err(QueueError::NoQueue(name.to_owned()));
            }
            let mut depth = Depth::default();
            for (_, value) in keys.scan(&messages_prefix(name)).map_err(storage)? {
                if Stored::decode(&value)?.visible_ms <= now_ms {
                    depth.ready += 1;
                } else {
                    depth.waiting += 1;
                }
            }
            Ok(depth)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Depth, QueueError, Queues};
    use eggdb_keyspace::MemoryKeyspace;

    #[test]
    fn messages_are_leased_acknowledged_and_redelivered() {
        let keyspace = MemoryKeyspace::default();
        let queues = Queues::new(&keyspace);
        assert!(queues.declare("jobs").unwrap());
        assert!(!queues.declare("jobs").unwrap());
        let first = queues.send("jobs", b"one", 0, 1_000).unwrap();
        queues.send("jobs", b"two", 0, 1_000).unwrap();
        queues.send("jobs", b"later", 5_000, 1_000).unwrap();
        let leased = queues.receive("jobs", 10, 30_000, 1_000).unwrap();
        assert_eq!(leased.iter().map(|message| message.body.as_slice()).collect::<Vec<_>>(), [&b"one"[..], b"two"]);
        assert!(queues.receive("jobs", 10, 30_000, 2_000).unwrap().is_empty());
        assert!(queues.ack("jobs", first).unwrap());
        assert_eq!(queues.depth("jobs", 7_000).unwrap(), Depth { ready: 1, waiting: 1 });
        let again = queues.receive("jobs", 10, 30_000, 40_000).unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(again[0].receive_count, 2);
        assert!(queues.release("jobs", again[1].id, 40_000).unwrap());
        assert_eq!(queues.receive("jobs", 1, 30_000, 40_000).unwrap()[0].body, b"later");
        assert_eq!(queues.purge("jobs").unwrap(), 2);
        assert_eq!(queues.delete("jobs").unwrap(), 0);
        assert!(matches!(queues.send("jobs", b"x", 0, 0), Err(QueueError::NoQueue(_))));
        assert!(matches!(queues.declare(""), Err(QueueError::BadName(_))));
    }

    #[test]
    fn send_all_is_all_or_nothing() {
        let keyspace = MemoryKeyspace::default();
        let queues = Queues::new(&keyspace);
        queues.declare("a").unwrap();
        queues.declare("b").unwrap();
        assert_eq!(queues.send_all(&[("a", b"1"), ("b", b"2"), ("a", b"3")], 0).unwrap(), [1, 1, 2]);
        assert!(matches!(queues.send_all(&[("a", b"4"), ("missing", b"5")], 0), Err(QueueError::NoQueue(_))));
        assert_eq!(queues.depth("a", 0).unwrap(), Depth { ready: 2, waiting: 0 });
    }
}
