//! EggDB's OPC UA client (D1071), written from the OPC UA specification (IEC 62541): the binary
//! encoding (part 6), the TCP transport's `HEL`/`ACK` and secure channel with the `None` security
//! policy, sessions activated anonymously or with a user name (part 4), and subscriptions whose
//! data changes [`OpcUaCollector`] stores as rows of the `opcua_values` table as they arrive.
//! OPC UA is a trademark of the OPC Foundation; EggDB is compatible with its protocol.

mod binary;
mod client;
mod collector;

pub use binary::NodeId;
pub use client::{Change, Client, Endpoint};
pub use collector::{OpcUaCollector, Subscription};

#[cfg(test)]
mod tests;
