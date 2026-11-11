//! EggDB's RESP server (D193): the RESP2 and RESP3 protocol from its public specification, and
//! the documented semantics of the core commands for strings, hashes, lists, sets and sorted
//! sets with expiry, bitmaps, HyperLogLog sketches and geospatial indexes (D330), and the
//! blocking pops of lists, sorted sets and streams (D347), over a [`Keyspace`] that keeps each
//! command in one transaction, so clients built for Redis connect to EggDB unchanged. No code
//! of Redis is used or read.

mod aggregates;
mod aof;
mod aof_manifest;
mod bitmaps;
mod blocking;
mod cluster;
mod commands;
mod eviction;
mod geo;
mod geohash;
mod hyperloglog;
mod more_commands;
mod more_commands_ordered;
mod more_commands_sets;
use eggdb_keyspace as keyspace;
mod pops;
mod protocol;
mod pubsub;
mod server;
mod stream_groups;
mod streams;
mod transactions;
mod value;

pub use aof::{AofReplay, AofReport, replay_aof};
pub use cluster::key_slot;
pub use aof_manifest::{AofFileKind, AofManifest, AofManifestFile, parse_aof_manifest};
pub use commands::{Client, Context, execute};
pub use eviction::{EvictingKeyspace, EvictionPolicy};
pub use keyspace::{EntryVisitor, KeysRead, KeysWrite, Keyspace, MemoryKeyspace};
pub use protocol::{ProtocolError, Reply, RequestReader, format_double};
pub use server::{ServerConfig, serve};
pub use transactions::execute_with_transactions;
pub use value::{Data, Entry, SortedSet};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_bitmaps_hll_geo;
#[cfg(test)]
mod tests_blocking;
