//! EggDB's SQL core: the PostgreSQL, MySQL, SQLite and Oracle dialects on any ordered key-value
//! store.
//!
//! [`Session`] runs SQL text, statement by statement, over a [`KeyValueStore`]: one writer
//! and snapshot readers over byte-string keys in order, which `eggdb::Database` provides. The
//! catalog and every table live in that store's key space under a reserved prefix, rows keyed
//! by an order-preserving encoding of their primary key, so the engine's transactions, crash
//! safety and backups cover SQL data as they cover every other key.
//!
//! These are the first parts of the SQL core: tables with primary keys, `INSERT`, `SELECT`
//! with `WHERE`, inner, left and cross joins, `GROUP BY` with `count`, `sum`, `avg`, `min` and
//! `max`, `HAVING`, `DISTINCT`, `ORDER BY`, `LIMIT` and `OFFSET`, `UPDATE`, `DELETE`,
//! transaction blocks, and the integer, floating-point, exact `numeric`, boolean, text, `date`,
//! `timestamp`, `timestamptz`, `json` and `jsonb` types. Everything
//! else answers SQLSTATE `0A000` (feature not supported). The guide is `docs/sql.md` in the
//! repository.

mod aggregate;
mod ast;
mod datetime;
mod dialect;
mod encoding;
mod error;
mod eval;
mod execute;
mod explain;
mod hash_index;
mod indexes;
mod join;
mod json;
mod lexer;
mod memory;
mod mysql_errors;
mod mysql_variables;
mod numeric;
mod oracle_errors;
mod oracle_functions;
mod parser;
mod plan;
mod schema;
mod session;
mod settings;
mod sqlite_errors;
mod sqlite_functions;
mod store;
mod types;
mod typing;

#[cfg(test)]
mod correctness;
#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod memory_store;
#[cfg(test)]
mod tests;

pub use dialect::Dialect;
pub use error::{SqlError, SqlResult, SqlState};
pub use indexes::IndexCheck;
pub use mysql_errors::Warning;
pub use mysql_variables::MYSQL_COMPATIBLE_VERSION;
pub use numeric::Numeric;
pub use parser::{ORACLE_COMPATIBLE_VERSION, SQLITE_COMPATIBLE_VERSION};
pub use schema::{ColumnSchema, IndexSchema, TableSchema, quoted};
pub use session::{
    OutputColumn, Parameter, PreparedStatement, QueryResult, Session, StatementDescription,
    TransactionStatus,
};
pub use settings::POSTGRESQL_COMPATIBLE_VERSION;
pub use sqlite_errors::sqlite_result_code_name;
pub use store::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, StoreFailure,
};
pub use types::{NumericModifier, SqlType, Value};

/// The first bytes of every key that SQL keeps in a database: the catalog and every table's
/// rows. Other keys of the same database never start with them unless written there on
/// purpose.
pub const RESERVED_KEY_PREFIX: &[u8] = encoding::NAMESPACE;
