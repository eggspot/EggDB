//! EggDB's metrics store (D220): points written in InfluxDB's line protocol (the `/write` and
//! `/api/v2/write` endpoints) or by Prometheus remote write land in SQL tables in the `metrics`
//! schema, one table per measurement with a `time` column, a text column per tag and a typed
//! column per field, so SQL, `time_bucket` and every protocol read them; PromQL selectors,
//! range functions, aggregations and scalar arithmetic answer through the Prometheus HTTP API
//! (`/api/v1/query`, `query_range`, `labels`, `label/<name>/values` and `series`). Syslog
//! messages (RFC 5424 and RFC 3164) taken over UDP and TCP by [`SyslogCollector`] land as rows
//! of the `syslog` table (D315). InfluxDB is a trademark of InfluxData and Prometheus of the
//! Linux Foundation; EggDB is compatible with their protocols.

mod api;
mod line_protocol;
mod promql;
mod remote_write;
mod store;
mod syslog;

pub use api::{Endpoint, MAX_REQUEST_BYTES};
pub use syslog::{MAX_SYSLOG_MESSAGE_BYTES, SyslogCollector};

#[cfg(test)]
mod tests;
