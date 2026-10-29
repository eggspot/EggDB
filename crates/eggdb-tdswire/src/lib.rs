//! EggDB's server for SQL Server's Tabular Data Stream (TDS) protocol, version 7.4, written from
//! Microsoft's public [MS-TDS] specification (D1, D122), so Microsoft's `sqlcmd` (go-sqlcmd)
//! connects to an EggDB database and runs Transact-SQL batches on it, and .NET and Java apps run
//! through Microsoft.Data.SqlClient and mssql-jdbc (D5, D10, D12, D132).
//!
//! - [`serve`] runs a server over any [`eggdb_sql::KeyValueStore`]: one thread and one
//!   [`eggdb_sql::Session`] in the SQL Server dialect per connection, a bounded number of
//!   connections, and a graceful stop.
//! - [`listen_on_loopback`] opens its listener and refuses every address but loopback: until
//!   EggDB has TLS, only this machine can connect (D102). The pre-login answers that
//!   encryption is not available, so a client asking for it ends the connection itself.
//! - Every connection logs in with a SQL login checked against [`TdsCredentials`]; there is no
//!   unauthenticated mode, and integrated (SSPI) and federated logins are refused.
//! - [`messages`] reads every client message (PRELOGIN, LOGIN7, SQL batches, transaction
//!   manager requests) and [`rpc`] reads remote procedure calls, refusing truncated, oversized
//!   or inconsistent input with an error, never a panic.
//! - Batches run with SQL Server's batch rules ([`eggdb_sql::Session::execute_batch`]) and
//!   their results go back as TDS token streams: column metadata, rows, errors and completion.
//! - Remote procedure calls run `sp_executesql`, `sp_prepare`, `sp_prepexec`, `sp_execute` and
//!   `sp_unprepare` with typed parameters bound by name
//!   ([`eggdb_sql::Session::execute_parameterized_batch`]), prepared handles kept per
//!   connection, output parameters and return status sent back, so Microsoft.Data.SqlClient
//!   and mssql-jdbc run parameterized statements; transaction manager requests begin, commit
//!   and roll back transactions, and the connection-reset flag of pooled connections resets
//!   the session.
//!
//! The guides are "Connecting with sqlcmd" and ".NET and Java apps over TDS" in `docs/sql.md`
//! in the repository.

mod auth;
mod connection;
pub mod messages;
mod procedures;
pub mod rpc;
mod server;
mod tokens;
mod types;
mod wire;

pub use auth::{DEFAULT_ITERATIONS, TdsCredentials};
pub use connection::{MAX_BATCH_BYTES, MAX_LOGIN_BYTES};
pub use server::{
    DEFAULT_AUTHENTICATION_TIMEOUT, DEFAULT_MAX_CONNECTIONS, ListenError,
    MAX_UNAUTHENTICATED_CONNECTIONS, ServerConfig, listen_on_loopback, serve,
};
pub use tokens::COLLATION;
