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
//! transaction blocks, `COPY ... FROM STDIN` and `COPY ... TO STDOUT` in the text, CSV and
//! binary formats, and the integer, floating-point, exact `numeric`, boolean, text,
//! `character(n)`, `bytea`, `uuid`, `date`, `time`, `timetz`, `timestamp`, `timestamptz`,
//! `interval`, `json` and `jsonb` types. Everything
//! else answers SQLSTATE `0A000` (feature not supported). The guide is `docs/sql.md` in the
//! repository.

mod advisory_locks;
mod aggregate;
mod alter;
mod arrays;
mod ast;
mod bulk_load;
mod bytea;
mod cancel;
mod comments;
mod constraints;
mod copy;
mod datetime;
mod deferred_checks;
mod destructive;
mod dialect;
mod encoding;
mod enums;
mod error;
mod eval;
mod execute;
mod explain;
mod hash_index;
mod hyperloglog;
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
mod mysql_catalog;
mod mysql_errors;
mod mysql_variables;
mod name_resolution;
mod numeric;
mod oracle_conversion;
mod oracle_date_functions;
mod oracle_datetime;
mod oracle_dictionary;
mod oracle_errors;
mod oracle_functions;
mod oracle_intervals;
mod oracle_listagg;
mod oracle_nls;
mod oracle_number_model;
mod oracle_numerals;
mod parser;
mod partition_ddl;
mod partition_dml;
mod partitions;
mod pg_binary;
mod pg_catalog;
mod pg_format;
mod pg_keywords;
mod plan;
mod plan_cache;
mod plpgsql;
mod plsql;
mod pushdown;
mod random;
mod ranges;
mod records;
mod regex;
mod regexp_functions;
mod relations;
mod restore;
mod routines;
mod schema;
mod schemas;
mod sequences;
mod session;
mod set_operations;
mod set_returning;
mod settings;
mod sqlite_errors;
mod sqlite_functions;
mod sqlserver_catalog;
mod sqlserver_conversion;
mod sqlserver_errors;
mod sqlserver_format;
mod sqlserver_functions;
mod sqlserver_numbers;
mod sqlserver_strings;
mod sqlserver_types;
mod statistics;
mod store;
mod string_functions;
mod subquery;
mod table_variables;
mod temp_tables;
mod temporal;
mod temporary;
mod time_zones;
mod triggers;
mod tsql;
mod types;
mod vector;
mod typing;
mod updatable_views;
mod user_types;
mod views;
mod visible_tables;
mod window;
mod with_statement;

#[cfg(test)]
mod arrays_tests;
#[cfg(test)]
mod batch_logic_tests;
#[cfg(test)]
mod bulk_load_tests;
#[cfg(test)]
mod constraint_tests;
#[cfg(test)]
mod copy_tests;
#[cfg(test)]
mod correctness;
#[cfg(test)]
mod destructive_tests;
#[cfg(test)]
mod implicit_transaction_tests;
#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod insert_select_tests;
#[cfg(test)]
mod memory_store;
#[cfg(test)]
mod migration_tool_tests;
#[cfg(test)]
mod one_statement_tests;
#[cfg(test)]
mod sql_speed_tests;
#[cfg(test)]
mod oracle_tests;
#[cfg(test)]
mod partition_tests;
#[cfg(test)]
mod savepoint_tests;
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
mod user_type_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod lateral_tests;
#[cfg(test)]
mod view_change_tests;
#[cfg(test)]
mod visible_tables_tests;
#[cfg(test)]
mod window_tests;
#[cfg(test)]
mod with_statement_tests;

pub use arrays::{ArrayElement, ArrayValue};
pub use bulk_load::{BcpCharacterReader, BcpTerminators, BulkRows};
pub use cancel::CancelSignal;
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
pub use pg_binary::{decode_pg_binary_text, encode_pg_binary};
pub use pg_catalog::{CatalogType, RegKind, RegValue, type_oid};
pub use plsql::ServerOutputReader;
pub use random::{RandomSource, set_random_source};
pub use relations::{EXACT_ROW_COUNT_UP_TO, Relation, RelationKind, find_relation};
pub use restore::{
    Consequence, IgnoredMetaCommand, IgnoredStatements, IncludeBase, IncludeOpener, IncludeRequest,
    KnownDifference, MAX_LISTED, RestoreError, RestoreOptions, RestoreProgress, RestoreReport,
    ServerOutputSetting, SkippedObject, SqlPlusCommand, SqlPlusScript, SqlPlusStep, SqlServerBatch,
    TableRows, restore_dump, sqlserver_batches, starts_plsql_unit,
};
pub use schema::{
    ColumnSchema, IndexColumnOrder, IndexSchema, TableSchema, ViewColumnSchema, ViewSchema, quoted,
};
pub use session::{
    BatchStep, InfoMessage, NamedParameter, OneStatementError, OneStatementRefusal, OutputColumn,
    Parameter, ParameterDeclaration, PreparedStatement, QueryResult, Session,
    StatementDescription, StoredProcedure, TransactionStatus,
};
pub use settings::POSTGRESQL_COMPATIBLE_VERSION;
pub use sqlite_errors::sqlite_result_code_name;
pub use sqlserver_catalog::is_catalog_procedure;
pub use store::{
    EntryVisitor, KeyValueRead, KeyValueStore, KeyValueWrite, ScanDirection, StoreFailure,
};
pub use enums::EnumValue;
pub use ranges::{RangeBound, RangeBounds, RangeValue};
pub use records::RecordValue;
pub use time_zones::TimeZone;
pub use types::{DefinedKind, DefinedType, DefinedValue, NumericModifier, SqlType, Value};
pub use memory::{MAX_ROW_BYTES, MAX_VALUE_BYTES};
pub use visible_tables::{UnknownTableName, VisibleRead, VisibleTables};

/// The first bytes of every key that SQL keeps in a database: the catalog and every table's
/// rows. Other keys of the same database never start with them unless written there on
/// purpose.
pub const RESERVED_KEY_PREFIX: &[u8] = encoding::NAMESPACE;
