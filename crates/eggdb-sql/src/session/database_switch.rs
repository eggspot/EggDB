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

fn moved_settings(settings: &Settings, database: &str) -> Settings {
    let mut moved = settings.clone();
    moved.database = database.to_owned();
    moved.checked_role = None;
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
        "Each database keeps its own roles, and a role of one database is never trusted in another by its name: connect to \"{database}\" directly as a role of that database."
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
        .with_detail(format!(
            "Role \"{role}\" logged in to another database and may not move to this one."
        )),
    }
    .with_hint(hint)
}

const MOST_NAMED_HOLDINGS: usize = 8;

fn listed(held: &[String]) -> String {
    let named = &held[..held.len().min(MOST_NAMED_HOLDINGS)];
    let mut text = match named {
        [] => String::new(),
        [only] => only.clone(),
        [first @ .., last] => format!("{} and {last}", first.join(", ")),
    };
    if held.len() > named.len() {
        text.push_str(&format!(" (and {} more)", held.len() - named.len()));
    }
    text
}

fn refused_while_held(database: &str, held: &[String], dialect: Dialect) -> SqlError {
    let error = SqlError::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!(
            "cannot switch to database \"{database}\" while this session holds {}",
            listed(held)
        ),
    )
    .with_detail(
        "Temporary tables and session locks live in the session a switch closes, so they would be dropped and released without a word.",
    )
    .with_hint(format!(
        "Drop the temporary tables and release the session locks first, or open another connection to \"{database}\"."
    ));
    match dialect {
        Dialect::SqlServer => error.with_sqlserver_code(40508),
        Dialect::MySql => error.with_vendor_code(1235),
        _ => error,
    }
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
    /// store is `store`. Only the server's own user moves between databases: a session that
    /// logged in as a stored role was authenticated by its own database's roles, and another
    /// database's role of the same name is a different role, so the switch fails with `42501`
    /// (MySQL's `1044`, SQL Server's `916`) for it. A session holding a temporary table or a
    /// session-level lock (an advisory lock, or an application lock of `@LockOwner = 'Session'`)
    /// does not move either: the switch fails with `0A000` (SQL Server's `40508`, MySQL's
    /// `1235`), naming what it holds, since closing the session would drop and release them.
    /// The caller closes this session and builds the new one with
    /// [`DatabaseSwitch::into_session`].
    pub fn switch_to<S: KeyValueStore>(
        &self,
        _store: &S,
        database: &str,
    ) -> SqlResult<DatabaseSwitch> {
        if let Some(role) = self.stored_login_role() {
            return Err(refused_database(role, database, self.connection_dialect));
        }
        self.refuse_switch_while_holding(database)?;
        Ok(DatabaseSwitch {
            settings: moved_settings(&self.settings, database),
            connection_settings: self
                .connection_settings
                .as_ref()
                .map(|remembered| moved_settings(remembered, database)),
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

    fn held_by_session(&self) -> Vec<String> {
        let mut held: Vec<String> = self
            .temporary
            .relations()
            .into_iter()
            .map(|relation| {
                let kind = if relation.is_view { "view" } else { "table" };
                format!("temporary {kind} {}", relation.name)
            })
            .collect();
        held.extend(self.advisory_locks.session_lock_names());
        held
    }

    fn refuse_switch_while_holding(&self, database: &str) -> SqlResult<()> {
        let held = self.held_by_session();
        if held.is_empty() {
            Ok(())
        } else {
            Err(refused_while_held(database, &held, self.connection_dialect))
        }
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
    /// with `25001`, and for a session logged in as a stored role with `42501`: a stored role
    /// is trusted only in the database that authenticated it, so it connects to another one
    /// directly. While the session holds a temporary table or a session-level lock it fails
    /// with `0A000` (SQL Server's `40508`, MySQL's `1235`), naming them, as
    /// [`Session::switch_to`] does.
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
        if let Some(role) = self.stored_login_role() {
            return Err(refused_database(role, &database, self.connection_dialect));
        }
        if self.in_block() {
            return Err(SqlError::new(
                SqlState::ACTIVE_SQL_TRANSACTION,
                format!("cannot switch to database {database} inside a transaction"),
            )
            .with_hint("COMMIT or ROLLBACK first."));
        }
        self.refuse_switch_while_holding(&database)?;
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
        superusers: [(&'static str, &'static str); 2],
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

        fn is_superuser_in(&self, name: &str, role: &str) -> bool {
            self.superusers
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
    fn a_stored_role_never_moves_to_another_database_even_where_its_name_is_a_role() {
        let databases = three_databases();
        for (dialect, code) in [
            (Dialect::MySql, Some(1044)),
            (Dialect::SqlServer, Some(916)),
            (Dialect::PostgreSql, None),
        ] {
            for (target, store) in [("ours", &databases.ours), ("theirs", &databases.theirs)] {
                let mut boss = Session::with_dialect(&databases.main, dialect);
                boss.identify_role("boss", "main").unwrap();
                let refused = boss.switch_to(store, target).unwrap_err();
                assert_eq!(refused.state(), SqlState::INSUFFICIENT_PRIVILEGE);
                assert_eq!(refused.vendor_code(), code, "{dialect:?} {target}");
                assert!(
                    refused.hint().is_some_and(|hint| hint.contains("directly")),
                    "{refused:?}"
                );
                let _ = boss.close();
            }
        }
    }

    #[test]
    fn a_role_named_like_another_databases_superuser_cannot_use_or_drop_that_database() {
        let databases = three_databases();
        prepared(
            &databases.main,
            "CREATE ROLE keeper LOGIN SUPERUSER PASSWORD 'mine'",
        );
        let served = Logins {
            databases: ["main", "ours", "theirs"],
            superusers: [("main", "keeper"), ("theirs", "keeper")],
        };
        let mut keeper = Session::with_dialect(&databases.main, Dialect::SqlServer);
        keeper.identify_role("keeper", "main").unwrap();
        keeper.set_served_databases(&served);
        let refused = run(&mut keeper, "USE theirs").unwrap_err();
        assert_eq!(refused.vendor_code(), Some(916));
        assert_eq!(keeper.take_database_switch(), None);
        let refused = run(&mut keeper, "DROP DATABASE theirs").unwrap_err();
        assert_eq!(
            refused.state(),
            SqlState::INSUFFICIENT_PRIVILEGE,
            "{refused}"
        );
        run(&mut keeper, "USE main").unwrap();
        assert_eq!(keeper.take_database_switch(), None);
    }

    fn served() -> Logins {
        Logins {
            databases: ["main", "ours", "theirs"],
            superusers: [("main", "nobody"), ("theirs", "nobody")],
        }
    }

    #[test]
    fn use_refuses_to_leave_a_temporary_table_or_a_session_lock_behind() {
        let databases = three_databases();
        let served = served();
        let mut session = Session::with_dialect(&databases.main, Dialect::SqlServer);
        session.set_served_databases(&served);
        run(&mut session, "CREATE TABLE #work (id INT)").unwrap();
        run(
            &mut session,
            "EXEC sp_getapplock @Resource = 'nightly', @LockMode = 'Exclusive', @LockOwner = 'Session'",
        )
        .unwrap();
        let refused = run(&mut session, "USE theirs").unwrap_err();
        assert_eq!(refused.state(), SqlState::FEATURE_NOT_SUPPORTED);
        assert_eq!(refused.vendor_code(), Some(40508));
        assert!(
            refused.message().contains("temporary table #work")
                && refused.message().contains("application lock 'nightly'"),
            "{refused}"
        );
        assert!(
            refused
                .hint()
                .is_some_and(|hint| hint.contains("another connection")),
            "{refused:?}"
        );
        assert_eq!(session.take_database_switch(), None);
        assert!(session.switch_to(&databases.theirs, "theirs").is_err());
        run(&mut session, "DROP TABLE #work").unwrap();
        let refused = run(&mut session, "USE theirs").unwrap_err();
        assert!(
            !refused.message().contains("#work")
                && refused.message().contains("application lock 'nightly'"),
            "{refused}"
        );
        run(
            &mut session,
            "EXEC sp_releaseapplock @Resource = 'nightly', @LockOwner = 'Session'",
        )
        .unwrap();
        run(&mut session, "USE theirs").unwrap();
        assert_eq!(session.take_database_switch().as_deref(), Some("theirs"));
        assert!(session.switch_to(&databases.theirs, "theirs").is_ok());
        let _ = session.close();
    }

    #[test]
    fn mysql_use_names_a_held_temporary_table_with_its_own_number() {
        let databases = three_databases();
        let served = served();
        let mut session = Session::with_dialect(&databases.main, Dialect::MySql);
        session.set_served_databases(&served);
        run(&mut session, "CREATE TEMPORARY TABLE scratch (id INT)").unwrap();
        let refused = run(&mut session, "USE ours").unwrap_err();
        assert_eq!(refused.vendor_code(), Some(1235));
        assert!(
            refused.message().contains("temporary table scratch"),
            "{refused}"
        );
        run(&mut session, "USE main").unwrap();
        assert_eq!(session.take_database_switch(), None);
        let _ = session.close();
    }

    #[test]
    fn a_long_list_of_holdings_is_cut_short() {
        let held: Vec<String> = (1..=10).map(|n| format!("advisory lock {n}")).collect();
        let error = refused_while_held("ours", &held, Dialect::PostgreSql);
        assert!(
            error
                .message()
                .ends_with("advisory lock 7 and advisory lock 8 (and 2 more)"),
            "{error}"
        );
        assert_eq!(listed(&held[..1]), "advisory lock 1");
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
