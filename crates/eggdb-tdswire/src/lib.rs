//! EggDB's server for SQL Server's Tabular Data Stream (TDS) protocol, version 7.4, written from
//! Microsoft's public [MS-TDS] specification (D1, D122), so Microsoft's `sqlcmd` (go-sqlcmd)
//! connects to an EggDB database and runs Transact-SQL batches on it, and .NET and Java apps run
//! through Microsoft.Data.SqlClient and mssql-jdbc (D5, D10, D12, D132).
//!
//! - [`serve`] runs a server over any [`eggdb_sql::KeyValueStore`]: one thread and one
//!   [`eggdb_sql::Session`] in the SQL Server dialect per connection, a bounded number of
//!   connections, and a graceful stop.
//! - [`listen`] opens its listener: on any address when the server has TLS, where connections
//!   from other machines must encrypt, and only on loopback without TLS (D161, superseding part
//!   of D102); [`listen_on_loopback`] is the loopback-only form.
//! - Encryption from [`eggdb_tls`] as [MS-TDS] negotiates it in PRELOGIN: full encryption,
//!   login-only encryption for a loopback client that asks only for that, and TDS 8 strict
//!   (TLS first, ALPN `tds/8.0`); without TLS the pre-login answers that encryption is not
//!   available, so a client asking for it ends the connection itself.
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
//! - Bulk loads run as SQL Server runs them for `bcp in` and `SqlBulkCopy`: an `INSERT BULK`
//!   statement, then a bulk load message whose rows [`bulk`] reads as they arrive and
//!   [`eggdb_sql::Session::execute_bulk_load`] loads in one statement; `SET FMTONLY ON` and
//!   `sp_tablecollations_100` describe the target table first, as `SqlBulkCopy` asks.
//!
//! The guides are "Connecting with sqlcmd", ".NET and Java apps over TDS" and "Bulk loads
//! with bcp and SqlBulkCopy" in `docs/sql.md` in the repository.

mod auth;
pub mod bulk;
mod connection;
pub mod messages;
mod procedures;
pub mod rpc;
mod server;
mod tls_transport;
mod tokens;
mod types;
mod wire;

pub use auth::{DEFAULT_ITERATIONS, TdsCredentials};
pub use connection::{MAX_BATCH_BYTES, MAX_LOGIN_BYTES};
pub use server::{
    DEFAULT_AUTHENTICATION_TIMEOUT, DEFAULT_MAX_CONNECTIONS, ListenError,
    MAX_UNAUTHENTICATED_CONNECTIONS, ServerConfig, listen, listen_on_loopback, serve,
};
pub use tokens::COLLATION;
