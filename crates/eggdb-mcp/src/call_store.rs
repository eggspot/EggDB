use crate::watchdog::Watchdog;
use eggdb_sql::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, SqlError, SqlResult,
    SqlState, StoreFailure, VisibleRead, VisibleTables,
};
use std::cell::Cell;
use std::time::Instant;

const READS_BETWEEN_CLOCK_CHECKS: u64 = 256;

pub(crate) const MAX_WRITTEN_BYTES: u64 = 256 << 20;

#[derive(Debug)]
pub(crate) struct CallLimits {
    deadline: Instant,
    reads: Cell<u64>,
    deadline_passed: Cell<bool>,
    write_refused: Cell<bool>,
    written_bytes: Cell<u64>,
    write_too_large: Cell<bool>,
    committed: Cell<bool>,
    commit_stopped: Cell<bool>,
}

impl CallLimits {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            reads: Cell::new(0),
            deadline_passed: Cell::new(false),
            write_refused: Cell::new(false),
            written_bytes: Cell::new(0),
            write_too_large: Cell::new(false),
            committed: Cell::new(false),
            commit_stopped: Cell::new(false),
        }
    }

    pub(crate) fn deadline_passed(&self) -> bool {
        self.deadline_passed.get() || self.commit_stopped.get()
    }

    pub(crate) fn write_refused(&self) -> bool {
        self.write_refused.get()
    }

    pub(crate) fn write_too_large(&self) -> bool {
        self.write_too_large.get()
    }

    pub(crate) fn committed(&self) -> bool {
        self.committed.get()
    }

    fn count_written(&self, bytes: usize) -> SqlResult<()> {
        let written = self.written_bytes.get().saturating_add(bytes as u64);
        self.written_bytes.set(written);
        if written > MAX_WRITTEN_BYTES {
            self.write_too_large.set(true);
            return Err(SqlError::store(
                StoreFailure::TooLarge,
                "the call's change is larger than 256 MiB",
            ));
        }
        Ok(())
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
    tables: Option<&'c VisibleTables>,
}

impl<'c, S: KeyValueStore> CallStore<'c, S> {
    pub(crate) fn new(
        inner: &'c S,
        limits: &'c CallLimits,
        tables: Option<&'c VisibleTables>,
    ) -> Self {
        Self {
            inner,
            limits,
            tables,
        }
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
        = VisibleRead<LimitedRead<'c, S::Read<'s>>>
    where
        Self: 's;
    type Write<'s>
        = NeverWriter
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<Self::Read<'_>> {
        self.limits.check()?;
        let limited = LimitedRead {
            inner: self.inner.begin_read()?,
            limits: self.limits,
        };
        match self.tables {
            Some(tables) => tables.read(limited),
            None => Ok(VisibleRead::everything(limited)),
        }
    }

    fn begin_write(&self) -> SqlResult<NeverWriter> {
        self.limits.write_refused.set(true);
        Err(SqlError::store(
            StoreFailure::ReadOnly,
            "cannot write: this MCP server is read-only",
        ))
    }
}

pub(crate) struct WritingCallStore<'c, S: KeyValueStore> {
    inner: &'c S,
    limits: &'c CallLimits,
    watchdog: Option<&'c Watchdog>,
}

impl<'c, S: KeyValueStore> WritingCallStore<'c, S> {
    pub(crate) fn new(
        inner: &'c S,
        limits: &'c CallLimits,
        watchdog: Option<&'c Watchdog>,
    ) -> Self {
        Self {
            inner,
            limits,
            watchdog,
        }
    }
}

pub(crate) struct LimitedWrite<'c, W> {
    inner: W,
    limits: &'c CallLimits,
    watchdog: Option<&'c Watchdog>,
}

impl<W: KeyValueWrite> KeyValueRead for LimitedWrite<'_, W> {
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

impl<W: KeyValueWrite> KeyValueWrite for LimitedWrite<'_, W> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.limits.check()?;
        self.limits.count_written(key.len() + value.len())?;
        self.inner.put(key, value)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        self.limits.check()?;
        self.limits.count_written(key.len() + value.len())?;
        self.inner.put_if_absent(key, value)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        self.limits.check()?;
        self.limits.count_written(key.len())?;
        self.inner.delete(key)
    }

    fn commit(self) -> SqlResult<()> {
        if let Some(watchdog) = self.watchdog
            && watchdog.disarm_for_commit().is_err()
        {
            self.limits.commit_stopped.set(true);
            let _ = self.inner.rollback();
            return Err(time_limit_error());
        }
        self.inner.commit()?;
        self.limits.committed.set(true);
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        self.inner.rollback()
    }
}

impl<'c, S: KeyValueStore> KeyValueStore for WritingCallStore<'c, S> {
    type Read<'s>
        = LimitedRead<'c, S::Read<'s>>
    where
        Self: 's;
    type Write<'s>
        = LimitedWrite<'c, S::Write<'s>>
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<Self::Read<'_>> {
        self.limits.check()?;
        Ok(LimitedRead {
            inner: self.inner.begin_read()?,
            limits: self.limits,
        })
    }

    fn begin_write(&self) -> SqlResult<Self::Write<'_>> {
        self.limits.check()?;
        Ok(LimitedWrite {
            inner: self.inner.begin_write()?,
            limits: self.limits,
            watchdog: self.watchdog,
        })
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
        let store = CallStore::new(&keys, &limits, None);
        let error = store.begin_write().err().unwrap();
        assert_eq!(error.state().code(), "25006");
        assert!(limits.write_refused());
        assert!(!limits.deadline_passed());
    }

    #[test]
    fn reads_past_the_deadline_fail_with_query_canceled() {
        let keys = keys();
        let limits = CallLimits::new(Instant::now() + Duration::from_secs(60));
        let store = CallStore::new(&keys, &limits, None);
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
        let store = CallStore::new(&keys, &passed, None);
        let error = store.begin_read().err().unwrap();
        assert_eq!(error.state().code(), "57014");
        assert!(passed.deadline_passed());
    }

    #[derive(Default)]
    struct Counted {
        committed: std::cell::Cell<u32>,
    }

    struct CountedWrite<'s>(&'s Counted);

    impl KeyValueRead for CountedWrite<'_> {
        fn get(&self, _key: &[u8]) -> eggdb_sql::SqlResult<Option<Vec<u8>>> {
            Ok(None)
        }

        fn scan(
            &self,
            _start: &[u8],
            _end: &[u8],
            _direction: ScanDirection,
            _visit: &mut eggdb_sql::EntryVisitor<'_>,
        ) -> eggdb_sql::SqlResult<()> {
            Ok(())
        }
    }

    impl eggdb_sql::KeyValueWrite for CountedWrite<'_> {
        fn put(&mut self, _key: &[u8], _value: &[u8]) -> eggdb_sql::SqlResult<()> {
            Ok(())
        }

        fn delete(&mut self, _key: &[u8]) -> eggdb_sql::SqlResult<bool> {
            Ok(true)
        }

        fn commit(self) -> eggdb_sql::SqlResult<()> {
            self.0.committed.set(self.0.committed.get() + 1);
            Ok(())
        }

        fn rollback(self) -> eggdb_sql::SqlResult<()> {
            Ok(())
        }
    }

    impl KeyValueStore for Counted {
        type Read<'s> = CountedWrite<'s>;
        type Write<'s> = CountedWrite<'s>;

        fn begin_read(&self) -> eggdb_sql::SqlResult<CountedWrite<'_>> {
            Ok(CountedWrite(self))
        }

        fn begin_write(&self) -> eggdb_sql::SqlResult<CountedWrite<'_>> {
            Ok(CountedWrite(self))
        }
    }

    #[test]
    fn a_write_past_256_mib_is_refused_and_a_commit_is_remembered() {
        use eggdb_sql::KeyValueWrite;
        let counted = Counted::default();
        let limits = CallLimits::new(Instant::now() + Duration::from_secs(600));
        let store = super::WritingCallStore::new(&counted, &limits, None);
        let mut writer = store.begin_write().unwrap();
        let value = vec![0u8; 1 << 20];
        for _ in 0..255 {
            writer.put(b"k", &value).unwrap();
        }
        let error = writer.put(b"k", &value).unwrap_err();
        assert_eq!(error.state().code(), "54000");
        assert!(limits.write_too_large());
        writer.rollback().unwrap();
        assert!(!limits.committed());
        let writer = store.begin_write().unwrap();
        writer.commit().unwrap();
        assert!(limits.committed());
        assert_eq!(counted.committed.get(), 1);
    }

    #[test]
    fn a_commit_is_refused_once_the_watchdog_fires() {
        use eggdb_sql::KeyValueWrite;
        let (fired, received) = std::sync::mpsc::channel();
        let (release, held) = std::sync::mpsc::channel::<()>();
        let held = std::sync::Mutex::new(held);
        let watchdog = crate::watchdog::Watchdog::start(Box::new(move |_| {
            let _ = fired.send(());
            let _ = held.lock().unwrap().recv();
        }))
        .unwrap();
        watchdog.arm(Instant::now(), "write_sql");
        received.recv_timeout(Duration::from_secs(30)).unwrap();
        let counted = Counted::default();
        let limits = CallLimits::new(Instant::now() + Duration::from_secs(600));
        let store = super::WritingCallStore::new(&counted, &limits, Some(&watchdog));
        let writer = store.begin_write().unwrap();
        assert_eq!(writer.commit().unwrap_err().state().code(), "57014");
        assert_eq!(counted.committed.get(), 0);
        assert!(!limits.committed());
        assert!(limits.deadline_passed());
        release.send(()).unwrap();
    }
}
