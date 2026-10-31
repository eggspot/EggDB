//! EggDB's server for the PostgreSQL wire protocol v3, written from PostgreSQL's public
//! protocol documentation and the SCRAM RFCs (D1), so psql and PostgreSQL drivers connect to
//! an EggDB database unchanged (D5, D12).
//!
//! - [`serve`] runs a server over any [`eggdb_sql::KeyValueStore`]: one thread and one
//!   [`eggdb_sql::Session`] per connection, a bounded number of connections, and a graceful
//!   stop.
//! - [`listen`] opens its listener: on any address when the server has TLS, where connections
//!   from other machines must use it, and only on loopback without TLS (D161, superseding part
//!   of D102); [`listen_on_loopback`] is the loopback-only form.
//! - TLS 1.3 and 1.2 from [`eggdb_tls`]: `SSLRequest` is answered `S` when [`ServerConfig::tls`]
//!   is set (`N` otherwise), and PostgreSQL 17's direct TLS with ALPN `postgresql` is accepted.
//!   `GSSENCRequest` is answered `N`.
//! - Every connection authenticates with SCRAM-SHA-256 ([`ScramVerifier`], [`ScramExchange`]),
//!   and over TLS also SCRAM-SHA-256-PLUS with `tls-server-end-point` channel binding; there is
//!   no unauthenticated mode.
//! - [`messages`] parses every frontend message and builds the backend ones, refusing
//!   truncated, oversized or inconsistent input with an error, never a panic.
//! - The simple query protocol and the extended one (`Parse`, `Bind`, `Describe`, `Execute`,
//!   `Close`, `Sync`, `Flush`) with `$n` parameters in text or binary form and results in
//!   either.
//! - Implicit transactions as PostgreSQL runs them: the statements of one `Query` message, and
//!   of an extended-query pipeline up to its `Sync`, commit or roll back together unless a
//!   `BEGIN` opened a block.
//! - `CancelRequest`: a client that sends the process ID and secret key a connection received
//!   in its `BackendKeyData` stops the statement that connection is running, with `57014`.
//! - The copy sub-protocol: `COPY ... FROM STDIN` (`CopyInResponse`, then the client's
//!   `CopyData`, `CopyDone` or `CopyFail`) and `COPY ... TO STDOUT` (`CopyOutResponse`, one
//!   `CopyData` per row, `CopyDone`), in the text and CSV formats, which psql's `\copy` uses,
//!   and in the binary format with binary format codes, which drivers' binary copy uses.
//!
//! The guide is "Connecting with psql" in `docs/sql.md` in the repository.

mod backend_keys;
mod connection;
pub mod messages;
mod scram;
mod server;
pub mod types;
mod wire;

pub use connection::{MAX_AUTHENTICATION_MESSAGE_BYTES, MAX_MESSAGE_BYTES};
pub use scram::{
    MIN_SCRAM_ITERATIONS, SCRAM_SALT_BYTES, SCRAM_SHA_256, SCRAM_SHA_256_PLUS, ScramExchange, ScramFailure,
    ScramVerifier,
};
pub use server::{
    DEFAULT_AUTHENTICATION_TIMEOUT, DEFAULT_MAX_CONNECTIONS, ListenError,
    MAX_UNAUTHENTICATED_CONNECTIONS, ServerConfig, listen, listen_on_loopback, serve,
};
