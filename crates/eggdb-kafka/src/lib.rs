//! EggDB's server for the Kafka protocol (D213), written from Apache Kafka's public protocol
//! guide: `ApiVersions`, `Metadata`, `Produce` (record batches, magic 2), `Fetch` and
//! `ListOffsets`, in their non-flexible versions. Each topic partition is a log of record batches
//! on EggDB's key-value transactions, stored as producers sent them with the broker's offsets
//! written in; batches are checked (length, magic and CRC-32C) before they are kept. Topics are
//! created on first use with one partition. Kafka is a trademark of the Apache Software
//! Foundation; EggDB is compatible with its protocol.

mod log;
mod server;
mod wire;

pub use server::{ServerConfig, serve};

#[cfg(test)]
mod tests;
