//! Where CouchDB-compatible databases live (D1109): a catalog in keyspace database 0 naming each
//! database's number, and in each database every document's revision tree, the bodies of its
//! leaf revisions, a sequence entry per changed document for the changes feed, and the
//! replicators' local checkpoint documents.

use crate::revs::{Rev, RevNode, leaves};
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};

const CATALOG: u32 = 0;
const NAME: u8 = b'n';
const NEXT_DATABASE: &[u8] = b"next";
const META: &[u8] = b"m";
const DOC: u8 = b'd';
const BODY: u8 = b'b';
const SEQ: u8 = b's';
const LOCAL: u8 = b'l';

/// What a database holds, for `GET /{db}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DbInfo {
    /// Documents whose winning revision is not deleted.
    pub doc_count: u64,
    /// Documents whose winning revision is a deletion.
    pub doc_del_count: u64,
    /// The sequence of the last change.
    pub update_seq: u64,
}

/// A document's revision tree and the sequence of its last change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DocInfo {
    /// The sequence of its last change.
    pub seq: u64,
    /// Every revision known.
    pub revs: Vec<RevNode>,
}

fn key(kind: u8, rest: &[u8]) -> Vec<u8> {
    let mut stored = vec![kind];
    stored.extend_from_slice(rest);
    stored
}

fn body_key(doc: &str, rev: &Rev) -> Vec<u8> {
    let mut stored = key(BODY, doc.as_bytes());
    stored.push(0);
    stored.extend_from_slice(rev.text().as_bytes());
    stored
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&u16::try_from(text.len()).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn encode_info(info: &DocInfo) -> Vec<u8> {
    let mut out = info.seq.to_be_bytes().to_vec();
    out.extend_from_slice(&u32::try_from(info.revs.len()).unwrap_or(u32::MAX).to_be_bytes());
    for node in &info.revs {
        put_text(&mut out, &node.rev.text());
        put_text(&mut out, &node.parent.as_ref().map(Rev::text).unwrap_or_default());
        out.push(u8::from(node.deleted));
    }
    out
}

fn decode_info(bytes: &[u8]) -> Option<DocInfo> {
    let mut at = 0usize;
    let mut take = |count: usize| -> Option<&[u8]> {
        let slice = bytes.get(at..at.checked_add(count)?)?;
        at += count;
        Some(slice)
    };
    let seq = u64::from_be_bytes(take(8)?.try_into().ok()?);
    let count = u32::from_be_bytes(take(4)?.try_into().ok()?) as usize;
    let mut revs = Vec::with_capacity(count.min(4_096));
    for _ in 0..count {
        let length = u16::from_be_bytes(take(2)?.try_into().ok()?) as usize;
        let rev = Rev::parse(std::str::from_utf8(take(length)?).ok()?)?;
        let length = u16::from_be_bytes(take(2)?.try_into().ok()?) as usize;
        let parent_text = std::str::from_utf8(take(length)?).ok()?;
        let parent = if parent_text.is_empty() { None } else { Some(Rev::parse(parent_text)?) };
        let deleted = take(1)?[0] == 1;
        revs.push(RevNode { rev, parent, deleted });
    }
    (at == bytes.len()).then_some(DocInfo { seq, revs })
}

/// The CouchDB-compatible databases of a keyspace.
pub struct Store<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
}

impl std::fmt::Debug for Store<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
}

fn damaged(what: &str) -> String {
    format!("the stored {what} is damaged")
}

fn read_u64(keys: &dyn KeysRead, at: &[u8]) -> Result<u64, String> {
    Ok(keys
        .get(at)?
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_be_bytes)
        .unwrap_or(0))
}

impl<'k> Store<'k> {
    /// The databases kept in `keyspace`.
    pub fn new(keyspace: &'k (dyn Keyspace + Sync)) -> Self {
        Self { keyspace }
    }

    fn read<T>(&self, db: u32, body: impl FnOnce(&dyn KeysRead) -> Result<T, String>) -> Result<T, String> {
        let mut body = Some(body);
        let mut out = None;
        self.keyspace.read(db, &mut |keys| {
            if let Some(body) = body.take() {
                out = Some(body(keys)?);
            }
            Ok(())
        })?;
        out.ok_or_else(|| "the read did not run".to_owned())
    }

    fn write<T>(&self, db: u32, body: impl FnOnce(&mut dyn KeysWrite) -> Result<T, String>) -> Result<T, String> {
        let mut body = Some(body);
        let mut out = None;
        self.keyspace.write(db, &mut |keys| {
            if let Some(body) = body.take() {
                out = Some(body(keys)?);
            }
            Ok(())
        })?;
        out.ok_or_else(|| "the write did not run".to_owned())
    }

    /// The number of database `name`, or `None` when there is none.
    pub fn database(&self, name: &str) -> Result<Option<u32>, String> {
        self.read(CATALOG, |keys| {
            Ok(keys
                .get(&key(NAME, name.as_bytes()))?
                .and_then(|bytes| bytes.try_into().ok())
                .map(u32::from_be_bytes))
        })
    }

    /// Creates database `name`; `false` when it already exists.
    pub fn create_database(&self, name: &str) -> Result<bool, String> {
        self.write(CATALOG, |keys| {
            if keys.get(&key(NAME, name.as_bytes()))?.is_some() {
                return Ok(false);
            }
            let next = u32::try_from(read_u64(&*keys, NEXT_DATABASE)?.max(1)).map_err(|_| "too many databases".to_owned())?;
            keys.put(&key(NAME, name.as_bytes()), &next.to_be_bytes())?;
            keys.put(NEXT_DATABASE, &(u64::from(next) + 1).to_be_bytes())?;
            Ok(true)
        })
    }

    /// Deletes database `name` and everything in it; `false` when there is none.
    pub fn delete_database(&self, name: &str) -> Result<bool, String> {
        let Some(db) = self.database(name)? else {
            return Ok(false);
        };
        self.write(db, |keys| keys.clear())?;
        self.write(CATALOG, |keys| keys.delete(&key(NAME, name.as_bytes())))
    }

    /// Every database's name, in order.
    pub fn databases(&self) -> Result<Vec<String>, String> {
        self.read(CATALOG, |keys| {
            Ok(keys
                .scan(&[NAME])?
                .into_iter()
                .map(|(stored, _)| String::from_utf8_lossy(&stored[1..]).into_owned())
                .collect())
        })
    }

    /// The counts and update sequence of database `db`.
    pub fn info(&self, db: u32) -> Result<DbInfo, String> {
        self.read(db, |keys| {
            let mut info = DbInfo { update_seq: read_u64(keys, META)?, ..DbInfo::default() };
            for (_, bytes) in keys.scan(&[DOC])? {
                let doc = decode_info(&bytes).ok_or_else(|| damaged("document"))?;
                match crate::revs::winner(&doc.revs) {
                    Some(node) if node.deleted => info.doc_del_count += 1,
                    Some(_) => info.doc_count += 1,
                    None => {}
                }
            }
            Ok(info)
        })
    }

    /// Document `doc`'s revision tree, or `None` when it was never written.
    pub fn doc(&self, db: u32, doc: &str) -> Result<Option<DocInfo>, String> {
        self.read(db, |keys| {
            keys.get(&key(DOC, doc.as_bytes()))?
                .map(|bytes| decode_info(&bytes).ok_or_else(|| damaged("document")))
                .transpose()
        })
    }

    /// The body of revision `rev` of `doc`, kept while it is a leaf.
    pub fn body(&self, db: u32, doc: &str, rev: &Rev) -> Result<Option<String>, String> {
        self.read(db, |keys| Ok(keys.get(&body_key(doc, rev))?.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())))
    }

    /// Adds a revision and its ancestry to `doc`: `path` is the new revision first, then its
    /// ancestors in order, as a replicator sends them; the new revision is a deletion when
    /// `deleted`, and `body` is kept as its body. Revisions already known are left as they are.
    /// Returns whether the tree changed.
    pub fn add_path(&self, db: u32, doc: &str, path: &[Rev], deleted: bool, body: &str) -> Result<bool, String> {
        let Some(newest) = path.first().cloned() else {
            return Ok(false);
        };
        self.write(db, |keys| {
            let stored = keys.get(&key(DOC, doc.as_bytes()))?;
            let mut info = match &stored {
                Some(bytes) => decode_info(bytes).ok_or_else(|| damaged("document"))?,
                None => DocInfo::default(),
            };
            if info.revs.iter().any(|node| node.rev == newest) {
                return Ok(false);
            }
            for (at, rev) in path.iter().enumerate().rev() {
                if info.revs.iter().any(|node| &node.rev == rev) {
                    continue;
                }
                info.revs.push(RevNode { rev: rev.clone(), parent: path.get(at + 1).cloned(), deleted: at == 0 && deleted });
            }
            let leaf_revs: Vec<Rev> = leaves(&info.revs).into_iter().map(|node| node.rev.clone()).collect();
            for node in &info.revs {
                if !leaf_revs.contains(&node.rev) {
                    keys.delete(&body_key(doc, &node.rev))?;
                }
            }
            keys.put(&body_key(doc, &newest), body.as_bytes())?;
            let seq = read_u64(&*keys, META)? + 1;
            if info.seq > 0 {
                keys.delete(&key(SEQ, &info.seq.to_be_bytes()))?;
            }
            keys.put(&key(SEQ, &seq.to_be_bytes()), doc.as_bytes())?;
            keys.put(META, &seq.to_be_bytes())?;
            info.seq = seq;
            keys.put(&key(DOC, doc.as_bytes()), &encode_info(&info))?;
            Ok(true)
        })
    }

    /// The documents changed after sequence `since`, oldest first, at most `limit`, as
    /// `(sequence, document id)`.
    pub fn changes(&self, db: u32, since: u64, limit: usize) -> Result<Vec<(u64, String)>, String> {
        self.read(db, |keys| {
            let mut found = Vec::new();
            for (stored, value) in keys.scan(&[SEQ])? {
                let seq = stored.get(1..9).and_then(|bytes| bytes.try_into().ok()).map(u64::from_be_bytes).ok_or_else(|| damaged("sequence"))?;
                if seq > since {
                    found.push((seq, String::from_utf8_lossy(&value).into_owned()));
                    if found.len() >= limit {
                        break;
                    }
                }
            }
            Ok(found)
        })
    }

    /// The ids of every document ever written, in order.
    pub fn doc_ids(&self, db: u32) -> Result<Vec<String>, String> {
        self.read(db, |keys| {
            Ok(keys.scan(&[DOC])?.into_iter().map(|(stored, _)| String::from_utf8_lossy(&stored[1..]).into_owned()).collect())
        })
    }

    /// Local document `id` (a replicator's checkpoint), never replicated.
    pub fn local(&self, db: u32, id: &str) -> Result<Option<String>, String> {
        self.read(db, |keys| Ok(keys.get(&key(LOCAL, id.as_bytes()))?.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())))
    }

    /// Keeps local document `id` as `body`, or removes it when `body` is `None`.
    pub fn set_local(&self, db: u32, id: &str, body: Option<&str>) -> Result<(), String> {
        self.write(db, |keys| match body {
            Some(body) => keys.put(&key(LOCAL, id.as_bytes()), body.as_bytes()),
            None => keys.delete(&key(LOCAL, id.as_bytes())).map(|_| ()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;

    fn rev(text: &str) -> Rev {
        Rev::parse(text).unwrap()
    }

    #[test]
    fn revision_trees_keep_leaf_bodies_and_one_sequence_per_document() {
        let keyspace = MemoryKeyspace::default();
        let store = Store::new(&keyspace);
        assert!(store.create_database("app").unwrap());
        assert!(!store.create_database("app").unwrap());
        let db = store.database("app").unwrap().unwrap();
        assert!(store.add_path(db, "doc", &[rev("1-a")], false, "{\"v\":1}").unwrap());
        assert!(store.add_path(db, "doc", &[rev("2-b"), rev("1-a")], false, "{\"v\":2}").unwrap());
        assert!(!store.add_path(db, "doc", &[rev("2-b"), rev("1-a")], false, "{\"v\":2}").unwrap());
        assert!(store.add_path(db, "other", &[rev("1-c")], true, "{}").unwrap());
        assert_eq!(store.body(db, "doc", &rev("1-a")).unwrap(), None);
        assert_eq!(store.body(db, "doc", &rev("2-b")).unwrap().as_deref(), Some("{\"v\":2}"));
        assert_eq!(store.changes(db, 0, 10).unwrap(), vec![(2, "doc".to_owned()), (3, "other".to_owned())]);
        assert_eq!(store.info(db).unwrap(), DbInfo { doc_count: 1, doc_del_count: 1, update_seq: 3 });
        assert!(store.delete_database("app").unwrap());
        assert_eq!(store.database("app").unwrap(), None);
    }

    #[test]
    fn damaged_document_info_is_refused() {
        assert_eq!(decode_info(&[0, 1]), None);
        let info = DocInfo { seq: 4, revs: vec![RevNode { rev: rev("1-a"), parent: None, deleted: false }] };
        assert_eq!(decode_info(&encode_info(&info)), Some(info));
    }
}
