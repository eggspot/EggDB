//! An in-process inbox: a program that embeds EggDB sends to and consumes from the same durable
//! queues the AMQP, STOMP and SQS servers serve, without a network connection, with each message
//! settled as done, retried or rejected to the queue's dead-letter queue, and a message that keeps
//! failing dead-lettered after a set number of receives.

use crate::Database;
use crate::keyspace::{DatabaseKeyspace, QUEUE_COLLECTIONS};
use eggdb_queues::{DEAD_LETTER_SUFFIX, Message, QueueError, Queues};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_LEASE: Duration = Duration::from_secs(30);
const DEFAULT_MAX_RECEIVES: u32 = 5;
const DEFAULT_POLL: Duration = Duration::from_millis(50);

/// What a consumer decided about a message it received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Handled: the message is removed.
    Done,
    /// Not handled now: the message is visible again at once.
    Retry,
    /// Never to be handled: the message moves to the queue's dead-letter queue.
    Reject,
}

/// A consumer and producer of one queue inside an EggDB file.
pub struct Inbox<'d> {
    keyspace: DatabaseKeyspace<'d>,
    queue: String,
    lease: Duration,
    max_receives: u32,
    poll: Duration,
}

impl std::fmt::Debug for Inbox<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Inbox")
            .field("queue", &self.queue)
            .field("lease", &self.lease)
            .field("max_receives", &self.max_receives)
            .finish_non_exhaustive()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl<'d> Inbox<'d> {
    /// Opens the queue `queue` of `db`, creating it when missing. A received message is leased
    /// for 30 seconds and dead-lettered on its sixth receive, until changed.
    pub fn open(db: &'d Database, queue: &str) -> Result<Self, QueueError> {
        let inbox = Self {
            keyspace: DatabaseKeyspace { db, prefix: QUEUE_COLLECTIONS },
            queue: queue.to_owned(),
            lease: DEFAULT_LEASE,
            max_receives: DEFAULT_MAX_RECEIVES,
            poll: DEFAULT_POLL,
        };
        inbox.queues().declare(queue)?;
        Ok(inbox)
    }

    /// Opens the dead-letter queue of this inbox's queue, where rejected messages go.
    pub fn dead_letters(&self) -> Result<Inbox<'d>, QueueError> {
        Inbox::open(self.keyspace.db, &format!("{}{DEAD_LETTER_SUFFIX}", self.queue))
    }

    /// How long a received message stays hidden from other consumers before it is visible
    /// again, unless settled.
    #[must_use]
    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease.max(Duration::from_millis(1));
        self
    }

    /// How many times a message may be received; the next receive moves it to the dead-letter
    /// queue instead of handing it out again.
    #[must_use]
    pub fn with_max_receives(mut self, most: u32) -> Self {
        self.max_receives = most.max(1);
        self
    }

    /// How often [`Inbox::receive`] looks again while it waits.
    #[must_use]
    pub fn with_poll_interval(mut self, poll: Duration) -> Self {
        self.poll = poll.max(Duration::from_millis(1));
        self
    }

    fn queues(&self) -> Queues<'_> {
        Queues::new(&self.keyspace)
    }

    /// Sends `body` to the queue, visible after `delay`; returns its id.
    pub fn send(&self, body: &[u8], delay: Duration) -> Result<u64, QueueError> {
        self.queues().send(&self.queue, body, millis(delay), now_ms())
    }

    /// Receives the oldest visible message, waiting up to `wait` for one; `None` when none came.
    pub fn receive(&self, wait: Duration) -> Result<Option<Message>, QueueError> {
        let deadline = Instant::now() + wait;
        loop {
            let now = now_ms();
            let leased = self.queues().receive(&self.queue, 1, millis(self.lease), now)?;
            match leased.into_iter().next() {
                Some(message) if message.receive_count > self.max_receives => {
                    self.queues().dead_letter(&self.queue, message.id, now)?;
                    continue;
                }
                Some(message) => return Ok(Some(message)),
                None => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            std::thread::sleep(self.poll.min(left));
        }
    }

    /// Settles a received message; returns whether it was still leased to this queue.
    pub fn settle(&self, message: &Message, outcome: Outcome) -> Result<bool, QueueError> {
        match outcome {
            Outcome::Done => self.queues().ack(&self.queue, message.id),
            Outcome::Retry => self.queues().release(&self.queue, message.id, now_ms()),
            Outcome::Reject => self.queues().dead_letter(&self.queue, message.id, now_ms()),
        }
    }

    /// Receives one message, waiting up to `wait`, hands it to `handler` and settles it as the
    /// handler says; `None` when no message came.
    pub fn handle(&self, wait: Duration, handler: impl FnOnce(&Message) -> Outcome) -> Result<Option<Outcome>, QueueError> {
        let Some(message) = self.receive(wait)? else {
            return Ok(None);
        };
        let outcome = handler(&message);
        self.settle(&message, outcome)?;
        Ok(Some(outcome))
    }
}

#[cfg(test)]
mod tests {
    use super::{Inbox, Outcome};
    use crate::Database;
    use std::time::Duration;

    #[test]
    fn messages_are_handled_retried_and_dead_lettered_in_process() {
        let db = Database::in_memory().unwrap();
        let inbox = Inbox::open(&db, "jobs").unwrap().with_max_receives(2);
        inbox.send(b"resize photo 7", Duration::ZERO).unwrap();
        inbox.send(b"poison", Duration::ZERO).unwrap();
        assert_eq!(inbox.handle(Duration::ZERO, |message| {
            assert_eq!(message.body, b"resize photo 7");
            Outcome::Done
        }).unwrap(), Some(Outcome::Done));
        assert_eq!(inbox.handle(Duration::ZERO, |_| Outcome::Retry).unwrap(), Some(Outcome::Retry));
        assert_eq!(inbox.handle(Duration::ZERO, |_| Outcome::Retry).unwrap(), Some(Outcome::Retry));
        assert_eq!(inbox.receive(Duration::ZERO).unwrap(), None);
        let dead = inbox.dead_letters().unwrap().receive(Duration::ZERO).unwrap().unwrap();
        assert_eq!(dead.body, b"poison");
    }
}
