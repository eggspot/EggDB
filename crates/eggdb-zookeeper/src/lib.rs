//! EggDB's server for Apache ZooKeeper's client protocol (D742), written from ZooKeeper's published
//! client protocol and jute record descriptions: znodes kept in an EggDB keyspace (each request one
//! transaction), sessions with timeouts, ephemeral and sequential znodes, one-shot data and child
//! watches, versions for conditional writes, and `multi`. ACLs are kept and returned but not
//! enforced, so the listener stays on loopback. ZooKeeper is a trademark of the Apache Software
//! Foundation; EggDB is compatible with its client protocol.

mod jute;
mod server;
mod tree;

pub use server::{ServerConfig, serve};

#[cfg(test)]
mod tests;
