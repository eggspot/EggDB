use crate::commands::glob;
use crate::protocol::Reply;
use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::sync::mpsc::Sender;

const MAX_PENDING_PER_SUBSCRIBER: usize = 10_000;

struct Subscriber {
    channels: BTreeSet<Vec<u8>>,
    patterns: BTreeSet<Vec<u8>>,
    resp3: bool,
    sender: Sender<Vec<u8>>,
    pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// The server's channels: who listens to what, and the delivery of published messages.
#[derive(Default)]
pub struct Hub {
    subscribers: Mutex<HashMap<u64, Subscriber>>,
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
}

fn frame(reply: &Reply, resp3: bool) -> Vec<u8> {
    let mut out = Vec::new();
    reply.encode(&mut out, resp3);
    out
}

impl Hub {
    fn locked(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Subscriber>> {
        self.subscribers.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Publishes `message` on `channel`; returns how many subscriptions received it.
    pub fn publish(&self, channel: &[u8], message: &[u8]) -> usize {
        let subscribers = self.locked();
        let mut delivered = 0;
        for subscriber in subscribers.values() {
            let mut frames = Vec::new();
            if subscriber.channels.contains(channel) {
                frames.push(Reply::Push(vec![Reply::bulk("message"), Reply::bulk(channel.to_vec()), Reply::bulk(message.to_vec())]));
            }
            for pattern in subscriber.patterns.iter().filter(|pattern| glob(pattern, channel)) {
                frames.push(Reply::Push(vec![
                    Reply::bulk("pmessage"),
                    Reply::bulk(pattern.clone()),
                    Reply::bulk(channel.to_vec()),
                    Reply::bulk(message.to_vec()),
                ]));
            }
            for reply in frames {
                if subscriber.pending.load(std::sync::atomic::Ordering::Acquire) >= MAX_PENDING_PER_SUBSCRIBER {
                    continue;
                }
                if subscriber.sender.send(frame(&reply, subscriber.resp3)).is_ok() {
                    subscriber.pending.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    delivered += 1;
                }
            }
        }
        delivered
    }

    /// Registers connection `id`'s outbox, the first time it subscribes.
    pub fn attach(&self, id: u64, resp3: bool, sender: Sender<Vec<u8>>, pending: std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        self.locked().entry(id).or_insert(Subscriber {
            channels: BTreeSet::new(),
            patterns: BTreeSet::new(),
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
        };
        let other = match kind {
            Kind::Channel => subscriber.patterns.len(),
            Kind::Pattern => subscriber.channels.len(),
        };
        let set = match kind {
            Kind::Channel => &mut subscriber.channels,
            Kind::Pattern => &mut subscriber.patterns,
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
            replies.push(Reply::Push(vec![Reply::bulk(word), Reply::bulk(name.clone()), Reply::Integer(held as i64)]));
        }
        if names.is_empty() {
            replies.push(Reply::Push(vec![Reply::bulk(word), Reply::null(), Reply::Integer(other as i64)]));
        }
        replies
    }

    /// How many channels and patterns connection `id` holds.
    pub fn held(&self, id: u64) -> usize {
        self.locked()
            .get(&id)
            .map_or(0, |subscriber| subscriber.channels.len() + subscriber.patterns.len())
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
    fn messages_reach_channel_and_pattern_subscribers() {
        let hub = Hub::default();
        let (sender, receiver) = std::sync::mpsc::channel();
        hub.attach(1, false, sender, Arc::new(AtomicUsize::new(0)));
        assert_eq!(hub.change(1, Kind::Channel, true, &[b"news".to_vec()]).len(), 1);
        hub.change(1, Kind::Pattern, true, &[b"n*".to_vec()]);
        assert_eq!(hub.held(1), 2);
        assert_eq!(hub.publish(b"news", b"hi"), 2);
        assert_eq!(receiver.recv().unwrap(), b"*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$2\r\nhi\r\n");
        assert!(receiver.recv().unwrap().starts_with(b"*4\r\n$8\r\npmessage"));
        assert_eq!(hub.publish(b"other", b"x"), 0);
        hub.change(1, Kind::Channel, false, &[]);
        assert_eq!(hub.held(1), 1);
        hub.detach(1);
        assert_eq!(hub.publish(b"news", b"x"), 0);
    }
}
