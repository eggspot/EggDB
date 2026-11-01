//! EggDB's STOMP server (D217), written from the public STOMP 1.2 specification (and accepting
//! 1.0 and 1.1 clients): `CONNECT` or `STOMP` with login and passcode, `SEND`, `SUBSCRIBE` with
//! the `auto`, `client` and `client-individual` acknowledgement modes, `ACK` and `NACK`,
//! transactions (`BEGIN`, `COMMIT`, `ABORT`), receipts and `DISCONNECT`. Destinations under
//! `/queue/` are EggDB's durable queues (`eggdb-queues`), shared with AMQP and SQS clients;
//! destinations under `/topic/` are delivered to every current subscriber.

mod frame;
mod server;

pub use server::{ServerConfig, serve};

#[cfg(test)]
mod tests;
