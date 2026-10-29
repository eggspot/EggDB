//! EggDB's built-in MCP server (D11, D45, D119, D121): AI agents read, and when the user allows
//! it change, a database over the Model Context Protocol, JSON-RPC 2.0 messages one per line on
//! stdin and stdout, written from the public specification at modelcontextprotocol.io with no
//! SDK (D1).
//!
//! - [`Server`] speaks the protocol versions that start with `initialize`
//!   ([`PROTOCOL_VERSIONS`]) and offers the tools `list_tables`, `describe_table`, `run_sql`,
//!   `explain_query`, `sample_rows`, `kv_get` and `kv_scan`, and the resource `eggdb://schema`.
//! - Those are read-only: each call opens the database read-only through a
//!   [`DatabaseOpener`] and closes it after, and the store the call runs on has no writer.
//! - With [`ServerConfig::allow_write`] it also offers `write_sql`, which opens the database
//!   read-write for its one statement. A destructive statement runs only once the user
//!   confirmed it, through the client (`elicitation/create`) when the client can ask, and
//!   otherwise through the agent's `confirm: true`.
//! - Every call has limits: one SQL statement, at most a number of rows and 1 MiB of result,
//!   256 MiB of change, and a time limit, past which reads fail and, a second later, a watchdog
//!   ends the process, never once a commit has started.
//! - Every call is recorded in an [`audit`] file before its result is sent, and a write also
//!   before it runs.
//! - Results are structured data marked as data, never instructions, and every message out is
//!   one line of ASCII, so stored text cannot change the protocol.
//! - The tools (names, descriptions, input schemas, annotations) and the `instructions` come from
//!   EggDB's command registry, `eggdb-commands` (D143), and every error carries its code's
//!   category shared with EggOS.
//!
//! The guide is `docs/mcp.md` in the repository.

pub mod audit;
mod call_store;
mod confirmation;
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
