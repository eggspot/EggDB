//! EggDB's SQL core: the PostgreSQL, MySQL, SQLite, Oracle and SQL Server (Transact-SQL)
//! dialects on any ordered key-value store.
//!
//! [`Session`] runs SQL text, statement by statement, over a [`KeyValueStore`]: one writer
//! and snapshot readers over byte-string keys in order, which `eggdb::Database` provides. The
//! catalog and every table live in that store's key space under a reserved prefix, rows keyed
//! by an order-preserving encoding of their primary key, so the engine's transactions, crash
//! safety and backups cover SQL data as they cover every other key.
//!
//! These are the first parts of the SQL core: tables with primary keys, `INSERT`, `SELECT`
//! with `WHERE`, inner, left, right, full and cross joins, subqueries, `WITH` queries and set
//! operations, `GROUP BY` with `count`, `sum`, `avg`, `min` and
//! `max`, `HAVING`, `DISTINCT`, `ORDER BY`, `LIMIT` and `OFFSET`, `UPDATE`, `DELETE`,
//! transaction blocks, `COPY ... FROM STDIN` and `COPY ... TO STDOUT` in the text and CSV
//! formats, and the integer, floating-point, exact `numeric`, boolean, text,
//! `character(n)`, `bytea`, `uuid`, `date`, `time`, `timetz`, `timestamp`, `timestamptz`,
//! `interval`, `json` and `jsonb` types. Everything
//! else answers SQLSTATE `0A000` (feature not supported). The guide is `docs/sql.md` in the
//! repository.

mod aggregate;
mod alter;
mod arrays;
mod ast;
mod bytea;
mod comments;
mod constraints;
mod copy;
mod datetime;
mod destructive;
mod dialect;
mod encoding;
mod error;
mod eval;
mod execute;
mod explain;
mod hash_index;
mod indexes;
mod information_schema;
mod interval;
mod join;
pub mod json;
mod json_functions;
mod lexer;
mod md5;
mod memory;
mod merge;
mod mysql_errors;
mod mysql_variables;
mod name_resolution;
mod numeric;
mod oracle_conversion;
mod oracle_datetime;
mod oracle_errors;
mod oracle_functions;
mod oracle_nls;
mod parser;
mod pg_catalog;
mod pg_format;
mod pg_keywords;
mod plan;
mod random;
mod regex;
mod regexp_functions;
mod restore;
mod schema;
mod schemas;
mod sequences;
mod session;
mod set_operations;
mod set_returning;
mod settings;
mod sqlite_errors;
mod sqlite_functions;
mod sqlserver_conversion;
mod sqlserver_errors;
mod sqlserver_functions;
mod sqlserver_types;
mod store;
mod string_functions;
mod subquery;
mod temp_tables;
mod temporal;
mod types;
mod typing;
mod views;
mod window;

#[cfg(test)]
mod arrays_tests;
#[cfg(test)]
mod constraint_tests;
#[cfg(test)]
mod copy_tests;
#[cfg(test)]
mod correctness;
#[cfg(test)]
mod destructive_tests;
#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod memory_store;
#[cfg(test)]
mod one_statement_tests;
#[cfg(test)]
mod oracle_tests;
#[cfg(test)]
mod regex_tests;
#[cfg(test)]
mod schema_tests;
#[cfg(test)]
mod set_returning_tests;
#[cfg(test)]
mod sqlserver_tests;
#[cfg(test)]
mod subquery_tests;
#[cfg(test)]
mod tests;

pub use arrays::{ArrayElement, ArrayValue};
pub use copy::{CopyChannel, CopyFormat, CopyStart};
pub use destructive::{
    DestructiveKind, DestructivePart, DestructiveStatement, destructive_statement,
};
pub use dialect::Dialect;
pub use error::{SqlError, SqlResult, SqlState};
pub use indexes::IndexCheck;
pub use interval::{Interval, IntervalFields};
pub use mysql_errors::Warning;
pub use mysql_variables::MYSQL_COMPATIBLE_VERSION;
pub use numeric::Numeric;
pub use parser::{
    ORACLE_COMPATIBLE_VERSION, SQLITE_COMPATIBLE_VERSION, SQLSERVER_COMPATIBLE_VERSION,
};
pub use pg_catalog::{CatalogType, RegKind, RegValue, type_oid};
pub use random::{RandomSource, set_random_source};
pub use restore::{
    Consequence, IgnoredMetaCommand, IgnoredStatements, IncludeBase, IncludeOpener, IncludeRequest,
    KnownDifference, MAX_LISTED, RestoreError, RestoreOptions, RestoreProgress, RestoreReport,
    SkippedObject, TableRows, restore_dump,
};
pub use schema::{ColumnSchema, IndexSchema, TableSchema, ViewColumnSchema, ViewSchema, quoted};
pub use session::{
    BatchStep, NamedParameter, OneStatementError, OneStatementRefusal, OutputColumn, Parameter,
    ParameterDeclaration, PreparedStatement, QueryResult, Session, StatementDescription,
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
