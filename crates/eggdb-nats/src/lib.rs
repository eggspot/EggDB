//! EggDB's NATS server (D216), written from the public NATS client protocol documentation:
//! `INFO` and `CONNECT` with user and password, `PUB` and `HPUB`, `SUB` with `*` and `>`
//! wildcards and queue groups, `UNSUB` with a message limit, `MSG` and `HMSG` delivery, `PING`
//! and `PONG`, verbose mode and `echo`. Core messages are delivered at most once; JetStream's
//! streams and durable consumers keep messages in the keyspace and deliver them again until
//! they are acknowledged. NATS is a trademark of the Linux Foundation; EggDB is compatible with
//! its client protocol.

mod jetstream;
mod nkeys;
mod protocol;
mod remote;
mod server;

pub use server::{ServerConfig, serve, serve_with_jetstream};

#[cfg(test)]
mod tests;
