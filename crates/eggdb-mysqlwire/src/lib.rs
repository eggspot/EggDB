//! EggDB's server for the MySQL client/server protocol, written from MySQL's public protocol
//! documentation (D1), so the `mysql` client and MySQL drivers connect to an EggDB database
//! unchanged and run MySQL's SQL on it (D5, D12, D106).
//!
//! - [`serve`] runs a server over any [`eggdb_sql::KeyValueStore`]: one thread and one
//!   [`eggdb_sql::Session`] in the MySQL dialect per connection, a bounded number of
//!   connections, and a graceful stop.
//! - [`listen_on_loopback`] opens its listener and refuses every address but loopback: until
//!   EggDB has TLS, only this machine can connect (D102). TLS is never offered in the handshake.
//! - Every connection authenticates with `caching_sha2_password` (its fast path) or
//!   `mysql_native_password` ([`MySqlCredentials`]); there is no unauthenticated mode.
//! - [`packets`] reads every client payload and builds the server's, refusing truncated,
//!   oversized or inconsistent input with an error, never a panic.
//! - `COM_QUERY` with text result sets and several statements per query, and prepared
//!   statements (`COM_STMT_PREPARE`, `COM_STMT_EXECUTE` with binary parameters and rows,
//!   `COM_STMT_SEND_LONG_DATA`, `COM_STMT_RESET`, `COM_STMT_CLOSE`).
//!
//! The guide is "Connecting with mysql" in `docs/sql.md` in the repository.

mod auth;
mod connection;
pub mod packets;
mod server;
mod wire;

pub use auth::{
    CACHING_SHA2_PASSWORD, MYSQL_NATIVE_PASSWORD, MySqlCredentials, NONCE_BYTES,
    caching_sha2_response, native_response,
};
pub use connection::{MAX_AUTHENTICATION_PACKET_BYTES, MAX_PACKET_BYTES};
pub use server::{
    DEFAULT_AUTHENTICATION_TIMEOUT, DEFAULT_MAX_CONNECTIONS, ListenError,
    MAX_UNAUTHENTICATED_CONNECTIONS, ServerConfig, listen_on_loopback, serve,
};
