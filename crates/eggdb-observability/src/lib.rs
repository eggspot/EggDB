//! EggDB's metrics store (D220): points written in InfluxDB's line protocol (the `/write` and
//! `/api/v2/write` endpoints) or by Prometheus remote write land in SQL tables in the `metrics`
//! schema, one table per measurement with a `time` column, a text column per tag and a typed
//! column per field, so SQL, `time_bucket` and every protocol read them; PromQL (selectors
//! with or without a metric name, range functions, subqueries, every aggregation, vector
//! matching, `histogram_quantile`, `label_replace` and `label_join`) answers through the
//! Prometheus HTTP API (`/api/v1/query`, `query_range`, `labels`, `label/<name>/values` and
//! `series`), each query bounded by a samples limit and the server's memory budget. Syslog
//! messages (RFC 5424 and RFC 3164) taken over UDP and TCP by [`SyslogCollector`] land as rows
//! of the `syslog` table (D315); the messages it drops are counted in
//! [`syslog_messages_dropped`] (D352). InfluxDB is a trademark of InfluxData and Prometheus of
//! the Linux Foundation; EggDB is compatible with their protocols.

mod api;
mod line_protocol;
mod promql;
mod remote_write;
mod store;
mod syslog;

pub use api::{DEFAULT_MAX_QUERY_SAMPLES, Endpoint, MAX_REQUEST_BYTES, wants_content_decoded};
pub use syslog::{MAX_SYSLOG_MESSAGE_BYTES, SyslogCollector, syslog_messages_dropped};

#[cfg(test)]
mod promql_tests;
#[cfg(test)]
mod tests;
