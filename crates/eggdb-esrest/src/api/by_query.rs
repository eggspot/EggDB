use super::{
    Answer, Api, document_key, document_prefix, error, index_key, json, missing_index, settings_key,
    over_memory_budget, query_flag, read, resolve_indices, script_error, scripted, stored,
    unstored, validation, write,
};
use crate::json_out::{Out, escaped};
use crate::query::{Query, Stats, analyze, parse, score};
use crate::script::{Operation, Script};
use eggdb_http::{Request, Response};
use eggdb_platform::connection_limits::Reservation;
use eggdb_sql::json::{Json, Node, parse_document};

const DEFAULT_SCROLL_SIZE: usize = 1000;
const MAX_SCROLL_SIZE: usize = 10_000;
const MATCHED_ENTRY_BYTES: usize = 56;
const BATCH_WRITE_BYTES: usize = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conflicts {
    Abort,
    Proceed,
}

#[derive(Debug, Clone, Copy)]
enum Change<'s> {
    Update(Option<&'s Script>),
    Delete,
}

#[derive(Debug)]
struct Matched {
    id: String,
    version: u64,
    fingerprint: u64,
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[derive(Debug)]
enum Snapshot {
    Matched(Vec<Matched>),
    MissingIndex,
    OverBudget,
}

#[derive(Debug, Default)]
struct Tally {
    total: u64,
    updated: u64,
    deleted: u64,
    noops: u64,
    version_conflicts: u64,
    batches: u64,
    failures: Vec<String>,
    status: u16,
}

impl Tally {
    fn add(&mut self, batch: &Tally) {
        self.total += batch.total;
        self.updated += batch.updated;
        self.deleted += batch.deleted;
        self.noops += batch.noops;
        self.version_conflicts += batch.version_conflicts;
    }

    fn fail(&mut self, index: &str, id: &str, kind: &str, reason: &str, status: u16) {
        self.failures.push(format!(
            "{{\"index\":{},\"id\":{},\"cause\":{{\"type\":{},\"reason\":{},\"index\":{}}},\"status\":{status}}}",
            escaped(index),
            escaped(id),
            escaped(kind),
            escaped(reason),
            escaped(index)
        ));
        self.status = self.status.max(status);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slice {
    id: u64,
    max: u64,
}

impl Slice {
    fn of(node: &Node) -> Result<Self, Response> {
        let refused = |reason: &str| error(400, "parse_exception", reason);
        let Json::Object(members) = &node.value else {
            return Err(refused("[slice] must be an object"));
        };
        let (mut id, mut max) = (None, None);
        for (key, value) in members {
            let Json::Number(text) = &value.value else {
                return Err(refused(&format!("[slice] [{key}] must be a whole number")));
            };
            let number = text
                .parse::<i64>()
                .map_err(|_| refused(&format!("[slice] [{key}] must be a whole number")))?;
            match key.as_str() {
                "id" => id = Some(number),
                "max" => max = Some(number),
                other => return Err(refused(&format!("[slice] does not support [{other}]"))),
            }
        }
        let illegal = |reason: &str| error(400, "illegal_argument_exception", reason);
        let (Some(id), Some(max)) = (id, max) else {
            return Err(illegal("[slice] needs both [id] and [max]"));
        };
        if max <= 1 {
            return Err(illegal("max must be greater than 1"));
        }
        if id < 0 {
            return Err(illegal("id must be greater than or equal to 0"));
        }
        if id >= max {
            return Err(illegal("max must be greater than id"));
        }
        Ok(Self {
            id: id.unsigned_abs(),
            max: max.unsigned_abs(),
        })
    }

    fn holds(self, document_id: &str) -> bool {
        fnv1a(document_id.as_bytes()) % self.max == self.id
    }
}

#[derive(Debug)]
struct ByQuery {
    query: Query,
    script: Option<Node>,
    conflicts: Conflicts,
    max_docs: Option<u64>,
    scroll_size: usize,
    slice: Option<Slice>,
}

fn count_of(node: &Node) -> Option<u64> {
    match &node.value {
        Json::Number(text) => text.parse().ok(),
        _ => None,
    }
}

impl ByQuery {
    fn of(request: &Request, updates: bool) -> Result<Self, Response> {
        let text = String::from_utf8_lossy(&request.body).into_owned();
        let body = if text.trim().is_empty() {
            None
        } else {
            Some(parse_document(&text).map_err(|syntax| {
                error(
                    400,
                    "parse_exception",
                    &format!(
                        "failed to parse the request body: {} at byte {}",
                        syntax.message, syntax.position
                    ),
                )
            })?)
        };
        let mut query = None;
        let mut script = None;
        let mut conflicts = query_flag(request, "conflicts");
        let mut max_docs = None;
        let mut slice = None;
        if let Some(body) = &body {
            let Json::Object(members) = &body.value else {
                return Err(error(
                    400,
                    "parse_exception",
                    "the request body must be a JSON object",
                ));
            };
            for (key, value) in members {
                match key.as_str() {
                    "query" => {
                        query = Some(
                            parse(value, 0)
                                .map_err(|reason| error(400, "parsing_exception", &reason))?,
                        );
                    }
                    "script" if updates => script = Some(value.clone()),
                    "max_docs" => {
                        max_docs = Some(count_of(value).ok_or_else(|| {
                            error(
                                400,
                                "parse_exception",
                                "[max_docs] must be a whole number of at least 1",
                            )
                        })?);
                    }
                    "conflicts" => match &value.value {
                        Json::String(text) => conflicts = Some(text.clone()),
                        _ => {
                            return Err(error(
                                400,
                                "parse_exception",
                                "[conflicts] must be \"abort\" or \"proceed\"",
                            ));
                        }
                    },
                    "slice" => slice = Some(Slice::of(value)?),
                    other => {
                        return Err(error(
                            400,
                            "parse_exception",
                            &format!("request does not support [{other}]"),
                        ));
                    }
                }
            }
        }
        let query = match (query, query_flag(request, "q")) {
            (Some(query), _) => query,
            (None, Some(text)) => Query::Match {
                fields: vec!["*".to_owned()],
                terms: analyze(&text),
                all: false,
            },
            (None, None) => Query::MatchAll,
        };
        let conflicts = match conflicts.as_deref() {
            None | Some("abort") => Conflicts::Abort,
            Some("proceed") => Conflicts::Proceed,
            Some(other) => {
                return Err(error(
                    400,
                    "illegal_argument_exception",
                    &format!("conflicts may only be \"proceed\" or \"abort\" but was [{other}]"),
                ));
            }
        };
        let max_docs = match (max_docs, query_flag(request, "max_docs")) {
            (Some(count), _) => Some(count),
            (None, Some(text)) => Some(text.parse::<u64>().map_err(|_| {
                error(
                    400,
                    "illegal_argument_exception",
                    &format!("[max_docs] must be a whole number but was [{text}]"),
                )
            })?),
            (None, None) => None,
        };
        if max_docs == Some(0) {
            return Err(validation(&[
                "maxDocs should be greater than 0, but was [0]".to_owned(),
            ]));
        }
        let scroll_size = match query_flag(request, "scroll_size") {
            None => DEFAULT_SCROLL_SIZE,
            Some(text) => match text.parse::<usize>() {
                Ok(size) if (1..=MAX_SCROLL_SIZE).contains(&size) => size,
                _ => {
                    return Err(error(
                        400,
                        "illegal_argument_exception",
                        &format!(
                            "[scroll_size] must be from 1 to {MAX_SCROLL_SIZE} but was [{text}]"
                        ),
                    ));
                }
            },
        };
        Ok(Self {
            query,
            script,
            conflicts,
            max_docs,
            scroll_size,
            slice,
        })
    }
}

fn matches(query: &Query, id: &str, source: &Node) -> bool {
    let stats = Stats::default();
    match query {
        Query::Match { fields, terms, all } if fields.iter().any(|field| field == "*") => {
            let names = match &source.value {
                Json::Object(members) => members.iter().map(|(key, _)| key.clone()).collect(),
                _ => Vec::new(),
            };
            let expanded = Query::Match {
                fields: names,
                terms: terms.clone(),
                all: *all,
            };
            score(&expanded, id, source, &stats).is_some()
        }
        _ => score(query, id, source, &stats).is_some(),
    }
}

fn report(took: u64, tally: &Tally, updates: bool) -> String {
    let mut out = Out::default();
    out.begin_object()
        .key("took")
        .integer(took)
        .key("timed_out")
        .boolean(false)
        .key("total")
        .integer(tally.total);
    if updates {
        out.key("updated").integer(tally.updated);
    }
    out.key("deleted")
        .integer(tally.deleted)
        .key("batches")
        .integer(tally.batches)
        .key("version_conflicts")
        .integer(tally.version_conflicts)
        .key("noops")
        .integer(tally.noops)
        .key("retries")
        .raw("{\"bulk\":0,\"search\":0}")
        .key("throttled_millis")
        .integer(0)
        .key("requests_per_second")
        .raw("-1.0")
        .key("throttled_until_millis")
        .integer(0)
        .key("failures")
        .begin_array();
    for failure in &tally.failures {
        out.raw(failure);
    }
    out.end_array().end_object();
    out.finish()
}

impl Api<'_> {
    pub(super) fn update_by_query(&self, index: &str, request: &Request) -> Answer {
        self.by_query_task(index, request, true)
    }

    pub(super) fn delete_by_query(&self, index: &str, request: &Request) -> Answer {
        self.by_query_task(index, request, false)
    }

    fn by_query_task(&self, index: &str, request: &Request, updates: bool) -> Answer {
        if let Some(slices) = query_flag(request, "slices")
            && slices != "auto"
            && !slices.parse::<u32>().is_ok_and(|count| count >= 1)
        {
            return Ok(error(
                400,
                "illegal_argument_exception",
                &format!("Failed to parse int parameter [slices] with value [{slices}]"),
            ));
        }
        if query_flag(request, "wait_for_completion").as_deref() != Some("false") {
            return self.by_query(index, request, updates);
        }
        let started = std::time::Instant::now();
        let response = self.by_query(index, request, updates)?;
        let running_nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let (action, kind) = if updates {
            ("indices:data/write/update/byquery", "update-by-query")
        } else {
            ("indices:data/write/delete/byquery", "delete-by-query")
        };
        Ok(self.keep_finished_task(
            action,
            format!("{kind} [{index}]"),
            running_nanos,
            &response,
        ))
    }

    fn by_query(&self, index: &str, request: &Request, updates: bool) -> Answer {
        let started = std::time::Instant::now();
        let options = match ByQuery::of(request, updates) {
            Ok(options) => options,
            Err(response) => return Ok(response),
        };
        let resolved = match options.script.as_ref().map(|node| self.resolved_script(node)) {
            Some(Ok(resolved)) => Some(resolved),
            Some(Err(refusal)) => return Ok(refusal),
            None => None,
        };
        let script = match &resolved {
            Some(node) => match Script::from_node(node) {
                Ok(script) => Some(script),
                Err(reason) => return Ok(script_error(&format!("compile error: {reason}"))),
            },
            None => None,
        };
        let change = if updates {
            Change::Update(script.as_ref())
        } else {
            Change::Delete
        };
        let ignore_unavailable =
            query_flag(request, "ignore_unavailable").as_deref() == Some("true");
        let resolved = read(self.keyspace, |keys| {
            resolve_indices(keys, &[index], ignore_unavailable)
        })?;
        let targets = match resolved {
            Ok(targets) => targets,
            Err(missing) => return Ok(missing_index(&missing)),
        };
        let mut tally = Tally::default();
        for target in &targets {
            let mut reservation = self.memory_budget.empty_reservation();
            let matched = match self.matched_documents(target, &options, &mut reservation)? {
                Snapshot::Matched(matched) => matched,
                Snapshot::MissingIndex => return Ok(missing_index(target)),
                Snapshot::OverBudget => return Ok(over_memory_budget()),
            };
            let part = self.change_matched(target, &matched, change, &options)?;
            tally.add(&part);
            tally.batches += part.batches;
            tally.failures.extend(part.failures);
            tally.status = tally.status.max(part.status);
        }
        if let Some(max_docs) = options.max_docs {
            tally.total = tally.total.min(max_docs);
        }
        let took = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let status = if tally.failures.is_empty() {
            200
        } else {
            tally.status
        };
        Ok(json(status, report(took, &tally, updates)))
    }

    fn matched_documents(
        &self,
        index: &str,
        options: &ByQuery,
        reservation: &mut Reservation<'_>,
    ) -> Result<Snapshot, String> {
        read(self.keyspace, |keys| {
            let Some(stored) = keys.get(&index_key(index))? else {
                return Ok(Snapshot::MissingIndex);
            };
            let mut types = crate::mappings::FieldTypes::default();
            if let Some(mappings) = String::from_utf8(stored)
                .ok()
                .and_then(|text| parse_document(&text).ok())
            {
                types.add_mappings(&mappings);
            }
            if let Some(settings) = keys
                .get(&settings_key(index))?
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|text| parse_document(&text).ok())
            {
                types.add_settings(&settings);
            }
            let _analyzers = crate::analysis::enter(types.field_analyzers());
            let query = crate::mappings::apply(options.query.clone(), &types, super::now_millis());
            let prefix = document_prefix(index);
            let mut matched = Vec::new();
            let mut held = 0usize;
            let mut over_budget = false;
            keys.visit_prefix(&prefix, &mut |key, value| {
                let (Some(id), Some((version, source))) =
                    (key.get(prefix.len()..), unstored(value))
                else {
                    return Ok(true);
                };
                let id = String::from_utf8_lossy(id).into_owned();
                if options.slice.is_some_and(|slice| !slice.holds(&id)) {
                    return Ok(true);
                }
                let Ok(document) = parse_document(&source) else {
                    return Ok(true);
                };
                if matches(&query, &id, &document) {
                    held = held.saturating_add(id.len() + MATCHED_ENTRY_BYTES);
                    if !reservation.hold_if_large(held) {
                        over_budget = true;
                        return Ok(false);
                    }
                    matched.push(Matched {
                        id,
                        version,
                        fingerprint: fnv1a(source.as_bytes()),
                    });
                }
                Ok(true)
            })?;
            Ok(if over_budget {
                Snapshot::OverBudget
            } else {
                Snapshot::Matched(matched)
            })
        })
    }

    fn change_matched(
        &self,
        index: &str,
        matched: &[Matched],
        change: Change<'_>,
        options: &ByQuery,
    ) -> Result<Tally, String> {
        let mut tally = Tally::default();
        let mut done = 0u64;
        let reached = |done: u64| options.max_docs.is_some_and(|max| done >= max);
        let mut next = 0;
        while let Some(rest) = matched.get(next..).filter(|rest| !rest.is_empty()) {
            if reached(done) {
                break;
            }
            let batch = rest.get(..options.scroll_size).unwrap_or(rest);
            tally.batches += 1;
            let mut step = Tally::default();
            let mut conflicted: Vec<&str> = Vec::new();
            let mut script_failure: Option<(&str, String)> = None;
            let mut batch_done = done;
            let mut consumed = 0;
            let mut written_bytes = 0usize;
            let outcome = write(self.keyspace, |keys| {
                for item in batch {
                    if reached(batch_done) || written_bytes > BATCH_WRITE_BYTES {
                        break;
                    }
                    consumed += 1;
                    let key = document_key(index, &item.id);
                    let current = keys.get(&key)?.as_deref().and_then(unstored).filter(
                        |(version, source)| {
                            *version == item.version && fnv1a(source.as_bytes()) == item.fingerprint
                        },
                    );
                    let Some((version, source)) = current else {
                        step.version_conflicts += 1;
                        step.total += 1;
                        conflicted.push(item.id.as_str());
                        continue;
                    };
                    let operation = match change {
                        Change::Delete => Operation::Delete,
                        Change::Update(None) => Operation::Index,
                        Change::Update(Some(script)) => {
                            match scripted(script, index, &item.id, version, &source) {
                                Ok((Operation::Index, changed)) => {
                                    written_bytes = written_bytes.saturating_add(changed.len());
                                    keys.put(&key, &stored(version + 1, &changed))?;
                                    step.updated += 1;
                                    step.total += 1;
                                    batch_done += 1;
                                    continue;
                                }
                                Ok((operation, _)) => operation,
                                Err(reason) => {
                                    script_failure = Some((item.id.as_str(), reason));
                                    return Err("the script failed".to_owned());
                                }
                            }
                        }
                    };
                    match operation {
                        Operation::Index | Operation::Create => {
                            written_bytes = written_bytes.saturating_add(source.len());
                            keys.put(&key, &stored(version + 1, &source))?;
                            step.updated += 1;
                        }
                        Operation::Noop => step.noops += 1,
                        Operation::Delete => {
                            keys.delete(&key)?;
                            step.deleted += 1;
                        }
                    }
                    step.total += 1;
                    batch_done += 1;
                }
                Ok(())
            });
            if let Some((id, reason)) = script_failure {
                tally.fail(
                    index,
                    id,
                    "script_exception",
                    &format!("runtime error: {reason}"),
                    400,
                );
                return Ok(tally);
            }
            outcome?;
            tally.add(&step);
            done = batch_done;
            next += consumed.max(1);
            if options.conflicts == Conflicts::Abort && !conflicted.is_empty() {
                for id in conflicted {
                    tally.fail(
                        index,
                        id,
                        "version_conflict_engine_exception",
                        &format!(
                            "[{id}]: version conflict, the document changed or was deleted after the query read it"
                        ),
                        409,
                    );
                }
                return Ok(tally);
            }
        }
        Ok(tally)
    }
}

#[cfg(test)]
mod tests {
    use super::{ByQuery, Change, Conflicts, Snapshot};
    use crate::tests::{api, request};
    use eggdb_keyspace::MemoryKeyspace;

    #[test]
    fn a_document_changed_after_the_query_read_it_is_a_version_conflict() {
        let keyspace = MemoryKeyspace::default();
        let api = api(&keyspace);
        for id in ["1", "2", "3"] {
            let response = api.answer(&request(
                "PUT",
                &format!("/logs/_doc/{id}"),
                "{\"level\":\"debug\"}",
            ));
            assert_eq!(response.status, 201);
        }
        let mut options = ByQuery::of(&request("POST", "/logs/_delete_by_query", "{}"), false)
            .expect("valid options");
        let mut reservation = api.memory_budget.empty_reservation();
        let Ok(Snapshot::Matched(matched)) =
            api.matched_documents("logs", &options, &mut reservation)
        else {
            panic!("the snapshot reads the index");
        };
        assert_eq!(matched.len(), 3);
        let changed = api.answer(&request("PUT", "/logs/_doc/2", "{\"level\":\"error\"}"));
        assert_eq!(changed.status, 200);
        options.conflicts = Conflicts::Proceed;
        options.scroll_size = 1;
        let tally = api
            .change_matched("logs", &matched, Change::Delete, &options)
            .expect("the deletes run");
        assert_eq!(
            (
                tally.total,
                tally.deleted,
                tally.version_conflicts,
                tally.batches
            ),
            (3, 2, 1, 3)
        );
        assert!(tally.failures.is_empty());
        let kept = api.answer(&request("GET", "/logs/_doc/2", ""));
        assert_eq!(kept.status, 200);

        for id in ["1", "3"] {
            let response = api.answer(&request(
                "PUT",
                &format!("/logs/_doc/{id}"),
                "{\"level\":\"debug\"}",
            ));
            assert_eq!(response.status, 201);
        }
        let Ok(Snapshot::Matched(matched)) =
            api.matched_documents("logs", &options, &mut reservation)
        else {
            panic!("the snapshot reads the index");
        };
        let changed = api.answer(&request("DELETE", "/logs/_doc/1", ""));
        assert_eq!(changed.status, 200);
        options.conflicts = Conflicts::Abort;
        let tally = api
            .change_matched("logs", &matched, Change::Update(None), &options)
            .expect("the updates run");
        assert_eq!(
            (tally.updated, tally.version_conflicts, tally.batches),
            (0, 1, 1)
        );
        assert_eq!(tally.status, 409);
        assert!(tally.failures[0].contains("version_conflict_engine_exception"));
    }

    #[test]
    fn a_document_deleted_and_created_again_is_a_version_conflict() {
        let keyspace = MemoryKeyspace::default();
        let api = api(&keyspace);
        for id in ["1", "2"] {
            let response = api.answer(&request(
                "PUT",
                &format!("/logs/_doc/{id}"),
                "{\"level\":\"debug\"}",
            ));
            assert_eq!(response.status, 201);
        }
        let mut options = ByQuery::of(&request("POST", "/logs/_delete_by_query", "{}"), false)
            .expect("valid options");
        options.conflicts = Conflicts::Proceed;
        let mut reservation = api.memory_budget.empty_reservation();
        let Ok(Snapshot::Matched(matched)) =
            api.matched_documents("logs", &options, &mut reservation)
        else {
            panic!("the snapshot reads the index");
        };
        assert_eq!(
            api.answer(&request("DELETE", "/logs/_doc/2", "")).status,
            200
        );
        let recreated = api.answer(&request("PUT", "/logs/_doc/2", "{\"level\":\"keep\"}"));
        assert_eq!(recreated.status, 201);
        let tally = api
            .change_matched("logs", &matched, Change::Delete, &options)
            .expect("the deletes run");
        assert_eq!(
            (tally.total, tally.deleted, tally.version_conflicts),
            (2, 1, 1)
        );
        assert_eq!(api.answer(&request("GET", "/logs/_doc/2", "")).status, 200);
    }
}
