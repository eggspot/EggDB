//! EggDB's REST API compatible with Elasticsearch and OpenSearch (D196), written from their
//! public API documentation: index and document APIs, `_bulk`, `_search` and `_count` with the
//! core query DSL (BM25 scoring from `eggdb-search`), and simple aggregations, served by our own
//! HTTP crate over EggDB's key-value transactions. Elasticsearch and OpenSearch are trademarks
//! of their owners; EggDB is compatible with their APIs.

mod api;
mod json_out;
mod query;

pub use api::Api;

#[cfg(test)]
mod tests;
