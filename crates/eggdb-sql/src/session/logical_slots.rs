use super::{Block, Session};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::logical_decoding::{DecodedTransaction, capture_lsn, decode_after, number, prune, slot_key, valid_slot_name};
use crate::store::KeyValueRead;

impl Session<'_> {
    fn with_block_reader<T>(&mut self, read: impl FnOnce(&dyn KeyValueRead) -> SqlResult<T>) -> SqlResult<T> {
        match &self.block {
            Block::Reading(reader) => read(reader.as_ref()),
            Block::Writing(writer) => read(&**writer),
            _ => {
                let reader = self.store.begin_boxed_read()?;
                read(reader.as_ref())
            }
        }
    }

    /// The capture position this session sees: inside a transaction block, the one its
    /// snapshot holds, which is the consistent point of a slot created with that snapshot.
    pub fn current_capture_lsn(&mut self) -> SqlResult<u64> {
        self.with_block_reader(capture_lsn)
    }

    /// Creates the logical replication slot `name` at the capture position the session sees,
    /// and returns that position (D1031).
    pub fn create_logical_slot(&mut self, name: &str) -> SqlResult<u64> {
        valid_slot_name(name)?;
        let lsn = self.current_capture_lsn()?;
        let mut write = self.store.begin_boxed_write()?;
        if write.get(&slot_key(name))?.is_some() {
            return Err(SqlError::new(SqlState::DUPLICATE_OBJECT, format!("replication slot \"{name}\" already exists")));
        }
        write.put(&slot_key(name), &lsn.to_le_bytes())?;
        write.commit_boxed()?;
        Ok(lsn)
    }

    /// Drops the slot `name`, letting the capture records it held go.
    pub fn drop_logical_slot(&mut self, name: &str) -> SqlResult<()> {
        let mut write = self.store.begin_boxed_write()?;
        if !write.delete(&slot_key(name))? {
            return Err(SqlError::new(SqlState::UNDEFINED_OBJECT, format!("replication slot \"{name}\" does not exist")));
        }
        prune(write.as_mut(), usize::MAX)?;
        write.commit_boxed()
    }

    /// The slot's confirmed position.
    pub fn logical_slot_position(&mut self, name: &str) -> SqlResult<u64> {
        let reader = self.store.begin_boxed_read()?;
        reader
            .get(&slot_key(name))?
            .as_deref()
            .and_then(number)
            .ok_or_else(|| SqlError::new(SqlState::UNDEFINED_OBJECT, format!("replication slot \"{name}\" does not exist")))
    }

    /// Records that the slot's subscriber applied everything up to `lsn`, and removes the
    /// capture records nobody needs any more.
    pub fn confirm_logical_slot(&mut self, name: &str, lsn: u64) -> SqlResult<()> {
        let mut write = self.store.begin_boxed_write()?;
        let current = write.get(&slot_key(name))?.as_deref().and_then(number).unwrap_or(0);
        if lsn > current {
            write.put(&slot_key(name), &lsn.to_le_bytes())?;
            prune(write.as_mut(), 10_000)?;
        }
        write.commit_boxed()
    }

    /// The captured transactions after `lsn`, at most `limit`, decoded (D1031).
    pub fn logical_changes_after(&mut self, lsn: u64, limit: usize) -> SqlResult<Vec<DecodedTransaction>> {
        let reader = self.store.begin_boxed_read()?;
        decode_after(reader.as_ref(), lsn, limit)
    }
}
