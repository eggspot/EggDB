use super::{Block, Session};
use crate::ast::{Name, SavepointCommand};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::QueryResult;
use crate::settings::SavedSettings;
use crate::store::{EntryVisitor, KeyValueRead, KeyValueWrite, ScanDirection};

const MOST_SAVEPOINTS: usize = 4096;
const UNDO_ENTRY_OVERHEAD: usize = 64;

pub(super) type Undo = Vec<UndoEntry>;

#[derive(Debug)]
pub(super) enum UndoEntry {
    Statement {
        key: Vec<u8>,
        previous: Option<Vec<u8>>,
    },
    Host {
        key: Vec<u8>,
        previous: Option<Vec<u8>>,
    },
}

impl UndoEntry {
    fn statement(key: &[u8], previous: Option<Vec<u8>>) -> Self {
        Self::Statement {
            key: key.to_vec(),
            previous,
        }
    }
}

#[derive(Debug)]
struct Mark {
    name: String,
    undo_at: usize,
    settings: Option<SavedSettings>,
}

#[derive(Debug, Default)]
pub(super) struct Savepoints {
    marks: Vec<Mark>,
    pub(super) undo: Undo,
    pub(super) aborted: bool,
    began_transaction: bool,
}

impl Savepoints {
    pub(super) fn is_active(&self) -> bool {
        !self.marks.is_empty()
    }

    pub(super) fn clear(&mut self) {
        self.marks.clear();
        self.undo.clear();
        self.aborted = false;
        self.began_transaction = false;
    }

    fn latest(&self, name: &str) -> Option<usize> {
        self.marks.iter().rposition(|mark| mark.name == name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UndoKeptFor {
    FailedStatement,
    RollbackToSavepoint,
}

pub(super) struct UndoWrite<'w> {
    inner: &'w mut dyn KeyValueWrite,
    log: &'w mut Undo,
    memory: crate::memory::WorkMemory,
    most_bytes: usize,
    kept_for: UndoKeptFor,
}

impl<'w> UndoWrite<'w> {
    pub(super) fn new(
        inner: &'w mut dyn KeyValueWrite,
        log: &'w mut Undo,
        kept_for: UndoKeptFor,
    ) -> Self {
        Self {
            inner,
            log,
            memory: crate::memory::WorkMemory::new(),
            most_bytes: crate::memory::WORK_MEMORY_LIMIT,
            kept_for,
        }
    }

    #[cfg(test)]
    fn with_most_bytes(
        inner: &'w mut dyn KeyValueWrite,
        log: &'w mut Undo,
        most_bytes: usize,
    ) -> Self {
        Self {
            inner,
            log,
            memory: crate::memory::WorkMemory::with_limit(most_bytes),
            most_bytes,
            kept_for: UndoKeptFor::FailedStatement,
        }
    }

    fn hold(&mut self, key: &[u8], previous: Option<&[u8]>) -> SqlResult<()> {
        let bytes = key
            .len()
            .saturating_add(previous.map_or(0, <[u8]>::len))
            .saturating_add(UNDO_ENTRY_OVERHEAD);
        self.memory.charge(bytes).map_err(|error| {
            if error.state() == SqlState::OUT_OF_MEMORY {
                undo_log_too_large(self.most_bytes, self.kept_for)
            } else {
                error
            }
        })
    }
}

fn undo_log_too_large(most_bytes: usize, kept_for: UndoKeptFor) -> SqlError {
    let error = SqlError::new(
        SqlState::OUT_OF_MEMORY,
        format!(
            "a statement inside a transaction can hold at most {most_bytes} bytes of work memory, \
             including the earlier values it overwrites"
        ),
    );
    match kept_for {
        UndoKeptFor::FailedStatement => error.with_hint(
            "The statement keeps every value it overwrites so that a failure undoes only it; split \
             the change into smaller statements.",
        ),
        UndoKeptFor::RollbackToSavepoint => error.with_hint(
            "After a SAVEPOINT the statement keeps every value it overwrites so that ROLLBACK TO \
             SAVEPOINT can restore it; split the change into smaller statements or release the \
             savepoint first.",
        ),
    }
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
        let previous = self.inner.get(key)?;
        self.hold(key, previous.as_deref())?;
        self.inner.put(key, value)?;
        self.log.push(UndoEntry::statement(key, previous));
        Ok(())
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        self.hold(key, None)?;
        let inserted = self.inner.put_if_absent(key, value)?;
        if inserted {
            self.log.push(UndoEntry::statement(key, None));
        }
        Ok(inserted)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        let previous = self.inner.get(key)?;
        self.hold(key, previous.as_deref())?;
        let deleted = self.inner.delete(key)?;
        if deleted {
            self.log.push(UndoEntry::statement(key, previous));
        }
        Ok(deleted)
    }

    fn commit(self) -> SqlResult<()> {
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

pub(super) fn undo_back_to(
    routed: &mut crate::temporary::TemporaryWrite<'_>,
    undo: &mut Undo,
    length: usize,
) -> SqlResult<()> {
    while undo.len() > length {
        let Some(entry) = undo.pop() else {
            break;
        };
        match entry {
            UndoEntry::Statement {
                key,
                previous: Some(value),
            } => routed.put(&key, &value)?,
            UndoEntry::Statement {
                key,
                previous: None,
            } => {
                routed.delete(&key)?;
            }
            UndoEntry::Host { key, previous } => {
                routed.inner.restore(&key, previous.as_deref())?;
            }
        }
    }
    Ok(())
}

fn missing(name: &Name, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::SAVEPOINT_EXCEPTION,
            format!("SAVEPOINT {} does not exist", name.text),
        )
        .with_vendor_code(1305),
        Dialect::Oracle => SqlError::new(
            SqlState::SAVEPOINT_EXCEPTION,
            format!(
                "savepoint '{}' never established in this session or is invalid",
                name.text.to_ascii_uppercase()
            ),
        )
        .with_oracle_code(1086),
        Dialect::Sqlite => SqlError::new(
            SqlState::SAVEPOINT_EXCEPTION,
            format!("no such savepoint: {}", name.text),
        )
        .with_sqlite_code(1),
        Dialect::SqlServer => SqlError::new(
            SqlState::SAVEPOINT_EXCEPTION,
            format!(
                "Cannot roll back {}. No transaction or savepoint of that name was found.",
                name.text
            ),
        )
        .with_sqlserver_code(6401),
        Dialect::PostgreSql => SqlError::new(
            SqlState::SAVEPOINT_EXCEPTION,
            format!("savepoint \"{}\" does not exist", name.text),
        ),
    }
    .at(name.position)
}

impl Session<'_> {
    /// Whether [`Session::record_savepoint_undo`] would record writes now: a savepoint is set
    /// in a block that writes.
    pub fn records_savepoint_undo(&self) -> bool {
        self.savepoints.is_active() && matches!(self.block, Block::Writing(_))
    }

    /// Adds to the open block's savepoint undo log writes made to its write transaction outside
    /// this session's statements, such as a host's key-value commands: each `(key, previous)`
    /// is a key of the store and what it held before the write, `None` when it was absent,
    /// oldest first. A later `ROLLBACK TO SAVEPOINT` puts them back in reverse order through
    /// [`KeyValueWrite::restore`], interleaved with the statements' own writes, and
    /// `RELEASE SAVEPOINT` keeps them. Returns `false`, recording nothing, when no savepoint is
    /// set in a block that writes.
    pub fn record_savepoint_undo(
        &mut self,
        changes: impl IntoIterator<Item = (Vec<u8>, Option<Vec<u8>>)>,
    ) -> bool {
        if !self.records_savepoint_undo() {
            return false;
        }
        self.savepoints.undo.extend(
            changes
                .into_iter()
                .map(|(key, previous)| UndoEntry::Host { key, previous }),
        );
        true
    }

    /// Whether a statement failed while a `SAVEPOINT` was set in PostgreSQL's dialect, leaving
    /// the block refusing statements until `ROLLBACK TO SAVEPOINT` or `ROLLBACK` (in the other
    /// dialects a failed statement undoes only itself and the block goes on, so this stays
    /// `false`); [`Session::transaction_status`]
    /// still answers [`TransactionStatus::InBlock`](crate::TransactionStatus::InBlock) then,
    /// since the block can go on after the rollback to the savepoint.
    pub fn failed_after_savepoint(&self) -> bool {
        self.savepoints.aborted
    }

    pub(super) fn savepoint_command(
        &mut self,
        command: &SavepointCommand,
    ) -> SqlResult<QueryResult> {
        match command {
            SavepointCommand::Set(name) => self.set_savepoint(name),
            SavepointCommand::RollbackTo(name) => self.rollback_to_savepoint(name),
            SavepointCommand::Release(name) => self.release_savepoint(name),
        }
    }

    pub(super) fn set_savepoint(&mut self, name: &Name) -> SqlResult<QueryResult> {
        if !self.in_block() {
            match self.dialect {
                Dialect::PostgreSql => {
                    return Err(SqlError::new(
                        SqlState::NO_ACTIVE_SQL_TRANSACTION,
                        "SAVEPOINT can only be used in transaction blocks",
                    )
                    .at(name.position));
                }
                Dialect::Sqlite => {
                    self.begin(false)?;
                    self.savepoints.began_transaction = true;
                }
                Dialect::Oracle => {
                    self.write("SAVEPOINT", |_, _| Ok(QueryResult::command("SAVEPOINT")))?;
                }
                _ => return Ok(QueryResult::command("SAVEPOINT")),
            }
        }
        if self.savepoints.marks.len() >= MOST_SAVEPOINTS {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("a transaction can hold at most {MOST_SAVEPOINTS} savepoints"),
            )
            .at(name.position));
        }
        if self.dialect == Dialect::MySql
            && let Some(at) = self.savepoints.latest(&name.text)
        {
            self.savepoints.marks.remove(at);
        }
        let undo_at = self.savepoints.undo.len();
        let settings = (self.dialect == Dialect::PostgreSql)
            .then(|| self.transaction_settings.save(&self.settings));
        self.savepoints.marks.push(Mark {
            name: name.text.clone(),
            undo_at,
            settings,
        });
        Ok(QueryResult::command("SAVEPOINT"))
    }

    pub(super) fn rollback_to_savepoint(&mut self, name: &Name) -> SqlResult<QueryResult> {
        let Some(at) = self.savepoints.latest(&name.text) else {
            if self.dialect == Dialect::PostgreSql && !self.in_block() {
                return Err(SqlError::new(
                    SqlState::NO_ACTIVE_SQL_TRANSACTION,
                    "ROLLBACK TO SAVEPOINT can only be used in transaction blocks",
                )
                .at(name.position));
            }
            return Err(missing(name, self.dialect));
        };
        let Some(mark) = self.savepoints.marks.get(at) else {
            return Err(missing(name, self.dialect));
        };
        let undo_at = mark.undo_at;
        if let Some(saved) = mark.settings.clone() {
            let settings = std::sync::Arc::make_mut(&mut self.settings);
            self.transaction_settings.restore(settings, saved);
        }
        match &mut self.block {
            Block::Writing(writer) => {
                let mut routed = crate::temporary::TemporaryWrite {
                    inner: writer,
                    objects: &mut self.temporary,
                };
                undo_back_to(&mut routed, &mut self.savepoints.undo, undo_at)?;
            }
            Block::Reading(reader) => {
                let mut unwritable = crate::plpgsql::ReadOnlyWrite { inner: &**reader };
                let mut routed = crate::temporary::TemporaryWrite {
                    inner: &mut unwritable,
                    objects: &mut self.temporary,
                };
                undo_back_to(&mut routed, &mut self.savepoints.undo, undo_at)?;
            }
            Block::None | Block::Failed => {
                self.savepoints.marks.truncate(at + 1);
                self.savepoints.aborted = false;
                return Ok(QueryResult::command("ROLLBACK"));
            }
        }
        self.savepoints.marks.truncate(at + 1);
        self.savepoints.aborted = false;
        self.tables = crate::execute::TableCache::default();
        Ok(QueryResult::command("ROLLBACK"))
    }

    pub(super) fn release_savepoint(&mut self, name: &Name) -> SqlResult<QueryResult> {
        let Some(at) = self.savepoints.latest(&name.text) else {
            return Err(missing(name, self.dialect));
        };
        self.savepoints.marks.truncate(at);
        if self.savepoints.marks.is_empty() {
            self.savepoints.undo.clear();
            if std::mem::take(&mut self.savepoints.began_transaction) {
                return self.commit().map(|_| QueryResult::command("RELEASE"));
            }
        }
        Ok(QueryResult::command("RELEASE"))
    }
}

#[cfg(test)]
#[path = "savepoints_tests.rs"]
mod tests;
