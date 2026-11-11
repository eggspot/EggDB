use eggdb::Database;
use std::fmt::Write as _;

fn metric(out: &mut String, name: &str, kind: &str, help: &str, samples: &[(String, u64)]) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    for (labels, value) in samples {
        let _ = writeln!(out, "{name}{labels} {value}");
    }
}

fn label(database: &str) -> String {
    let escaped = database
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n");
    format!("{{database=\"{escaped}\"}}")
}

/// One metric with its samples: no database label for the server's own, one sample per
/// served database otherwise.
pub(super) struct Family {
    pub(super) name: &'static str,
    pub(super) kind: &'static str,
    pub(super) help: &'static str,
    pub(super) samples: Vec<(Option<String>, u64)>,
}

fn server_wide(name: &'static str, kind: &'static str, help: &'static str, value: u64) -> Family {
    Family {
        name,
        kind,
        help,
        samples: vec![(None, value)],
    }
}

/// Every metric the server reports, for the Prometheus text and the OTLP export alike.
pub(super) fn families(databases: &[(&str, &Database)]) -> Vec<Family> {
    let activity = eggdb_sql::activity::activity();
    let mut families = vec![
        server_wide("eggdb_up", "gauge", "Whether the server is serving.", 1),
        server_wide(
            "eggdb_sessions_open",
            "gauge",
            "Sessions open now, over every protocol.",
            activity.sessions_open,
        ),
        server_wide(
            "eggdb_statements_total",
            "counter",
            "Statements run.",
            activity.statements,
        ),
        server_wide(
            "eggdb_statement_errors_total",
            "counter",
            "Statements that failed.",
            activity.statement_errors,
        ),
        server_wide(
            "eggdb_commits_total",
            "counter",
            "Transactions committed.",
            activity.commits,
        ),
        server_wide(
            "eggdb_rollbacks_total",
            "counter",
            "Transactions rolled back.",
            activity.rollbacks,
        ),
        server_wide(
            "eggdb_syslog_messages_dropped_total",
            "counter",
            "Syslog messages dropped: too long, badly framed, or refused by the syslog table.",
            eggdb_observability::syslog_messages_dropped(),
        ),
    ];
    let stats: Vec<(String, eggdb::Stats)> = databases
        .iter()
        .filter_map(|(name, db)| db.stats().ok().map(|stats| ((*name).to_owned(), stats)))
        .collect();
    let per_database = |name: &'static str,
                        kind: &'static str,
                        help: &'static str,
                        pick: fn(&eggdb::Stats) -> u64| Family {
        name,
        kind,
        help,
        samples: stats
            .iter()
            .map(|(database, stats)| (Some(database.clone()), pick(stats)))
            .collect(),
    };
    families.extend([
        per_database(
            "eggdb_file_bytes",
            "gauge",
            "The database file's size as of the last commit.",
            |stats| stats.file_bytes,
        ),
        per_database(
            "eggdb_free_pages",
            "gauge",
            "Pages free for later commits to reuse.",
            |stats| stats.free_pages,
        ),
        per_database(
            "eggdb_cache_pages",
            "gauge",
            "Pages the page cache holds now.",
            |stats| stats.cached_pages as u64,
        ),
        per_database(
            "eggdb_cache_hits_total",
            "counter",
            "Page lookups answered from the cache.",
            |stats| stats.cache_hits,
        ),
        per_database(
            "eggdb_cache_misses_total",
            "counter",
            "Page lookups that read the file.",
            |stats| stats.cache_misses,
        ),
        per_database(
            "eggdb_log_bytes_since_checkpoint",
            "gauge",
            "Write-ahead log bytes the next open would replay.",
            |stats| stats.log_bytes_since_checkpoint,
        ),
    ]);
    families
}

pub(super) fn metrics_text(databases: &[(&str, &Database)]) -> String {
    let mut out = String::new();
    for family in families(databases) {
        let samples: Vec<(String, u64)> = family
            .samples
            .iter()
            .map(|(database, value)| (database.as_deref().map(label).unwrap_or_default(), *value))
            .collect();
        metric(&mut out, family.name, family.kind, family.help, &samples);
    }
    out
}

pub(super) fn answer(
    request: &eggdb_http::Request,
    databases: &[(&str, &Database)],
) -> eggdb_http::Response {
    if request.method != "GET" && request.method != "HEAD" {
        return eggdb_http::Response::text(405, "the metrics endpoint answers GET and HEAD");
    }
    match request.path.as_str() {
        "/metrics" => eggdb_http::Response::ok(
            "text/plain; version=0.0.4; charset=utf-8",
            metrics_text(databases),
        ),
        "/" | "/health" => eggdb_http::Response::text(200, "ok"),
        _ => eggdb_http::Response::text(404, "not found; metrics are at /metrics"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exposition_names_every_metric_with_its_type() {
        let db = Database::in_memory().unwrap();
        db.put("k", "v").unwrap();
        let text = metrics_text(&[("shop\"x", &db)]);
        for name in [
            "eggdb_up 1",
            "# TYPE eggdb_statements_total counter",
            "# HELP eggdb_syslog_messages_dropped_total Syslog messages dropped",
            "# TYPE eggdb_syslog_messages_dropped_total counter",
            "eggdb_syslog_messages_dropped_total ",
            "eggdb_file_bytes{database=\"shop\\\"x\"}",
        ] {
            assert!(text.contains(name), "{name} missing from:\n{text}");
        }
        assert!(
            text.lines()
                .all(|line| line.starts_with('#') || line.starts_with("eggdb_"))
        );
    }
}
