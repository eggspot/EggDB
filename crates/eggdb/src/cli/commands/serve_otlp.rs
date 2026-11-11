use super::serve_metrics::Family;
use std::fmt::Write as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EXPORT_EVERY: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_millis(500);
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
const CUMULATIVE: u8 = 2;
const MOST_HELD_RECORDS: usize = 10_000;
const SERVER_SPAN: u8 = 2;

/// A slow statement held for the next trace export.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct StatementSpan {
    pub(super) start_nanos: u128,
    pub(super) end_nanos: u128,
    pub(super) database: String,
    pub(super) user: String,
    pub(super) protocol: String,
    pub(super) rows: u64,
    pub(super) query: String,
}

/// The server's log lines and slow statements waiting for the next OTLP export, at most 10,000
/// of each (the oldest dropped first).
#[derive(Debug, Default)]
pub(super) struct HeldTelemetry {
    logs: std::sync::Mutex<std::collections::VecDeque<(u128, String)>>,
    spans: std::sync::Mutex<std::collections::VecDeque<StatementSpan>>,
}

fn held<T>(queue: &std::sync::Mutex<std::collections::VecDeque<T>>, item: T) {
    let mut queue = queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if queue.len() >= MOST_HELD_RECORDS {
        queue.pop_front();
    }
    queue.push_back(item);
}

fn drained<T>(queue: &std::sync::Mutex<std::collections::VecDeque<T>>) -> Vec<T> {
    queue
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
        .collect()
}

impl HeldTelemetry {
    pub(super) fn log(&self, line: &str) {
        held(&self.logs, (now_nanos(), line.to_owned()));
    }

    pub(super) fn slow_statement(&self, statement: &eggdb_sql::statement_statistics::SlowStatement<'_>) {
        let end_nanos = now_nanos();
        held(
            &self.spans,
            StatementSpan {
                start_nanos: end_nanos.saturating_sub(statement.duration.as_nanos()),
                end_nanos,
                database: statement.database.to_owned(),
                user: statement.user.to_owned(),
                protocol: statement.protocol.to_owned(),
                rows: statement.rows,
                query: statement.query.to_owned(),
            },
        );
    }
}

fn resource() -> String {
    format!(
        "{{\"attributes\":[{{\"key\":\"service.name\",\"value\":{{\"stringValue\":\"eggdb\"}}}},{{\"key\":\"service.version\",\"value\":{{\"stringValue\":{}}}}}]}}",
        json_string(env!("CARGO_PKG_VERSION"))
    )
}

fn text_attribute(key: &str, value: &str) -> String {
    format!(
        "{{\"key\":{},\"value\":{{\"stringValue\":{}}}}}",
        json_string(key),
        json_string(value)
    )
}

/// An OTLP/HTTP `ExportLogsServiceRequest` in its JSON encoding: each log line as an `INFO`
/// record with its time and text.
pub(super) fn logs_request(lines: &[(u128, String)]) -> String {
    let records: Vec<String> = lines
        .iter()
        .map(|(nanos, line)| {
            format!(
                "{{\"timeUnixNano\":\"{nanos}\",\"severityNumber\":9,\"severityText\":\"INFO\",\"body\":{{\"stringValue\":{}}}}}",
                json_string(line)
            )
        })
        .collect();
    format!(
        "{{\"resourceLogs\":[{{\"resource\":{},\"scopeLogs\":[{{\"scope\":{{\"name\":\"eggdb\"}},\"logRecords\":[{}]}}]}}]}}",
        resource(),
        records.join(",")
    )
}

/// An OTLP/HTTP `ExportTraceServiceRequest` in its JSON encoding: each slow statement as a server
/// span of its own trace, with OpenTelemetry's database attributes.
pub(super) fn traces_request(spans: &[StatementSpan]) -> String {
    let encoded: Vec<String> = spans
        .iter()
        .enumerate()
        .map(|(at, span)| {
            let identity = span.start_nanos ^ (at as u128) << 64;
            let attributes = [
                text_attribute("db.system", "eggdb"),
                text_attribute("db.name", &span.database),
                text_attribute("db.user", &span.user),
                text_attribute("network.protocol.name", &span.protocol),
                text_attribute("db.statement", &span.query),
                format!(
                    "{{\"key\":\"db.response.returned_rows\",\"value\":{{\"intValue\":\"{}\"}}}}",
                    span.rows
                ),
            ];
            format!(
                "{{\"traceId\":\"{identity:032x}\",\"spanId\":\"{:016x}\",\"name\":{},\"kind\":{SERVER_SPAN},\"startTimeUnixNano\":\"{}\",\"endTimeUnixNano\":\"{}\",\"attributes\":[{}]}}",
                (identity as u64) | 1,
                json_string(&format!("{} statement", span.protocol)),
                span.start_nanos,
                span.end_nanos,
                attributes.join(",")
            )
        })
        .collect();
    format!(
        "{{\"resourceSpans\":[{{\"resource\":{},\"scopeSpans\":[{{\"scope\":{{\"name\":\"eggdb\"}},\"spans\":[{}]}}]}}]}}",
        resource(),
        encoded.join(",")
    )
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control < ' ' => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
}

/// An OTLP/HTTP `ExportMetricsServiceRequest` in its JSON encoding: every family as a gauge or
/// a cumulative monotonic sum of integer points, stamped `now_nanos`, sums counted from
/// `start_nanos`, with the database as a `database` attribute.
pub(super) fn metrics_request(families: &[Family], start_nanos: u128, now_nanos: u128) -> String {
    let mut metrics = Vec::with_capacity(families.len());
    for family in families {
        let counter = family.kind == "counter";
        let points: Vec<String> = family
            .samples
            .iter()
            .map(|(database, value)| {
                let attributes = database.as_deref().map_or_else(String::new, |database| {
                    format!(
                        ",\"attributes\":[{{\"key\":\"database\",\"value\":{{\"stringValue\":{}}}}}]",
                        json_string(database)
                    )
                });
                let start = if counter {
                    format!(",\"startTimeUnixNano\":\"{start_nanos}\"")
                } else {
                    String::new()
                };
                format!("{{\"asInt\":\"{value}\",\"timeUnixNano\":\"{now_nanos}\"{start}{attributes}}}")
            })
            .collect();
        let points = points.join(",");
        let data = if counter {
            format!(
                "\"sum\":{{\"aggregationTemporality\":{CUMULATIVE},\"isMonotonic\":true,\"dataPoints\":[{points}]}}"
            )
        } else {
            format!("\"gauge\":{{\"dataPoints\":[{points}]}}")
        };
        metrics.push(format!(
            "{{\"name\":{},\"description\":{},\"unit\":\"1\",{data}}}",
            json_string(family.name),
            json_string(family.help)
        ));
    }
    format!(
        "{{\"resourceMetrics\":[{{\"resource\":{{\"attributes\":[{{\"key\":\"service.name\",\"value\":{{\"stringValue\":\"eggdb\"}}}},{{\"key\":\"service.version\",\"value\":{{\"stringValue\":{}}}}}]}},\"scopeMetrics\":[{{\"scope\":{{\"name\":\"eggdb\"}},\"metrics\":[{}]}}]}}]}}",
        json_string(env!("CARGO_PKG_VERSION")),
        metrics.join(",")
    )
}

/// Posts the server's metrics, and the log lines and slow statements `held` gathered, to the
/// OTLP/HTTP collector at `endpoint` (its `/v1/metrics`, `/v1/logs` and `/v1/traces`) once a
/// minute until `stop`, starting at once; a collector that is down or refuses a request misses
/// that minute's records.
pub(super) fn export_telemetry(
    endpoint: &str,
    collect: &(dyn Fn() -> Vec<Family> + Sync),
    held: &HeldTelemetry,
    stop: &(dyn Fn() -> bool + Sync),
) {
    let base = endpoint.trim_end_matches('/');
    let target = |path: &str| eggdb_http::parse_http_url(&format!("{base}{path}")).ok();
    let (Some(metrics), Some(logs), Some(traces)) =
        (target("/v1/metrics"), target("/v1/logs"), target("/v1/traces"))
    else {
        return;
    };
    let start = now_nanos();
    let mut next = Instant::now();
    loop {
        let stopping = stop();
        if stopping || Instant::now() >= next {
            let body = metrics_request(&collect(), start, now_nanos());
            let _ = eggdb_http::post(&metrics, "application/json", body.as_bytes(), EXPORT_TIMEOUT);
            let lines = drained(&held.logs);
            if !lines.is_empty() {
                let body = logs_request(&lines);
                let _ = eggdb_http::post(&logs, "application/json", body.as_bytes(), EXPORT_TIMEOUT);
            }
            let spans = drained(&held.spans);
            if !spans.is_empty() {
                let body = traces_request(&spans);
                let _ = eggdb_http::post(&traces, "application/json", body.as_bytes(), EXPORT_TIMEOUT);
            }
            next = Instant::now() + EXPORT_EVERY;
        }
        if stopping {
            break;
        }
        std::thread::sleep(CHECK_EVERY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_become_cumulative_sums_and_gauges_stay_gauges() {
        let families = [
            Family {
                name: "eggdb_commits_total",
                kind: "counter",
                help: "Transactions committed.",
                samples: vec![(None, 7)],
            },
            Family {
                name: "eggdb_file_bytes",
                kind: "gauge",
                help: "The \"file\" size.",
                samples: vec![(Some("shop".to_owned()), 4096)],
            },
        ];
        let json = metrics_request(&families, 10, 20);
        assert!(json.contains("\"sum\":{\"aggregationTemporality\":2,\"isMonotonic\":true,\"dataPoints\":[{\"asInt\":\"7\",\"timeUnixNano\":\"20\",\"startTimeUnixNano\":\"10\"}]}"), "{json}");
        assert!(json.contains("\"gauge\":{\"dataPoints\":[{\"asInt\":\"4096\",\"timeUnixNano\":\"20\",\"attributes\":[{\"key\":\"database\",\"value\":{\"stringValue\":\"shop\"}}]}]}"), "{json}");
        assert!(json.contains("\"description\":\"The \\\"file\\\" size.\""), "{json}");
        assert!(eggdb_sql::json::parse_document(&json).is_ok(), "{json}");
    }

    #[test]
    fn log_lines_and_slow_statements_become_records_and_spans() {
        let logs = logs_request(&[(5, "server \"started\"".to_owned())]);
        assert!(logs.contains("\"timeUnixNano\":\"5\""), "{logs}");
        assert!(eggdb_sql::json::parse_document(&logs).is_ok(), "{logs}");
        let span = StatementSpan {
            start_nanos: 100,
            end_nanos: 250,
            database: "shop".to_owned(),
            user: "ada".to_owned(),
            protocol: "postgresql".to_owned(),
            rows: 3,
            query: "SELECT $1".to_owned(),
        };
        let traces = traces_request(&[span]);
        assert!(traces.contains("\"name\":\"postgresql statement\""), "{traces}");
        assert!(traces.contains("\"endTimeUnixNano\":\"250\""), "{traces}");
        assert!(eggdb_sql::json::parse_document(&traces).is_ok(), "{traces}");
    }
}
