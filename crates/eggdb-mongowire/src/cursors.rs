use eggdb_bson::Document;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const MOST_CURSORS: usize = 1_000;
const IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);

/// Documents a `find` or `aggregate` has not returned yet.
#[derive(Debug)]
pub(crate) struct Cursor {
    pub(crate) namespace: String,
    pub(crate) remaining: VecDeque<Document>,
    last_used: Instant,
}

/// The server's open cursors, shared by every connection as drivers may continue a cursor on
/// another connection of their pool. Cursors idle for ten minutes close, and the oldest close
/// first when there are too many.
#[derive(Debug, Default)]
pub(crate) struct Cursors {
    open: HashMap<i64, Cursor>,
    next: u64,
    seed: u64,
}

impl Cursors {
    pub(crate) fn new(seed: u64) -> Self {
        Self { open: HashMap::new(), next: 1, seed }
    }

    fn sweep(&mut self, now: Instant) {
        self.open.retain(|_, cursor| now.duration_since(cursor.last_used) < IDLE_LIMIT);
        while self.open.len() >= MOST_CURSORS {
            let Some(oldest) = self.open.iter().min_by_key(|(_, cursor)| cursor.last_used).map(|(id, _)| *id) else {
                break;
            };
            self.open.remove(&oldest);
        }
    }

    pub(crate) fn open(&mut self, namespace: String, remaining: VecDeque<Document>) -> i64 {
        let now = Instant::now();
        self.sweep(now);
        let mut mixed = self.next.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ self.seed;
        mixed ^= mixed >> 29;
        self.next += 1;
        let id = (mixed & 0x7fff_ffff_ffff_ffff).max(1) as i64;
        self.open.insert(id, Cursor { namespace, remaining, last_used: now });
        id
    }

    pub(crate) fn take(&mut self, id: i64) -> Option<Cursor> {
        self.open.remove(&id)
    }

    pub(crate) fn put_back(&mut self, id: i64, mut cursor: Cursor) {
        cursor.last_used = Instant::now();
        self.open.insert(id, cursor);
    }

    pub(crate) fn kill(&mut self, id: i64) -> bool {
        self.open.remove(&id).is_some()
    }
}
