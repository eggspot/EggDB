use crate::error::{SqlError, SqlResult, SqlState};
use std::cell::RefCell;
use std::collections::BTreeSet;

const MAX_PAYLOAD_BYTES: usize = 7999;
const MAX_CHANNEL_BYTES: usize = 63;
const MAX_QUEUED: usize = 10_000;

/// A notification `NOTIFY` or `pg_notify` sent, delivered to listeners when its transaction
/// commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The channel it was sent on.
    pub channel: String,
    /// Its payload, empty when none was given.
    pub payload: String,
}

#[derive(Debug, Default)]
pub(crate) struct Notifications {
    listening: BTreeSet<String>,
    pending: Vec<Notification>,
    committed: Vec<Notification>,
}

impl Notifications {
    pub(crate) fn listen(&mut self, channel: &str) {
        self.listening.insert(channel.to_owned());
    }

    pub(crate) fn unlisten(&mut self, channel: Option<&str>) {
        match channel {
            Some(channel) => {
                self.listening.remove(channel);
            }
            None => self.listening.clear(),
        }
    }

    pub(crate) fn listening(&self) -> &BTreeSet<String> {
        &self.listening
    }

    pub(crate) fn queue(&mut self, notification: Notification, in_block: bool) -> SqlResult<()> {
        let queue = if in_block {
            &mut self.pending
        } else {
            &mut self.committed
        };
        if queue.contains(&notification) {
            return Ok(());
        }
        if queue.len() >= MAX_QUEUED {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                "too many notifications in the NOTIFY queue",
            ));
        }
        queue.push(notification);
        Ok(())
    }

    pub(crate) fn commit(&mut self) {
        for notification in std::mem::take(&mut self.pending) {
            if !self.committed.contains(&notification) && self.committed.len() < MAX_QUEUED {
                self.committed.push(notification);
            }
        }
    }

    pub(crate) fn rollback(&mut self) {
        self.pending.clear();
    }

    pub(crate) fn take_committed(&mut self) -> Vec<Notification> {
        std::mem::take(&mut self.committed)
    }
}

pub(crate) fn checked(channel: &str, payload: &str) -> SqlResult<Notification> {
    if channel.is_empty() {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "channel name cannot be empty",
        ));
    }
    if channel.len() > MAX_CHANNEL_BYTES {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "channel name too long",
        ));
    }
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "payload string too long",
        ));
    }
    Ok(Notification {
        channel: channel.to_owned(),
        payload: payload.to_owned(),
    })
}

thread_local! {
    static RAISED: RefCell<Option<Vec<Notification>>> = const { RefCell::new(None) };
}

pub(crate) struct RaisedGuard(Option<Vec<Notification>>);

impl Drop for RaisedGuard {
    fn drop(&mut self) {
        let outer = self.0.take();
        RAISED.with(|raised| *raised.borrow_mut() = outer);
    }
}

pub(crate) fn enter_statement() -> RaisedGuard {
    RaisedGuard(RAISED.with(|raised| raised.borrow_mut().replace(Vec::new())))
}

pub(crate) fn raise(channel: &str, payload: &str) -> SqlResult<()> {
    let notification = checked(channel, payload)?;
    RAISED.with(|raised| match raised.borrow_mut().as_mut() {
        Some(list) if list.len() >= MAX_QUEUED => Err(SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            "too many notifications in the NOTIFY queue",
        )),
        Some(list) => {
            list.push(notification);
            Ok(())
        }
        None => Err(SqlError::not_supported(
            "pg_notify() outside a statement of a session",
        )),
    })
}

pub(crate) fn take_raised() -> Vec<Notification> {
    RAISED.with(|raised| {
        raised
            .borrow_mut()
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(channel: &str) -> Notification {
        checked(channel, "x").unwrap()
    }

    #[test]
    fn a_block_delivers_its_notifications_once_at_commit_and_none_after_rollback() {
        let mut notifications = Notifications::default();
        notifications.queue(note("a"), true).unwrap();
        notifications.queue(note("a"), true).unwrap();
        assert!(notifications.take_committed().is_empty());
        notifications.commit();
        assert_eq!(notifications.take_committed(), [note("a")]);
        notifications.queue(note("b"), true).unwrap();
        notifications.rollback();
        notifications.commit();
        assert!(notifications.take_committed().is_empty());
        notifications.queue(note("c"), false).unwrap();
        assert_eq!(notifications.take_committed(), [note("c")]);
    }

    #[test]
    fn channels_and_payloads_have_postgresql_limits() {
        assert!(checked("", "").is_err());
        assert!(checked(&"c".repeat(64), "").is_err());
        assert!(checked("c", &"p".repeat(8000)).is_err());
        assert!(checked("c", &"p".repeat(7999)).is_ok());
    }

    #[test]
    fn pg_notify_needs_a_statement_scope() {
        assert!(raise("a", "").is_err());
        let guard = enter_statement();
        raise("a", "1").unwrap();
        assert_eq!(take_raised(), [checked("a", "1").unwrap()]);
        drop(guard);
        assert!(raise("a", "").is_err());
    }
}
