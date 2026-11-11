use eggdb_bson::{Bson, Document};
use eggdb_keyspace::{Entries, EntryVisitor, KeysRead, KeysWrite};

const CHANGE_RECORD: u8 = 4;
const CHANGE_COUNTER: &[u8] = &[5];
const DOCUMENT_RECORD: u8 = 2;
/// How many change events the file keeps; older ones are deleted as new ones arrive, as an
/// oplog of fixed size forgets its oldest entries.
pub const KEPT_CHANGES: u64 = 100_000;

/// A write transaction that remembers every document it stores or deletes, with the bytes
/// the document had before, so the store can append change events in the same transaction.
pub(crate) struct Recording<'a> {
    pub(crate) inner: &'a mut dyn KeysWrite,
    pub(crate) changes: Vec<(Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)>,
}

impl KeysRead for Recording<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.inner.get(key)
    }

    fn keys(&self) -> Result<Vec<Vec<u8>>, String> {
        self.inner.keys()
    }

    fn visit_keys(&self, visit: &mut dyn FnMut(Vec<u8>) -> Result<bool, String>) -> Result<(), String> {
        self.inner.visit_keys(visit)
    }

    fn scan(&self, prefix: &[u8]) -> Result<Entries, String> {
        self.inner.scan(prefix)
    }

    fn visit_prefix(&self, prefix: &[u8], visit: &mut EntryVisitor<'_>) -> Result<(), String> {
        self.inner.visit_prefix(prefix, visit)
    }
}

impl KeysWrite for Recording<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> Result<(), String> {
        if key.first() == Some(&DOCUMENT_RECORD) {
            let before = self.inner.get(key)?;
            self.changes.push((key.to_vec(), before, Some(value.to_vec())));
        }
        self.inner.put(key, value)
    }

    fn delete(&mut self, key: &[u8]) -> Result<bool, String> {
        if key.first() == Some(&DOCUMENT_RECORD)
            && let Some(before) = self.inner.get(key)?
        {
            self.changes.push((key.to_vec(), Some(before), None));
        }
        self.inner.delete(key)
    }

    fn clear(&mut self) -> Result<(), String> {
        self.inner.clear()
    }
}

fn change_key(sequence: u64) -> Vec<u8> {
    let mut key = vec![CHANGE_RECORD];
    key.extend_from_slice(&sequence.to_be_bytes());
    key
}

/// The sequence number the next change event takes; every event before it is in the log.
pub(crate) fn position(keys: &dyn KeysRead) -> Result<u64, String> {
    Ok(keys
        .get(CHANGE_COUNTER)?
        .and_then(|bytes| bytes.get(..8).and_then(|raw| raw.try_into().ok()))
        .map_or(1, u64::from_be_bytes))
}

fn namespace_of(key: &[u8]) -> Option<(String, String)> {
    let rest = key.get(1..)?;
    let mut parts = rest.splitn(3, |byte| *byte == 0);
    let database = String::from_utf8(parts.next()?.to_vec()).ok()?;
    let collection = String::from_utf8(parts.next()?.to_vec()).ok()?;
    Some((database, collection))
}

fn update_description(before: &Document, after: &Document) -> Document {
    let mut updated = Vec::new();
    for (name, value) in after.iter() {
        if name != "_id" && before.get(name) != Some(value) {
            updated.push((name.clone(), value.clone()));
        }
    }
    let removed: Vec<Bson> = before
        .iter()
        .filter(|(name, _)| after.get(name).is_none())
        .map(|(name, _)| Bson::String(name.clone()))
        .collect();
    Document::from_entries(vec![
        ("updatedFields".to_owned(), Bson::Document(Document::from_entries(updated))),
        ("removedFields".to_owned(), Bson::Array(removed)),
        ("truncatedArrays".to_owned(), Bson::Array(Vec::new())),
    ])
}

/// Appends one event per recorded document change to the log, in the order they happened,
/// deleting events older than the last [`KEPT_CHANGES`].
pub(crate) fn append(
    keys: &mut dyn KeysWrite,
    changes: Vec<(Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>)>,
    wall_ms: i64,
) -> Result<(), String> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut next = position(keys)?;
    for (key, before, after) in changes {
        let Some((database, collection)) = namespace_of(&key) else {
            continue;
        };
        let before = before.and_then(|bytes| Document::from_bytes(&bytes).ok());
        let after = after.and_then(|bytes| Document::from_bytes(&bytes).ok());
        let id = after
            .as_ref()
            .or(before.as_ref())
            .and_then(|document| document.get("_id").cloned())
            .unwrap_or(Bson::Null);
        let (operation, extra) = match (&before, &after) {
            (None, Some(document)) => ("insert", Some(("fullDocument", Bson::Document(document.clone())))),
            (Some(old), Some(new)) => (
                "update",
                Some(("updateDescription", Bson::Document(update_description(old, new)))),
            ),
            (Some(_), None) => ("delete", None),
            (None, None) => continue,
        };
        let mut entries = vec![
            ("sequence".to_owned(), Bson::Int64(next as i64)),
            ("operationType".to_owned(), Bson::String(operation.to_owned())),
            ("db".to_owned(), Bson::String(database)),
            ("coll".to_owned(), Bson::String(collection)),
            ("documentKey".to_owned(), Bson::Document(Document::from_entries(vec![("_id".to_owned(), id)]))),
            ("wallTime".to_owned(), Bson::DateTime(wall_ms)),
        ];
        if let Some((name, value)) = extra {
            entries.push((name.to_owned(), value));
        }
        if let Some(document) = &after
            && operation == "update"
        {
            entries.push(("afterChange".to_owned(), Bson::Document(document.clone())));
        }
        keys.put(&change_key(next), &Document::from_entries(entries).to_bytes())?;
        if next > KEPT_CHANGES {
            keys.delete(&change_key(next - KEPT_CHANGES))?;
        }
        next += 1;
    }
    keys.put(CHANGE_COUNTER, &next.to_be_bytes())
}

/// The stored change events from `after` on (inclusive), oldest first, at most `limit`.
pub(crate) fn events_from(keys: &dyn KeysRead, after: u64, limit: usize) -> Result<Vec<Document>, String> {
    let mut found = Vec::new();
    let start = change_key(after);
    keys.visit_prefix(&[CHANGE_RECORD], &mut |key, value| {
        if key < start.as_slice() {
            return Ok(true);
        }
        if let Ok(event) = Document::from_bytes(value) {
            found.push(event);
        }
        Ok(found.len() < limit)
    })?;
    Ok(found)
}
