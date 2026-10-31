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

pub(super) fn metrics_text(databases: &[(&str, &Database)]) -> String {
    let mut out = String::new();
    let activity = eggdb_sql::activity::activity();
    metric(&mut out, "eggdb_up", "gauge", "Whether the server is serving.", &[(String::new(), 1)]);
    metric(
        &mut out,
        "eggdb_sessions_open",
        "gauge",
        "Sessions open now, over every protocol.",
        &[(String::new(), activity.sessions_open)],
    );
    for (name, kind, help, value) in [
        ("eggdb_statements_total", "counter", "Statements run.", activity.statements),
        ("eggdb_statement_errors_total", "counter", "Statements that failed.", activity.statement_errors),
        ("eggdb_commits_total", "counter", "Transactions committed.", activity.commits),
        ("eggdb_rollbacks_total", "counter", "Transactions rolled back.", activity.rollbacks),
    ] {
        metric(&mut out, name, kind, help, &[(String::new(), value)]);
    }
    let stats: Vec<(String, eggdb::Stats)> = databases
        .iter()
        .filter_map(|(name, db)| db.stats().ok().map(|stats| (label(name), stats)))
        .collect();
    let per_database = |pick: fn(&eggdb::Stats) -> u64| -> Vec<(String, u64)> {
        stats
            .iter()
            .map(|(labels, stats)| (labels.clone(), pick(stats)))
            .collect()
    };
    metric(&mut out, "eggdb_file_bytes", "gauge", "The database file's size as of the last commit.", &per_database(|stats| stats.file_bytes));
    metric(&mut out, "eggdb_free_pages", "gauge", "Pages free for later commits to reuse.", &per_database(|stats| stats.free_pages));
    metric(&mut out, "eggdb_cache_pages", "gauge", "Pages the page cache holds now.", &per_database(|stats| stats.cached_pages as u64));
    metric(&mut out, "eggdb_cache_hits_total", "counter", "Page lookups answered from the cache.", &per_database(|stats| stats.cache_hits));
    metric(&mut out, "eggdb_cache_misses_total", "counter", "Page lookups that read the file.", &per_database(|stats| stats.cache_misses));
    metric(&mut out, "eggdb_log_bytes_since_checkpoint", "gauge", "Write-ahead log bytes the next open would replay.", &per_database(|stats| stats.log_bytes_since_checkpoint));
    out
}

pub(super) fn answer(request: &eggdb_http::Request, databases: &[(&str, &Database)]) -> eggdb_http::Response {
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
        for name in ["eggdb_up 1", "# TYPE eggdb_statements_total counter", "eggdb_file_bytes{database=\"shop\\\"x\"}"] {
            assert!(text.contains(name), "{name} missing from:\n{text}");
        }
        assert!(text.lines().all(|line| line.starts_with('#') || line.starts_with("eggdb_")));
    }
}
