//! EggDB's SQL core: the PostgreSQL dialect on any ordered key-value store.
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
//! transaction blocks, and the integer, floating-point, boolean and text types. Everything
//! else answers SQLSTATE `0A000` (feature not supported). The guide is `docs/sql.md` in the
//! repository.

mod aggregate;
mod ast;
mod encoding;
mod error;
mod eval;
mod execute;
mod hash_index;
mod join;
mod lexer;
mod memory;
mod parser;
mod schema;
mod session;
mod settings;
mod store;
mod types;
mod typing;

#[cfg(test)]
mod memory_store;
#[cfg(test)]
mod tests;

pub use error::{SqlError, SqlResult, SqlState};
pub use schema::{ColumnSchema, TableSchema, quoted};
pub use session::{
    OutputColumn, Parameter, PreparedStatement, QueryResult, Session, StatementDescription,
    TransactionStatus,
};
pub use settings::POSTGRESQL_COMPATIBLE_VERSION;
pub use store::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, StoreFailure,
};
pub use types::{SqlType, Value};

/// The first bytes of every key that SQL keeps in a database: the catalog and every table's
/// rows. Other keys of the same database never start with them unless written there on
/// purpose.
pub const RESERVED_KEY_PREFIX: &[u8] = encoding::NAMESPACE;
