use eggdb_platform::time::{SystemTime, UNIX_EPOCH};
use std::time::Duration;

const CHECK_EVERY: Duration = Duration::from_millis(500);

fn epoch_minute() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs() / 60).unwrap_or(i64::MAX)
        })
}

fn epoch_second() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

pub(super) fn run_scheduled_jobs(
    store: &impl eggdb_sql::KeyValueStore,
    stop: &(dyn Fn() -> bool + Sync),
) {
    let mut last = epoch_minute() - 1;
    let mut last_second = epoch_second();
    while !stop() {
        std::thread::sleep(CHECK_EVERY);
        let second = epoch_second();
        if second > last_second {
            last_second = second;
            let _ = eggdb_sql::jobs::run_due_second_jobs(store, second);
            let _ = eggdb_sql::jobs::run_due_events(store, second);
        }
        let now = epoch_minute();
        if now > last {
            last = now;
            let _ = eggdb_sql::jobs::run_due_jobs(store, now * 60);
            let _ = eggdb_sql::jobs::run_due_agent_jobs(store, now * 60);
            let _ = eggdb_sql::jobs::run_due_scheduler_jobs(store, now * 60);
        }
    }
}

const POWER_CHECK_EVERY: Duration = Duration::from_secs(60);

pub(super) fn follow_power_source(
    apply: &(dyn Fn(eggdb::PowerSource) + Sync),
    stop: &(dyn Fn() -> bool + Sync),
) {
    let mut applied = eggdb::PowerSource::Mains;
    let mut waited = POWER_CHECK_EVERY;
    while !stop() {
        if waited >= POWER_CHECK_EVERY {
            waited = Duration::ZERO;
            let source = eggdb_platform::power_source().unwrap_or(eggdb::PowerSource::Mains);
            if source != applied {
                apply(source);
                applied = source;
            }
        }
        std::thread::sleep(CHECK_EVERY);
        waited += CHECK_EVERY;
    }
}

const DELIVER_EVERY: Duration = Duration::from_secs(1);
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);
const WEBHOOK_CERTIFICATE_VARIABLE: &str = "EGGDB_CDC_CA_FILE";
const EVENTS_PER_ROUND: u64 = 100;

pub(super) fn deliver_changes(
    store: &impl eggdb_sql::KeyValueStore,
    database: &str,
    url: &str,
    engine_capture: bool,
    stop: &(dyn Fn() -> bool + Sync),
) {
    let Some(target) = super::report_delivery::WebTarget::parse(url) else {
        return;
    };
    while !stop() {
        std::thread::sleep(DELIVER_EVERY);
        let mut send = |event: &str| {
            target
                .post("application/json", event.as_bytes(), WEBHOOK_TIMEOUT, WEBHOOK_CERTIFICATE_VARIABLE)
                .is_ok_and(|status| (200..300).contains(&status))
        };
        let _ = eggdb_sql::cdc::deliver_changes_with_retries(store, database, "webhook", EVENTS_PER_ROUND, now_ms(), &mut send);
        if engine_capture {
            let _ = eggdb_sql::cdc::deliver_captured_changes(store, database, EVENTS_PER_ROUND, now_ms(), &mut send);
        }
    }
}

pub(super) fn deliver_changes_to_queue(
    store: &impl eggdb_sql::KeyValueStore,
    database: &str,
    keyspace: &(dyn eggdb_keyspace::Keyspace + Sync),
    queue: &str,
    engine_capture: bool,
    stop: &(dyn Fn() -> bool + Sync),
) {
    let queues = eggdb_queues::Queues::new(keyspace);
    while !stop() {
        std::thread::sleep(DELIVER_EVERY);
        if queues.declare(queue).is_err() {
            continue;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
            });
        let mut send = |event: &str| queues.send(queue, event.as_bytes(), 0, now).is_ok();
        let _ = eggdb_sql::cdc::deliver_changes_with_retries(store, database, "queue", EVENTS_PER_ROUND, now_ms(), &mut send);
        if engine_capture {
            let _ = eggdb_sql::cdc::deliver_captured_changes(store, database, EVENTS_PER_ROUND, now_ms(), &mut send);
        }
    }
}

pub(super) fn deliver_changes_to_topic(
    store: &impl eggdb_sql::KeyValueStore,
    database: &str,
    keyspace: &(dyn eggdb_keyspace::Keyspace + Sync),
    topic: &str,
    engine_capture: bool,
    stop: &(dyn Fn() -> bool + Sync),
) {
    while !stop() {
        std::thread::sleep(DELIVER_EVERY);
        let mut send = |event: &str| eggdb_kafka::publish(keyspace, topic, event.as_bytes()).is_ok();
        let _ = eggdb_sql::cdc::deliver_changes_with_retries(store, database, "topic", EVENTS_PER_ROUND, now_ms(), &mut send);
        if engine_capture {
            let _ = eggdb_sql::cdc::deliver_captured_changes(store, database, EVENTS_PER_ROUND, now_ms(), &mut send);
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

const REPORT_TARGETS: &str = "EGGDB_REPORT_TARGETS";

const MAIL_EVERY: Duration = Duration::from_secs(5);
const SMTP_SERVER: &str = "EGGDB_SMTP_SERVER";

pub(super) fn deliver_database_mail(
    store: &impl eggdb_sql::KeyValueStore,
    stop: &(dyn Fn() -> bool + Sync),
) {
    if std::env::var(SMTP_SERVER).is_err() {
        return;
    }
    while !stop() {
        std::thread::sleep(MAIL_EVERY);
        let _ = eggdb_sql::jobs::deliver_database_mail(store, EVENTS_PER_ROUND, &mut |to, subject, body| {
            super::report_delivery::send_mail(to, subject, body, WEBHOOK_TIMEOUT).map_err(|error| error.to_string())
        });
    }
}

pub(super) fn deliver_reports(
    store: &impl eggdb_sql::KeyValueStore,
    stop: &(dyn Fn() -> bool + Sync),
) {
    let Ok(allowed) = std::env::var(REPORT_TARGETS) else {
        return;
    };
    let prefixes: Vec<String> = allowed
        .split(',')
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty())
        .map(str::to_owned)
        .collect();
    if prefixes.is_empty() {
        return;
    }
    while !stop() {
        std::thread::sleep(DELIVER_EVERY);
        let _ =
            eggdb_sql::reports::deliver_reports(store, EVENTS_PER_ROUND, &mut |target, body| {
                if let Some(address) = target.strip_prefix("mailto:") {
                    if !prefixes.iter().any(|allowed| mail_target_allowed(allowed, address)) {
                        return true;
                    }
                    let (subject, text) = mail_of(body);
                    return super::report_delivery::send_mail(address, &subject, &text, WEBHOOK_TIMEOUT).is_ok();
                }
                if let Some(secure) = super::report_delivery::parse_https_url(target) {
                    let allowed = prefixes.iter().any(|allowed| {
                        super::report_delivery::parse_https_url(allowed).is_some_and(|entry| {
                            entry.host.eq_ignore_ascii_case(&secure.host) && entry.port == secure.port && path_within(&entry.path, &secure.path)
                        })
                    });
                    if !allowed {
                        return true;
                    }
                    return super::report_delivery::post_https(&secure, "application/json", body.as_bytes(), WEBHOOK_TIMEOUT)
                        .is_ok_and(|status| (200..300).contains(&status));
                }
                let Ok(url) = eggdb_http::parse_http_url(target) else {
                    return true;
                };
                if !prefixes
                    .iter()
                    .any(|allowed| report_target_allowed(allowed, &url))
                {
                    return true;
                }
                eggdb_http::post(&url, "application/json", body.as_bytes(), WEBHOOK_TIMEOUT)
                    .is_ok_and(|status| (200..300).contains(&status))
            });
    }
}

fn mail_target_allowed(allowed: &str, address: &str) -> bool {
    let Some(entry) = allowed.strip_prefix("mailto:") else {
        return false;
    };
    if let Some(domain) = entry.strip_prefix('@') {
        return address.rsplit_once('@').is_some_and(|(_, host)| host.eq_ignore_ascii_case(domain));
    }
    entry.eq_ignore_ascii_case(address)
}

fn mail_of(body: &str) -> (String, String) {
    let Ok(parsed) = eggdb_sql::json::parse_document(body) else {
        return ("EggDB report".to_owned(), body.to_owned());
    };
    let field = |name: &str| match &parsed.value {
        eggdb_sql::json::Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, node)| node),
        _ => None,
    };
    let text_of = |name: &str| match field(name).map(|node| &node.value) {
        Some(eggdb_sql::json::Json::String(text)) => Some(text.clone()),
        _ => None,
    };
    let subject = text_of("report").map_or_else(|| "EggDB report".to_owned(), |name| format!("EggDB report: {name}"));
    let rows = field("rows").and_then(|node| body.get(node.start..node.end)).unwrap_or("[]");
    (subject, format!("{}\n\n{rows}\n", text_of("text").unwrap_or_default()))
}

fn report_target_allowed(allowed: &str, target: &eggdb_http::HttpTarget) -> bool {
    eggdb_http::parse_http_url(allowed).is_ok_and(|entry| {
        entry.host.eq_ignore_ascii_case(&target.host)
            && entry.port == target.port
            && path_within(&entry.path, &target.path)
    })
}

fn path_within(allowed: &str, target: &str) -> bool {
    let target_path = target.split('?').next().unwrap_or_default();
    let lowered = target_path.to_ascii_lowercase();
    if target_path.contains('\\') || lowered.contains("%2f") || lowered.contains("%5c") {
        return false;
    }
    let climbs = target_path.split('/').any(|segment| {
        let decoded = segment.to_ascii_lowercase().replace("%2e", ".");
        decoded == "." || decoded == ".."
    });
    if climbs {
        return false;
    }
    let allowed = allowed.split('?').next().unwrap_or_default();
    let allowed = allowed.trim_end_matches('/');
    allowed.is_empty()
        || target_path == allowed
        || target_path
            .strip_prefix(allowed)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod report_target_tests {
    use super::report_target_allowed;

    #[test]
    fn a_report_target_must_be_on_the_allowed_host_and_port() {
        let target = |url: &str| eggdb_http::parse_http_url(url).unwrap();
        let entry = "http://hooks.example.com";
        assert!(report_target_allowed(
            entry,
            &target("http://hooks.example.com/a")
        ));
        assert!(!report_target_allowed(
            entry,
            &target("http://hooks.example.com.evil.net/a")
        ));
        assert!(!report_target_allowed(
            entry,
            &target("http://hooks.example.com:8080/a")
        ));
    }

    #[test]
    fn a_report_target_must_stay_under_the_allowed_path() {
        let target = |url: &str| eggdb_http::parse_http_url(url).unwrap();
        let entry = "http://hooks.example.com/hooks";
        for allowed in [
            "http://hooks.example.com/hooks",
            "http://hooks.example.com/hooks/orders?x=1",
        ] {
            assert!(report_target_allowed(entry, &target(allowed)), "{allowed}");
        }
        for refused in [
            "http://hooks.example.com/hooksX",
            "http://hooks.example.com/hooks/../admin",
            "http://hooks.example.com/hooks/%2E%2E/admin",
            "http://hooks.example.com/admin",
            "http://hooks.example.com/hooks/%2e%2e%2fadmin",
            "http://hooks.example.com/hooks/..\\admin",
        ] {
            assert!(!report_target_allowed(entry, &target(refused)), "{refused}");
        }
    }
}
