//! EggDB's MQTT broker (D215), written from the OASIS MQTT 3.1.1 and 5.0 specifications:
//! `CONNECT` with username and password, `PUBLISH` at QoS 0, 1 and 2 (with the `PUBREC`,
//! `PUBREL`, `PUBCOMP` exchange), `SUBSCRIBE` and `UNSUBSCRIBE` with `+` and `#` filters,
//! retained messages kept on EggDB's key-value transactions, will messages, keep-alive and
//! `PINGREQ`. Version 5 clients are answered with empty property sections; subscriptions are
//! delivered at most at QoS 1.

mod broker;
mod packet;

pub use broker::{ServerConfig, serve};

#[cfg(test)]
mod tests;
