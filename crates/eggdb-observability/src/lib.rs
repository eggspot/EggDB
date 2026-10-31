//! EggDB's metrics store (D220): points written in InfluxDB's line protocol (the `/write` and
//! `/api/v2/write` endpoints) or by Prometheus remote write land in SQL tables in the `metrics`
//! schema, one table per measurement with a `time` column, a text column per tag and a typed
//! column per field, so SQL, `time_bucket` and every protocol read them; PromQL selectors,
//! range functions, aggregations and scalar arithmetic answer through the Prometheus HTTP API
//! (`/api/v1/query`, `query_range`, `labels`, `label/<name>/values` and `series`). InfluxDB is
//! a trademark of InfluxData and Prometheus of the Linux Foundation; EggDB is compatible with
//! their protocols.

mod api;
mod line_protocol;
mod promql;
mod remote_write;
mod store;

pub use api::{Endpoint, MAX_REQUEST_BYTES};

#[cfg(test)]
mod tests;
