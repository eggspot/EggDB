//! EggDB's NATS server (D216), written from the public NATS client protocol documentation:
//! `INFO` and `CONNECT` with user and password, `PUB` and `HPUB`, `SUB` with `*` and `>`
//! wildcards and queue groups, `UNSUB` with a message limit, `MSG` and `HMSG` delivery, `PING`
//! and `PONG`, verbose mode and `echo`. Messages are delivered at most once, as core NATS does;
//! JetStream's durable streams are not served yet. NATS is a trademark of the Linux Foundation;
//! EggDB is compatible with its client protocol.

mod protocol;
mod server;

pub use server::{ServerConfig, serve};

#[cfg(test)]
mod tests;
