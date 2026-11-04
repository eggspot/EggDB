use super::{Block, Session};
use crate::ast::{CharacteristicsTarget, TransactionCharacteristics};
use crate::dialect::Dialect;
use crate::encoding::is_temporary_key;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::QueryResult;
use crate::settings::{SettingScope, boolean_setting};
use crate::store::{EntryVisitor, KeyValueRead, KeyValueWrite, ScanDirection};
use crate::types::Value;
use std::sync::Arc;

const SET_TRANSACTION_OUTSIDE_TRANSACTION: &str =
    "WARNING:  SET TRANSACTION can only be used in transaction blocks";

const DEFINITION_COMMANDS: [&str; 9] = [
    "CREATE",
    "ALTER",
    "DROP",
    "COMMENT",
    "GRANT",
    "REVOKE",
    "TRUNCATE",
    "SECURITY LABEL",
    "SELECT INTO",
];

#[derive(Debug, Default)]
pub(super) struct TransactionMode {
    pub(super) writing_block_read_only: bool,
    pub(super) has_read: bool,
    pub(super) isolation: Option<String>,
    pub(super) next_read_only: Option<bool>,
}

impl TransactionMode {
    pub(super) fn forget_transaction(&mut self) {
        self.writing_block_read_only = false;
        self.has_read = false;
        self.isolation = None;
    }

    pub(super) fn started(&mut self) {
        self.forget_transaction();
        self.next_read_only = None;
    }
}

pub(super) fn refused_in_read_only_transaction(command: &str) -> SqlError {
    SqlError::new(
        SqlState::READ_ONLY_SQL_TRANSACTION,
        format!("cannot execute {command} in a read-only transaction"),
    )
}

pub(super) fn changes_definitions(command: &str) -> bool {
    DEFINITION_COMMANDS.iter().any(|word| {
        command
            .strip_prefix(word)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    })
}

pub(super) fn refuse_permanent_target(
    refuses_writes: bool,
    command: &str,
    target: &crate::ast::TableReference,
) -> SqlResult<()> {
    if target.derived.is_some() || target.generated.is_some() {
        return Ok(());
    }
    refuse_permanent_name(refuses_writes, command, &target.name)
}

pub(super) fn refuse_permanent_statement_target(
    refuses_writes: bool,
    statement: &crate::ast::Statement,
) -> SqlResult<()> {
    use crate::ast::Statement;
    match statement {
        Statement::Insert(insert) => {
            refuse_permanent_target(refuses_writes, "INSERT", &insert.table)
        }
        Statement::Update(update) => {
            refuse_permanent_target(refuses_writes, "UPDATE", &update.table)
        }
        Statement::Delete(delete) => {
            refuse_permanent_target(refuses_writes, "DELETE", &delete.table)
        }
        Statement::Merge(merge) => refuse_permanent_target(refuses_writes, "MERGE", &merge.target),
        _ => Ok(()),
    }
}

pub(super) fn refuse_permanent_name(
    refuses_writes: bool,
    command: &str,
    target: &crate::ast::Name,
) -> SqlResult<()> {
    if refuses_writes
        && !crate::temporary::is_temporary_name(&target.text)
        && !crate::temp_tables::is_temp_table(&target.text)
    {
        return Err(refused_in_read_only_transaction(command));
    }
    Ok(())
}

fn before_any_query(what: &str) -> SqlError {
    SqlError::new(
        SqlState::ACTIVE_SQL_TRANSACTION,
        format!("{what} must be called before any query"),
    )
}

pub(super) struct ReadOnlyTransactionWrite<'w> {
    pub(super) inner: &'w mut dyn KeyValueWrite,
    pub(super) command: &'w str,
}

impl ReadOnlyTransactionWrite<'_> {
    fn allowed(&self, key: &[u8]) -> SqlResult<()> {
        if is_temporary_key(key) {
            Ok(())
        } else {
            Err(refused_in_read_only_transaction(self.command))
        }
    }
}

impl KeyValueRead for ReadOnlyTransactionWrite<'_> {
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

impl KeyValueWrite for ReadOnlyTransactionWrite<'_> {
    fn put(&mut self, key: &[u8], value: &[u8]) -> SqlResult<()> {
        self.allowed(key)?;
        self.inner.put(key, value)
    }

    fn put_if_absent(&mut self, key: &[u8], value: &[u8]) -> SqlResult<bool> {
        self.allowed(key)?;
        self.inner.put_if_absent(key, value)
    }

    fn delete(&mut self, key: &[u8]) -> SqlResult<bool> {
        self.allowed(key)?;
        self.inner.delete(key)
    }

    fn restore(&mut self, key: &[u8], previous: Option<&[u8]>) -> SqlResult<()> {
        self.allowed(key)?;
        self.inner.restore(key, previous)
    }

    fn commit(self) -> SqlResult<()> {
        Ok(())
    }

    fn rollback(self) -> SqlResult<()> {
        Ok(())
    }
}

fn mysql_isolation(level: &str) -> String {
    level.to_ascii_uppercase().replace(' ', "-")
}

impl<'s> Session<'s> {
    pub(super) fn transaction_is_read_only(&self) -> bool {
        match self.block {
            Block::Reading(_) => true,
            Block::Writing(_) => self.transaction_mode.writing_block_read_only,
            Block::None | Block::Failed => self.new_transactions_read_only(),
        }
    }

    pub(super) fn new_transactions_read_only(&self) -> bool {
        match self.dialect {
            Dialect::PostgreSql => self.settings.is_on("default_transaction_read_only"),
            Dialect::MySql => self.transaction_mode.next_read_only.unwrap_or_else(|| {
                self.settings
                    .mysql
                    .system("transaction_read_only")
                    .and_then(|value| value.as_integer())
                    .is_some_and(|flag| flag != 0)
            }),
            Dialect::Oracle | Dialect::Sqlite | Dialect::SqlServer => false,
        }
    }

    pub(super) fn sync_transaction_read_only(&mut self) {
        if self.dialect != Dialect::PostgreSql {
            return;
        }
        let read_only = self.transaction_is_read_only();
        let shown = if read_only { "on" } else { "off" };
        if self.settings.known_value("transaction_read_only") == Some(shown) {
            return;
        }
        Arc::make_mut(&mut self.settings).show_transaction_read_only(read_only);
    }

    pub(super) fn write_in_statement_snapshot(
        &mut self,
        command: &str,
        body: impl FnOnce(
            &mut dyn KeyValueWrite,
            &mut crate::execute::TableCache,
        ) -> SqlResult<QueryResult>,
    ) -> SqlResult<QueryResult> {
        let keeps_transaction_open = self.dialect == Dialect::MySql && !self.autocommit();
        self.block = Block::Reading(self.store.begin_boxed_read()?);
        let result = self.write(command, body);
        if keeps_transaction_open {
            return result;
        }
        if matches!(self.block, Block::Reading(_)) {
            self.block = Block::None;
            if result.is_ok() {
                self.temporary.commit();
            } else {
                self.temporary.rollback();
            }
        }
        result
    }

    pub(super) fn set_transaction_statement(
        &mut self,
        characteristics: &TransactionCharacteristics,
    ) -> SqlResult<QueryResult> {
        match self.dialect {
            Dialect::PostgreSql => self.set_postgresql_transaction(characteristics),
            Dialect::MySql => self.set_mysql_transaction(characteristics),
            Dialect::Oracle => self.set_oracle_transaction(characteristics),
            Dialect::Sqlite | Dialect::SqlServer => Ok(QueryResult::command("SET")),
        }
    }

    pub(super) fn set_transaction_parameter(
        &mut self,
        name: &str,
        value: Option<&str>,
    ) -> SqlResult<QueryResult> {
        let mut characteristics = TransactionCharacteristics {
            target: CharacteristicsTarget::CurrentTransaction,
            read_only: None,
            isolation: None,
        };
        if name.eq_ignore_ascii_case("transaction_read_only") {
            let read_only = match value {
                Some(value) => boolean_setting(value).ok_or_else(|| {
                    SqlError::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "parameter \"transaction_read_only\" requires a Boolean value",
                    )
                })?,
                None => self.new_transactions_read_only(),
            };
            characteristics.read_only = Some(read_only);
        } else {
            let level = value
                .map(|value| value.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "serializable".to_owned());
            if !matches!(
                level.as_str(),
                "serializable" | "repeatable read" | "read committed" | "read uncommitted"
            ) {
                return Err(SqlError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid value for parameter \"transaction_isolation\": \"{level}\""),
                ));
            }
            characteristics.isolation = Some(level);
        }
        if matches!(self.block, Block::None) {
            return Ok(QueryResult::command("SET"));
        }
        self.set_postgresql_transaction(&characteristics)
    }

    fn set_postgresql_transaction(
        &mut self,
        characteristics: &TransactionCharacteristics,
    ) -> SqlResult<QueryResult> {
        let set = QueryResult::command("SET");
        if characteristics.target == CharacteristicsTarget::Session {
            if let Some(read_only) = characteristics.read_only {
                self.change_setting(
                    "default_transaction_read_only",
                    Some(if read_only { "on" } else { "off" }),
                    SettingScope::Session,
                )?;
            }
            return Ok(set);
        }
        if matches!(self.block, Block::None) {
            return Ok(set.with_notice(SET_TRANSACTION_OUTSIDE_TRANSACTION.into()));
        }
        if let Some(level) = &characteristics.isolation {
            let current = self
                .transaction_mode
                .isolation
                .clone()
                .unwrap_or_else(|| "serializable".to_owned());
            if *level != current {
                if self.savepoints.is_active() {
                    return Err(SqlError::new(
                        SqlState::ACTIVE_SQL_TRANSACTION,
                        "SET TRANSACTION ISOLATION LEVEL must not be called in a subtransaction",
                    ));
                }
                if self.transaction_mode.has_read {
                    return Err(before_any_query("SET TRANSACTION ISOLATION LEVEL"));
                }
            }
        }
        if characteristics.read_only == Some(false) && self.transaction_is_read_only() {
            if self.savepoints.is_active() {
                return Err(SqlError::new(
                    SqlState::ACTIVE_SQL_TRANSACTION,
                    "cannot set transaction read-write mode inside a read-only transaction",
                ));
            }
            if self.transaction_mode.has_read {
                return Err(SqlError::new(
                    SqlState::ACTIVE_SQL_TRANSACTION,
                    "transaction read-write mode must be set before any query",
                ));
            }
        }
        if let Some(level) = &characteristics.isolation {
            self.transaction_mode.isolation = Some(level.clone());
        }
        if let Some(read_only) = characteristics.read_only {
            self.change_transaction_access(read_only)?;
        }
        Ok(set)
    }

    fn change_transaction_access(&mut self, read_only: bool) -> SqlResult<()> {
        let untouched = !self.transaction_mode.has_read
            && !self.savepoints.is_active()
            && !self.block_is_implicit;
        let writing = matches!(self.block, Block::Writing(_));
        let reading = matches!(self.block, Block::Reading(_));
        if writing && read_only && untouched {
            let reader = self.store.begin_boxed_read()?;
            if let Block::Writing(writer) =
                std::mem::replace(&mut self.block, Block::Reading(reader))
            {
                writer.rollback()?;
            }
            self.transaction_mode.writing_block_read_only = false;
        } else if writing {
            self.transaction_mode.writing_block_read_only = read_only;
        } else if reading && !read_only {
            let writer = self.store.begin_boxed_write()?;
            self.block = Block::Writing(writer);
            self.transaction_mode.writing_block_read_only = false;
        }
        Ok(())
    }

    fn set_mysql_transaction(
        &mut self,
        characteristics: &TransactionCharacteristics,
    ) -> SqlResult<QueryResult> {
        let set = QueryResult::command("SET");
        match characteristics.target {
            CharacteristicsTarget::Global => Ok(set),
            CharacteristicsTarget::Session => {
                let variables = &mut Arc::make_mut(&mut self.settings).mysql;
                if let Some(read_only) = characteristics.read_only {
                    for name in ["transaction_read_only", "tx_read_only"] {
                        let _ =
                            variables.set_system(name, Some(Value::BigInt(i64::from(read_only))));
                    }
                }
                if let Some(level) = &characteristics.isolation {
                    let _ = variables.set_system(
                        "transaction_isolation",
                        Some(Value::Text(mysql_isolation(level))),
                    );
                }
                Ok(set)
            }
            CharacteristicsTarget::CurrentTransaction | CharacteristicsTarget::NextTransaction => {
                if self.in_block() {
                    return Err(SqlError::new(
                        SqlState::ACTIVE_SQL_TRANSACTION,
                        "Transaction characteristics can't be changed while a transaction is in progress",
                    )
                    .with_vendor_code(1568));
                }
                if let Some(read_only) = characteristics.read_only {
                    self.transaction_mode.next_read_only = Some(read_only);
                }
                Ok(set)
            }
        }
    }

    fn set_oracle_transaction(
        &mut self,
        characteristics: &TransactionCharacteristics,
    ) -> SqlResult<QueryResult> {
        if !matches!(self.block, Block::None) {
            return Err(SqlError::new(
                SqlState::ACTIVE_SQL_TRANSACTION,
                "SET TRANSACTION must be first statement of transaction",
            )
            .with_oracle_code(1453));
        }
        if characteristics.read_only == Some(true) {
            self.begin(true)?;
        }
        Ok(QueryResult::command("SET TRANSACTION"))
    }
}
