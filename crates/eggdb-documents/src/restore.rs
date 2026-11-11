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
            Ok(0) => {
                return Err(DocumentError::failed_to_parse(
                    "the dump ends inside a document's length",
                ));
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(DocumentError::storage(format!(
                    "cannot read the dump: {error}"
                )));
            }
        }
    }
    let length = i32::from_le_bytes(length_bytes);
    let Some(length) = usize::try_from(length)
        .ok()
        .filter(|length| (5..=MAX_DOCUMENT_BYTES).contains(length))
    else {
        return Err(DocumentError::failed_to_parse(format!(
            "a document in the dump has the impossible length {length}"
        )));
    };
    let mut bytes = vec![0u8; length];
    bytes[..4].copy_from_slice(&length_bytes);
    reader.read_exact(&mut bytes[4..]).map_err(|error| {
        DocumentError::failed_to_parse(format!("the dump ends inside a document: {error}"))
    })?;
    Ok(Some(bytes))
}

fn flush(
    store: &Store<'_>,
    database: &str,
    collection: &str,
    pending: &mut Vec<Document>,
    outcome: &mut RestoredCollection,
) -> DocumentResult<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let inserted = store.insert(database, collection, std::mem::take(pending), false)?;
    outcome.documents += inserted.inserted;
    outcome.failed += inserted.errors.len() as u64;
    if outcome.first_error.is_none() {
        outcome.first_error = inserted
            .errors
            .first()
            .map(|(_, error)| error.message.clone());
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
        let mut unique = Vec::new();
        for index in metadata.get_array("indexes").into_iter().flatten() {
            let Some(index) = index.as_document() else {
                continue;
            };
            let name = index.get_str("name").unwrap_or_default().to_owned();
            if name == "_id_" {
                continue;
            }
            let mut definition = Document::new();
            definition.insert("key", index.get("key").cloned().unwrap_or(Bson::Null));
            definition.insert("name", Bson::String(name));
            for option in ["unique", "sparse"] {
                if index.get_bool(option) == Some(true) {
                    definition.insert(option, Bson::Boolean(true));
                }
            }
            if index.get_bool("unique") == Some(true) {
                unique.push(definition);
            } else {
                wanted.push(definition);
            }
        }
        let (before, after) = store.create_indexes(database, collection, &wanted)?;
        outcome.indexes = (after - before) as u64;
        for definition in unique {
            let name = definition.get_str("name").unwrap_or_default().to_owned();
            match store.create_indexes(database, collection, std::slice::from_ref(&definition)) {
                Ok((before, after)) => outcome.indexes += (after - before) as u64,
                Err(error) if error.code == 11000 => outcome.skipped_indexes.push(format!(
                    "{name}: the restored documents break its uniqueness ({})",
                    error.message
                )),
                Err(error) => return Err(error),
            }
        }
    }
    Ok(outcome)
}

/// The first four bytes of a `mongodump --archive` file.
pub const MONGODUMP_ARCHIVE_MAGIC: [u8; 4] = [0x6d, 0xe2, 0x99, 0x81];

const ARCHIVE_TERMINATOR: [u8; 4] = [0xff; 4];

/// The most bytes a gzipped dump file may expand to.
pub const MAX_GUNZIPPED_DUMP_BYTES: usize = u32::MAX as usize;

/// One collection read out of a `mongodump --archive` file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchivedCollection {
    /// The database.
    pub database: String,
    /// The collection.
    pub collection: String,
    /// Its metadata JSON (options and indexes), as a `.metadata.json` file holds it.
    pub metadata: Option<String>,
    /// Its documents one after another, as a `.bson` file holds them.
    pub documents: Vec<u8>,
}

/// Decompresses a gzipped `mongodump` file (`--gzip` output, or a gzipped archive).
pub fn gunzip_dump(bytes: &[u8]) -> DocumentResult<Vec<u8>> {
    eggdb_content::gunzip(bytes, MAX_GUNZIPPED_DUMP_BYTES).map_err(|error| {
        DocumentError::failed_to_parse(format!("the gzipped dump cannot be read: {error}"))
    })
}

fn archive_document(bytes: &[u8], at: usize) -> DocumentResult<(Document, usize)> {
    let rest = bytes.get(at..).unwrap_or_default();
    eggdb_bson::read_document(rest).map_err(|error| {
        DocumentError::failed_to_parse(format!("the archive is damaged at byte {at}: {error}"))
    })
}

fn at_terminator(bytes: &[u8], at: usize) -> bool {
    bytes.get(at..at + 4) == Some(&ARCHIVE_TERMINATOR[..])
}

/// Reads a `mongodump --archive` file: the magic number, a prelude (a header document, then
/// one metadata document per collection, then a terminator), and blocks of documents, each
/// after a namespace header and ending with a terminator, possibly interleaved between
/// collections.
pub fn read_mongodump_archive(bytes: &[u8]) -> DocumentResult<Vec<ArchivedCollection>> {
    if !bytes.starts_with(&MONGODUMP_ARCHIVE_MAGIC) {
        return Err(DocumentError::failed_to_parse(
            "the file is not a mongodump archive: its magic number is missing",
        ));
    }
    let mut at = MONGODUMP_ARCHIVE_MAGIC.len();
    let (_, used) = archive_document(bytes, at)?;
    at += used;
    let mut collections: Vec<ArchivedCollection> = Vec::new();
    let position_of = |collections: &mut Vec<ArchivedCollection>, database: &str, collection: &str| {
        match collections
            .iter()
            .position(|known| known.database == database && known.collection == collection)
        {
            Some(position) => position,
            None => {
                collections.push(ArchivedCollection {
                    database: database.to_owned(),
                    collection: collection.to_owned(),
                    ..ArchivedCollection::default()
                });
                collections.len() - 1
            }
        }
    };
    loop {
        if at_terminator(bytes, at) {
            at += 4;
            break;
        }
        if at >= bytes.len() {
            return Err(DocumentError::failed_to_parse("the archive's prelude is not terminated"));
        }
        let (metadata, used) = archive_document(bytes, at)?;
        at += used;
        let database = metadata.get_str("db").unwrap_or_default().to_owned();
        let collection = metadata.get_str("collection").unwrap_or_default().to_owned();
        let position = position_of(&mut collections, &database, &collection);
        collections[position].metadata = metadata
            .get_str("metadata")
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
    }
    while at < bytes.len() {
        let (header, used) = archive_document(bytes, at)?;
        at += used;
        let database = header.get_str("db").unwrap_or_default().to_owned();
        let collection = header.get_str("collection").unwrap_or_default().to_owned();
        let position = position_of(&mut collections, &database, &collection);
        loop {
            if at_terminator(bytes, at) {
                at += 4;
                break;
            }
            let length = bytes
                .get(at..at + 4)
                .and_then(|length| length.try_into().ok())
                .map(i32::from_le_bytes)
                .and_then(|length| usize::try_from(length).ok())
                .filter(|length| (5..=MAX_DOCUMENT_BYTES).contains(length))
                .ok_or_else(|| {
                    DocumentError::failed_to_parse(format!(
                        "the archive has a document of impossible length at byte {at}"
                    ))
                })?;
            let Some(document) = bytes.get(at..at + length) else {
                return Err(DocumentError::failed_to_parse("the archive ends inside a document"));
            };
            collections[position].documents.extend_from_slice(document);
            at += length;
        }
    }
    collections.retain(|archived| !archived.collection.is_empty());
    Ok(collections)
}
