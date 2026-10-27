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
    writing: Cell<bool>,
}

pub(crate) struct MemoryRead {
    map: Rc<Map>,
}

pub(crate) struct MemoryWrite<'s> {
    store: &'s MemoryStore,
    map: Map,
}

fn scan_map(
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
}

impl KeyValueRead for MemoryRead {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        Ok(self.map.get(key).cloned())
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        scan_map(&self.map, start, end, direction, visit)
    }
}

impl KeyValueRead for MemoryWrite<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        Ok(self.map.get(key).cloned())
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        scan_map(&self.map, start, end, direction, visit)
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
