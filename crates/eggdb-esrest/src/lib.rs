//! EggDB's REST API compatible with Elasticsearch and OpenSearch (D196), written from their
//! public API documentation: index and document APIs, `_bulk`, `_search` and `_count` with the
//! core query DSL (BM25 scoring from `eggdb-search`), simple aggregations, multi-field `sort`
//! with `search_after` and `_source` filtering, `_update` with upserts and noop detection (D348),
//! `_mget`, `_msearch`, and `_update_by_query` and `_delete_by_query` with update scripts
//! (D331), served by our own HTTP crate over EggDB's key-value transactions. Elasticsearch and
//! OpenSearch are trademarks of their owners; EggDB is compatible with their APIs.

mod api;
mod json_out;
mod query;
mod script;
mod sort;
mod source_filter;

pub use api::{Api, over_memory_budget};

#[cfg(test)]
mod tests;
