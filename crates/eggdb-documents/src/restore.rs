use crate::error::{DocumentError, DocumentResult};
use crate::store::{MAX_DOCUMENT_BYTES, Store};
use eggdb_bson::{Bson, Document};
use std::io::Read;

const BATCH: usize = 1_000;

/// What restoring one collection from `mongodump` output did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoredCollection {
    /// Documents inserted.
    pub documents: u64,
    /// Documents that could not be inserted (a duplicate `_id`, for example).
    pub failed: u64,
    /// The first failure's message.
    pub first_error: Option<String>,
    /// Index definitions recorded.
    pub indexes: u64,
    /// Index definitions left out, with why.
    pub skipped_indexes: Vec<String>,
}

fn next_document(reader: &mut impl Read) -> DocumentResult<Option<Vec<u8>>> {
    let mut length_bytes = [0u8; 4];
    let mut filled = 0;
    while filled < 4 {
        match reader.read(&mut length_bytes[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(DocumentError::failed_to_parse("the dump ends inside a document's length")),
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(DocumentError::storage(format!("cannot read the dump: {error}"))),
        }
    }
    let length = i32::from_le_bytes(length_bytes);
    let Some(length) = usize::try_from(length).ok().filter(|length| (5..=MAX_DOCUMENT_BYTES).contains(length)) else {
        return Err(DocumentError::failed_to_parse(format!("a document in the dump has the impossible length {length}")));
    };
    let mut bytes = vec![0u8; length];
    bytes[..4].copy_from_slice(&length_bytes);
    reader
        .read_exact(&mut bytes[4..])
        .map_err(|error| DocumentError::failed_to_parse(format!("the dump ends inside a document: {error}")))?;
    Ok(Some(bytes))
}

fn flush(store: &Store<'_>, database: &str, collection: &str, pending: &mut Vec<Document>, outcome: &mut RestoredCollection) -> DocumentResult<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let inserted = store.insert(database, collection, std::mem::take(pending), false)?;
    outcome.documents += inserted.inserted;
    outcome.failed += inserted.errors.len() as u64;
    if outcome.first_error.is_none() {
        outcome.first_error = inserted.errors.first().map(|(_, error)| error.message.clone());
    }
    Ok(())
}

/// Restores one collection that `mongodump` wrote: `documents` is its `.bson` file (documents
/// one after another) and `metadata` its `.metadata.json`, when there is one, whose index
/// definitions are recorded.
pub fn restore_collection(
    store: &Store<'_>,
    database: &str,
    collection: &str,
    mut documents: impl Read,
    metadata: Option<&str>,
) -> DocumentResult<RestoredCollection> {
    let mut outcome = RestoredCollection::default();
    match store.create_collection(database, collection) {
        Ok(()) => {}
        Err(error) if error.code == 48 => {}
        Err(error) => return Err(error),
    }
    let mut pending = Vec::with_capacity(BATCH);
    while let Some(bytes) = next_document(&mut documents)? {
        match Document::from_bytes(&bytes) {
            Ok(document) => pending.push(document),
            Err(error) => {
                outcome.failed += 1;
                outcome.first_error.get_or_insert_with(|| error.to_string());
            }
        }
        if pending.len() >= BATCH {
            flush(store, database, collection, &mut pending, &mut outcome)?;
        }
    }
    flush(store, database, collection, &mut pending, &mut outcome)?;
    if let Some(metadata) = metadata {
        let metadata = crate::extended_json::parse_extended_json(metadata)?;
        let mut wanted = Vec::new();
        for index in metadata.get_array("indexes").into_iter().flatten() {
            let Some(index) = index.as_document() else {
                continue;
            };
            let name = index.get_str("name").unwrap_or_default().to_owned();
            if name == "_id_" {
                continue;
            }
            if index.get_bool("unique") == Some(true) {
                outcome.skipped_indexes.push(format!("{name}: unique indexes besides _id are not enforced yet"));
                continue;
            }
            let mut definition = Document::new();
            definition.insert("key", index.get("key").cloned().unwrap_or(Bson::Null));
            definition.insert("name", Bson::String(name));
            wanted.push(definition);
        }
        let (before, after) = store.create_indexes(database, collection, &wanted)?;
        outcome.indexes = (after - before) as u64;
    }
    Ok(outcome)
}
