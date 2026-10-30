use crate::error::{SqlError, SqlResult, SqlState};
use crate::store::{EntryVisitor, KeyValueRead, KeyValueWrite, ScanDirection};

#[derive(Debug, Default)]
pub(crate) struct UndoLog {
    entries: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    open: usize,
}

impl UndoLog {
    pub(crate) fn is_recording(&self) -> bool {
        self.open > 0
    }

    pub(crate) fn begin(&mut self) -> usize {
        self.open += 1;
        self.entries.len()
    }

    pub(crate) fn release(&mut self) {
        self.open = self.open.saturating_sub(1);
        if self.open == 0 {
            self.entries.clear();
        }
    }

    pub(crate) fn roll_back(&mut self, writer: &mut dyn KeyValueWrite, mark: usize) -> SqlResult<()> {
        while self.entries.len() > mark {
            let Some((key, previous)) = self.entries.pop() else {
                break;
            };
            match previous {
                Some(value) => writer.put(&key, &value)?,
                None => {
                    writer.delete(&key)?;
                }
            }
        }
        self.release();
        Ok(())
    }

    fn record(&mut self, reader: &dyn KeyValueRead, key: &[u8]) -> SqlResult<()> {
        let previous = reader.get(key)?;
        self.entries.push((key.to_vec(), previous));
        Ok(())
    }
}

pub(crate) struct UndoWrite<'w> {
    pub(crate) inner: &'w mut dyn KeyValueWrite,
    pub(crate) log: &'w mut UndoLog,
}

impl KeyValueRead for UndoWrite<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.inner.get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        self.inner.get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        self.inner.scan(start, end, direction, visit)
    }

    fn committed_version(&self) -> Option<u64> {
        self.inner.committed_version()
    }
}

impl KeyValueWrite for UndoWrite<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.log.record(&*self.inner, key)?;
        self.inner.put(key, value)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        if self.inner.get(key)?.is_some() {
            return Ok(false);
        }
        self.log.entries.push((key.to_vec(), None));
        self.inner.put(key, value)?;
        Ok(true)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        self.log.record(&*self.inner, key)?;
        self.inner.delete(key)
    }

    fn commit(self) -> SqlResult<()> {
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

pub(crate) struct ReadOnlyWrite<'r> {
    pub(crate) inner: &'r dyn KeyValueRead,
}

fn read_only() -> SqlError {
    SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "a PL/pgSQL function called inside a query cannot change data",
    )
    .with_hint(
        "Call a function that writes on its own, as in SELECT f(...) or PERFORM f(...), or make it a procedure and CALL it.",
    )
}

impl KeyValueRead for ReadOnlyWrite<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.inner.get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        self.inner.get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        self.inner.scan(start, end, direction, visit)
    }

    fn committed_version(&self) -> Option<u64> {
        self.inner.committed_version()
    }
}

impl KeyValueWrite for ReadOnlyWrite<'_> {
    fn put(&mut self, _key: &[u8], _value: &[u8]) -> SqlResult<()> {
        Err(read_only())
    }

    fn put_if_absent(&mut self, _key: &[u8], _value: &[u8]) -> SqlResult<bool> {
        Err(read_only())
    }

    fn delete(&mut self, _key: &[u8]) -> SqlResult<bool> {
        Err(read_only())
    }

    fn commit(self) -> SqlResult<()> {
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ReadOnlyWrite, UndoLog, UndoWrite};
    use crate::memory_store::MemoryStore;
    use crate::store::{KeyValueRead, KeyValueStore, KeyValueWrite};

    #[test]
    fn rolling_back_restores_every_key_written_since_the_mark() {
        let store = MemoryStore::default();
        let mut writer = store.begin_write().expect("writer");
        writer.put(b"kept", b"1").expect("put");
        writer.put(b"changed", b"old").expect("put");
        let mut log = UndoLog::default();
        let mark = log.begin();
        {
            let mut undo = UndoWrite {
                inner: &mut writer,
                log: &mut log,
            };
            undo.put(b"changed", b"new").expect("put");
            undo.put(b"added", b"x").expect("put");
            undo.delete(b"kept").expect("delete");
            assert!(undo.put_if_absent(b"fresh", b"y").expect("put"));
        }
        log.roll_back(&mut writer, mark).expect("roll back");
        assert_eq!(writer.get(b"kept").expect("get"), Some(b"1".to_vec()));
        assert_eq!(writer.get(b"changed").expect("get"), Some(b"old".to_vec()));
        assert_eq!(writer.get(b"added").expect("get"), None);
        assert_eq!(writer.get(b"fresh").expect("get"), None);
        assert!(!log.is_recording());
    }

    #[test]
    fn an_inner_release_keeps_entries_for_the_outer_rollback() {
        let store = MemoryStore::default();
        let mut writer = store.begin_write().expect("writer");
        let mut log = UndoLog::default();
        let outer = log.begin();
        let _inner = log.begin();
        UndoWrite {
            inner: &mut writer,
            log: &mut log,
        }
        .put(b"k", b"v")
        .expect("put");
        log.release();
        assert!(log.is_recording());
        log.roll_back(&mut writer, outer).expect("roll back");
        assert_eq!(writer.get(b"k").expect("get"), None);
    }

    #[test]
    fn read_only_writes_refuse_changes_and_still_read() {
        let store = MemoryStore::default();
        let reader = store.begin_read().expect("reader");
        let mut read_only = ReadOnlyWrite { inner: &reader };
        assert_eq!(read_only.get(b"k").expect("get"), None);
        assert!(read_only.put(b"k", b"v").is_err());
        assert!(read_only.delete(b"k").is_err());
    }
}
