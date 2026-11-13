//! EggDB's AMQP 0-9-1 server (D214), written from the published AMQP 0-9-1 specification:
//! connection handshake with PLAIN authentication, channels, the default, direct, fanout and
//! topic exchanges with bindings, queues on EggDB's durable queues (`eggdb-queues`), publishing,
//! pushed consumers with prefetch, `basic.get`, acknowledgements, rejects and requeues,
//! publisher confirms, and heartbeats (the server proposes 60 seconds, follows the client's
//! `tune-ok`, sends one every half interval and closes a connection silent for two intervals). RabbitMQ is a trademark of its owner; EggDB is compatible with the AMQP
//! 0-9-1 protocol its clients speak.
//!
//! The same listener serves AMQP 1.0 (D217), told apart by the protocol header: SASL PLAIN,
//! sessions, links to queues in both directions with credit-based flow control, multi-frame
//! transfers, and settlement with the accepted, rejected, released and modified outcomes, on the
//! same queues. Messages keep their properties, application properties and annotations across
//! both protocols; topics (the `topic` capability) fan out through `amq.topic` to subscription
//! queues, durable ones kept until unsubscribed; the anonymous relay routes by each message's
//! `to`; JMS selectors filter receivers; and local transactions run through the coordinator, so
//! JMS clients work unchanged (D1062).

mod amqp1;
mod codec;
mod field_table;
mod server;

pub use server::{ServerConfig, serve};

#[cfg(test)]
mod tests;
