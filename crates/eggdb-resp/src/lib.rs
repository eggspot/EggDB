//! EggDB's RESP server (D193): the RESP2 and RESP3 protocol from its public specification, and
//! the documented semantics of the core commands for strings, hashes, lists, sets and sorted
//! sets with expiry, over a [`Keyspace`] that keeps each command in one transaction, so clients
//! built for Redis connect to EggDB unchanged. No code of Redis is used or read.

mod aggregates;
mod commands;
mod keyspace;
mod protocol;
mod server;
mod value;

pub use commands::{Client, Context, execute};
pub use keyspace::{KeysRead, KeysWrite, Keyspace, MemoryKeyspace};
pub use protocol::{ProtocolError, Reply, RequestReader, format_double};
pub use server::{ServerConfig, serve};
pub use value::{Data, Entry, SortedSet};

#[cfg(test)]
mod tests;
