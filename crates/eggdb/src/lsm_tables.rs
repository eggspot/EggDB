use crate::collection::COLLECTIONS_PREFIX;
use crate::error::{Error, ErrorKind, Result};
use crate::scan::{Reader, Scan};
use eggdb_storage::{BTree, LsmTree};
use std::ops::Bound;

const ROUTE_TAG: u8 = 0x08;

pub(crate) fn routes_start() -> Vec<u8> {
    let mut key = COLLECTIONS_PREFIX.to_vec();
    key.push(ROUTE_TAG);
    key
}

pub(crate) fn routes_end() -> Vec<u8> {
    let mut key = COLLECTIONS_PREFIX.to_vec();
    key.push(ROUTE_TAG + 1);
    key
}

pub(crate) fn route_key(prefix: &[u8]) -> Vec<u8> {
    let mut key = routes_start();
    key.extend_from_slice(prefix);
    key
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LsmRoutes {
    routes: Vec<(Vec<u8>, LsmTree)>,
}

impl LsmRoutes {
    pub(crate) fn load(reader: Reader<'_>, tree: Option<BTree>) -> Result<Self> {
        let start = routes_start();
        let end = routes_end();
        let mut routes = Vec::new();
        let mut scan = Scan::within_between(reader, tree, b"", &(start.clone()..end));
        while let Some((key, value)) = scan.next_entry()? {
            let prefix = key.get(start.len()..).unwrap_or_default().to_vec();
            let root: [u8; 8] = value
                .try_into()
                .map_err(|_| Error::new(ErrorKind::Corrupt, "an LSM table's root is not 8 bytes"))?;
            routes.push((prefix, u64::from_be_bytes(root)));
        }
        drop(scan);
        let mut opened = Vec::with_capacity(routes.len());
        for (prefix, root) in routes {
            opened.push((prefix, reader.open_lsm(root)?));
        }
        Ok(Self { routes: opened })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    pub(crate) fn route(&self, key: &[u8]) -> Option<LsmTree> {
        if self.routes.is_empty() {
            return None;
        }
        self.routes
            .iter()
            .find(|(prefix, _)| key.starts_with(prefix))
            .map(|(_, tree)| *tree)
    }

    pub(crate) fn has(&self, prefix: &[u8]) -> bool {
        self.routes.iter().any(|(owned, _)| owned == prefix)
    }

    pub(crate) fn overlaps(&self, prefix: &[u8]) -> bool {
        self.routes
            .iter()
            .any(|(owned, _)| owned.starts_with(prefix) || prefix.starts_with(owned))
    }

    pub(crate) fn add(&mut self, prefix: Vec<u8>, tree: LsmTree) {
        let at = self.routes.partition_point(|(owned, _)| *owned < prefix);
        self.routes.insert(at, (prefix, tree));
    }

    pub(crate) fn split(
        &self,
        segments: Vec<(Bound<Vec<u8>>, Bound<Vec<u8>>)>,
    ) -> Vec<(Bound<Vec<u8>>, Bound<Vec<u8>>, Option<LsmTree>)> {
        let mut pieces: Vec<(Bound<Vec<u8>>, Bound<Vec<u8>>, Option<LsmTree>)> = segments
            .into_iter()
            .map(|(start, end)| (start, end, None))
            .collect();
        for (prefix, tree) in &self.routes {
            let owned_start = Bound::Included(prefix.clone());
            let owned_end = crate::scan::keys_after_prefix(prefix);
            let mut next = Vec::with_capacity(pieces.len() + 2);
            for (start, end, source) in pieces {
                if source.is_some() || !overlap(&start, &end, &owned_start, &owned_end) {
                    next.push((start, end, source));
                    continue;
                }
                if starts_before(&start, prefix) {
                    next.push((start.clone(), Bound::Excluded(prefix.clone()), None));
                }
                let inner_start = later_start(&start, &owned_start);
                let inner_end = earlier_end(&end, &owned_end);
                next.push((inner_start, inner_end, Some(*tree)));
                if let Bound::Excluded(after) = &owned_end
                    && ends_after(&end, after)
                {
                    next.push((Bound::Included(after.clone()), end, None));
                }
            }
            pieces = next;
        }
        pieces
    }
}

fn starts_before(start: &Bound<Vec<u8>>, at: &[u8]) -> bool {
    match start {
        Bound::Unbounded => true,
        Bound::Included(key) | Bound::Excluded(key) => key.as_slice() < at,
    }
}

fn ends_after(end: &Bound<Vec<u8>>, at: &[u8]) -> bool {
    match end {
        Bound::Unbounded => true,
        Bound::Included(key) => key.as_slice() >= at,
        Bound::Excluded(key) => key.as_slice() > at,
    }
}

fn later_start(a: &Bound<Vec<u8>>, b: &Bound<Vec<u8>>) -> Bound<Vec<u8>> {
    match (a, b) {
        (Bound::Unbounded, other) | (other, Bound::Unbounded) => other.clone(),
        (Bound::Included(x) | Bound::Excluded(x), Bound::Included(y) | Bound::Excluded(y)) => {
            if x > y {
                a.clone()
            } else if y > x {
                b.clone()
            } else if matches!(a, Bound::Excluded(_)) {
                a.clone()
            } else {
                b.clone()
            }
        }
    }
}

fn earlier_end(a: &Bound<Vec<u8>>, b: &Bound<Vec<u8>>) -> Bound<Vec<u8>> {
    match (a, b) {
        (Bound::Unbounded, other) | (other, Bound::Unbounded) => other.clone(),
        (Bound::Included(x) | Bound::Excluded(x), Bound::Included(y) | Bound::Excluded(y)) => {
            if x < y {
                a.clone()
            } else if y < x {
                b.clone()
            } else if matches!(a, Bound::Excluded(_)) {
                a.clone()
            } else {
                b.clone()
            }
        }
    }
}

fn overlap(
    start: &Bound<Vec<u8>>,
    end: &Bound<Vec<u8>>,
    owned_start: &Bound<Vec<u8>>,
    owned_end: &Bound<Vec<u8>>,
) -> bool {
    let low = later_start(start, owned_start);
    let high = earlier_end(end, owned_end);
    match (&low, &high) {
        (_, Bound::Unbounded) | (Bound::Unbounded, _) => true,
        (Bound::Included(x), Bound::Included(y)) => x <= y,
        (Bound::Included(x) | Bound::Excluded(x), Bound::Included(y) | Bound::Excluded(y)) => x < y,
    }
}

#[cfg(test)]
mod tests {
    use super::{earlier_end, later_start, overlap};
    use crate::Database;
    use std::ops::Bound;

    fn keys(db: &Database) -> Vec<Vec<u8>> {
        let read = db.begin_read().unwrap();
        read.scan(..)
            .map(|entry| entry.map(|(key, _)| key))
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn keys_under_an_lsm_prefix_read_scan_and_delete_as_before() {
        let db = Database::in_memory().unwrap();
        db.put("a", "1").unwrap();
        db.put("z", "26").unwrap();
        db.write(|tx| {
            assert!(tx.store_prefix_in_lsm_tree(b"t/")?);
            assert!(!tx.store_prefix_in_lsm_tree(b"t/")?);
            for n in 0..200u32 {
                tx.put([b"t/".as_slice(), &n.to_be_bytes()].concat(), n.to_string())?;
            }
            Ok(())
        })
        .unwrap();
        assert!(db.has_lsm_tables());
        assert_eq!(db.get([b"t/".as_slice(), &7u32.to_be_bytes()].concat()).unwrap(), Some(b"7".to_vec()));
        let all = keys(&db);
        assert_eq!(all.len(), 202);
        assert_eq!(all.first().unwrap(), b"a");
        assert_eq!(all.last().unwrap(), b"z");
        assert!(all.windows(2).all(|pair| pair[0] < pair[1]));
        let read = db.begin_read().unwrap();
        let backwards: Vec<Vec<u8>> = read
            .scan_prefix("t/")
            .rev()
            .map(|entry| entry.map(|(key, _)| key))
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(backwards.len(), 200);
        assert_eq!(backwards[0], [b"t/".as_slice(), &199u32.to_be_bytes()].concat());
        drop(read);
        db.write(|tx| {
            assert!(tx.delete([b"t/".as_slice(), &0u32.to_be_bytes()].concat())?);
            Ok(())
        })
        .unwrap();
        assert_eq!(keys(&db).len(), 201);
        let report = db.check().unwrap();
        assert_eq!(report.leaked_pages, 0);
    }

    #[test]
    fn a_prefix_with_keys_or_inside_another_stays_in_the_b_tree() {
        let db = Database::in_memory().unwrap();
        db.put("t/1", "x").unwrap();
        let mut tx = db.begin_write().unwrap();
        assert!(tx.store_prefix_in_lsm_tree(b"t/").is_err());
        assert!(tx.store_prefix_in_lsm_tree(b"").is_err());
        assert!(tx.store_prefix_in_lsm_tree(b"u/").unwrap());
        assert!(tx.store_prefix_in_lsm_tree(b"u/v").is_err());
        tx.rollback().unwrap();
        assert_eq!(keys(&db), vec![b"t/1".to_vec()]);
    }

    fn included(key: &[u8]) -> Bound<Vec<u8>> {
        Bound::Included(key.to_vec())
    }

    fn excluded(key: &[u8]) -> Bound<Vec<u8>> {
        Bound::Excluded(key.to_vec())
    }

    #[test]
    fn bounds_meet_where_ranges_overlap() {
        assert_eq!(later_start(&Bound::Unbounded, &included(b"t")), included(b"t"));
        assert_eq!(later_start(&excluded(b"t"), &included(b"t")), excluded(b"t"));
        assert_eq!(earlier_end(&excluded(b"u"), &Bound::Unbounded), excluded(b"u"));
        assert!(overlap(&Bound::Unbounded, &Bound::Unbounded, &included(b"t"), &excluded(b"u")));
        assert!(!overlap(&included(b"a"), &excluded(b"t"), &included(b"t"), &excluded(b"u")));
        assert!(overlap(&included(b"a"), &included(b"t"), &included(b"t"), &excluded(b"u")));
        assert!(!overlap(&included(b"u"), &Bound::Unbounded, &included(b"t"), &excluded(b"u")));
    }
}
