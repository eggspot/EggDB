use eggdb_sql::CancelSignal;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

pub(crate) const SECRET_KEY_BYTES: usize = 4;

struct Registered {
    secret_key: [u8; SECRET_KEY_BYTES],
    signal: CancelSignal,
}

pub(crate) struct BackendKeys {
    registered: Mutex<HashMap<i32, Registered>>,
}

pub(crate) struct Registration<'k> {
    keys: &'k BackendKeys,
    process_id: i32,
}

impl Drop for Registration<'_> {
    fn drop(&mut self) {
        self.keys.lock().remove(&self.process_id);
    }
}

impl BackendKeys {
    pub(crate) fn new() -> Self {
        Self {
            registered: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<i32, Registered>> {
        self.registered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn register(
        &self,
        process_id: i32,
        secret_key: [u8; SECRET_KEY_BYTES],
        signal: CancelSignal,
    ) -> Registration<'_> {
        self.lock().insert(process_id, Registered { secret_key, signal });
        Registration {
            keys: self,
            process_id,
        }
    }

    pub(crate) fn cancel(&self, process_id: i32, secret_key: &[u8]) -> bool {
        let registered = self.lock();
        let Some(found) = registered.get(&process_id) else {
            return false;
        };
        if !keys_match(&found.secret_key, secret_key) {
            return false;
        }
        found.signal.cancel();
        true
    }
}

fn keys_match(expected: &[u8], given: &[u8]) -> bool {
    if expected.len() != given.len() {
        return false;
    }
    expected
        .iter()
        .zip(given)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::BackendKeys;
    use eggdb_sql::CancelSignal;

    #[test]
    fn only_the_right_key_cancels_and_only_while_registered() {
        let keys = BackendKeys::new();
        let signal = CancelSignal::default();
        let registration = keys.register(7, [1, 2, 3, 4], signal.clone());
        assert!(!keys.cancel(7, &[1, 2, 3, 5]));
        assert!(!keys.cancel(7, &[1, 2, 3]));
        assert!(!keys.cancel(8, &[1, 2, 3, 4]));
        assert!(!signal.is_cancelled());
        assert!(keys.cancel(7, &[1, 2, 3, 4]));
        assert!(signal.is_cancelled());
        drop(registration);
        signal.clear();
        assert!(!keys.cancel(7, &[1, 2, 3, 4]));
        assert!(!signal.is_cancelled());
    }
}
