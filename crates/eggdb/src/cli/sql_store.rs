use eggdb::{Database, Error, ErrorKind, ReadTransaction, WriteTransaction};
use eggdb_sql::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, SqlError, SqlResult,
    StoreFailure,
};
use std::ops::Bound;

pub(crate) struct DatabaseStore<'db>(pub(crate) &'db Database);

fn store_error(error: Error) -> SqlError {
    let failure = match error.kind() {
        ErrorKind::Locked | ErrorKind::WriterActive => StoreFailure::Busy,
        ErrorKind::ReadOnly => StoreFailure::ReadOnly,
        ErrorKind::TooLarge | ErrorKind::CacheFull | ErrorKind::DatabaseFull => {
            StoreFailure::TooLarge
        }
        ErrorKind::Corrupt => StoreFailure::Corrupt,
        ErrorKind::Io | ErrorKind::NeedsReopen => StoreFailure::Io,
        ErrorKind::OutOfMemory => StoreFailure::OutOfMemory,
        _ => StoreFailure::Other,
    };
    SqlError::store(failure, error.to_string())
}

fn scan_through(
    mut scan: eggdb::Scan<'_>,
    direction: ScanDirection,
    visit: &mut EntryVisitor<'_>,
) -> SqlResult<()> {
    loop {
        let entry = match direction {
            ScanDirection::Forward => scan.next_entry(),
            ScanDirection::Backward => scan.next_back_entry(),
        }
        .map_err(store_error)?;
        let Some((key, value)) = entry else {
            return Ok(());
        };
        if !visit(key, value)? {
            return Ok(());
        }
    }
}

pub(crate) struct SqlSnapshot<'db>(ReadTransaction<'db>);

pub(crate) struct SqlWriter<'db>(WriteTransaction<'db>);

impl KeyValueRead for SqlSnapshot<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.0.get(key).map_err(store_error)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        match self.0.get_with(key, read).map_err(store_error)? {
            Some(outcome) => outcome.map(|()| true),
            None => Ok(false),
        }
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let range = (Bound::Included(start), Bound::Excluded(end));
        scan_through(self.0.scan(range), direction, visit)
    }

    fn committed_version(&self) -> Option<u64> {
        Some(self.0.stamp().bits())
    }
}

impl KeyValueRead for SqlWriter<'_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.0.get(key).map_err(store_error)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        match self.0.get_with(key, read).map_err(store_error)? {
            Some(outcome) => outcome.map(|()| true),
            None => Ok(false),
        }
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        let range = (Bound::Included(start), Bound::Excluded(end));
        scan_through(self.0.scan(range), direction, visit)
    }
}

impl KeyValueWrite for SqlWriter<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.0.put(key, value).map_err(store_error)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        self.0.put_if_absent(key, value).map_err(store_error)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        self.0.delete(key).map_err(store_error)
    }

    fn commit(self) -> SqlResult<()> {
        self.0.commit().map(|_| ()).map_err(store_error)
    }

    fn rollback(self) -> SqlResult<()> {
        self.0.rollback().map_err(store_error)
    }
}

impl KeyValueStore for DatabaseStore<'_> {
    type Read<'s>
        = SqlSnapshot<'s>
    where
        Self: 's;
    type Write<'s>
        = SqlWriter<'s>
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<SqlSnapshot<'_>> {
        self.0.begin_read().map(SqlSnapshot).map_err(store_error)
    }

    fn begin_write(&self) -> SqlResult<SqlWriter<'_>> {
        self.0.begin_write().map(SqlWriter).map_err(store_error)
    }
}
