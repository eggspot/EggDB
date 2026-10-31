use super::{Block, Session};
use crate::ast::{Name, SavepointCommand};
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::QueryResult;
use crate::store::{EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection};

const MOST_SAVEPOINTS: usize = 4096;

type Undo = Vec<(Vec<u8>, Option<Vec<u8>>)>;

#[derive(Debug, Default)]
pub(super) struct Savepoints {
    marks: Vec<(String, usize)>,
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
        self.marks.iter().rposition(|(mark, _)| mark == name)
    }
}

pub(super) struct UndoWrite<'w> {
    pub(super) inner: &'w mut dyn KeyValueWrite,
    pub(super) log: &'w mut Undo,
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
        self.inner.put(key, value)?;
        self.log.push((key.to_vec(), previous));
        Ok(())
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        let inserted = self.inner.put_if_absent(key, value)?;
        if inserted {
            self.log.push((key.to_vec(), None));
        }
        Ok(inserted)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        let previous = self.inner.get(key)?;
        let deleted = self.inner.delete(key)?;
        if deleted {
            self.log.push((key.to_vec(), previous));
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

impl<S: KeyValueStore> Session<'_, S> {
    pub(super) fn savepoint_command(&mut self, command: &SavepointCommand) -> SqlResult<QueryResult> {
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
        self.savepoints.marks.push((name.text.clone(), undo_at));
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
        let undo_at = self.savepoints.marks[at].1;
        let Block::Writing(writer) = &mut self.block else {
            self.savepoints.marks.truncate(at + 1);
            self.savepoints.aborted = false;
            return Ok(QueryResult::command("ROLLBACK"));
        };
        let mut routed = crate::temporary::TemporaryWrite {
            inner: writer,
            objects: &mut self.temporary,
        };
        while self.savepoints.undo.len() > undo_at {
            let Some((key, previous)) = self.savepoints.undo.pop() else {
                break;
            };
            match previous {
                Some(value) => routed.put(&key, &value)?,
                None => {
                    routed.delete(&key)?;
                }
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
