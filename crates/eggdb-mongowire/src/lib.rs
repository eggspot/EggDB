//! EggDB's server for the MongoDB wire protocol (D197), written from MongoDB's public wire
//! protocol and command documentation: `OP_MSG` with document sequences (and `OP_QUERY` for the
//! handshake older drivers start with), SCRAM-SHA-256 authentication, and the commands drivers
//! and `mongosh` use for CRUD, cursors, indexes and aggregation, over EggDB's document model in
//! `eggdb-documents`. MongoDB is a trademark of its owner; EggDB is compatible with its
//! protocol. No MongoDB code is used or read.

mod aggregate;
mod commands;
mod cursors;
mod message;
mod server;

pub use message::MAX_MESSAGE_BYTES;
pub use server::{ServerConfig, serve};

#[cfg(test)]
mod tests;
