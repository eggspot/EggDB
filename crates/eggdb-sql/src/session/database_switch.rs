use super::Session;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::settings::{Settings, SqlServerState};
use crate::store::KeyValueStore;
use std::sync::Arc;

/// What a session carries to the session that replaces it when its client switches to another
/// served database (MySQL's `USE` and `COM_INIT_DB`, SQL Server's `USE`): the user, the dialect,
/// the run-time settings, the limits and the cancel signal (DNEW-R-M6a-db). Made by
/// [`Session::switch_to`]; [`DatabaseSwitch::into_session`] builds the new session.
pub struct DatabaseSwitch {
    settings: Settings,
    connection_settings: Option<Settings>,
    dialect: Dialect,
    connection_dialect: Dialect,
    max_value_bytes: usize,
    max_row_bytes: usize,
    max_result_rows: Option<u64>,
    cancel: crate::cancel::CancelSignal,
    oracle_nls: Arc<crate::oracle_nls::NlsParameters>,
}

impl std::fmt::Debug for DatabaseSwitch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DatabaseSwitch")
            .field("database", &self.settings.database)
            .finish()
    }
}

fn moved_settings(settings: &Settings, database: &str, checked_role: Option<&String>) -> Settings {
    let mut moved = settings.clone();
    moved.database = database.to_owned();
    moved.checked_role = checked_role.cloned();
    moved.oracle_user_schema = None;
    moved.sqlserver = SqlServerState {
        connection_id: settings.sqlserver.connection_id,
        no_count: settings.sqlserver.no_count,
        implicit_transactions: settings.sqlserver.implicit_transactions,
        xact_abort: settings.sqlserver.xact_abort,
        format_only: settings.sqlserver.format_only,
        ..SqlServerState::default()
    };
    moved
}

impl DatabaseSwitch {
    /// The session over `store`, the database the switch went to, with what the old session
    /// carried. The server gives it its databases again
    /// ([`Session::set_served_databases`]).
    pub fn into_session<S: KeyValueStore>(self, store: &S) -> Session<'_> {
        let mut session = Session::with_dialect(store, self.connection_dialect);
        session.dialect = self.dialect;
        session.settings = Arc::new(self.settings);
        session.connection_settings = self.connection_settings.map(Arc::new);
        session.max_value_bytes = self.max_value_bytes;
        session.max_row_bytes = self.max_row_bytes;
        session.max_result_rows = self.max_result_rows;
        session.cancel = self.cancel;
        session.oracle_nls = self.oracle_nls;
        if session.dialect == Dialect::Oracle {
            let _ = session.refresh_oracle_user_schema();
        }
        session
    }
}

impl Session<'_> {
    /// Prepares the switch of this session's client to the served database `database`, whose
    /// store is `store`: the user's role is checked again there, since each database keeps its
    /// own roles. Fails with `28000` when the role may not log in to it. The caller closes this
    /// session and builds the new one with [`DatabaseSwitch::into_session`].
    pub fn switch_to<S: KeyValueStore>(
        &self,
        store: &S,
        database: &str,
    ) -> SqlResult<DatabaseSwitch> {
        let checked_role = match &self.settings.checked_role {
            None => None,
            Some(role) => {
                let mut probe = Session::with_dialect(store, self.connection_dialect);
                let stored = probe.read(|reader, _| crate::roles::stored_role(reader, role))?;
                let _ = probe.close();
                match stored.filter(|stored| stored.login) {
                    None => {
                        return Err(SqlError::new(
                            SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
                            format!(
                                "role \"{role}\" is not permitted to log in to database \"{database}\""
                            ),
                        ));
                    }
                    Some(stored) if stored.superuser => None,
                    Some(stored) => Some(stored.name),
                }
            }
        };
        Ok(DatabaseSwitch {
            settings: moved_settings(&self.settings, database, checked_role.as_ref()),
            connection_settings: self.connection_settings.as_ref().map(|remembered| {
                moved_settings(remembered, database, checked_role.as_ref())
            }),
            dialect: self.dialect,
            connection_dialect: self.connection_dialect,
            max_value_bytes: self.max_value_bytes,
            max_row_bytes: self.max_row_bytes,
            max_result_rows: self.max_result_rows,
            cancel: self.cancel.clone(),
            oracle_nls: Arc::clone(&self.oracle_nls),
        })
    }

    /// The served database a `USE` of the last statements switched this session's client to,
    /// taken once: the server then attaches it and moves the client there with
    /// [`Session::switch_to`]. Statements after such a `USE` in the same batch fail with
    /// `0A000` until then.
    pub fn take_database_switch(&mut self) -> Option<String> {
        self.database_switch.take()
    }

    pub(super) fn refuse_after_database_switch(&self) -> SqlResult<()> {
        match &self.database_switch {
            None => Ok(()),
            Some(database) => Err(SqlError::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("this statement follows USE {database} in the same batch"),
            )
            .with_hint(
                "Send the statements after USE in the next batch, which runs on the database USE switched to.",
            )),
        }
    }

    /// Runs `USE name` in MySQL's and SQL Server's sense: `Ok(Some)` with the database's served
    /// name when `name` is this session's database or one the server serves (ignoring ASCII
    /// case), `Ok(None)` when it is neither. For another database the switch is then
    /// [taken](Session::take_database_switch) by the server; inside a transaction it fails
    /// with `25001`.
    pub fn use_database(&mut self, name: &str) -> SqlResult<Option<String>> {
        if name.eq_ignore_ascii_case(&self.settings.database) {
            return Ok(Some(self.settings.database.clone()));
        }
        let Some(served) = self.served_databases else {
            return Ok(None);
        };
        let Some(database) = served.matching_name(name) else {
            return Ok(None);
        };
        if self.in_block() {
            return Err(SqlError::new(
                SqlState::ACTIVE_SQL_TRANSACTION,
                format!("cannot switch to database {database} inside a transaction"),
            )
            .with_hint("COMMIT or ROLLBACK first."));
        }
        self.database_switch = Some(database.clone());
        Ok(Some(database))
    }
}
