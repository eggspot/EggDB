use crate::error::{DocumentError, DocumentResult};
use crate::filter::{Filter, matches, only_id};
use crate::sort::{SortSpec, sort_documents};
use crate::update::{Update, apply_update, upsert_document};
use eggdb_bson::{Bson, Document, new_object_id, type_rank};
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};
use std::time::{SystemTime, UNIX_EPOCH};

const COLLECTION_RECORD: u8 = 1;
const DOCUMENT_RECORD: u8 = 2;
const ROLLED_BACK: &str = "the document operation was rolled back";

/// The largest document a collection holds, as MongoDB documents it: 16 MiB.
pub const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

/// Collections of BSON documents in databases, on a [`Keyspace`] (its database 0): each
/// operation is one engine transaction.
pub struct Store<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
    process_unique: [u8; 5],
    counter_start: u32,
}

impl std::fmt::Debug for Store<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

/// What an insert did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InsertOutcome {
    /// How many documents were inserted.
    pub inserted: u64,
    /// The documents that were not, by their position, with why.
    pub errors: Vec<(usize, DocumentError)>,
}

/// What an update did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdateOutcome {
    /// How many documents matched the filter.
    pub matched: u64,
    /// How many of them changed.
    pub modified: u64,
    /// The `_id` of the document an upsert inserted.
    pub upserted: Option<Bson>,
}

/// How an update chooses and treats documents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpdateOptions {
    /// Every matching document, not only the first.
    pub multi: bool,
    /// Insert a document when none matches.
    pub upsert: bool,
}

/// What `findAndModify` does to the document it finds.
#[derive(Debug, Clone, PartialEq)]
pub enum Modification {
    /// Delete it.
    Remove,
    /// Update it, upserting when asked, and return it as it is after the update when `new`.
    Update {
        /// The update.
        update: Update,
        /// Insert when nothing matches.
        upsert: bool,
        /// Return the document after the change instead of before.
        new: bool,
    },
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

/// Checks a database name as MongoDB restricts it.
pub fn check_database_name(name: &str) -> DocumentResult<()> {
    if name.is_empty() || name.len() >= 64 || name.chars().any(|character| "/\\. \"$*<>:|?\0".contains(character)) {
        return Err(DocumentError::invalid_namespace(format!("Invalid database name: '{name}'")));
    }
    Ok(())
}

/// Checks a collection name as MongoDB restricts it.
pub fn check_collection_name(name: &str) -> DocumentResult<()> {
    if name.is_empty() || name.len() > 255 || name.contains(['\0', '$']) || name.starts_with('.') {
        return Err(DocumentError::invalid_namespace(format!("Invalid collection name: '{name}'")));
    }
    Ok(())
}

fn collection_key(database: &str, collection: &str) -> Vec<u8> {
    let mut key = vec![COLLECTION_RECORD];
    key.extend_from_slice(database.as_bytes());
    key.push(0);
    key.extend_from_slice(collection.as_bytes());
    key
}

fn documents_prefix(database: &str, collection: &str) -> Vec<u8> {
    let mut key = vec![DOCUMENT_RECORD];
    key.extend_from_slice(database.as_bytes());
    key.push(0);
    key.extend_from_slice(collection.as_bytes());
    key.push(0);
    key
}

/// The bytes that stand for an `_id` in a document's key: equal ids (such as `1` and `1.0`)
/// give the same bytes.
fn id_key(id: &Bson) -> Vec<u8> {
    let mut key = vec![type_rank(id)];
    match id {
        number if number.is_number() => match number.as_i64() {
            Some(integer) if !matches!(number, Bson::Decimal128(_)) => {
                key.push(b'i');
                key.extend_from_slice(&((integer as u64) ^ (1 << 63)).to_be_bytes());
            }
            _ => {
                let bits = number.as_f64().unwrap_or(f64::NAN).to_bits();
                let sortable = if bits >> 63 == 1 { !bits } else { bits | (1 << 63) };
                key.push(b'f');
                key.extend_from_slice(&sortable.to_be_bytes());
            }
        },
        Bson::String(text) | Bson::Symbol(text) => key.extend_from_slice(text.as_bytes()),
        Bson::ObjectId(bytes) => key.extend_from_slice(bytes),
        other => key.extend_from_slice(&Document::from_entries(vec![(String::new(), other.clone())]).to_bytes()),
    }
    key
}

fn document_key(database: &str, collection: &str, id: &Bson) -> Vec<u8> {
    let mut key = documents_prefix(database, collection);
    key.extend_from_slice(&id_key(id));
    key
}

fn decoded(bytes: &[u8]) -> DocumentResult<Document> {
    Document::from_bytes(bytes).map_err(|error| DocumentError::storage(format!("a stored document is damaged: {error}")))
}

fn check_document(document: &Document) -> DocumentResult<Vec<u8>> {
    match document.get("_id") {
        Some(Bson::Array(_)) => return Err(DocumentError::bad_value("The '_id' value cannot be of type array")),
        Some(Bson::Regex { .. }) => return Err(DocumentError::bad_value("can't use a regex for _id")),
        Some(Bson::Undefined) => return Err(DocumentError::bad_value("can't use a undefined for _id")),
        _ => {}
    }
    let bytes = document.to_bytes();
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(DocumentError::bad_value(format!("object to insert too large. size in bytes: {}, max size: {MAX_DOCUMENT_BYTES}", bytes.len())));
    }
    Ok(bytes)
}

fn duplicate(database: &str, collection: &str, id: &Bson) -> DocumentError {
    DocumentError::duplicate_key(format!(
        "E11000 duplicate key error collection: {database}.{collection} index: _id_ dup key: {{ _id: {} }}",
        eggdb_bson::relaxed_json(&Document::from_entries(vec![(String::new(), id.clone())]))
            .trim_start_matches("{\"\": ")
            .trim_end_matches('}')
    ))
}

fn ensure_collection(keys: &mut dyn KeysWrite, database: &str, collection: &str) -> DocumentResult<()> {
    check_database_name(database)?;
    check_collection_name(collection)?;
    let key = collection_key(database, collection);
    if keys.get(&key).map_err(DocumentError::storage)?.is_none() {
        keys.put(&key, &Document::new().to_bytes()).map_err(DocumentError::storage)?;
    }
    Ok(())
}

fn matching(keys: &dyn KeysRead, database: &str, collection: &str, filter: &Filter) -> DocumentResult<Vec<(Vec<u8>, Document)>> {
    if let Some(id) = only_id(filter) {
        let key = document_key(database, collection, id);
        return Ok(match keys.get(&key).map_err(DocumentError::storage)? {
            Some(bytes) => {
                let document = decoded(&bytes)?;
                if matches(filter, &document) { vec![(key, document)] } else { Vec::new() }
            }
            None => Vec::new(),
        });
    }
    let mut found = Vec::new();
    for (key, bytes) in keys.scan(&documents_prefix(database, collection)).map_err(DocumentError::storage)? {
        let document = decoded(&bytes)?;
        if matches(filter, &document) {
            found.push((key, document));
        }
    }
    Ok(found)
}

impl<'k> Store<'k> {
    /// A store on `keyspace`; `seed` (unpredictable bytes) makes this process's object ids
    /// unique.
    pub fn new(keyspace: &'k (dyn Keyspace + Sync), seed: [u8; 8]) -> Self {
        let mut process_unique = [0u8; 5];
        process_unique.copy_from_slice(&seed[..5]);
        let counter_start = u32::from_be_bytes([0, seed[5], seed[6], seed[7]]);
        Self { keyspace, process_unique, counter_start }
    }

    /// A new object id.
    pub fn new_object_id(&self) -> [u8; 12] {
        let seconds = (now_ms() / 1000) as u32;
        new_object_id(seconds, || self.process_unique, self.counter_start)
    }

    fn read<T>(&self, body: impl FnOnce(&dyn KeysRead) -> DocumentResult<T>) -> DocumentResult<T> {
        let mut body = Some(body);
        let mut out = None;
        let result = self.keyspace.read(0, &mut |keys| {
            if let Some(body) = body.take() {
                out = Some(body(keys));
            }
            Ok(())
        });
        match (result, out) {
            (Err(message), _) => Err(DocumentError::storage(message)),
            (Ok(()), Some(outcome)) => outcome,
            (Ok(()), None) => Err(DocumentError::storage("the read did not run")),
        }
    }

    fn write<T>(&self, body: impl FnOnce(&mut dyn KeysWrite) -> DocumentResult<T>) -> DocumentResult<T> {
        let mut body = Some(body);
        let mut out = None;
        let result = self.keyspace.write(0, &mut |keys| {
            let Some(body) = body.take() else {
                return Err(ROLLED_BACK.to_owned());
            };
            let outcome = body(keys);
            let failed = outcome.is_err();
            out = Some(outcome);
            if failed { Err(ROLLED_BACK.to_owned()) } else { Ok(()) }
        });
        match (result, out) {
            (_, Some(Err(error))) => Err(error),
            (Err(message), _) => Err(DocumentError::storage(message)),
            (Ok(()), Some(outcome)) => outcome,
            (Ok(()), None) => Err(DocumentError::storage("the write did not run")),
        }
    }

    fn collections(keys: &dyn KeysRead) -> DocumentResult<Vec<(String, String)>> {
        Ok(keys
            .scan(&[COLLECTION_RECORD])
            .map_err(DocumentError::storage)?
            .into_iter()
            .filter_map(|(key, _)| {
                let text = String::from_utf8(key[1..].to_vec()).ok()?;
                let (database, collection) = text.split_once('\0')?;
                Some((database.to_owned(), collection.to_owned()))
            })
            .collect())
    }

    /// The databases that hold at least one collection, with their documents' stored bytes.
    pub fn databases(&self) -> DocumentResult<Vec<(String, u64)>> {
        self.read(|keys| {
            let mut out: Vec<(String, u64)> = Vec::new();
            for (database, collection) in Self::collections(keys)? {
                let bytes: u64 = keys
                    .scan(&documents_prefix(&database, &collection))
                    .map_err(DocumentError::storage)?
                    .iter()
                    .map(|(_, value)| value.len() as u64)
                    .sum();
                match out.iter_mut().find(|(name, _)| *name == database) {
                    Some((_, total)) => *total += bytes,
                    None => out.push((database, bytes)),
                }
            }
            Ok(out)
        })
    }

    /// The collections of `database`, with their stored options and indexes.
    pub fn collections_of(&self, database: &str) -> DocumentResult<Vec<(String, Document)>> {
        self.read(|keys| {
            let mut out = Vec::new();
            for (owner, collection) in Self::collections(keys)? {
                if owner == database {
                    let info = keys.get(&collection_key(&owner, &collection)).map_err(DocumentError::storage)?;
                    out.push((collection, info.map(|bytes| decoded(&bytes)).transpose()?.unwrap_or_default()));
                }
            }
            Ok(out)
        })
    }

    /// Creates an empty collection; fails when it exists.
    pub fn create_collection(&self, database: &str, collection: &str) -> DocumentResult<()> {
        self.write(|keys| {
            check_database_name(database)?;
            check_collection_name(collection)?;
            if keys.get(&collection_key(database, collection)).map_err(DocumentError::storage)?.is_some() {
                return Err(DocumentError::namespace_exists(format!("Collection {database}.{collection} already exists.")));
            }
            ensure_collection(keys, database, collection)
        })
    }

    /// Drops a collection and its documents; returns whether it existed.
    pub fn drop_collection(&self, database: &str, collection: &str) -> DocumentResult<bool> {
        self.write(|keys| {
            let key = collection_key(database, collection);
            if keys.get(&key).map_err(DocumentError::storage)?.is_none() {
                return Ok(false);
            }
            for (document, _) in keys.scan(&documents_prefix(database, collection)).map_err(DocumentError::storage)? {
                keys.delete(&document).map_err(DocumentError::storage)?;
            }
            keys.delete(&key).map_err(DocumentError::storage)?;
            Ok(true)
        })
    }

    /// Drops every collection of `database`.
    pub fn drop_database(&self, database: &str) -> DocumentResult<()> {
        for (collection, _) in self.collections_of(database)? {
            self.drop_collection(database, &collection)?;
        }
        Ok(())
    }

    /// The index definitions stored for a collection (`_id_` always first).
    pub fn indexes(&self, database: &str, collection: &str) -> DocumentResult<Option<Vec<Document>>> {
        self.read(|keys| {
            let Some(bytes) = keys.get(&collection_key(database, collection)).map_err(DocumentError::storage)? else {
                return Ok(None);
            };
            let info = decoded(&bytes)?;
            let mut indexes = vec![Document::from_entries(vec![
                ("v".to_owned(), Bson::Int32(2)),
                ("key".to_owned(), Bson::Document(Document::from_entries(vec![("_id".to_owned(), Bson::Int32(1))]))),
                ("name".to_owned(), Bson::String("_id_".to_owned())),
            ])];
            if let Some(stored) = info.get_array("indexes") {
                indexes.extend(stored.iter().filter_map(|index| index.as_document().cloned()));
            }
            Ok(Some(indexes))
        })
    }

    /// Records index definitions (`createIndexes`); returns how many there were before and
    /// after. The definitions are kept for listing; queries do not use them yet.
    pub fn create_indexes(&self, database: &str, collection: &str, definitions: &[Document]) -> DocumentResult<(usize, usize)> {
        self.write(|keys| {
            ensure_collection(keys, database, collection)?;
            let key = collection_key(database, collection);
            let mut info = match keys.get(&key).map_err(DocumentError::storage)? {
                Some(bytes) => decoded(&bytes)?,
                None => Document::new(),
            };
            let mut stored: Vec<Bson> = info.get_array("indexes").cloned().unwrap_or_default();
            let before = stored.len() + 1;
            for definition in definitions {
                let Some(name) = definition.get_str("name") else {
                    return Err(DocumentError::bad_value("an index needs a name"));
                };
                if !matches!(definition.get("key"), Some(Bson::Document(fields)) if !fields.is_empty()) {
                    return Err(DocumentError::bad_value("an index needs a non-empty key document"));
                }
                if definition.get_bool("unique") == Some(true) {
                    return Err(DocumentError::not_implemented("unique indexes besides _id are not enforced yet"));
                }
                if name == "_id_" || stored.iter().any(|index| index.as_document().and_then(|index| index.get_str("name")) == Some(name)) {
                    continue;
                }
                let mut index = Document::new();
                index.insert("v", Bson::Int32(2));
                index.insert("key", definition.get("key").cloned().unwrap_or(Bson::Null));
                index.insert("name", Bson::String(name.to_owned()));
                stored.push(Bson::Document(index));
            }
            let after = stored.len() + 1;
            info.insert("indexes", Bson::Array(stored));
            keys.put(&key, &info.to_bytes()).map_err(DocumentError::storage)?;
            Ok((before, after))
        })
    }

    /// Removes index definitions by name, or all but `_id_` for `*`; returns how many there
    /// were before.
    pub fn drop_indexes(&self, database: &str, collection: &str, name: &str) -> DocumentResult<usize> {
        self.write(|keys| {
            let key = collection_key(database, collection);
            let Some(bytes) = keys.get(&key).map_err(DocumentError::storage)? else {
                return Err(DocumentError::namespace_not_found(format!("ns not found {database}.{collection}")));
            };
            let mut info = decoded(&bytes)?;
            let stored: Vec<Bson> = info.get_array("indexes").cloned().unwrap_or_default();
            let before = stored.len() + 1;
            if name == "_id_" {
                return Err(DocumentError::bad_value("cannot drop _id index"));
            }
            let kept: Vec<Bson> = if name == "*" {
                Vec::new()
            } else {
                let kept: Vec<Bson> = stored.iter().filter(|index| index.as_document().and_then(|index| index.get_str("name")) != Some(name)).cloned().collect();
                if kept.len() == stored.len() {
                    return Err(DocumentError::new_index_not_found(name));
                }
                kept
            };
            info.insert("indexes", Bson::Array(kept));
            keys.put(&key, &info.to_bytes()).map_err(DocumentError::storage)?;
            Ok(before)
        })
    }

    /// Inserts documents, giving each without an `_id` a new object id first. `ordered` stops
    /// at the first failure; otherwise every document is tried. Each document is its own
    /// change: a failing one does not undo the others.
    pub fn insert(&self, database: &str, collection: &str, documents: Vec<Document>, ordered: bool) -> DocumentResult<InsertOutcome> {
        self.write(|keys| {
            ensure_collection(keys, database, collection)?;
            let mut outcome = InsertOutcome::default();
            for (at, mut document) in documents.into_iter().enumerate() {
                match document.get("_id").cloned() {
                    Some(id) => document.insert_first("_id", id),
                    None => document.insert_first("_id", Bson::ObjectId(self.new_object_id())),
                }
                let id = document.get("_id").cloned().unwrap_or(Bson::Null);
                let result = check_document(&document).and_then(|bytes| {
                    let key = document_key(database, collection, &id);
                    if keys.get(&key).map_err(DocumentError::storage)?.is_some() {
                        return Err(duplicate(database, collection, &id));
                    }
                    keys.put(&key, &bytes).map_err(DocumentError::storage)
                });
                match result {
                    Ok(()) => outcome.inserted += 1,
                    Err(error) if error.code == 1 => return Err(error),
                    Err(error) => {
                        outcome.errors.push((at, error));
                        if ordered {
                            break;
                        }
                    }
                }
            }
            Ok(outcome)
        })
    }

    /// The documents of a collection matching `filter`, sorted by `sort`.
    pub fn find(&self, database: &str, collection: &str, filter: &Filter, sort: &SortSpec) -> DocumentResult<Vec<Document>> {
        let mut documents: Vec<Document> =
            self.read(|keys| matching(keys, database, collection, filter))?.into_iter().map(|(_, document)| document).collect();
        sort_documents(&mut documents, sort);
        Ok(documents)
    }

    /// Updates the first matching document, or every one with `multi`, upserting when asked.
    pub fn update(&self, database: &str, collection: &str, filter: &Filter, update: &Update, options: UpdateOptions) -> DocumentResult<UpdateOutcome> {
        if options.multi && matches!(update, Update::Replacement(_)) {
            return Err(DocumentError::failed_to_parse("multi update is not supported for replacement-style update"));
        }
        let now = now_ms();
        self.write(|keys| {
            let found = matching(keys, database, collection, filter)?;
            let mut outcome = UpdateOutcome::default();
            for (key, mut document) in found.into_iter().take(if options.multi { usize::MAX } else { 1 }) {
                outcome.matched += 1;
                if apply_update(update, &mut document, false, now)? {
                    keys.put(&key, &check_document(&document)?).map_err(DocumentError::storage)?;
                    outcome.modified += 1;
                }
            }
            if outcome.matched == 0 && options.upsert {
                ensure_collection(keys, database, collection)?;
                let mut document = upsert_document(filter, update, now)?;
                let id = match document.get("_id").cloned() {
                    Some(id) => id,
                    None => Bson::ObjectId(self.new_object_id()),
                };
                document.insert_first("_id", id.clone());
                let key = document_key(database, collection, &id);
                if keys.get(&key).map_err(DocumentError::storage)?.is_some() {
                    return Err(duplicate(database, collection, &id));
                }
                keys.put(&key, &check_document(&document)?).map_err(DocumentError::storage)?;
                outcome.upserted = Some(id);
            }
            Ok(outcome)
        })
    }

    /// Deletes the first matching document, or every one; returns how many.
    pub fn delete(&self, database: &str, collection: &str, filter: &Filter, only_one: bool) -> DocumentResult<u64> {
        self.write(|keys| {
            let found = matching(keys, database, collection, filter)?;
            let mut deleted = 0;
            for (key, _) in found.into_iter().take(if only_one { 1 } else { usize::MAX }) {
                if keys.delete(&key).map_err(DocumentError::storage)? {
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
    }

    /// Finds the first document matching `filter` in `sort` order and changes it, in one
    /// transaction; returns the document before or after, and what an update did.
    pub fn find_and_modify(
        &self,
        database: &str,
        collection: &str,
        filter: &Filter,
        sort: &SortSpec,
        modification: &Modification,
    ) -> DocumentResult<(Option<Document>, UpdateOutcome)> {
        let now = now_ms();
        self.write(|keys| {
            let mut found = matching(keys, database, collection, filter)?;
            if !sort.is_empty() {
                found.sort_by(|left, right| crate::sort::compare_by(sort, &left.1, &right.1));
            }
            let first = found.into_iter().next();
            let mut outcome = UpdateOutcome::default();
            match (modification, first) {
                (Modification::Remove, Some((key, document))) => {
                    keys.delete(&key).map_err(DocumentError::storage)?;
                    outcome.matched = 1;
                    Ok((Some(document), outcome))
                }
                (Modification::Remove, None) => Ok((None, outcome)),
                (Modification::Update { update, new, .. }, Some((key, document))) => {
                    let mut changed = document.clone();
                    outcome.matched = 1;
                    if apply_update(update, &mut changed, false, now)? {
                        keys.put(&key, &check_document(&changed)?).map_err(DocumentError::storage)?;
                        outcome.modified = 1;
                    }
                    Ok((Some(if *new { changed } else { document }), outcome))
                }
                (Modification::Update { update, upsert: true, new }, None) => {
                    ensure_collection(keys, database, collection)?;
                    let mut document = upsert_document(filter, update, now)?;
                    let id = document.get("_id").cloned().unwrap_or_else(|| Bson::ObjectId(self.new_object_id()));
                    document.insert_first("_id", id.clone());
                    keys.put(&document_key(database, collection, &id), &check_document(&document)?).map_err(DocumentError::storage)?;
                    outcome.upserted = Some(id);
                    Ok((new.then_some(document), outcome))
                }
                (Modification::Update { .. }, None) => Ok((None, outcome)),
            }
        })
    }
}

impl DocumentError {
    fn new_index_not_found(name: &str) -> Self {
        Self { code: 27, code_name: "IndexNotFound", message: format!("index not found with name [{name}]") }
    }
}
