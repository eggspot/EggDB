//! EggDB's Bolt server (D207), written from the public Bolt protocol and PackStream
//! documentation: the handshake, chunked messages, versions 4.4 and 5.0, basic authentication,
//! auto-commit `RUN`/`PULL`/`DISCARD`, explicit transactions with the limits D207 states, and
//! nodes and relationships as Bolt structures, answering Cypher from `eggdb-graph`. Neo4j is a
//! trademark of its owner; EggDB is compatible with its protocol.

mod packstream;
mod server;

pub use server::{ServerConfig, serve};

#[cfg(test)]
mod tests;
