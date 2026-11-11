use super::serve_metrics::Family;
use std::fmt::Write as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EXPORT_EVERY: Duration = Duration::from_secs(60);
const CHECK_EVERY: Duration = Duration::from_millis(500);
const EXPORT_TIMEOUT: Duration = Duration::from_secs(10);
const CUMULATIVE: u8 = 2;

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

/// Posts the server's metrics to the OTLP/HTTP collector at `endpoint` (its `/v1/metrics`)
/// once a minute until `stop`, starting at once; a collector that is down or refuses a
/// request misses that minute's points.
pub(super) fn export_metrics(
    endpoint: &str,
    collect: &(dyn Fn() -> Vec<Family> + Sync),
    stop: &(dyn Fn() -> bool + Sync),
) {
    let url = format!("{}/v1/metrics", endpoint.trim_end_matches('/'));
    let Ok(target) = eggdb_http::parse_http_url(&url) else {
        return;
    };
    let start = now_nanos();
    let mut next = Instant::now();
    while !stop() {
        if Instant::now() >= next {
            let body = metrics_request(&collect(), start, now_nanos());
            let _ = eggdb_http::post(&target, "application/json", body.as_bytes(), EXPORT_TIMEOUT);
            next = Instant::now() + EXPORT_EVERY;
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
}
