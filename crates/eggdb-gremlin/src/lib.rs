//! EggDB's server for Apache TinkerPop Gremlin, written from the TinkerPop reference
//! documentation: the Gremlin Server WebSocket protocol with GraphSON 3.0 messages, SASL `PLAIN`
//! authentication, string scripts (`op: eval`) and bytecode traversals (`op: bytecode`). Each
//! traversal is compiled into one Cypher query and run on EggDB's property graph, the same graph
//! Bolt and Cypher clients see (D1011).

mod compile;
mod graphson;
mod json;
mod server;
mod traversal;
mod websocket;

pub use server::{ServerConfig, serve};
