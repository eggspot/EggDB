//! Live queries over Server-Sent Events (D878): the query runs again when the database has
//! committed a change since its last run (checked every interval, D1178) and its result is
//! sent as an event whenever it differs from the last one sent, until the client goes away.
//! A comment line every 15 seconds keeps proxies from closing a quiet stream and finds clients
//! that left.

use crate::{Endpoint, rows_json};
use eggdb_http::{BodyWriter, Reply, Request, StreamedResponse};
use eggdb_sql::{KeyValueRead, KeyValueStore};
use std::io::{self, Write};
use std::time::Duration;

const HEARTBEAT: Duration = Duration::from_secs(15);
const DEFAULT_INTERVAL: Duration = Duration::from_millis(250);
const MIN_INTERVAL: Duration = Duration::from_millis(100);
const MAX_INTERVAL: Duration = Duration::from_secs(60);

/// Whether the client asked for an event stream.
pub(crate) fn wanted(request: &Request) -> bool {
    request
        .header("accept")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("text/event-stream")
}

/// How often a live query runs again and how many events it sends before it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiveOptions {
    pub(crate) interval: Duration,
    pub(crate) max_events: Option<u64>,
}

impl LiveOptions {
    /// The options a request sets with `X-EggDB-Live-Interval` (milliseconds, 100 to 60000)
    /// and `X-EggDB-Live-Events`; one second and no end otherwise.
    pub(crate) fn of(request: &Request) -> Self {
        let interval = request
            .header("x-eggdb-live-interval")
            .and_then(|text| text.trim().parse::<u64>().ok())
            .map_or(DEFAULT_INTERVAL, |millis| {
                Duration::from_millis(millis).clamp(MIN_INTERVAL, MAX_INTERVAL)
            });
        let max_events = request
            .header("x-eggdb-live-events")
            .and_then(|text| text.trim().parse::<u64>().ok())
            .filter(|count| *count > 0);
        Self { interval, max_events }
    }
}

/// Writes one event, splitting `data` into one `data:` line per line, as the format needs.
pub(crate) fn write_event(out: &mut BodyWriter<'_>, event: &str, data: &str) -> io::Result<()> {
    let mut text = format!("event: {event}\n");
    for line in data.split(['\n', '\r']) {
        text.push_str("data: ");
        text.push_str(line);
        text.push('\n');
    }
    text.push('\n');
    out.write_all(text.as_bytes())?;
    out.flush()
}

/// What a live query's run produced: an event to send, or the end of the stream with a last
/// event.
pub(crate) enum Polled {
    Result(String),
    Failed(String),
    Unchanged,
}

/// A poll that runs `poll` only when the store's committed version moved since its last run,
/// so a live query costs a version check, not a query, while nothing changes. A store that
/// does not tell its version runs `poll` every time.
pub(crate) fn on_change<'p>(
    mut version: impl FnMut() -> Option<u64> + 'p,
    mut poll: impl FnMut() -> Polled + 'p,
) -> impl FnMut() -> Polled + 'p {
    let mut seen: Option<u64> = None;
    let mut ran = false;
    move || {
        let now = version();
        if ran && now.is_some() && now == seen {
            return Polled::Unchanged;
        }
        ran = true;
        seen = now;
        poll()
    }
}

/// A streamed reply that calls `poll` every interval and sends its result as a `result_event`
/// whenever it changed; a failure is sent as `failure_event` and ends the stream, and
/// `closing_event`, when given, is sent last.
pub(crate) fn live_reply<'s>(
    options: LiveOptions,
    result_event: &'static str,
    failure_event: &'static str,
    closing_event: Option<&'static str>,
    mut poll: impl FnMut() -> Polled + 's,
) -> Reply<'s> {
    StreamedResponse::new(200, "text/event-stream; charset=utf-8", move |out| {
        let mut last: Option<String> = None;
        let mut sent = 0u64;
        let mut quiet = Duration::ZERO;
        let mut generation = eggdb_sql::commit_generation();
        loop {
            match poll() {
                Polled::Result(payload) => {
                    if last.as_deref() != Some(payload.as_str()) {
                        write_event(out, result_event, &payload)?;
                        sent += 1;
                        quiet = Duration::ZERO;
                        last = Some(payload);
                    }
                }
                Polled::Failed(payload) => {
                    write_event(out, failure_event, &payload)?;
                    if let Some(closing) = closing_event {
                        write_event(out, closing, "")?;
                    }
                    return Ok(());
                }
                Polled::Unchanged => {}
            }
            if options.max_events.is_some_and(|max| sent >= max) {
                if let Some(closing) = closing_event {
                    write_event(out, closing, "")?;
                }
                return Ok(());
            }
            let waited = std::time::Instant::now();
            generation = eggdb_sql::wait_for_commit(generation, options.interval);
            if let Some(rest) = MIN_INTERVAL.checked_sub(waited.elapsed()) {
                std::thread::sleep(rest);
            }
            quiet += waited.elapsed();
            if quiet >= HEARTBEAT {
                out.write_all(b": keep-alive\n\n")?;
                out.flush()?;
                quiet = Duration::ZERO;
            }
        }
    })
    .with_header("Cache-Control", "no-cache")
    .into()
}

impl<'s, S: KeyValueStore> Endpoint<'s, S> {
    /// The committed version the store's next read sees, when the store tells it.
    pub(crate) fn committed_version(&self) -> Option<u64> {
        self.store.begin_read().ok().and_then(|read| read.committed_version())
    }

    /// A table read kept open: `sql`'s rows as a JSON array in a `rows` event each time they
    /// change.
    pub(crate) fn live_rows(&self, sql: String, options: LiveOptions) -> Reply<'s> {
        live_reply(options, "rows", "error", None, self.rows_poller(sql))
    }

    /// What re-runs `sql` when the database changed: its rows as a JSON array, or the error.
    pub(crate) fn rows_poller(&self, sql: String) -> impl FnMut() -> Polled + 's {
        let endpoint = self.copy();
        let versions = self.copy();
        on_change(move || versions.committed_version(), move || match endpoint.run(&sql) {
            Ok(results) => Polled::Result(
                results
                    .iter()
                    .rev()
                    .find(|result| result.columns.is_some())
                    .map_or_else(|| "[]".to_owned(), rows_json),
            ),
            Err(error) => Polled::Failed(format!(
                "{{\"code\":{},\"message\":{}}}",
                crate::json_string(error.state().code()),
                crate::json_string(error.message())
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_split_their_data_into_lines() {
        let written = StreamedResponse::new(200, "text/event-stream", |out| write_event(out, "rows", "[1,\n2]")).collected();
        assert_eq!(String::from_utf8(written.body).unwrap(), "event: rows\ndata: [1,\ndata: 2]\n\n");
    }

    #[test]
    fn a_live_reply_sends_changes_only_and_ends_after_its_events() {
        let mut runs = 0;
        let reply = live_reply(
            LiveOptions {
                interval: Duration::from_millis(1),
                max_events: Some(2),
            },
            "next",
            "next",
            Some("complete"),
            move || {
                runs += 1;
                Polled::Result(if runs < 3 { "a".to_owned() } else { "b".to_owned() })
            },
        );
        let text = String::from_utf8(reply.into_response().body).unwrap();
        assert_eq!(text, "event: next\ndata: a\n\nevent: next\ndata: b\n\nevent: complete\ndata: \n\n");
    }
}
