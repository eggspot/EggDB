//! The databases one server serves (D184, D270): `CREATE DATABASE` and
//! `DROP DATABASE` run through [`ServedDatabases`], which the server implements over its data
//! directory, one file per database.

use crate::ast::Name;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::session::QueryResult;
use crate::store::KeyValueStore;

const MAX_DATABASE_NAME_BYTES: usize = 63;
const RESERVED_FILE_NAMES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Why a served database could not be created or dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServedDatabaseError {
    /// A database with that name is already served.
    Exists,
    /// No database with that name is served.
    Missing,
    /// A session is connected to the database.
    InUse,
    /// The server cannot create or drop databases, with the reason.
    Unsupported(String),
    /// Creating or removing the database's file failed, with the reason.
    Failed(String),
}

/// The databases one server serves, each reached by name: the server lists, creates and drops
/// them for `CREATE DATABASE`, `DROP DATABASE`, `pg_database`, `SHOW DATABASES` and
/// `sys.databases`.
pub trait ServedDatabases: Sync {
    /// The names of the served databases, in the order the catalogs list them.
    fn names(&self) -> Vec<String>;

    /// Creates an empty database named `name`, which [`is_valid_database_name`] accepts.
    fn create(&self, name: &str) -> Result<(), ServedDatabaseError>;

    /// Drops the database named `name` and removes its data, unless a session is connected to
    /// it.
    fn drop_database(&self, name: &str) -> Result<(), ServedDatabaseError>;

    /// A number that changes whenever the list of databases does, so a session reads
    /// [`ServedDatabases::names`] again only then; `None`, the default, reads it at every
    /// statement.
    fn listing_version(&self) -> Option<u64> {
        None
    }

    /// The served name equal to `name` ignoring ASCII case, as MySQL and SQL Server compare
    /// database names, or `None` when no database has it.
    fn matching_name(&self, name: &str) -> Option<String> {
        let names = self.names();
        if names.iter().any(|served| served == name) {
            return Some(name.to_owned());
        }
        names
            .into_iter()
            .find(|served| served.eq_ignore_ascii_case(name))
    }
}

/// [`ServedDatabases`] that a server's connections attach to by name: a connection runs its
/// session on the store [`DatabaseDirectory::attach`] hands out and holds it while it is
/// connected, which keeps the database from being dropped under it.
pub trait DatabaseDirectory: ServedDatabases {
    /// The store a connection holds while its session runs on one database.
    type Attached<'d>: KeyValueStore
    where
        Self: 'd;

    /// The database named exactly `name`, or `None` when none is served by that name.
    fn attach(&self, name: &str) -> Option<Self::Attached<'_>>;
}

/// A fixed list of databases, each reached by name, for a server that neither creates nor
/// drops databases while it runs: `CREATE DATABASE` and `DROP DATABASE` fail with `0A000`.
#[derive(Debug)]
pub struct FixedDatabases<'a, S> {
    databases: &'a [(&'a str, &'a S)],
}

impl<'a, S> FixedDatabases<'a, S> {
    /// The databases `databases` lists, the server's default first.
    #[must_use]
    pub fn new(databases: &'a [(&'a str, &'a S)]) -> Self {
        Self { databases }
    }
}

impl<S: Sync> ServedDatabases for FixedDatabases<'_, S> {
    fn names(&self) -> Vec<String> {
        self.databases
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect()
    }

    fn create(&self, _name: &str) -> Result<(), ServedDatabaseError> {
        Err(fixed_list())
    }

    fn listing_version(&self) -> Option<u64> {
        Some(0)
    }

    fn drop_database(&self, _name: &str) -> Result<(), ServedDatabaseError> {
        Err(fixed_list())
    }
}

impl<'a, S: KeyValueStore + Sync> DatabaseDirectory for FixedDatabases<'a, S> {
    type Attached<'d>
        = &'a S
    where
        Self: 'd;

    fn attach(&self, name: &str) -> Option<&'a S> {
        self.databases
            .iter()
            .find(|(served, _)| *served == name)
            .map(|(_, store)| *store)
    }
}

fn fixed_list() -> ServedDatabaseError {
    ServedDatabaseError::Unsupported(
        "CREATE DATABASE and DROP DATABASE are not supported by a server with a fixed list of databases; eggdb serve <directory> serves one database per .eggdb file and creates and drops them"
            .to_owned(),
    )
}

/// Whether `name` may name a served database: 1 to 63 bytes of letters, digits, `_`, `-` and
/// `$`, not starting with `-`, and not a name Windows reserves for devices, so that it is a
/// safe file name on every platform.
#[must_use]
pub fn is_valid_database_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_DATABASE_NAME_BYTES
        && !name.starts_with('-')
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '-' | '$'))
        && !RESERVED_FILE_NAMES
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DatabaseStatement {
    Create { name: Name, if_not_exists: bool },
    Drop { names: Vec<Name>, if_exists: bool },
}

impl DatabaseStatement {
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            Self::Create { .. } => "CREATE DATABASE",
            Self::Drop { .. } => "DROP DATABASE",
        }
    }
}

pub(crate) struct DatabaseCommandContext<'a> {
    pub(crate) served: Option<&'a dyn ServedDatabases>,
    pub(crate) dialect: Dialect,
    pub(crate) current_database: &'a str,
    pub(crate) superuser: bool,
    pub(crate) may_create_databases: bool,
    pub(crate) in_block: bool,
}

pub(crate) fn run(
    context: &DatabaseCommandContext<'_>,
    statement: &DatabaseStatement,
) -> SqlResult<QueryResult> {
    let tag = statement.tag();
    let Some(served) = context.served else {
        return Err(SqlError::not_supported(format!("{tag} outside a server"))
            .with_hint("eggdb serve <directory> serves one database per .eggdb file in the directory and runs CREATE DATABASE and DROP DATABASE; outside it, create a database with eggdb sql <name>.eggdb --create."));
    };
    if context.in_block && context.dialect != Dialect::MySql {
        return Err(SqlError::new(
            SqlState::ACTIVE_SQL_TRANSACTION,
            format!("{tag} cannot run inside a transaction block"),
        ));
    }
    match statement {
        DatabaseStatement::Create {
            name,
            if_not_exists,
        } => {
            if !(context.superuser || context.may_create_databases) {
                return Err(denied("create database").at(name.position));
            }
            if !is_valid_database_name(&name.text) {
                return Err(invalid_name(&name.text).at(name.position));
            }
            match served.create(&name.text) {
                Ok(()) => Ok(QueryResult::command(tag)),
                Err(ServedDatabaseError::Exists) if *if_not_exists => Ok(QueryResult::command(tag)
                    .with_notice(exists_notice(&name.text, context.dialect))),
                Err(error) => {
                    Err(create_error(&name.text, &error, context.dialect).at(name.position))
                }
            }
        }
        DatabaseStatement::Drop { names, if_exists } => {
            let mut result = QueryResult::command(tag);
            for name in names {
                if !context.superuser {
                    return Err(denied("drop database").at(name.position));
                }
                if name.text.eq_ignore_ascii_case(context.current_database) {
                    return Err(numbered_in_use(
                        SqlError::new(
                            SqlState::OBJECT_IN_USE,
                            "cannot drop the currently open database",
                        ),
                        context.dialect,
                    )
                    .with_hint("Connect to another database first.")
                    .at(name.position));
                }
                match served.drop_database(&name.text) {
                    Ok(()) => {}
                    Err(ServedDatabaseError::Missing) if *if_exists => {
                        result = result.with_notice(missing_notice(&name.text, context.dialect));
                    }
                    Err(error) => {
                        return Err(
                            drop_error(&name.text, &error, context.dialect).at(name.position)
                        );
                    }
                }
            }
            Ok(result)
        }
    }
}

fn numbered_in_use(error: SqlError, dialect: Dialect) -> SqlError {
    match dialect {
        Dialect::SqlServer => error.with_sqlserver_code(3702),
        _ => error,
    }
}

fn denied(what: &str) -> SqlError {
    SqlError::new(
        SqlState::INSUFFICIENT_PRIVILEGE,
        format!("permission denied to {what}"),
    )
    .with_hint("Only a superuser, or a role with CREATEDB for CREATE DATABASE, may do this.")
}

fn invalid_name(name: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_NAME,
        format!("database name \"{name}\" is not allowed"),
    )
    .with_hint("A database is a file named after it: use 1 to 63 letters, digits, _, - or $.")
}

fn exists_notice(name: &str, dialect: Dialect) -> String {
    match dialect {
        Dialect::MySql => format!("NOTICE:  Can't create database '{name}'; database exists"),
        _ => format!("NOTICE:  database \"{name}\" already exists, skipping"),
    }
}

fn missing_notice(name: &str, dialect: Dialect) -> String {
    match dialect {
        Dialect::MySql => format!("NOTICE:  Can't drop database '{name}'; database doesn't exist"),
        _ => format!("NOTICE:  database \"{name}\" does not exist, skipping"),
    }
}

fn create_error(name: &str, error: &ServedDatabaseError, dialect: Dialect) -> SqlError {
    match (error, dialect) {
        (ServedDatabaseError::Exists, Dialect::MySql) => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!("Can't create database '{name}'; database exists"),
        )
        .with_vendor_code(1007),
        (ServedDatabaseError::Exists, Dialect::SqlServer) => SqlError::new(
            SqlState::DUPLICATE_DATABASE,
            format!("Database '{name}' already exists. Choose a different database name."),
        )
        .with_sqlserver_code(1801),
        (ServedDatabaseError::Exists, _) => SqlError::new(
            SqlState::DUPLICATE_DATABASE,
            format!("database \"{name}\" already exists"),
        ),
        _ => common_error(name, error),
    }
}

fn drop_error(name: &str, error: &ServedDatabaseError, dialect: Dialect) -> SqlError {
    match (error, dialect) {
        (ServedDatabaseError::Missing, Dialect::MySql) => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!("Can't drop database '{name}'; database doesn't exist"),
        )
        .with_vendor_code(1008),
        (ServedDatabaseError::Missing, Dialect::SqlServer) => SqlError::new(
            SqlState::INVALID_CATALOG_NAME,
            format!(
                "Cannot drop the database '{name}', because it does not exist or you do not have permission."
            ),
        )
        .with_sqlserver_code(3701),
        (ServedDatabaseError::Missing, _) => SqlError::new(
            SqlState::INVALID_CATALOG_NAME,
            format!("database \"{name}\" does not exist"),
        ),
        (ServedDatabaseError::InUse, Dialect::SqlServer) => SqlError::new(
            SqlState::OBJECT_IN_USE,
            format!("Cannot drop database \"{name}\" because it is currently in use."),
        )
        .with_sqlserver_code(3702),
        (ServedDatabaseError::InUse, _) => SqlError::new(
            SqlState::OBJECT_IN_USE,
            format!("database \"{name}\" is being accessed by other users"),
        )
        .with_detail("Other sessions are connected to the database."),
        _ => common_error(name, error),
    }
}

fn common_error(name: &str, error: &ServedDatabaseError) -> SqlError {
    match error {
        ServedDatabaseError::Exists => SqlError::new(
            SqlState::DUPLICATE_DATABASE,
            format!("database \"{name}\" already exists"),
        ),
        ServedDatabaseError::Missing => SqlError::new(
            SqlState::INVALID_CATALOG_NAME,
            format!("database \"{name}\" does not exist"),
        ),
        ServedDatabaseError::InUse => SqlError::new(
            SqlState::OBJECT_IN_USE,
            format!("database \"{name}\" is being accessed by other users"),
        ),
        ServedDatabaseError::Unsupported(reason) => SqlError::not_supported(reason.clone()),
        ServedDatabaseError::Failed(reason) => {
            SqlError::new(SqlState::IO_ERROR, format!("database \"{name}\": {reason}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Session, SqlError};
    use std::sync::Mutex;

    #[derive(Default)]
    struct Served {
        databases: Mutex<Vec<(String, usize)>>,
    }

    impl Served {
        fn with(names: &[(&str, usize)]) -> Self {
            Self {
                databases: Mutex::new(
                    names
                        .iter()
                        .map(|(name, sessions)| ((*name).to_owned(), *sessions))
                        .collect(),
                ),
            }
        }
    }

    impl ServedDatabases for Served {
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = self
                .databases
                .lock()
                .unwrap()
                .iter()
                .map(|(name, _)| name.clone())
                .collect();
            names.sort();
            names
        }

        fn create(&self, name: &str) -> Result<(), ServedDatabaseError> {
            let mut databases = self.databases.lock().unwrap();
            if databases.iter().any(|(known, _)| known == name) {
                return Err(ServedDatabaseError::Exists);
            }
            databases.push((name.to_owned(), 0));
            Ok(())
        }

        fn drop_database(&self, name: &str) -> Result<(), ServedDatabaseError> {
            let mut databases = self.databases.lock().unwrap();
            let Some(at) = databases.iter().position(|(known, _)| known == name) else {
                return Err(ServedDatabaseError::Missing);
            };
            if databases[at].1 > 0 {
                return Err(ServedDatabaseError::InUse);
            }
            databases.remove(at);
            Ok(())
        }
    }

    fn run(session: &mut Session<'_>, sql: &str) -> Result<Vec<String>, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs)
    }

    fn code(session: &mut Session<'_>, sql: &str) -> String {
        match run(session, sql) {
            Ok(outputs) => panic!("{sql} succeeded: {outputs:?}"),
            Err(error) => error.state().code().to_owned(),
        }
    }

    fn listed(session: &mut Session<'_>) -> String {
        run(session, "SELECT datname FROM pg_database")
            .unwrap()
            .concat()
    }

    #[test]
    fn create_and_drop_database_need_a_server() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        assert_eq!(code(&mut session, "CREATE DATABASE shop"), "0A000");
        assert_eq!(code(&mut session, "DROP DATABASE shop"), "0A000");
    }

    #[test]
    fn a_session_creates_lists_and_drops_served_databases() {
        let store = MemoryStore::default();
        let served = Served::with(&[("main", 1), ("busy", 1)]);
        let mut session = Session::new(&store);
        session.identify("eggdb", "main");
        session.set_served_databases(&served);
        assert!(!listed(&mut session).contains("shop"));
        run(
            &mut session,
            "CREATE DATABASE shop WITH OWNER = eggdb ENCODING 'UTF8' TEMPLATE template0",
        )
        .unwrap();
        assert!(listed(&mut session).contains("shop"));
        assert_eq!(code(&mut session, "CREATE DATABASE shop"), "42P04");
        assert_eq!(code(&mut session, "CREATE DATABASE \"a/b\""), "42602");
        assert_eq!(code(&mut session, "CREATE DATABASE con"), "42602");
        assert_eq!(code(&mut session, "DROP DATABASE busy"), "55006");
        assert_eq!(code(&mut session, "DROP DATABASE main"), "55006");
        assert_eq!(code(&mut session, "BEGIN; DROP DATABASE shop"), "25001");
        run(&mut session, "ROLLBACK").unwrap();
        run(&mut session, "DROP DATABASE shop WITH (FORCE)").unwrap();
        assert!(!listed(&mut session).contains("shop"));
        assert_eq!(code(&mut session, "DROP DATABASE shop"), "3D000");
        run(&mut session, "DROP DATABASE IF EXISTS shop").unwrap();
    }

    #[test]
    fn only_superusers_and_createdb_roles_create_databases() {
        let store = MemoryStore::default();
        let served = Served::with(&[("main", 1)]);
        let mut admin = Session::new(&store);
        run(
            &mut admin,
            "CREATE ROLE plain LOGIN PASSWORD 'x'; CREATE ROLE maker LOGIN CREATEDB PASSWORD 'x'",
        )
        .unwrap();
        let mut plain = Session::new(&store);
        plain.identify_role("plain", "main").unwrap();
        plain.set_served_databases(&served);
        assert_eq!(code(&mut plain, "CREATE DATABASE shop"), "42501");
        let mut maker = Session::new(&store);
        maker.identify_role("maker", "main").unwrap();
        maker.set_served_databases(&served);
        run(&mut maker, "CREATE DATABASE shop").unwrap();
        assert_eq!(code(&mut maker, "DROP DATABASE shop"), "42501");
        let flag = |admin: &mut Session<'_>, role: &str| {
            run(
                admin,
                &format!("SELECT rolcreatedb FROM pg_roles WHERE rolname = '{role}'"),
            )
            .unwrap()
            .concat()
        };
        assert_ne!(flag(&mut admin, "maker"), flag(&mut admin, "plain"));
    }

    #[test]
    fn mysql_and_sql_server_name_databases_their_way() {
        let store = MemoryStore::default();
        let served = Served::with(&[("main", 1)]);
        let mut mysql = Session::with_dialect(&store, Dialect::MySql);
        mysql.identify("root", "main");
        mysql.set_served_databases(&served);
        run(
            &mut mysql,
            "CREATE DATABASE IF NOT EXISTS shop DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
        )
        .unwrap();
        run(&mut mysql, "CREATE SCHEMA IF NOT EXISTS shop").unwrap();
        assert_eq!(code(&mut mysql, "CREATE SCHEMA shop"), "HY000");
        run(&mut mysql, "CREATE DATABASE other").unwrap();
        let mut sqlserver = Session::with_dialect(&store, Dialect::SqlServer);
        sqlserver.identify("sa", "main");
        sqlserver.set_served_databases(&served);
        run(&mut sqlserver, "DROP DATABASE shop, other").unwrap();
        assert_eq!(served.names(), ["main"]);
        run(&mut sqlserver, "CREATE DATABASE [Sales]").unwrap();
        assert_eq!(code(&mut sqlserver, "CREATE DATABASE Sales"), "42P04");
        assert_eq!(code(&mut mysql, "DROP DATABASE nowhere"), "HY000");
    }

    #[test]
    fn database_names_are_safe_file_names() {
        for name in ["shop", "Shop_2", "a-b", "x$", "données"] {
            assert!(is_valid_database_name(name), "{name}");
        }
        let long = "a".repeat(MAX_DATABASE_NAME_BYTES + 1);
        for name in [
            "", "../x", "a/b", "a\\b", "a.b", ".", "..", "-x", "con", "NUL", "Com1", "a b", "a\0b",
            &long,
        ] {
            assert!(!is_valid_database_name(name), "{name:?}");
        }
    }
}
