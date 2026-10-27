//! EggDB's server for the PostgreSQL wire protocol v3, written from PostgreSQL's public
//! protocol documentation and the SCRAM RFCs (D1), so psql and PostgreSQL drivers connect to
//! an EggDB database unchanged (D5, D12).
//!
//! - [`serve`] runs a server over any [`eggdb_sql::KeyValueStore`]: one thread and one
//!   [`eggdb_sql::Session`] per connection, a bounded number of connections, and a graceful
//!   stop.
//! - [`listen_on_loopback`] opens its listener and refuses every address but loopback: until
//!   EggDB has TLS, only this machine can connect (D102).
//! - Every connection authenticates with SCRAM-SHA-256 ([`ScramVerifier`], [`ScramExchange`]);
//!   there is no unauthenticated mode. `SSLRequest` and `GSSENCRequest` are answered `N`.
//! - [`messages`] parses every frontend message and builds the backend ones, refusing
//!   truncated, oversized or inconsistent input with an error, never a panic.
//! - The simple query protocol and the extended one (`Parse`, `Bind`, `Describe`, `Execute`,
//!   `Close`, `Sync`, `Flush`) with `$n` parameters in text or binary form and results in
//!   either.
//!
//! The guide is "Connecting with psql" in `docs/sql.md` in the repository.

mod connection;
pub mod messages;
mod scram;
mod server;
pub mod types;
mod wire;

pub use connection::{MAX_AUTHENTICATION_MESSAGE_BYTES, MAX_MESSAGE_BYTES};
pub use scram::{
    MIN_SCRAM_ITERATIONS, SCRAM_SALT_BYTES, SCRAM_SHA_256, ScramExchange, ScramFailure,
    ScramVerifier,
};
pub use server::{
    DEFAULT_AUTHENTICATION_TIMEOUT, DEFAULT_MAX_CONNECTIONS, ListenError,
    MAX_UNAUTHENTICATED_CONNECTIONS, ServerConfig, listen_on_loopback, serve,
};
