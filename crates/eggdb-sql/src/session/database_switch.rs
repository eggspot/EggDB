use super::Session;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::settings::{Settings, SqlServerState};
use crate::store::KeyValueStore;
use std::sync::Arc;

/// What a session carries to the session that replaces it when its client switches to another
/// served database (MySQL's `USE` and `COM_INIT_DB`, SQL Server's `USE`): the user, the dialect,
/// the run-time settings, the limits and the cancel signal (D270). Made by
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

fn refused_database(role: &str, database: &str, dialect: Dialect) -> SqlError {
    let hint = format!(
        "Each database keeps its own roles: create the role \"{role}\" with LOGIN in \"{database}\", or connect as a role that may log in there."
    );
    match dialect {
        Dialect::MySql => SqlError::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("Access denied for user '{role}'@'%' to database '{database}'"),
        )
        .with_vendor_code(1044),
        Dialect::SqlServer => SqlError::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!(
                "The server principal \"{role}\" is not able to access the database \"{database}\" under the current security context."
            ),
        )
        .with_sqlserver_code(916),
        _ => SqlError::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for database \"{database}\""),
        )
        .with_detail(format!("Role \"{role}\" may not log in to it.")),
    }
    .with_hint(hint)
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
    /// store is `store`. A session that logged in as a stored role takes that role's
    /// privileges in `database`, since each database keeps its own roles: it is a superuser
    /// there only when the role is one there, and the switch fails with `42501` (MySQL's
    /// `1044`, SQL Server's `916`) when the role does not exist there or may not log in. The
    /// server's own user keeps its rights. The caller closes this session and builds the new
    /// one with [`DatabaseSwitch::into_session`].
    pub fn switch_to<S: KeyValueStore>(
        &self,
        store: &S,
        database: &str,
    ) -> SqlResult<DatabaseSwitch> {
        let checked_role = match self.stored_login_role() {
            None => None,
            Some(role) => {
                let mut probe = Session::with_dialect(store, self.connection_dialect);
                let stored = probe.read(|reader, _| crate::roles::stored_role(reader, role))?;
                let _ = probe.close();
                match stored.filter(|stored| stored.login) {
                    None => {
                        return Err(refused_database(role, database, self.connection_dialect));
                    }
                    Some(stored) if stored.superuser => None,
                    Some(stored) => Some(stored.name),
                }
            }
        };
        Ok(DatabaseSwitch {
            settings: moved_settings(&self.settings, database, checked_role.as_ref()),
            connection_settings: self
                .connection_settings
                .as_ref()
                .map(|remembered| moved_settings(remembered, database, checked_role.as_ref())),
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

    fn stored_login_role(&self) -> Option<&str> {
        self.settings
            .login_role
            .as_deref()
            .or(self.settings.checked_role.as_deref())
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
    /// with `25001`, and for a session logged in as a stored role that may not log in to it
    /// ([`ServedDatabases::may_log_in`](crate::ServedDatabases::may_log_in)) with `42501`.
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
        if let Some(role) = self.stored_login_role()
            && !served.may_log_in(&database, role)
        {
            return Err(refused_database(role, &database, self.connection_dialect));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_store::MemoryStore;
    use crate::served_databases::{ServedDatabaseError, ServedDatabases};
    use crate::tests::render;

    struct Logins {
        databases: [&'static str; 3],
        logins: [(&'static str, &'static str); 2],
    }

    impl ServedDatabases for Logins {
        fn names(&self) -> Vec<String> {
            self.databases
                .iter()
                .map(|name| (*name).to_owned())
                .collect()
        }

        fn create(&self, _name: &str) -> Result<(), ServedDatabaseError> {
            Err(ServedDatabaseError::Unsupported("fixed".to_owned()))
        }

        fn drop_database(&self, _name: &str) -> Result<(), ServedDatabaseError> {
            Err(ServedDatabaseError::Unsupported("fixed".to_owned()))
        }

        fn may_log_in(&self, name: &str, role: &str) -> bool {
            self.logins
                .iter()
                .any(|(database, login)| database.eq_ignore_ascii_case(name) && *login == role)
        }
    }

    fn run(session: &mut Session<'_>, sql: &str) -> SqlResult<Vec<String>> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs)
    }

    fn prepared(store: &MemoryStore, setup: &str) {
        let mut admin = Session::new(store);
        run(&mut admin, setup).unwrap();
        let _ = admin.close();
    }

    struct ThreeDatabases {
        main: MemoryStore,
        ours: MemoryStore,
        theirs: MemoryStore,
    }

    fn three_databases() -> ThreeDatabases {
        let databases = ThreeDatabases {
            main: MemoryStore::default(),
            ours: MemoryStore::default(),
            theirs: MemoryStore::default(),
        };
        prepared(
            &databases.main,
            "CREATE ROLE boss LOGIN SUPERUSER PASSWORD 'x'",
        );
        prepared(
            &databases.ours,
            "CREATE ROLE boss LOGIN PASSWORD 'x'; CREATE TABLE secrets (id INT); INSERT INTO secrets VALUES (1)",
        );
        prepared(
            &databases.theirs,
            "CREATE ROLE keeper LOGIN SUPERUSER PASSWORD 'x'; CREATE TABLE secrets (id INT); INSERT INTO secrets VALUES (2)",
        );
        databases
    }

    #[test]
    fn a_superuser_of_one_database_takes_only_the_privileges_its_role_has_in_the_target() {
        let databases = three_databases();
        let mut boss = Session::with_dialect(&databases.main, Dialect::MySql);
        boss.identify_role("boss", "main").unwrap();
        let switch = boss.switch_to(&databases.ours, "ours").unwrap();
        let _ = boss.close();
        let mut moved = switch.into_session(&databases.ours);
        let refused = run(&mut moved, "SELECT id FROM secrets").unwrap_err();
        assert!(refused.message().contains("permission denied"), "{refused}");
        assert_eq!(
            moved.settings.checked_role.as_deref(),
            Some("boss"),
            "the role is checked in the target"
        );
    }

    #[test]
    fn a_role_that_may_not_log_in_to_the_target_is_refused_with_each_protocols_error() {
        let databases = three_databases();
        for (dialect, code) in [(Dialect::MySql, 1044), (Dialect::SqlServer, 916)] {
            let mut boss = Session::with_dialect(&databases.main, dialect);
            boss.identify_role("boss", "main").unwrap();
            let refused = boss.switch_to(&databases.theirs, "theirs").unwrap_err();
            assert_eq!(refused.state(), SqlState::INSUFFICIENT_PRIVILEGE);
            assert_eq!(refused.vendor_code(), Some(code));
            let _ = boss.close();
        }
        let mut postgres = Session::new(&databases.main);
        postgres.identify_role("boss", "main").unwrap();
        let refused = postgres.switch_to(&databases.theirs, "theirs").unwrap_err();
        assert_eq!(refused.state(), SqlState::INSUFFICIENT_PRIVILEGE);
        assert_eq!(refused.vendor_code(), None);
    }

    #[test]
    fn use_refuses_a_database_the_role_may_not_log_in_to_before_the_switch() {
        let databases = three_databases();
        let served = Logins {
            databases: ["main", "ours", "theirs"],
            logins: [("main", "boss"), ("ours", "boss")],
        };
        let mut boss = Session::with_dialect(&databases.main, Dialect::SqlServer);
        boss.identify_role("boss", "main").unwrap();
        boss.set_served_databases(&served);
        let refused = run(&mut boss, "USE theirs").unwrap_err();
        assert_eq!(refused.vendor_code(), Some(916));
        assert_eq!(boss.take_database_switch(), None);
        run(&mut boss, "USE ours").unwrap();
        assert_eq!(boss.take_database_switch().as_deref(), Some("ours"));
    }

    #[test]
    fn the_servers_own_user_keeps_its_rights_in_every_database() {
        let databases = three_databases();
        let mut owner = Session::with_dialect(&databases.main, Dialect::MySql);
        owner.identify("root", "main");
        let switch = owner.switch_to(&databases.theirs, "theirs").unwrap();
        let _ = owner.close();
        let mut moved = switch.into_session(&databases.theirs);
        assert_eq!(
            run(&mut moved, "SELECT id FROM secrets").unwrap().concat(),
            run(
                &mut Session::new(&databases.theirs),
                "SELECT id FROM secrets"
            )
            .unwrap()
            .concat()
        );
    }
}
