use eggdb_bson::Document;
use eggdb_platform::connection_limits::{MemoryBudget, Reservation};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const MOST_CURSORS: usize = 1_000;
const IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);

/// Documents a `find` or `aggregate` has not returned yet, holding their bytes against the
/// server's memory budget until they are sent or the cursor closes.
#[derive(Debug)]
pub(crate) struct Cursor<'b> {
    pub(crate) namespace: String,
    pub(crate) remaining: VecDeque<Document>,
    pub(crate) change_stream: Option<crate::change_streams::ChangeStream>,
    pub(crate) tail: Option<crate::tailable::Tail>,
    held: Reservation<'b>,
    last_used: Instant,
}

impl Cursor<'_> {
    pub(crate) fn release_sent(&mut self, bytes: usize) {
        let left = self.held.bytes().saturating_sub(bytes);
        self.held.resize(left);
    }
}

/// The server's open cursors, shared by every connection as drivers may continue a cursor on
/// another connection of their pool. Cursors idle for ten minutes close, and the oldest close
/// first when there are too many.
#[derive(Debug)]
pub(crate) struct Cursors<'b> {
    open: HashMap<i64, Cursor<'b>>,
    next: u64,
    seed: u64,
    budget: &'b MemoryBudget,
}

impl<'b> Cursors<'b> {
    pub(crate) fn new(seed: u64, budget: &'b MemoryBudget) -> Self {
        Self {
            open: HashMap::new(),
            next: 1,
            seed,
            budget,
        }
    }

    fn sweep(&mut self, now: Instant) {
        self.open
            .retain(|_, cursor| now.duration_since(cursor.last_used) < IDLE_LIMIT);
        while self.open.len() >= MOST_CURSORS {
            let Some(oldest) = self
                .open
                .iter()
                .min_by_key(|(_, cursor)| cursor.last_used)
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.open.remove(&oldest);
        }
    }

    pub(crate) fn open(&mut self, namespace: String, remaining: VecDeque<Document>) -> Option<i64> {
        let now = Instant::now();
        self.sweep(now);
        let bytes = remaining
            .iter()
            .map(|document| document.to_bytes().len())
            .fold(namespace.len(), usize::saturating_add);
        let held = self.budget.reserve(bytes)?;
        let mut mixed = self.next.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ self.seed;
        mixed ^= mixed >> 29;
        self.next += 1;
        let id = (mixed & 0x7fff_ffff_ffff_ffff).max(1) as i64;
        self.open.insert(
            id,
            Cursor {
                namespace,
                remaining,
                change_stream: None,
                tail: None,
                held,
                last_used: now,
            },
        );
        Some(id)
    }

    /// Opens a change stream's cursor: it holds no documents, only where the stream reads next,
    /// and stays open until it is killed or idle.
    pub(crate) fn open_change_stream(
        &mut self,
        namespace: String,
        stream: crate::change_streams::ChangeStream,
    ) -> Option<i64> {
        let id = self.open(namespace, VecDeque::new())?;
        if let Some(cursor) = self.open.get_mut(&id) {
            cursor.change_stream = Some(stream);
        }
        Some(id)
    }

    /// Opens a tailable cursor on a capped collection: the first batch's leftovers, then the
    /// documents inserted after it; it stays open when empty, until it is killed or idle.
    pub(crate) fn open_tailable(
        &mut self,
        namespace: String,
        remaining: VecDeque<Document>,
        tail: crate::tailable::Tail,
    ) -> Option<i64> {
        let id = self.open(namespace, remaining)?;
        if let Some(cursor) = self.open.get_mut(&id) {
            cursor.tail = Some(tail);
        }
        Some(id)
    }

    pub(crate) fn take(&mut self, id: i64) -> Option<Cursor<'b>> {
        self.open.remove(&id)
    }

    pub(crate) fn put_back(&mut self, id: i64, mut cursor: Cursor<'b>) {
        cursor.last_used = Instant::now();
        self.open.insert(id, cursor);
    }

    pub(crate) fn kill(&mut self, id: i64) -> bool {
        self.open.remove(&id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::Cursors;
    use eggdb_bson::{Bson, Document};
    use eggdb_platform::connection_limits::MemoryBudget;
    use std::collections::VecDeque;

    fn documents(count: usize, text_bytes: usize) -> VecDeque<Document> {
        (0..count)
            .map(|_| {
                Document::from_entries(vec![("v".to_owned(), Bson::String("x".repeat(text_bytes)))])
            })
            .collect()
    }

    #[test]
    fn open_cursors_hold_their_documents_until_sent_or_killed() {
        let budget = MemoryBudget::new(10_000);
        let mut cursors = Cursors::new(1, &budget);
        let first = cursors
            .open("app.t".to_owned(), documents(4, 1_000))
            .unwrap();
        let held = budget.used_bytes();
        assert!(held > 4_000, "{held}");
        assert!(
            cursors
                .open("app.t".to_owned(), documents(8, 1_000))
                .is_none()
        );
        assert_eq!(budget.used_bytes(), held);
        let mut cursor = cursors.take(first).unwrap();
        cursor.remaining.pop_front();
        cursor.release_sent(1_000);
        assert_eq!(budget.used_bytes(), held - 1_000);
        cursors.put_back(first, cursor);
        assert!(cursors.kill(first));
        assert_eq!(budget.used_bytes(), 0);
        let taken = cursors
            .open("app.t".to_owned(), documents(8, 1_000))
            .unwrap();
        drop(cursors.take(taken));
        assert_eq!(budget.used_bytes(), 0);
    }
}
