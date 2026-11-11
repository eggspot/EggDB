use crate::commands::Shared;
use eggdb_bson::{Bson, Document};
use eggdb_documents::{DocumentError, Filter, Projection, matches};
use std::time::{Duration, Instant};

const EVENTS_PER_READ: usize = 1_000;
const DEFAULT_AWAIT: Duration = Duration::from_secs(1);
const LONGEST_AWAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

/// A tailable cursor's place on a capped collection: the change log sequence it reads inserts
/// from next, the find's filter and projection, and the ids the first batch already returned
/// that inserts logged before it finished must not repeat.
#[derive(Debug, Clone)]
pub(crate) struct Tail {
    pub(crate) next: u64,
    pub(crate) database: String,
    pub(crate) collection: String,
    pub(crate) filter: Filter,
    pub(crate) projection: Option<Projection>,
    pub(crate) await_data: bool,
    pub(crate) returned_until: u64,
    pub(crate) returned_ids: Vec<Bson>,
}

impl Tail {
    fn inserted(&mut self, shared: &Shared<'_>) -> Result<Vec<Document>, DocumentError> {
        let stored = shared.store.changes_from(self.next, EVENTS_PER_READ)?;
        let mut found = Vec::new();
        for change in &stored {
            let Some(sequence) = change.get_i64("sequence").and_then(|value| u64::try_from(value).ok()) else {
                continue;
            };
            self.next = self.next.max(sequence + 1);
            if change.get_str("operationType") != Some("insert")
                || change.get_str("db") != Some(self.database.as_str())
                || change.get_str("coll") != Some(self.collection.as_str())
            {
                continue;
            }
            let Some(Bson::Document(inserted)) = change.get("fullDocument") else {
                continue;
            };
            if sequence < self.returned_until
                && inserted.get("_id").is_some_and(|id| self.returned_ids.contains(id))
            {
                continue;
            }
            if !matches(&self.filter, inserted) {
                continue;
            }
            found.push(match &self.projection {
                Some(projection) => projection.apply_matching(inserted, &self.filter),
                None => inserted.clone(),
            });
        }
        if self.next >= self.returned_until {
            self.returned_ids.clear();
        }
        Ok(found)
    }
}

/// The documents inserted into a tailable cursor's collection since its last batch, waiting up
/// to `maxTimeMS` (one second when absent, at most ten) for the first with `awaitData`.
pub(crate) fn get_more(
    shared: &Shared<'_>,
    mut tail: Tail,
    body: &Document,
) -> Result<(Vec<Document>, Tail), DocumentError> {
    let wait = if tail.await_data {
        body.get("maxTimeMS")
            .and_then(Bson::as_i64)
            .and_then(|millis| u64::try_from(millis).ok())
            .map_or(DEFAULT_AWAIT, Duration::from_millis)
            .min(LONGEST_AWAIT)
    } else {
        Duration::ZERO
    };
    let deadline = Instant::now() + wait;
    loop {
        let found = tail.inserted(shared)?;
        if !found.is_empty() || Instant::now() >= deadline {
            return Ok((found, tail));
        }
        std::thread::sleep(POLL);
    }
}
