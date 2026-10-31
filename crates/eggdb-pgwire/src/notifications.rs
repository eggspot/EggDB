use eggdb_sql::Notification;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Mutex;

const MAX_WAITING_PER_LISTENER: usize = 10_000;

pub(crate) struct Delivered {
    pub(crate) sender: i32,
    pub(crate) channel: String,
    pub(crate) payload: String,
}

#[derive(Default)]
struct Listener {
    channels: BTreeSet<String>,
    waiting: VecDeque<Delivered>,
}

#[derive(Default)]
pub(crate) struct NotificationHub {
    listeners: Mutex<HashMap<i32, Listener>>,
}

impl NotificationHub {
    fn locked(&self) -> std::sync::MutexGuard<'_, HashMap<i32, Listener>> {
        self.listeners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn exchange(
        &self,
        process_id: i32,
        channels: Vec<String>,
        sent: Vec<Notification>,
    ) -> Vec<Delivered> {
        let mut listeners = self.locked();
        if channels.is_empty() {
            if let Some(listener) = listeners.get_mut(&process_id) {
                listener.channels.clear();
            }
        } else {
            listeners.entry(process_id).or_default().channels = channels.into_iter().collect();
        }
        for notification in sent {
            for listener in listeners.values_mut() {
                if listener.channels.contains(&notification.channel)
                    && listener.waiting.len() < MAX_WAITING_PER_LISTENER
                {
                    listener.waiting.push_back(Delivered {
                        sender: process_id,
                        channel: notification.channel.clone(),
                        payload: notification.payload.clone(),
                    });
                }
            }
        }
        let delivered = listeners
            .get_mut(&process_id)
            .map(|listener| listener.waiting.drain(..).collect())
            .unwrap_or_default();
        listeners.retain(|_, listener| !listener.channels.is_empty() || !listener.waiting.is_empty());
        delivered
    }

    pub(crate) fn waiting(&self, process_id: i32) -> Vec<Delivered> {
        self.locked()
            .get_mut(&process_id)
            .map(|listener| listener.waiting.drain(..).collect())
            .unwrap_or_default()
    }

    pub(crate) fn remove(&self, process_id: i32) {
        self.locked().remove(&process_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(channel: &str, payload: &str) -> Vec<Notification> {
        vec![Notification {
            channel: channel.to_owned(),
            payload: payload.to_owned(),
        }]
    }

    #[test]
    fn notifications_reach_every_listener_of_their_channel_and_no_one_else() {
        let hub = NotificationHub::default();
        assert!(hub.exchange(1, vec!["jobs".into()], Vec::new()).is_empty());
        assert!(hub.exchange(2, vec!["mail".into()], Vec::new()).is_empty());
        let own = hub.exchange(1, vec!["jobs".into()], sent("jobs", "42"));
        assert_eq!(own.len(), 1);
        assert_eq!((own[0].sender, own[0].payload.as_str()), (1, "42"));
        assert!(hub.waiting(2).is_empty());
        hub.exchange(3, Vec::new(), sent("mail", "hi"));
        let mail = hub.waiting(2);
        assert_eq!((mail.len(), mail[0].sender), (1, 3));
        hub.remove(2);
        hub.exchange(3, Vec::new(), sent("mail", "again"));
        assert!(hub.waiting(2).is_empty());
    }
}
