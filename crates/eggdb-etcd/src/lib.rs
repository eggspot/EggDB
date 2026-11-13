//! EggDB's server for etcd's v3 gRPC API (D1052), written from etcd's published API reference
//! and protocol buffer descriptions: keys with a store-wide revision, each key's create and
//! modify revisions and version, ranges (at the current or a past revision, with limits, keys or
//! counts only), puts and deletes with previous values, transactions of comparisons and
//! operations (nested ones too), compaction, leases that expire with their keys, the history a
//! watch replays from a revision, and the status and member list clients ask for. It runs over
//! EggDB's own HTTP/2 and gRPC framing, and every request is one transaction of an EggDB keyspace.
//! etcd is a trademark of The Linux Foundation; EggDB is compatible with its v3 API.

mod proto;
mod server;
mod store;

pub use server::{ServerConfig, serve};

#[cfg(test)]
mod tests;
