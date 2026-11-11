use crate::commands::glob;
use crate::protocol::Reply;
use eggdb_platform::connection_limits::{MemoryBudget, OwnedReservation};
use std::collections::{BTreeSet, HashMap};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

const MAX_PENDING_PER_SUBSCRIBER: usize = 10_000;
const PUSH_FRAME_BYTES: usize = 64;

/// A published message waiting for one subscriber's connection to write it, holding its bytes
/// against the server's memory budget until then.
pub struct Queued {
    /// The message as its subscriber's protocol frames it.
    pub frame: Vec<u8>,
    /// Its bytes in the memory budget.
    pub held: OwnedReservation,
}

struct Subscriber {
    channels: BTreeSet<Vec<u8>>,
    patterns: BTreeSet<Vec<u8>>,
    shard_channels: BTreeSet<Vec<u8>>,
    resp3: bool,
    sender: Sender<Queued>,
    pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// The server's channels: who listens to what, and the delivery of published messages.
#[derive(Default)]
pub struct Hub {
    subscribers: Mutex<HashMap<u64, Subscriber>>,
    memory_budget: Arc<MemoryBudget>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hub").finish_non_exhaustive()
    }
}

/// What kind of subscription a command changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Exact channel names (`SUBSCRIBE`).
    Channel,
    /// Glob patterns (`PSUBSCRIBE`).
    Pattern,
    /// Shard channels (`SSUBSCRIBE`), which only `SPUBLISH` reaches.
    Shard,
}

fn frame(reply: &Reply, resp3: bool) -> Vec<u8> {
    let mut out = Vec::new();
    reply.encode(&mut out, resp3);
    out
}

impl Hub {
    /// A hub whose queued messages hold their bytes against `memory_budget` (D307).
    pub fn new(memory_budget: Arc<MemoryBudget>) -> Self {
        Self {
            subscribers: Mutex::default(),
            memory_budget,
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Subscriber>> {
        self.subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Publishes `message` on `channel`; returns how many subscriptions received it. A
    /// subscription whose connection already has its most messages waiting, or for which the
    /// memory budget has no room left, does not receive it.
    pub fn publish(&self, channel: &[u8], message: &[u8]) -> usize {
        let subscribers = self.locked();
        let mut delivered = 0;
        for subscriber in subscribers.values() {
            let channel_match = subscriber.channels.contains(channel).then_some(None);
            let pattern_matches = subscriber
                .patterns
                .iter()
                .filter(|pattern| glob(pattern, channel))
                .map(Some);
            for pattern in channel_match.into_iter().chain(pattern_matches) {
                if subscriber
                    .pending
                    .load(std::sync::atomic::Ordering::Acquire)
                    >= MAX_PENDING_PER_SUBSCRIBER
                {
                    continue;
                }
                let frame_bytes = message
                    .len()
                    .saturating_add(channel.len())
                    .saturating_add(pattern.map_or(0, Vec::len))
                    .saturating_add(PUSH_FRAME_BYTES);
                let Some(held) = MemoryBudget::reserve_owned(&self.memory_budget, frame_bytes)
                else {
                    continue;
                };
                let reply = match pattern {
                    None => Reply::Push(vec![
                        Reply::bulk("message"),
                        Reply::bulk(channel.to_vec()),
                        Reply::bulk(message.to_vec()),
                    ]),
                    Some(pattern) => Reply::Push(vec![
                        Reply::bulk("pmessage"),
                        Reply::bulk(pattern.clone()),
                        Reply::bulk(channel.to_vec()),
                        Reply::bulk(message.to_vec()),
                    ]),
                };
                let queued = Queued {
                    frame: frame(&reply, subscriber.resp3),
                    held,
                };
                if subscriber.sender.send(queued).is_ok() {
                    subscriber
                        .pending
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    delivered += 1;
                }
            }
        }
        delivered
    }

    /// Publishes `message` on shard channel `channel` (`SPUBLISH`), as `smessage` to the
    /// connections that `SSUBSCRIBE`d to it; returns how many received it.
    pub fn publish_shard(&self, channel: &[u8], message: &[u8]) -> usize {
        let subscribers = self.locked();
        let mut delivered = 0;
        for subscriber in subscribers.values() {
            if !subscriber.shard_channels.contains(channel)
                || subscriber
                    .pending
                    .load(std::sync::atomic::Ordering::Acquire)
                    >= MAX_PENDING_PER_SUBSCRIBER
            {
                continue;
            }
            let frame_bytes = message
                .len()
                .saturating_add(channel.len())
                .saturating_add(PUSH_FRAME_BYTES);
            let Some(held) = MemoryBudget::reserve_owned(&self.memory_budget, frame_bytes) else {
                continue;
            };
            let reply = Reply::Push(vec![
                Reply::bulk("smessage"),
                Reply::bulk(channel.to_vec()),
                Reply::bulk(message.to_vec()),
            ]);
            let queued = Queued {
                frame: frame(&reply, subscriber.resp3),
                held,
            };
            if subscriber.sender.send(queued).is_ok() {
                subscriber
                    .pending
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                delivered += 1;
            }
        }
        delivered
    }

    /// Registers connection `id`'s outbox, the first time it subscribes.
    pub fn attach(
        &self,
        id: u64,
        resp3: bool,
        sender: Sender<Queued>,
        pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        self.locked().entry(id).or_insert(Subscriber {
            channels: BTreeSet::new(),
            patterns: BTreeSet::new(),
            shard_channels: BTreeSet::new(),
            resp3,
            sender,
            pending,
        });
    }

    /// Changes connection `id`'s subscriptions and returns the replies to send it, one per
    /// name: `subscribe`/`unsubscribe` (or the pattern forms) with the count it holds after.
    /// Unsubscribing from no names leaves every one of that kind.
    pub fn change(&self, id: u64, kind: Kind, subscribe: bool, names: &[Vec<u8>]) -> Vec<Reply> {
        let mut subscribers = self.locked();
        let Some(subscriber) = subscribers.get_mut(&id) else {
            return Vec::new();
        };
        let word = match (kind, subscribe) {
            (Kind::Channel, true) => "subscribe",
            (Kind::Channel, false) => "unsubscribe",
            (Kind::Pattern, true) => "psubscribe",
            (Kind::Pattern, false) => "punsubscribe",
            (Kind::Shard, true) => "ssubscribe",
            (Kind::Shard, false) => "sunsubscribe",
        };
        let other = match kind {
            Kind::Channel => subscriber.patterns.len(),
            Kind::Pattern => subscriber.channels.len(),
            Kind::Shard => 0,
        };
        let set = match kind {
            Kind::Channel => &mut subscriber.channels,
            Kind::Pattern => &mut subscriber.patterns,
            Kind::Shard => &mut subscriber.shard_channels,
        };
        let names: Vec<Vec<u8>> = if names.is_empty() && !subscribe {
            set.iter().cloned().collect()
        } else {
            names.to_vec()
        };
        let mut replies = Vec::new();
        for name in &names {
            if subscribe {
                set.insert(name.clone());
            } else {
                set.remove(name);
            }
            let held = other + set.len();
            replies.push(Reply::Push(vec![
                Reply::bulk(word),
                Reply::bulk(name.clone()),
                Reply::Integer(held as i64),
            ]));
        }
        if names.is_empty() {
            replies.push(Reply::Push(vec![
                Reply::bulk(word),
                Reply::null(),
                Reply::Integer(other as i64),
            ]));
        }
        replies
    }

    /// How many channels and patterns connection `id` holds.
    pub fn held(&self, id: u64) -> usize {
        self.locked().get(&id).map_or(0, |subscriber| {
            subscriber.channels.len()
                + subscriber.patterns.len()
                + subscriber.shard_channels.len()
        })
    }

    /// How many channels, patterns and shard channels connection `id` holds, each on its own.
    pub fn held_by_kind(&self, id: u64) -> (usize, usize, usize) {
        self.locked().get(&id).map_or((0, 0, 0), |subscriber| {
            (
                subscriber.channels.len(),
                subscriber.patterns.len(),
                subscriber.shard_channels.len(),
            )
        })
    }

    /// Forgets connection `id`.
    pub fn detach(&self, id: u64) {
        self.locked().remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn shard_channels_are_apart_from_channels() {
        let hub = Hub::default();
        let (sender, receiver) = std::sync::mpsc::channel();
        hub.attach(1, false, sender, Arc::new(AtomicUsize::new(0)));
        hub.change(1, Kind::Shard, true, &[b"orders".to_vec()]);
        assert_eq!(hub.publish(b"orders", b"x"), 0, "PUBLISH does not reach shard channels");
        assert_eq!(hub.publish_shard(b"orders", b"paid"), 1);
        assert_eq!(
            receiver.recv().unwrap().frame,
            b"*3\r\n$8\r\nsmessage\r\n$6\r\norders\r\n$4\r\npaid\r\n"
        );
        hub.change(1, Kind::Channel, true, &[b"orders".to_vec()]);
        assert_eq!(hub.publish_shard(b"orders", b"again"), 1);
        assert_eq!(hub.held(1), 2);
    }

    #[test]
    fn messages_reach_channel_and_pattern_subscribers() {
        let hub = Hub::default();
        let (sender, receiver) = std::sync::mpsc::channel();
        hub.attach(1, false, sender, Arc::new(AtomicUsize::new(0)));
        assert_eq!(
            hub.change(1, Kind::Channel, true, &[b"news".to_vec()])
                .len(),
            1
        );
        hub.change(1, Kind::Pattern, true, &[b"n*".to_vec()]);
        assert_eq!(hub.held(1), 2);
        assert_eq!(hub.publish(b"news", b"hi"), 2);
        assert_eq!(
            receiver.recv().unwrap().frame,
            b"*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$2\r\nhi\r\n"
        );
        assert!(
            receiver
                .recv()
                .unwrap()
                .frame
                .starts_with(b"*4\r\n$8\r\npmessage")
        );
        assert_eq!(hub.publish(b"other", b"x"), 0);
        hub.change(1, Kind::Channel, false, &[]);
        assert_eq!(hub.held(1), 1);
        hub.detach(1);
        assert_eq!(hub.publish(b"news", b"x"), 0);
    }

    #[test]
    fn queued_messages_hold_the_memory_budget_until_they_are_written() {
        let budget = Arc::new(MemoryBudget::new(1_000));
        let hub = Hub::new(Arc::clone(&budget));
        let (sender, receiver) = std::sync::mpsc::channel();
        hub.attach(1, false, sender, Arc::new(AtomicUsize::new(0)));
        hub.change(1, Kind::Channel, true, &[b"news".to_vec()]);
        let message = vec![b'x'; 400];
        assert_eq!(hub.publish(b"news", &message), 1);
        assert_eq!(hub.publish(b"news", &message), 1);
        assert_eq!(hub.publish(b"news", &message), 0);
        assert_eq!(budget.used_bytes(), 2 * (400 + 4 + PUSH_FRAME_BYTES));
        let first = receiver.recv().unwrap();
        assert!(first.frame.len() > message.len());
        drop(first);
        assert_eq!(budget.used_bytes(), 400 + 4 + PUSH_FRAME_BYTES);
        assert_eq!(hub.publish(b"news", &message), 1);
        hub.detach(1);
        drop(receiver);
        assert_eq!(budget.used_bytes(), 0);
    }
}
