use crate::SqlError;
use crate::error::SqlResult;
use crate::store::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, StoreFailure,
};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

type Map = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Default)]
pub(crate) struct MemoryStore {
    committed: RefCell<Rc<Map>>,
    commits: Cell<u64>,
    writing: Cell<bool>,
    reads: Rc<Cell<u64>>,
}

pub(crate) struct MemoryRead {
    map: Rc<Map>,
    version: u64,
    reads: Rc<Cell<u64>>,
}

pub(crate) struct MemoryWrite<'s> {
    store: &'s MemoryStore,
    map: Map,
}

fn scan_map(
    reads: &Cell<u64>,
    map: &Map,
    start: &[u8],
    end: &[u8],
    direction: ScanDirection,
    visit: &mut EntryVisitor<'_>,
) -> SqlResult<()> {
    if start >= end {
        return Ok(());
    }
    let range = map.range(start.to_vec()..end.to_vec());
    let entries: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match direction {
        ScanDirection::Forward => Box::new(range),
        ScanDirection::Backward => Box::new(range.rev()),
    };
    for (key, value) in entries {
        reads.set(reads.get() + 1);
        if !visit(key, value)? {
            break;
        }
    }
    Ok(())
}

impl MemoryStore {
    pub(crate) fn snapshot(&self) -> Map {
        self.committed.borrow().as_ref().clone()
    }

    pub(crate) fn entries_read(&self) -> u64 {
        self.reads.get()
    }

    pub(crate) fn restore(&self, map: Map) {
        *self.committed.borrow_mut() = Rc::new(map);
        self.commits.set(self.commits.get() + 1);
    }
}

impl KeyValueRead for MemoryRead {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.reads.set(self.reads.get() + 1);
        Ok(self.map.get(key).cloned())
    }

    fn committed_version(&self) -> Option<u64> {
        Some(self.version)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        scan_map(&self.reads, &self.map, start, end, direction, visit)
    }
}

impl KeyValueRead for MemoryWrite<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.store.reads.set(self.store.reads.get() + 1);
        Ok(self.map.get(key).cloned())
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        scan_map(&self.store.reads, &self.map, start, end, direction, visit)
    }
}

impl KeyValueWrite for MemoryWrite<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.map.insert(key.to_vec(), value.to_vec());
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        Ok(self.map.remove(key).is_some())
    }

    fn commit(mut self) -> SqlResult<()> {
        *self.store.committed.borrow_mut() = Rc::new(std::mem::take(&mut self.map));
        self.store.commits.set(self.store.commits.get() + 1);
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

impl Drop for MemoryWrite<'_> {
    fn drop(&mut self) {
        self.store.writing.set(false);
    }
}

impl KeyValueStore for MemoryStore {
    type Read<'s> = MemoryRead;
    type Write<'s> = MemoryWrite<'s>;

    fn begin_read(&self) -> SqlResult<MemoryRead> {
        Ok(MemoryRead {
            map: Rc::clone(&self.committed.borrow()),
            version: self.commits.get(),
            reads: Rc::clone(&self.reads),
        })
    }

    fn begin_write(&self) -> SqlResult<MemoryWrite<'_>> {
        if self.writing.replace(true) {
            return Err(SqlError::store(
                StoreFailure::Busy,
                "another write transaction is open",
            ));
        }
        Ok(MemoryWrite {
            store: self,
            map: self.committed.borrow().as_ref().clone(),
        })
    }
}
