//! EggDB's AMQP 0-9-1 server (D214), written from the published AMQP 0-9-1 specification:
//! connection handshake with PLAIN authentication, channels, the default, direct, fanout and
//! topic exchanges with bindings, queues on EggDB's durable queues (`eggdb-queues`), publishing,
//! pushed consumers with prefetch, `basic.get`, acknowledgements, rejects and requeues, and
//! publisher confirms. RabbitMQ is a trademark of its owner; EggDB is compatible with the AMQP
//! 0-9-1 protocol its clients speak.

mod codec;
mod server;

pub use server::{ServerConfig, serve};

#[cfg(test)]
mod tests;
