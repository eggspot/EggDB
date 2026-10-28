use eggdb_sql::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, SqlError, SqlResult,
    SqlState, StoreFailure,
};
use std::cell::Cell;
use std::time::Instant;

const READS_BETWEEN_CLOCK_CHECKS: u64 = 256;

#[derive(Debug)]
pub(crate) struct CallLimits {
    deadline: Instant,
    reads: Cell<u64>,
    deadline_passed: Cell<bool>,
    write_refused: Cell<bool>,
}

impl CallLimits {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            reads: Cell::new(0),
            deadline_passed: Cell::new(false),
            write_refused: Cell::new(false),
        }
    }

    pub(crate) fn deadline_passed(&self) -> bool {
        self.deadline_passed.get()
    }

    pub(crate) fn write_refused(&self) -> bool {
        self.write_refused.get()
    }

    fn check(&self) -> SqlResult<()> {
        if self.deadline_passed.get() {
            return Err(time_limit_error());
        }
        let reads = self.reads.get() + 1;
        self.reads.set(reads);
        if reads % READS_BETWEEN_CLOCK_CHECKS == 1 && Instant::now() >= self.deadline {
            self.deadline_passed.set(true);
            return Err(time_limit_error());
        }
        Ok(())
    }
}

fn time_limit_error() -> SqlError {
    SqlError::new(
        SqlState::QUERY_CANCELED,
        "canceling statement due to statement timeout",
    )
}

pub(crate) struct CallStore<'c, S: KeyValueStore> {
    inner: &'c S,
    limits: &'c CallLimits,
}

impl<'c, S: KeyValueStore> CallStore<'c, S> {
    pub(crate) fn new(inner: &'c S, limits: &'c CallLimits) -> Self {
        Self { inner, limits }
    }
}

pub(crate) struct LimitedRead<'c, R> {
    inner: R,
    limits: &'c CallLimits,
}

impl<R: KeyValueRead> KeyValueRead for LimitedRead<'_, R> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        self.limits.check()?;
        self.inner.get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        self.limits.check()?;
        self.inner.get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        self.limits.check()?;
        let limits = self.limits;
        self.inner.scan(start, end, direction, &mut |key, value| {
            limits.check()?;
            visit(key, value)
        })
    }

    fn committed_version(&self) -> Option<u64> {
        self.inner.committed_version()
    }
}

pub(crate) enum NeverWriter {}

impl KeyValueRead for NeverWriter {
    fn get(&self, _key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        match *self {}
    }

    fn scan(
        &self,
        _start: &[u8],
        _end: &[u8],
        _direction: ScanDirection,
        _visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        match *self {}
    }
}

impl KeyValueWrite for NeverWriter {
    fn put(&mut self, _key: &[u8], _value: &[u8]) -> SqlResult<()> {
        match *self {}
    }

    fn delete(&mut self, _key: &[u8]) -> SqlResult<bool> {
        match *self {}
    }

    fn commit(self) -> SqlResult<()> {
        match self {}
    }

    fn rollback(self) -> SqlResult<()> {
        match self {}
    }
}

impl<'c, S: KeyValueStore> KeyValueStore for CallStore<'c, S> {
    type Read<'s>
        = LimitedRead<'c, S::Read<'s>>
    where
        Self: 's;
    type Write<'s>
        = NeverWriter
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<Self::Read<'_>> {
        self.limits.check()?;
        Ok(LimitedRead {
            inner: self.inner.begin_read()?,
            limits: self.limits,
        })
    }

    fn begin_write(&self) -> SqlResult<NeverWriter> {
        self.limits.write_refused.set(true);
        Err(SqlError::store(
            StoreFailure::ReadOnly,
            "cannot write: this MCP server is read-only",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{CallLimits, CallStore};
    use eggdb_sql::{KeyValueRead, KeyValueStore, ScanDirection};
    use std::time::{Duration, Instant};

    struct Keys(Vec<(Vec<u8>, Vec<u8>)>);

    impl KeyValueRead for &Keys {
        fn get(&self, key: &[u8]) -> eggdb_sql::SqlResult<Option<Vec<u8>>> {
            Ok(self
                .0
                .iter()
                .find(|(stored, _)| stored == key)
                .map(|(_, value)| value.clone()))
        }

        fn scan(
            &self,
            start: &[u8],
            end: &[u8],
            _direction: ScanDirection,
            visit: &mut eggdb_sql::EntryVisitor<'_>,
        ) -> eggdb_sql::SqlResult<()> {
            for (key, value) in &self.0 {
                if key.as_slice() >= start && key.as_slice() < end && !visit(key, value)? {
                    break;
                }
            }
            Ok(())
        }
    }

    impl KeyValueStore for Keys {
        type Read<'s> = &'s Keys;
        type Write<'s> = super::NeverWriter;

        fn begin_read(&self) -> eggdb_sql::SqlResult<&Keys> {
            Ok(self)
        }

        fn begin_write(&self) -> eggdb_sql::SqlResult<super::NeverWriter> {
            unreachable!("the call store never asks for a writer")
        }
    }

    fn keys() -> Keys {
        Keys(
            (0u32..2000)
                .map(|n| (n.to_be_bytes().to_vec(), vec![1]))
                .collect(),
        )
    }

    #[test]
    fn a_write_is_refused_as_read_only_and_remembered() {
        let keys = keys();
        let limits = CallLimits::new(Instant::now() + Duration::from_secs(60));
        let store = CallStore::new(&keys, &limits);
        let error = store.begin_write().err().unwrap();
        assert_eq!(error.state().code(), "25006");
        assert!(limits.write_refused());
        assert!(!limits.deadline_passed());
    }

    #[test]
    fn reads_past_the_deadline_fail_with_query_canceled() {
        let keys = keys();
        let limits = CallLimits::new(Instant::now() + Duration::from_secs(60));
        let store = CallStore::new(&keys, &limits);
        let mut seen = 0;
        store
            .begin_read()
            .unwrap()
            .scan(&[0; 4], &[0xff; 4], ScanDirection::Forward, &mut |_, _| {
                seen += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(seen, 2000);
        let passed = CallLimits::new(Instant::now());
        let store = CallStore::new(&keys, &passed);
        let error = store.begin_read().err().unwrap();
        assert_eq!(error.state().code(), "57014");
        assert!(passed.deadline_passed());
    }
}
