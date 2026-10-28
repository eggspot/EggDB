//! EggDB's built-in MCP server (D11, D45, D119): AI agents read a database over the Model
//! Context Protocol, JSON-RPC 2.0 messages one per line on stdin and stdout, written from the
//! public specification at modelcontextprotocol.io with no SDK (D1).
//!
//! - [`Server`] speaks the protocol versions that start with `initialize`
//!   ([`PROTOCOL_VERSIONS`]) and offers the tools `list_tables`, `describe_table`, `run_sql`,
//!   `explain_query`, `sample_rows`, `kv_get` and `kv_scan`, and the resource `eggdb://schema`.
//! - It is read-only: each call opens the database read-only through a [`DatabaseOpener`] and
//!   closes it after, and the store the call runs on has no writer at all.
//! - Every call has limits: one SQL statement, at most a number of rows and 1 MiB of result,
//!   and a time limit, past which reads fail and, a second later, a watchdog ends the process.
//! - Every call is recorded in an [`audit`] file before its result is sent.
//! - Results are structured data marked as data, never instructions, and every message out is
//!   one line of ASCII, so stored text cannot change the protocol.
//!
//! The guide is `docs/mcp.md` in the repository.

pub mod audit;
mod call_store;
mod json_out;
mod jsonrpc;
mod results;
mod server;
mod tools;
mod watchdog;

pub use jsonrpc::MAX_MESSAGE_BYTES;
pub use server::{
    DatabaseOpener, OpenRefusal, PROTOCOL_VERSIONS, Server, ServerConfig, StartFailure,
};
