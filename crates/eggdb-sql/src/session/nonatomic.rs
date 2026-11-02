use super::{Session, read_only_refusal};
use crate::sequences;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::TableCache;
use crate::notifications::Notifications;
use crate::plpgsql::{TransactionControl, TransactionEnd};
use crate::session::QueryResult;
use crate::store::{BoxedWrite, EntryVisitor, ErasedStore, KeyValueRead, KeyValueWrite, ScanDirection};
use crate::temporary::{TemporaryObjects, TemporaryRead, TemporaryWrite};

pub(super) struct NonatomicWrite<'a, 's> {
    store: &'s dyn ErasedStore,
    writer: Option<Box<dyn BoxedWrite + 's>>,
    objects: &'a mut TemporaryObjects,
    notifications: &'a mut Notifications,
}

fn no_transaction() -> SqlError {
    SqlError::new(
        SqlState::INVALID_TRANSACTION_TERMINATION,
        "the procedure's transaction has ended",
    )
}

impl NonatomicWrite<'_, '_> {
    fn current(&self) -> SqlResult<&(dyn BoxedWrite + '_)> {
        match &self.writer {
            Some(writer) => Ok(&**writer),
            None => Err(no_transaction()),
        }
    }

    fn routed(&mut self) -> SqlResult<TemporaryWrite<'_>> {
        match &mut self.writer {
            Some(writer) => Ok(TemporaryWrite {
                inner: writer,
                objects: &mut *self.objects,
            }),
            None => Err(no_transaction()),
        }
    }

    fn commit_current(&mut self, tables: &mut TableCache) -> SqlResult<()> {
        sequences::save_changed(&mut self.routed()?)?;
        let writer = self.writer.take().ok_or_else(no_transaction)?;
        let pending = crate::deferred_checks::take_statement_pending();
        if !pending.is_empty() {
            let checked = crate::constraints::check_pending(
                &TemporaryRead {
                    inner: &writer,
                    objects: &*self.objects,
                },
                tables,
                &pending,
            );
            if let Err(error) = checked {
                self.discard(writer);
                return Err(error);
            }
        }
        if let Err(error) = writer.commit() {
            self.objects.rollback();
            self.notifications.rollback();
            crate::activity::rolled_back();
            return Err(error);
        }
        self.objects.commit();
        for notification in crate::notifications::take_raised() {
            self.notifications.queue(notification, true)?;
        }
        self.notifications.commit();
        crate::activity::committed();
        sequences::mark_changes_committed();
        Ok(())
    }

    fn discard(&mut self, writer: Box<dyn BoxedWrite + '_>) {
        let _ = writer.rollback();
        let _ = crate::notifications::take_raised();
        self.objects.rollback();
        self.notifications.rollback();
        crate::activity::rolled_back();
    }
}

impl TransactionControl for NonatomicWrite<'_, '_> {
    fn end_transaction(&mut self, tables: &mut TableCache, end: TransactionEnd) -> SqlResult<()> {
        match end {
            TransactionEnd::Commit => self.commit_current(tables)?,
            TransactionEnd::Rollback => {
                let writer = self.writer.take().ok_or_else(no_transaction)?;
                let _ = crate::deferred_checks::take_statement_pending();
                self.discard(writer);
            }
        }
        let next = self.store.begin_boxed_write()?;
        self.writer = Some(next);
        sequences::load_for_sequence_calls(&*self)
    }
}

impl KeyValueRead for NonatomicWrite<'_, '_> {
    fn get(&self, key: &[u8]) -> SqlResult<Option<Vec<u8>>> {
        TemporaryRead {
            inner: self.current()?,
            objects: &*self.objects,
        }
        .get(key)
    }

    fn get_with(
        &self,
        key: &[u8],
        read: &mut dyn FnMut(&[u8]) -> SqlResult<()>,
    ) -> SqlResult<bool> {
        TemporaryRead {
            inner: self.current()?,
            objects: &*self.objects,
        }
        .get_with(key, read)
    }

    fn scan(
        &self,
        start: &[u8],
        end: &[u8],
        direction: ScanDirection,
        visit: &mut EntryVisitor<'_>,
    ) -> SqlResult<()> {
        TemporaryRead {
            inner: self.current()?,
            objects: &*self.objects,
        }
        .scan(start, end, direction, visit)
    }
}

impl KeyValueWrite for NonatomicWrite<'_, '_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.routed()?.put(key, value)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        self.routed()?.put_if_absent(key, value)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        self.routed()?.delete(key)
    }

    fn commit(self) -> SqlResult<()> {
        Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "a procedure's transaction commits through the session",
        ))
    }

    fn rollback(self) -> SqlResult<()> {
        Err(SqlError::new(
            SqlState::INTERNAL_ERROR,
            "a procedure's transaction rolls back through the session",
        ))
    }
}

impl<'s> Session<'s> {
    pub(super) fn runs_procedures_nonatomic(&self) -> bool {
        self.dialect == crate::dialect::Dialect::PostgreSql
            && matches!(self.block, super::Block::None)
            && self.autocommit()
            && !self.implicit_transaction
            && !self.savepoints.is_active()
    }

    pub(super) fn write_nonatomic(
        &mut self,
        command: &str,
        body: impl FnOnce(&mut dyn TransactionControl, &mut TableCache) -> SqlResult<QueryResult>,
    ) -> SqlResult<QueryResult> {
        self.refuse_writes_when_uncommittable()?;
        let _types = crate::user_types::enter_if_absent();
        let writer = self
            .store
            .begin_boxed_write()
            .map_err(|error| read_only_refusal(error, command))?;
        let mut control = NonatomicWrite {
            store: self.store,
            writer: Some(writer),
            objects: &mut self.temporary,
            notifications: &mut self.notifications,
        };
        let outcome = run_nonatomic(&mut control, &mut self.tables, body);
        let Some(writer) = control.writer.take() else {
            return outcome.and_then(|_| Err(no_transaction()));
        };
        self.end_autocommit_statement(writer, outcome, command, false, false, false)
    }
}

fn run_nonatomic(
    control: &mut NonatomicWrite<'_, '_>,
    tables: &mut TableCache,
    body: impl FnOnce(&mut dyn TransactionControl, &mut TableCache) -> SqlResult<QueryResult>,
) -> SqlResult<QueryResult> {
    sequences::load_for_sequence_calls(&*control)?;
    let result = body(&mut *control, tables).map_err(|error| sequences::explain_missing(&*control, error))?;
    sequences::save_changed(control)?;
    Ok(result)
}
