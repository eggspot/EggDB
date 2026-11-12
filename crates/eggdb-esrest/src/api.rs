mod aliases;
mod by_query;
mod multi;
mod pit;
mod scroll;
mod stored_scripts;
mod tasks;
mod templates;

use crate::json_out::{Out, escaped, written};
use crate::query::{Stats, parse, score};
use crate::script::{Context, Operation, Script};
use crate::sort::{Sort, SortValue, write_values};
use crate::source_filter::SourceFilter;
use eggdb_http::{Reply, Request, Response, StreamedResponse};
use std::io::Write;
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};
use eggdb_platform::connection_limits::MemoryBudget;
use eggdb_sql::json::{Json, JsonSyntaxError, Node, parse_document};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const INDEX_RECORD: u8 = 1;
const DOCUMENT_RECORD: u8 = 2;
const SETTINGS_RECORD: u8 = 8;
const DEFAULT_SIZE: usize = 10;
const MAX_RESULT_WINDOW: usize = 10_000;
const VERSION: &str = "2.11.0";

/// What the REST API needs to answer requests.
pub struct Api<'k> {
    keyspace: &'k (dyn Keyspace + Sync),
    user: String,
    password_sha256: [u8; 32],
    next_id: AtomicU64,
    memory_budget: Arc<MemoryBudget>,
    scrolls: scroll::Scrolls,
    pits: pit::Pits,
    tasks: tasks::Tasks,
}

impl std::fmt::Debug for Api<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// Why a request failed: the request itself, or the storage under it.
#[derive(Debug)]
enum Failure {
    BadRequest(String),
    Storage(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Storage(message)
    }
}

impl From<JsonSyntaxError> for Failure {
    fn from(error: JsonSyntaxError) -> Self {
        Self::BadRequest(format!(
            "failed to parse the request body: {} at byte {}",
            error.message, error.position
        ))
    }
}

type Answer = Result<Response, Failure>;

/// The answer to a request the server's memory budget cannot take: `429` with a
/// `circuit_breaking_exception`, as Elasticsearch's parent circuit breaker answers, so clients
/// back off and retry.
pub fn over_memory_budget() -> Response {
    error(
        429,
        "circuit_breaking_exception",
        "[parent] the server's connections already hold their memory budget",
    )
}

fn error_cause(kind: &str, reason: &str) -> String {
    let mut out = Out::default();
    out.begin_object()
        .key("root_cause")
        .begin_array()
        .begin_object()
        .key("type")
        .string(kind)
        .key("reason")
        .string(reason)
        .end_object()
        .end_array()
        .key("type")
        .string(kind)
        .key("reason")
        .string(reason)
        .end_object();
    out.finish()
}

fn error(status: u16, kind: &str, reason: &str) -> Response {
    json(
        status,
        format!(
            "{{\"error\":{},\"status\":{status}}}",
            error_cause(kind, reason)
        ),
    )
}

fn validation(problems: &[String]) -> Response {
    let listed: String = problems
        .iter()
        .enumerate()
        .map(|(at, problem)| format!("{}: {problem};", at + 1))
        .collect();
    error(
        400,
        "action_request_validation_exception",
        &format!("Validation Failed: {listed}"),
    )
}

fn member_of(node: &Node, name: &str) -> Option<Node> {
    match &node.value {
        Json::Object(members) => members
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone()),
        _ => None,
    }
}

fn json(status: u16, body: String) -> Response {
    Response {
        status,
        content_type: "application/json; charset=UTF-8".to_owned(),
        body: body.into_bytes(),
        headers: Vec::new(),
    }
}

fn index_key(index: &str) -> Vec<u8> {
    let mut key = vec![INDEX_RECORD];
    key.extend_from_slice(index.as_bytes());
    key
}

fn document_prefix(index: &str) -> Vec<u8> {
    let mut key = vec![DOCUMENT_RECORD];
    key.extend_from_slice(index.as_bytes());
    key.push(0);
    key
}

fn settings_key(index: &str) -> Vec<u8> {
    let mut key = vec![SETTINGS_RECORD];
    key.extend_from_slice(index.as_bytes());
    key
}

fn document_key(index: &str, id: &str) -> Vec<u8> {
    let mut key = document_prefix(index);
    key.extend_from_slice(id.as_bytes());
    key
}

fn stored(version: u64, source: &str) -> Vec<u8> {
    let mut bytes = version.to_le_bytes().to_vec();
    bytes.extend_from_slice(source.as_bytes());
    bytes
}

fn unstored(bytes: &[u8]) -> Option<(u64, String)> {
    let version = u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
    Some((version, String::from_utf8(bytes[8..].to_vec()).ok()?))
}

fn valid_index(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.starts_with(['-', '_', '+'])
        && name.chars().all(|character| {
            !character.is_uppercase()
                && !"\\/*?\"<>| ,#:".contains(character)
                && !character.is_control()
        })
}

#[derive(Debug, Clone, Copy)]
struct ExpectedSequence {
    seq_no: i64,
    primary_term: i64,
}

fn expected_sequence(request: &Request) -> Result<Option<ExpectedSequence>, Response> {
    let seq_no = query_flag(request, "if_seq_no");
    let primary_term = query_flag(request, "if_primary_term");
    if let Some(retries) = query_flag(request, "retry_on_conflict")
        && retries.parse::<u32>().is_err()
    {
        return Err(error(
            400,
            "illegal_argument_exception",
            &format!("Failed to parse int parameter [retry_on_conflict] with value [{retries}]"),
        ));
    }
    let parsed = |name: &str, text: &str| {
        text.parse::<i64>().map_err(|_| {
            error(
                400,
                "illegal_argument_exception",
                &format!("Failed to parse long parameter [{name}] with value [{text}]"),
            )
        })
    };
    match (seq_no, primary_term) {
        (None, None) => Ok(None),
        (Some(seq_no), Some(primary_term)) => Ok(Some(ExpectedSequence {
            seq_no: parsed("if_seq_no", &seq_no)?,
            primary_term: parsed("if_primary_term", &primary_term)?,
        })),
        (Some(_), None) => Err(error(
            400,
            "action_request_validation_exception",
            "Validation Failed: 1: ifSeqNo is set, but primary term is [0];",
        )),
        (None, Some(_)) => Err(error(
            400,
            "action_request_validation_exception",
            "Validation Failed: 1: ifSeqNo is unassigned, but primary term is set;",
        )),
    }
}

fn sequence_conflict(id: &str, expected: ExpectedSequence, current: Option<u64>) -> Option<Response> {
    let required = format!(
        "required seqNo [{}], primary term [{}]",
        expected.seq_no, expected.primary_term
    );
    match current {
        None => Some(error(
            409,
            "version_conflict_engine_exception",
            &format!("[{id}]: version conflict, {required} but no document was found"),
        )),
        Some(version) => {
            let seq_no = i64::try_from(version.saturating_sub(1)).unwrap_or(i64::MAX);
            (seq_no != expected.seq_no || expected.primary_term != 1).then(|| {
                error(
                    409,
                    "version_conflict_engine_exception",
                    &format!(
                        "[{id}]: version conflict, {required}. current document has seqNo [{seq_no}] and primary term [1]"
                    ),
                )
            })
        }
    }
}

fn query_flag(request: &Request, name: &str) -> Option<String> {
    request.query.as_deref()?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then(|| {
            let spaced = value.replace('+', " ");
            eggdb_http::percent_decoded(&spaced).unwrap_or(spaced)
        })
    })
}

fn read<T>(
    keyspace: &dyn Keyspace,
    body: impl FnOnce(&dyn KeysRead) -> Result<T, String>,
) -> Result<T, String> {
    let mut body = Some(body);
    let mut out = None;
    keyspace.read(0, &mut |keys| {
        if let Some(body) = body.take() {
            out = Some(body(keys)?);
        }
        Ok(())
    })?;
    out.ok_or_else(|| "the read did not run".to_owned())
}

fn write<T>(
    keyspace: &dyn Keyspace,
    body: impl FnOnce(&mut dyn KeysWrite) -> Result<T, String>,
) -> Result<T, String> {
    let mut body = Some(body);
    let mut out = None;
    keyspace.write(0, &mut |keys| {
        if let Some(body) = body.take() {
            out = Some(body(keys)?);
        }
        Ok(())
    })?;
    out.ok_or_else(|| "the write did not run".to_owned())
}

fn documents_of(keys: &dyn KeysRead, index: &str) -> Result<Vec<(String, u64, String)>, String> {
    let prefix = document_prefix(index);
    Ok(keys
        .scan(&prefix)?
        .into_iter()
        .filter_map(|(key, value)| {
            let (version, source) = unstored(&value)?;
            Some((
                String::from_utf8_lossy(&key[prefix.len()..]).into_owned(),
                version,
                source,
            ))
        })
        .collect())
}

fn indices_of(keys: &dyn KeysRead) -> Result<Vec<String>, String> {
    Ok(keys
        .scan(&[INDEX_RECORD])?
        .into_iter()
        .map(|(key, _)| String::from_utf8_lossy(&key[1..]).into_owned())
        .collect())
}

fn index_glob(pattern: &str, name: &str) -> bool {
    let pieces: Vec<&str> = pattern.split('*').collect();
    let [first, middle @ .., last] = pieces.as_slice() else {
        return pattern == name;
    };
    if !name.starts_with(first) {
        return false;
    }
    let mut rest = &name[first.len()..];
    for piece in middle {
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

pub(super) fn resolve_indices(
    keys: &dyn KeysRead,
    written: &[&str],
    ignore_unavailable: bool,
) -> Result<Result<Vec<String>, String>, String> {
    let mut resolved: Vec<String> = Vec::new();
    let mut every: Option<Vec<String>> = None;
    for part in written.iter().flat_map(|names| names.split(',')) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(excluded) = part.strip_prefix('-')
            && !resolved.is_empty()
        {
            resolved.retain(|name| !index_glob(excluded, name));
            continue;
        }
        if part == "_all" || part.contains('*') {
            if every.is_none() {
                every = Some(indices_of(keys)?);
            }
            for name in every.iter().flatten() {
                let pattern = if part == "_all" { "*" } else { part };
                if index_glob(pattern, name) && !resolved.contains(name) {
                    resolved.push(name.clone());
                }
            }
            continue;
        }
        if keys.get(&index_key(part))?.is_some() {
            if !resolved.iter().any(|name| name == part) {
                resolved.push(part.to_owned());
            }
            continue;
        }
        let aliased = aliases::indices_of_alias(keys, part)?;
        if !aliased.is_empty() {
            for name in aliased {
                if !resolved.contains(&name) {
                    resolved.push(name);
                }
            }
        } else if !ignore_unavailable {
            return Ok(Err(part.to_owned()));
        }
    }
    Ok(Ok(resolved))
}

fn ensure_index(keys: &mut dyn KeysWrite, index: &str) -> Result<(), String> {
    if keys.get(&index_key(index))?.is_none() {
        let applied = templates::template_for(keys, index)?;
        keys.put(&index_key(index), templates::mappings_text(&applied, None).as_bytes())?;
        for alias in &applied.aliases {
            aliases::add_alias(keys, alias, index)?;
        }
    }
    Ok(())
}

impl<'k> Api<'k> {
    /// An API storing indices in `keyspace` for the user `user` with the password whose SHA-256
    /// is `password_sha256` (HTTP basic authentication).
    pub fn new(keyspace: &'k (dyn Keyspace + Sync), user: &str, password_sha256: [u8; 32]) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |since| since.as_nanos() as u64);
        Self {
            keyspace,
            user: user.to_owned(),
            password_sha256,
            next_id: AtomicU64::new(seed),
            memory_budget: Arc::new(MemoryBudget::default()),
            scrolls: scroll::Scrolls::default(),
            pits: pit::Pits::default(),
            tasks: tasks::Tasks::default(),
        }
    }

    /// The same API holding what `_mget`, `_msearch`, `_update_by_query` and
    /// `_delete_by_query` gather against `budget`, the server's one connection memory budget
    /// (D307), instead of a budget of its own.
    pub fn with_memory_budget(mut self, budget: Arc<MemoryBudget>) -> Self {
        self.memory_budget = budget;
        self
    }

    fn authorized(&self, request: &Request) -> bool {
        let Some(encoded) = request
            .header("authorization")
            .and_then(|value| value.strip_prefix("Basic "))
        else {
            return false;
        };
        let Ok(decoded) = eggdb_crypto::base64_decode(encoded.trim().as_bytes()) else {
            return false;
        };
        let Ok(text) = String::from_utf8(decoded) else {
            return false;
        };
        let Some((user, password)) = text.split_once(':') else {
            return false;
        };
        user == self.user
            && eggdb_crypto::constant_time_eq(
                &eggdb_crypto::sha256(password.as_bytes()),
                &self.password_sha256,
            )
    }

    fn new_id(&self) -> String {
        let mut value = self
            .next_id
            .fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut id = String::with_capacity(20);
        for _ in 0..20 {
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            id.push(char::from(ALPHABET[(value % 64) as usize]));
        }
        id
    }

    /// Answers one request; the hits of a `_search` are a [`StreamedResponse`], each hit
    /// rendered as it is sent.
    pub fn reply(&self, request: &Request) -> Reply<'static> {
        let searched = matches!(request.method.as_str(), "GET" | "POST")
            && self.authorized(request)
            && !request.path.contains('%')
            && query_flag(request, "scroll").is_none();
        if searched {
            let parts: Vec<&str> = request.path.split('/').filter(|part| !part.is_empty()).collect();
            let indices = match parts.as_slice() {
                ["_search"] => Some(None),
                [index, "_search"] if valid_index(index) => Some(Some(*index)),
                _ => None,
            };
            if let Some(index) = indices {
                let holder: Vec<&str> = index.into_iter().collect();
                let named = index.map(|_| holder.as_slice());
                return match self.search_page(named, request, false, false) {
                    Ok(SearchOutcome::Page(page)) => Reply::Streamed(page.stream()),
                    Ok(SearchOutcome::Whole(response)) => Reply::Whole(response),
                    Err(Failure::BadRequest(reason)) => {
                        Reply::Whole(error(400, "parse_exception", &reason))
                    }
                    Err(Failure::Storage(reason)) => {
                        Reply::Whole(error(500, "eggdb_exception", &reason))
                    }
                };
            }
        }
        Reply::Whole(self.answer(request))
    }

    /// Answers one request with its whole response in memory.
    pub fn answer(&self, request: &Request) -> Response {
        if !self.authorized(request) {
            return error(
                401,
                "security_exception",
                "missing authentication credentials for REST request",
            )
            .with_header("WWW-Authenticate", "Basic realm=\"eggdb\"");
        }
        let Some(decoded) = request
            .path
            .split('/')
            .filter(|part| !part.is_empty())
            .map(eggdb_http::percent_decoded)
            .collect::<Option<Vec<String>>>()
        else {
            return error(
                400,
                "illegal_argument_exception",
                "the request path has an escape that is not two hex digits or not UTF-8",
            );
        };
        let parts: Vec<&str> = decoded.iter().map(String::as_str).collect();
        let method = request.method.as_str();
        let outcome = match (method, parts.as_slice()) {
            ("GET" | "HEAD", []) => Ok(self.root()),
            ("GET", ["_cluster", "health"]) => Ok(json(200, "{\"cluster_name\":\"eggdb\",\"status\":\"green\",\"timed_out\":false,\"number_of_nodes\":1,\"number_of_data_nodes\":1,\"active_primary_shards\":0,\"active_shards\":0,\"relocating_shards\":0,\"initializing_shards\":0,\"unassigned_shards\":0}".to_owned())),
            ("GET", ["_cat", "indices"]) => self.cat_indices(),
            ("GET", ["_cat", "aliases"]) => self.cat_aliases(),
            ("GET", ["_cat", "count"]) => self.cat_count(None),
            ("GET", ["_cat", "count", index]) => self.cat_count(Some(index)),
            ("GET", ["_cat", "health"]) => Ok(cat_health()),
            ("POST", ["_aliases"]) => self.update_aliases(request),
            ("GET", ["_tasks"]) => self.list_tasks(),
            ("GET", ["_tasks", id]) => self.get_task(id),
            ("POST", ["_tasks", id, "_cancel"]) => self.settle_task(id),
            ("POST", ["_update_by_query" | "_delete_by_query" | "_reindex", id, "_rethrottle"]) => {
                self.settle_task(id)
            }
            ("GET", ["_alias"] | ["_aliases"]) => self.get_aliases(None, None, false),
            ("GET" | "HEAD", ["_alias", alias]) => {
                self.get_aliases(None, Some(alias), method == "HEAD")
            }
            ("GET", [index, "_alias"] | [index, "_aliases"]) if valid_index(index) || index.contains('*') || *index == "_all" => {
                self.get_aliases(Some(index), None, false)
            }
            ("GET" | "HEAD", [index, "_alias", alias]) if valid_index(index) || index.contains('*') || *index == "_all" => {
                self.get_aliases(Some(index), Some(alias), method == "HEAD")
            }
            ("PUT" | "POST", [index, "_alias" | "_aliases", alias]) if valid_index(index) || index.contains('*') => {
                self.put_alias(index, alias)
            }
            ("DELETE", [index, "_alias" | "_aliases", alias]) if valid_index(index) || index.contains('*') => {
                self.delete_alias(index, alias)
            }
            ("POST", ["_bulk"]) => self.bulk(None, request),
            ("GET" | "POST", ["_search", "template"]) => self.search_template(None, request),
            ("PUT" | "POST", ["_index_template", name]) => {
                self.put_template(templates::TemplateKind::Index, name, request)
            }
            ("PUT" | "POST", ["_template", name]) => {
                self.put_template(templates::TemplateKind::Legacy, name, request)
            }
            ("GET", ["_template"]) => self.get_templates(templates::TemplateKind::Legacy, None, false),
            ("GET" | "HEAD", ["_template", name]) => {
                self.get_templates(templates::TemplateKind::Legacy, Some(name), method == "HEAD")
            }
            ("DELETE", ["_template", name]) => self.delete_template(templates::TemplateKind::Legacy, name),
            ("GET", ["_index_template"]) => self.get_templates(templates::TemplateKind::Index, None, false),
            ("GET" | "HEAD", ["_index_template", name]) => {
                self.get_templates(templates::TemplateKind::Index, Some(name), method == "HEAD")
            }
            ("DELETE", ["_index_template", name]) => self.delete_template(templates::TemplateKind::Index, name),
            ("PUT" | "POST", ["_component_template", name]) => {
                self.put_template(templates::TemplateKind::Component, name, request)
            }
            ("GET", ["_component_template"]) => {
                self.get_templates(templates::TemplateKind::Component, None, false)
            }
            ("GET" | "HEAD", ["_component_template", name]) => {
                self.get_templates(templates::TemplateKind::Component, Some(name), method == "HEAD")
            }
            ("DELETE", ["_component_template", name]) => {
                self.delete_template(templates::TemplateKind::Component, name)
            }
            ("PUT" | "POST", ["_scripts", id]) => self.put_stored_script(id, request),
            ("PUT" | "POST", ["_scripts", id, _context]) => self.put_stored_script(id, request),
            ("GET", ["_scripts", id]) => self.get_stored_script(id),
            ("DELETE", ["_scripts", id]) => self.delete_stored_script(id),
            ("GET" | "POST", ["_search", "scroll"]) => self.continue_scroll(request, None),
            ("GET" | "POST", ["_search", "scroll", id]) => self.continue_scroll(request, Some(id)),
            ("DELETE", ["_search", "scroll"]) => self.clear_scroll(request, None),
            ("DELETE", ["_pit"]) => self.close_pit(request),
            ("POST", [index, "_pit"]) if valid_index(index) => self.open_pit(index, request),
            ("DELETE", ["_search", "scroll", id]) => self.clear_scroll(request, Some(id)),
            ("GET" | "POST", ["_search"]) if query_flag(request, "scroll").is_some() => {
                self.open_scroll(None, request)
            }
            ("GET" | "POST", [index, "_search"])
                if valid_index(index) && query_flag(request, "scroll").is_some() =>
            {
                self.open_scroll(Some(std::slice::from_ref(index)), request)
            }
            ("GET" | "POST", ["_render", "template"]) => self.render_template(request),
            ("GET" | "POST", [index, "_search", "template"]) => {
                self.search_template(Some(std::slice::from_ref(index)), request)
            }
            ("GET" | "POST", ["_search"]) => self.search(None, request, false),
            ("GET" | "POST", ["_count"]) => self.search(None, request, true),
            ("GET" | "POST", ["_mget"]) => self.mget(None, request),
            ("GET" | "POST", ["_msearch"]) => self.msearch(None, request),
            ("GET" | "POST", ["_msearch", "template"]) => self.msearch_template(None, request),
            ("GET" | "POST", [index, "_msearch", "template"]) => {
                self.msearch_template(Some(index), request)
            }
            ("POST", ["_refresh"]) => Ok(refreshed()),
            (_, [index, ..]) if !valid_index(index) => {
                Ok(error(400, "invalid_index_name_exception", &format!("Invalid index name [{index}]")))
            }
            ("PUT", [index]) => self.create_index(index, request),
            ("DELETE", [index]) => self.delete_index(index),
            ("HEAD", [index]) => self.index_exists(index),
            ("GET", [index]) => self.get_index(index),
            ("POST", [_, "_refresh"]) => Ok(refreshed()),
            ("POST", [index, "_bulk"]) => self.bulk(Some(index), request),
            ("GET" | "POST", [index, "_search"]) => {
                self.search(Some(std::slice::from_ref(index)), request, false)
            }
            ("GET" | "POST", [index, "_count"]) => {
                self.search(Some(std::slice::from_ref(index)), request, true)
            }
            ("GET" | "POST", [index, "_mget"]) => self.mget(Some(index), request),
            ("GET" | "POST", [index, "_msearch"]) => self.msearch(Some(index), request),
            ("POST", [index, "_update_by_query"]) => self.update_by_query(index, request),
            ("POST", [index, "_delete_by_query"]) => self.delete_by_query(index, request),
            ("POST", [index, "_doc"]) => {
                let id = self.new_id();
                self.index_document(index, &id, request, false)
            }
            ("PUT" | "POST", [index, "_doc", id]) => self.index_document(index, id, request, false),
            ("PUT" | "POST", [index, "_create", id]) => self.index_document(index, id, request, true),
            ("GET" | "HEAD", [index, "_doc", id]) => {
                self.get_document(index, id, request, method == "HEAD")
            }
            ("GET" | "HEAD", [index, "_source", id]) => {
                self.get_source(index, id, request, method == "HEAD")
            }
            ("DELETE", [index, "_doc", id]) => self.delete_document(index, id, request),
            ("POST", [index, "_update", id]) => self.update_document(index, id, request),
            _ => Ok(error(405, "method_not_allowed", &format!("Incorrect HTTP method for uri [{}] and method [{method}]", request.path))),
        };
        outcome.unwrap_or_else(|failure| match failure {
            Failure::BadRequest(reason) => error(400, "parse_exception", &reason),
            Failure::Storage(reason) => error(500, "eggdb_exception", &reason),
        })
    }

    fn root(&self) -> Response {
        json(
            200,
            format!(
                "{{\"name\":\"eggdb\",\"cluster_name\":\"eggdb\",\"cluster_uuid\":\"eggdb\",\"version\":{{\"distribution\":\"eggdb\",\"number\":\"{VERSION}\",\"minimum_wire_compatibility_version\":\"7.10.0\",\"minimum_index_compatibility_version\":\"7.0.0\"}},\"tagline\":\"EggDB, compatible with the Elasticsearch and OpenSearch REST APIs\"}}"
            ),
        )
    }

    fn cat_count(&self, index: Option<&str>) -> Answer {
        let counted = read(self.keyspace, |keys| {
            let searched = match index {
                Some(named) => match resolve_indices(keys, &[named], false)? {
                    Ok(searched) => searched,
                    Err(missing) => return Ok(Err(missing)),
                },
                None => indices_of(keys)?,
            };
            let mut total = 0usize;
            for name in searched {
                total += documents_of(keys, &name)?.len();
            }
            Ok(Ok(total))
        })?;
        let total = match counted {
            Ok(total) => total,
            Err(missing) => return Ok(missing_index(&missing)),
        };
        let (epoch, clock) = epoch_and_clock();
        Ok(json(
            200,
            format!(
                "[{{\"epoch\":\"{epoch}\",\"timestamp\":\"{clock}\",\"count\":\"{total}\"}}]"
            ),
        ))
    }

    fn cat_indices(&self) -> Answer {
        let rows = read(self.keyspace, |keys| {
            let mut rows = Vec::new();
            for index in indices_of(keys)? {
                let count = documents_of(keys, &index)?.len();
                rows.push((index, count));
            }
            Ok(rows)
        })?;
        let mut out = Out::default();
        out.begin_array();
        for (index, count) in rows {
            out.begin_object()
                .key("health")
                .string("green")
                .key("status")
                .string("open")
                .key("index")
                .string(&index)
                .key("docs.count")
                .string(&count.to_string())
                .end_object();
        }
        out.end_array();
        Ok(json(200, out.finish()))
    }

    fn create_index(&self, index: &str, request: &Request) -> Answer {
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let parsed = if body.trim().is_empty() {
            None
        } else {
            Some(parse_document(&body)?)
        };
        let requested_mappings = parsed.as_ref().and_then(|parsed| member_of(parsed, "mappings"));
        let requested_settings: Option<String> = parsed
            .as_ref()
            .and_then(|parsed| member_of(parsed, "settings"))
            .and_then(|settings| body.get(settings.start..settings.end).map(str::to_owned));
        let requested_aliases: Vec<String> = parsed
            .as_ref()
            .and_then(|parsed| member_of(parsed, "aliases"))
            .and_then(|node| node.value.into_object())
            .map(|members| members.into_iter().map(|(name, _)| name).collect())
            .unwrap_or_default();
        let created = write(self.keyspace, |keys| {
            if keys.get(&index_key(index))?.is_some() {
                return Ok(false);
            }
            let applied = templates::template_for(keys, index)?;
            let mappings = templates::mappings_text(&applied, requested_mappings.as_ref());
            keys.put(&index_key(index), mappings.as_bytes())?;
            if let Some(settings) = &requested_settings {
                keys.put(&settings_key(index), settings.as_bytes())?;
            }
            for alias in applied.aliases.iter().chain(&requested_aliases) {
                aliases::add_alias(keys, alias, index)?;
            }
            Ok(true)
        })?;
        if !created {
            return Ok(error(
                400,
                "resource_already_exists_exception",
                &format!("index [{index}] already exists"),
            ));
        }
        Ok(json(
            200,
            format!(
                "{{\"acknowledged\":true,\"shards_acknowledged\":true,\"index\":{}}}",
                escaped(index)
            ),
        ))
    }

    fn delete_index(&self, index: &str) -> Answer {
        let deleted = write(self.keyspace, |keys| {
            if keys.get(&index_key(index))?.is_none() {
                return Ok(false);
            }
            for (id, _, _) in documents_of(keys, index)? {
                keys.delete(&document_key(index, &id))?;
            }
            keys.delete(&index_key(index))?;
            keys.delete(&settings_key(index))?;
            aliases::forget_index(keys, index)?;
            Ok(true)
        })?;
        Ok(if deleted {
            json(200, "{\"acknowledged\":true}".to_owned())
        } else {
            missing_index(index)
        })
    }

    fn index_exists(&self, index: &str) -> Answer {
        let exists = read(self.keyspace, |keys| {
            Ok(keys.get(&index_key(index))?.is_some())
        })?;
        Ok(json(if exists { 200 } else { 404 }, String::new()))
    }

    fn get_index(&self, index: &str) -> Answer {
        let mappings = read(self.keyspace, |keys| keys.get(&index_key(index)))?;
        let Some(mappings) = mappings else {
            return Ok(missing_index(index));
        };
        Ok(json(
            200,
            format!(
                "{{{}:{{\"aliases\":{{}},\"mappings\":{},\"settings\":{{\"index\":{{\"number_of_shards\":\"1\",\"number_of_replicas\":\"0\"}}}}}}}}",
                escaped(index),
                String::from_utf8_lossy(&mappings)
            ),
        ))
    }

    fn index_document(
        &self,
        index: &str,
        id: &str,
        request: &Request,
        create_only: bool,
    ) -> Answer {
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let parsed = match parse_document(&body) {
            Ok(parsed) if matches!(parsed.value, Json::Object(_)) => parsed,
            _ => {
                return Ok(error(
                    400,
                    "mapper_parsing_exception",
                    "failed to parse: the document is not a JSON object",
                ));
            }
        };
        let source = written(&parsed);
        let expected = match expected_sequence(request) {
            Ok(expected) => expected,
            Err(refusal) => return Ok(refusal),
        };
        let mut conflict = None;
        let outcome = write(self.keyspace, |keys| {
            ensure_index(keys, index)?;
            let key = document_key(index, id);
            let previous = keys
                .get(&key)?
                .as_deref()
                .and_then(unstored)
                .map(|(version, _)| version);
            if let Some(expected) = expected
                && let Some(refusal) = sequence_conflict(id, expected, previous)
            {
                conflict = Some(refusal);
                return Ok(None);
            }
            if create_only && previous.is_some() {
                return Ok(None);
            }
            let version = previous.map_or(1, |version| version + 1);
            keys.put(&key, &stored(version, &source))?;
            Ok(Some((version, previous.is_none())))
        })?;
        if let Some(refusal) = conflict {
            return Ok(refusal);
        }
        let Some((version, created)) = outcome else {
            return Ok(error(
                409,
                "version_conflict_engine_exception",
                &format!("[{id}]: version conflict, document already exists"),
            ));
        };
        Ok(json(
            if created { 201 } else { 200 },
            write_result(
                index,
                id,
                version,
                if created { "created" } else { "updated" },
            ),
        ))
    }

    fn get_document(&self, index: &str, id: &str, request: &Request, head: bool) -> Answer {
        let found = read(self.keyspace, |keys| {
            Ok(keys
                .get(&document_key(index, id))?
                .as_deref()
                .and_then(unstored))
        })?;
        let filter = multi::flag_filter(request);
        Ok(match found {
            Some(_) if head => json(200, String::new()),
            Some((version, source)) => {
                let shown = filter
                    .apply(&source)
                    .map(|filtered| format!(",\"_source\":{filtered}"))
                    .unwrap_or_default();
                json(
                    200,
                    format!(
                        "{{\"_index\":{},\"_id\":{},\"_version\":{version},\"_seq_no\":{},\"_primary_term\":1,\"found\":true{shown}}}",
                        escaped(index),
                        escaped(id),
                        version.saturating_sub(1)
                    ),
                )
            }
            None => json(
                404,
                format!(
                    "{{\"_index\":{},\"_id\":{},\"found\":false}}",
                    escaped(index),
                    escaped(id)
                ),
            ),
        })
    }

    fn get_source(&self, index: &str, id: &str, request: &Request, head: bool) -> Answer {
        let found = read(self.keyspace, |keys| {
            Ok(keys
                .get(&document_key(index, id))?
                .as_deref()
                .and_then(unstored))
        })?;
        let filter = multi::flag_filter(request);
        Ok(match found {
            Some(_) if head => json(200, String::new()),
            Some((_, source)) => json(200, filter.apply(&source).unwrap_or_else(|| "{}".to_owned())),
            None => error(
                404,
                "resource_not_found_exception",
                &format!("Document not found [{index}]/[{id}]"),
            ),
        })
    }

    fn delete_document(&self, index: &str, id: &str, request: &Request) -> Answer {
        let expected = match expected_sequence(request) {
            Ok(expected) => expected,
            Err(refusal) => return Ok(refusal),
        };
        let mut conflict = None;
        let removed = write(self.keyspace, |keys| {
            let key = document_key(index, id);
            let version = keys
                .get(&key)?
                .as_deref()
                .and_then(unstored)
                .map(|(version, _)| version);
            if let Some(expected) = expected
                && let Some(refusal) = sequence_conflict(id, expected, version)
            {
                conflict = Some(refusal);
                return Ok(None);
            }
            if version.is_some() {
                keys.delete(&key)?;
            }
            Ok(version)
        })?;
        if let Some(refusal) = conflict {
            return Ok(refusal);
        }
        Ok(match removed {
            Some(version) => json(200, write_result(index, id, version + 1, "deleted")),
            None => json(404, write_result(index, id, 1, "not_found")),
        })
    }

    fn update_document(&self, index: &str, id: &str, request: &Request) -> Answer {
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let mut parsed = parse_document(&body)?;
        if let Json::Object(members) = &mut parsed.value {
            for (key, value) in members.iter_mut() {
                if *key == "script" {
                    match self.resolved_script(value) {
                        Ok(resolved) => *value = resolved,
                        Err(refusal) => return Ok(refusal),
                    }
                }
            }
        }
        let update = match update_request_of(&parsed, request) {
            Ok(update) => update,
            Err(refusal) => return Ok(refusal),
        };
        let expected = match expected_sequence(request) {
            Ok(expected) => expected,
            Err(refusal) => return Ok(refusal),
        };
        let mut conflict = None;
        let outcome = write(self.keyspace, |keys| {
            let key = document_key(index, id);
            let current = keys.get(&key)?.as_deref().and_then(unstored);
            if let Some(expected) = expected
                && let Some(refusal) =
                    sequence_conflict(id, expected, current.as_ref().map(|(version, _)| *version))
            {
                conflict = Some(refusal);
                return Ok(UpdateOutcome::Missing);
            }
            let outcome = planned_update(&update, index, id, current)?;
            match &outcome {
                UpdateOutcome::Created { source } => {
                    ensure_index(keys, index)?;
                    keys.put(&key, &stored(1, source))?;
                }
                UpdateOutcome::Updated { version, source } => {
                    keys.put(&key, &stored(*version, source))?;
                }
                UpdateOutcome::Deleted { .. } => {
                    keys.delete(&key)?;
                }
                UpdateOutcome::Noop { .. }
                | UpdateOutcome::SkippedUpsert
                | UpdateOutcome::Missing
                | UpdateOutcome::ScriptFailed(_) => {}
            }
            Ok(outcome)
        })?;
        if let Some(refusal) = conflict {
            return Ok(refusal);
        }
        Ok(update_response(
            index,
            id,
            outcome,
            update.returned_source.as_ref(),
        ))
    }

    fn bulk(&self, default_index: Option<&str>, request: &Request) -> Answer {
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let mut lines = body.lines().filter(|line| !line.trim().is_empty());
        let mut items = Vec::new();
        let mut failed = false;
        while let Some(action_line) = lines.next() {
            let action = parse_document(action_line)?;
            let Json::Object(members) = action.value else {
                return Ok(error(
                    400,
                    "illegal_argument_exception",
                    "Malformed action/metadata line",
                ));
            };
            let Some((kind, meta)) = members.into_iter().next() else {
                return Ok(error(
                    400,
                    "illegal_argument_exception",
                    "Malformed action/metadata line",
                ));
            };
            let field = |name: &str| {
                meta.value.clone().into_object().and_then(|members| {
                    members.into_iter().find(|(key, _)| key == name).and_then(
                        |(_, node)| match node.value {
                            Json::String(text) => Some(text),
                            Json::Number(text) => Some(text),
                            _ => None,
                        },
                    )
                })
            };
            let Some(index) = field("_index").or_else(|| default_index.map(str::to_owned)) else {
                return Ok(error(
                    400,
                    "action_request_validation_exception",
                    "Validation Failed: 1: index is missing;",
                ));
            };
            let id = field("_id").unwrap_or_else(|| self.new_id());
            let document = if kind == "delete" { None } else { lines.next() };
            let fake = Request {
                method: "POST".to_owned(),
                path: String::new(),
                query: None,
                headers: Vec::new(),
                body: document.unwrap_or_default().as_bytes().to_vec(),
            };
            let response = match kind.as_str() {
                "index" | "create" | "update" | "delete" if !valid_index(&index) => error(
                    400,
                    "invalid_index_name_exception",
                    &format!("Invalid index name [{index}]"),
                ),
                "index" => self.index_document(&index, &id, &fake, false)?,
                "create" => self.index_document(&index, &id, &fake, true)?,
                "update" => self.update_document(&index, &id, &fake)?,
                "delete" => self.delete_document(&index, &id, &fake)?,
                other => {
                    return Ok(error(
                        400,
                        "illegal_argument_exception",
                        &format!("Malformed action/metadata line [{other}]"),
                    ));
                }
            };
            failed |= response.status >= 300 && !(kind == "delete" && response.status == 404);
            let body = String::from_utf8_lossy(&response.body).into_owned();
            let item = if response.status >= 300 {
                body
            } else {
                body.strip_suffix('}').unwrap_or(&body).to_owned()
                    + &format!(",\"status\":{}}}", response.status)
            };
            items.push(format!("{{{}:{item}}}", escaped(&kind)));
        }
        Ok(json(
            200,
            format!(
                "{{\"took\":0,\"errors\":{failed},\"items\":[{}]}}",
                items.join(",")
            ),
        ))
    }

    pub(super) fn rendered_template(&self, request: &Request) -> Result<String, Response> {
        let text = String::from_utf8_lossy(&request.body).into_owned();
        let parsed = parse_document(&text)
            .map_err(|_| error(400, "parse_exception", "the template request is not JSON"))?;
        let Json::Object(members) = &parsed.value else {
            return Err(error(400, "parse_exception", "the template request is not an object"));
        };
        let mut source = None;
        let mut params = None;
        let mut stored_id = None;
        for (key, node) in members {
            match key.as_str() {
                "source" | "inline" => source = Some(node),
                "params" => params = Some(node),
                "id" => match &node.value {
                    Json::String(id) => stored_id = Some(id.clone()),
                    _ => return Err(error(400, "parse_exception", "[id] must be a string")),
                },
                "explain" | "profile" => {}
                other => {
                    return Err(error(
                        400,
                        "parse_exception",
                        &format!("unknown key [{other}] in the template request"),
                    ));
                }
            }
        }
        let template = match (source, stored_id) {
            (Some(_), Some(_)) => {
                return Err(error(
                    400,
                    "action_request_validation_exception",
                    "Validation Failed: 1: template source and stored script id are mutually exclusive;",
                ));
            }
            (Some(source), None) => match &source.value {
                Json::String(text) => text.clone(),
                _ => crate::mustache::json_of(source),
            },
            (None, Some(id)) => self.stored_template(&id)?,
            (None, None) => {
                return Err(error(
                    400,
                    "action_request_validation_exception",
                    "Validation Failed: 1: template is missing;",
                ));
            }
        };
        let empty = eggdb_sql::json::Node {
            value: Json::Object(Vec::new()),
            start: 0,
            end: 0,
        };
        crate::mustache::render(&template, params.unwrap_or(&empty))
            .map_err(|reason| error(400, "general_script_exception", &reason))
    }

    fn search_template(&self, indices: Option<&[&str]>, request: &Request) -> Answer {
        let rendered = match self.rendered_template(request) {
            Ok(rendered) => rendered,
            Err(refusal) => return Ok(refusal),
        };
        let mut inner = request.clone();
        inner.body = rendered.into_bytes();
        self.search(indices, &inner, false)
    }

    fn render_template(&self, request: &Request) -> Answer {
        let rendered = match self.rendered_template(request) {
            Ok(rendered) => rendered,
            Err(refusal) => return Ok(refusal),
        };
        Ok(match parse_document(&rendered) {
            Ok(_) => json(200, format!("{{\"template_output\":{rendered}}}")),
            Err(_) => error(
                400,
                "parse_exception",
                "the rendered template is not JSON",
            ),
        })
    }

    fn search(&self, indices: Option<&[&str]>, request: &Request, count_only: bool) -> Answer {
        Ok(match self.search_page(indices, request, count_only, false)? {
            SearchOutcome::Whole(response) => response,
            SearchOutcome::Page(page) => json(200, page.whole()),
        })
    }

    fn field_types<'i>(
        &self,
        indices: impl Iterator<Item = &'i str>,
    ) -> Result<crate::mappings::FieldTypes, Failure> {
        let mut names: Vec<&str> = indices.collect();
        names.sort_unstable();
        names.dedup();
        let mut types = crate::mappings::FieldTypes::default();
        for name in names {
            let stored = read(self.keyspace, |keys| keys.get(&index_key(name)))?;
            if let Some(mappings) = stored
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|text| parse_document(&text).ok())
            {
                types.add_mappings(&mappings);
            }
            if let Some(settings) = read(self.keyspace, |keys| keys.get(&settings_key(name)))?
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|text| parse_document(&text).ok())
            {
                types.add_settings(&settings);
            }
        }
        Ok(types)
    }

    fn gather(
        &self,
        indices: Option<&[&str]>,
        ignore_unavailable: bool,
    ) -> Result<Result<Vec<(String, String, String)>, String>, Failure> {
        Ok(read(self.keyspace, |keys| {
            let searched = match indices {
                Some(named) => match resolve_indices(keys, named, ignore_unavailable)? {
                    Ok(searched) => searched,
                    Err(missing) => return Ok(Err(missing)),
                },
                None => indices_of(keys)?,
            };
            let mut all = Vec::new();
            for index in searched {
                for (id, _, source) in documents_of(keys, &index)? {
                    all.push((index.clone(), id, source));
                }
            }
            Ok(Ok(all))
        })?)
    }

    fn search_page(
        &self,
        indices: Option<&[&str]>,
        request: &Request,
        count_only: bool,
        scrolling: bool,
    ) -> Result<SearchOutcome, Failure> {
        let body_text = String::from_utf8_lossy(&request.body).into_owned();
        let body = if body_text.trim().is_empty() {
            None
        } else {
            Some(parse_document(&body_text)?)
        };
        let member = |name: &str| {
            body.as_ref()
                .and_then(|body| body.value.clone().into_object())
                .and_then(|members| {
                    members
                        .into_iter()
                        .find(|(key, _)| key == name)
                        .map(|(_, node)| node)
                })
        };
        let query_given = member("query").is_some() || query_flag(request, "q").is_some();
        let query = match member("query") {
            Some(node) => match parse(&node, 0) {
                Ok(query) => query,
                Err(reason) => {
                    return Ok(SearchOutcome::Whole(error(400, "parsing_exception", &reason)));
                }
            },
            None => match query_flag(request, "q") {
                Some(text) => {
                    let options = crate::query_string::Options {
                        fields: vec![query_flag(request, "df").unwrap_or_else(|| "*".to_owned())],
                        all_by_default: query_flag(request, "default_operator")
                            .is_some_and(|operator| operator.eq_ignore_ascii_case("and")),
                        simple: false,
                    };
                    match crate::query_string::query_of(&text, &options) {
                        Ok(query) => query,
                        Err(reason) => {
                            return Ok(SearchOutcome::Whole(error(400, "query_shard_exception", &reason)));
                        }
                    }
                }
                None => crate::query::Query::MatchAll,
            },
        };
        let number = |name: &str, default: usize| {
            member(name)
                .and_then(|node| match node.value {
                    Json::Number(text) => text.parse().ok(),
                    _ => None,
                })
                .or_else(|| query_flag(request, name).and_then(|text| text.parse().ok()))
                .unwrap_or(default)
        };
        let size = number("size", DEFAULT_SIZE);
        let knn = match member("knn") {
            Some(node) => match crate::knn::parse_knn(&node, size.max(1)) {
                Ok(knn) => knn,
                Err(reason) => {
                    return Ok(SearchOutcome::Whole(error(400, "parsing_exception", &reason)));
                }
            },
            None => Vec::new(),
        };
        let from = number("from", 0);
        let window_end = if scrolling { size } else { from.saturating_add(size) };
        if window_end > MAX_RESULT_WINDOW {
            return Ok(SearchOutcome::Whole(error(
                400,
                "illegal_argument_exception",
                "Result window is too large, from + size must be less than or equal to: [10000]",
            )));
        }
        let presentation = if count_only {
            HitPresentation::default()
        } else {
            match presentation_of(
                member("sort"),
                member("search_after"),
                member("_source"),
                member("track_scores"),
                request,
                from,
            ) {
                Ok(presentation) => presentation,
                Err(refusal) => return Ok(SearchOutcome::Whole(refusal)),
            }
        };
        let ignore_unavailable =
            query_flag(request, "ignore_unavailable").as_deref() == Some("true");
        let pit = member("pit");
        let (gathered, pit_id) = match &pit {
            Some(pit) => {
                if indices.is_some() {
                    return Ok(SearchOutcome::Whole(error(
                        400,
                        "action_request_validation_exception",
                        "Validation Failed: 1: [indices] cannot be used with point in time;",
                    )));
                }
                match self.pit_documents(pit) {
                    Ok(found) => found,
                    Err(refusal) => return Ok(SearchOutcome::Whole(refusal)),
                }
            }
            None => match self.gather(indices, ignore_unavailable)? {
                Ok(gathered) => (Arc::new(gathered), None),
                Err(missing) => return Ok(SearchOutcome::Whole(missing_index(&missing))),
            },
        };
        let (documents, origins): (Vec<(String, Node)>, Vec<usize>) = gathered
            .iter()
            .enumerate()
            .filter_map(|(origin, (_, id, source))| {
                parse_document(source)
                    .ok()
                    .map(|node| ((id.clone(), node), origin))
            })
            .unzip();
        let query = expand_wildcard_fields(query, &documents);
        let field_types = self.field_types(gathered.iter().map(|(index, _, _)| index.as_str()))?;
        let _analyzers = crate::analysis::enter(field_types.field_analyzers());
        if !documents.is_empty() {
            let sources: Vec<&Node> = documents.iter().map(|(_, source)| source).collect();
            if let Some(name) = presentation
                .sort
                .unmapped_field(&sources, &|name| field_types.resolve(name).is_some())
            {
                return Ok(SearchOutcome::Whole(error(
                    400,
                    "search_phase_execution_exception",
                    &format!("No mapping found for [{name}] in order to sort on"),
                )));
            }
        }
        let query = crate::mappings::apply(query, &field_types, now_millis());
        let highlight = match member("highlight") {
            Some(node) if !count_only => match crate::highlight::highlight_of(&node, &query) {
                Ok(highlight) => Some(highlight),
                Err(reason) => {
                    return Ok(SearchOutcome::Whole(error(400, "parsing_exception", &reason)));
                }
            },
            _ => None,
        };
        let stats = Stats::of(&query, &documents);
        let mut hits: Vec<(f64, usize)> = if knn.is_empty() || query_given {
            documents
                .iter()
                .enumerate()
                .filter_map(|(at, (id, source))| {
                    score(&query, id, source, &stats).map(|value| (value, at))
                })
                .collect()
        } else {
            Vec::new()
        };
        if !knn.is_empty() {
            let nearest = crate::knn::nearest(&knn, &documents, &field_types, &stats);
            for hit in &mut hits {
                if let Some(value) = nearest.get(&hit.1) {
                    hit.0 += value;
                }
            }
            let mut added: Vec<(f64, usize)> = nearest
                .into_iter()
                .filter(|(at, _)| !hits.iter().any(|hit| hit.1 == *at))
                .map(|(at, value)| (value, at))
                .collect();
            added.sort_by_key(|(_, at)| *at);
            hits.extend(added);
        }
        if count_only {
            return Ok(SearchOutcome::Whole(json(
                200,
                format!(
                    "{{\"count\":{},\"_shards\":{{\"total\":1,\"successful\":1,\"skipped\":0,\"failed\":0}}}}",
                    hits.len()
                ),
            )));
        }
        let matched: Vec<&Node> = hits.iter().map(|(_, at)| &documents[*at].1).collect();
        let sort = &presentation.sort;
        let reports_scores = sort.is_empty() || sort.uses_score() || presentation.track_scores;
        let mut ranked: Vec<(f64, usize, Vec<SortValue>)> = Vec::with_capacity(hits.len());
        if sort.is_empty() {
            hits.sort_by(|left, right| right.0.total_cmp(&left.0).then(left.1.cmp(&right.1)));
            ranked.extend(hits.iter().map(|&(value, at)| (value, at, Vec::new())));
        } else {
            let ordered = sort.over(&matched);
            let after = match &presentation.search_after {
                Some(values) => match ordered.after(values) {
                    Ok(after) => Some(after),
                    Err(reason) => {
                        return Ok(SearchOutcome::Whole(error(
                            400,
                            "illegal_argument_exception",
                            &reason,
                        )));
                    }
                },
                None => None,
            };
            ranked.extend(
                hits.iter().map(|&(value, at)| {
                    (value, at, ordered.values_of(value, at, &documents[at].1))
                }),
            );
            ranked.sort_by(|left, right| {
                ordered
                    .compare(&left.2, &right.2)
                    .then(left.1.cmp(&right.1))
            });
            if let Some(after) = &after {
                ranked.retain(|(_, _, values)| {
                    ordered.compare(values, after) == std::cmp::Ordering::Greater
                });
            }
        }
        let total = hits.len();
        let max_score = hits
            .iter()
            .map(|(value, _)| *value)
            .fold(None, |best: Option<f64>, value| {
                Some(best.map_or(value, |held| held.max(value)))
            });
        let mut out = Out::default();
        out.begin_object();
        if let Some(pit_id) = &pit_id {
            out.key("pit_id").string(pit_id);
        }
        out.key("took")
            .integer(0)
            .key("timed_out")
            .boolean(false)
            .key("_shards")
            .raw("{\"total\":1,\"successful\":1,\"skipped\":0,\"failed\":0}")
            .key("hits")
            .begin_object()
            .key("total")
            .begin_object()
            .key("value")
            .integer(total as u64)
            .key("relation")
            .string("eq")
            .end_object()
            .key("max_score");
        match max_score {
            Some(value) if reports_scores => out.number(value),
            _ => out.null(),
        };
        out.key("hits").begin_array();
        let head = out.finish();
        let mut tail = "]}".to_owned();
        if let Some(aggregations) = member("aggs").or_else(|| member("aggregations")) {
            let mut rendered = Out::default();
            aggregate(&mut rendered, &aggregations, &matched);
            tail.push_str(",\"aggregations\":");
            tail.push_str(&rendered.finish());
        }
        if let Some(suggest) = member("suggest") {
            let candidates: Vec<crate::suggest::Candidate<'_>> = documents
                .iter()
                .zip(&origins)
                .filter_map(|((id, source), origin)| {
                    let (index, _, source_text) = gathered.get(*origin)?;
                    Some(crate::suggest::Candidate {
                        index,
                        id,
                        source,
                        source_text,
                    })
                })
                .collect();
            let mut rendered = Out::default();
            crate::suggest::write(&mut rendered, &suggest, &candidates);
            tail.push_str(",\"suggest\":");
            tail.push_str(&rendered.finish());
        }
        tail.push('}');
        let sorted = !sort.is_empty();
        let (skipped, taken) = if scrolling { (0, usize::MAX) } else { (from, size) };
        let page_hits = ranked
            .into_iter()
            .skip(skipped)
            .take(taken)
            .filter_map(|(value, at, sort_values)| {
                let origin = *origins.get(at)?;
                Some(PageHit {
                    score: reports_scores.then_some(value),
                    origin,
                    sort_values: sorted.then_some(sort_values),
                })
            })
            .collect();
        drop(matched);
        Ok(SearchOutcome::Page(SearchPage {
            head,
            gathered,
            hits: page_hits,
            source_filter: presentation.source_filter,
            highlight,
            tail,
            size,
        }))
    }
}

enum SearchOutcome {
    Whole(Response),
    Page(SearchPage),
}

struct PageHit {
    score: Option<f64>,
    origin: usize,
    sort_values: Option<Vec<SortValue>>,
}

struct SearchPage {
    head: String,
    gathered: Arc<Vec<(String, String, String)>>,
    hits: Vec<PageHit>,
    source_filter: SourceFilter,
    highlight: Option<crate::highlight::Highlight>,
    tail: String,
    size: usize,
}

impl SearchPage {
    fn hit(&self, hit: &PageHit) -> Option<String> {
        let (index, id, source) = self.gathered.get(hit.origin)?;
        let mut out = Out::default();
        out.begin_object()
            .key("_index")
            .string(index)
            .key("_id")
            .string(id)
            .key("_score");
        match hit.score {
            Some(value) => out.number(value),
            None => out.null(),
        };
        if let Some(filtered) = self.source_filter.apply(source) {
            out.key("_source").raw(&filtered);
        }
        if let Some(marked) = self
            .highlight
            .as_ref()
            .and_then(|highlight| highlight.of(&parse_document(source).ok()?))
        {
            out.key("highlight").raw(&marked);
        }
        if let Some(values) = &hit.sort_values {
            out.key("sort");
            write_values(&mut out, values);
        }
        out.end_object();
        Some(out.finish())
    }

    fn whole(&self) -> String {
        let mut text = self.head.clone();
        let mut first = true;
        for hit in &self.hits {
            if let Some(rendered) = self.hit(hit) {
                if !first {
                    text.push(',');
                }
                first = false;
                text.push_str(&rendered);
            }
        }
        text.push_str(&self.tail);
        text
    }

    fn stream(self) -> StreamedResponse<'static> {
        StreamedResponse::new(200, "application/json; charset=UTF-8", move |out| {
            out.write_all(self.head.as_bytes())?;
            let mut first = true;
            for hit in &self.hits {
                if let Some(rendered) = self.hit(hit) {
                    if !first {
                        out.write_all(b",")?;
                    }
                    first = false;
                    out.write_all(rendered.as_bytes())?;
                }
            }
            out.write_all(self.tail.as_bytes())
        })
    }
}

fn expand_wildcard_fields(
    query: crate::query::Query,
    documents: &[(String, Node)],
) -> crate::query::Query {
    use crate::query::Query;
    let names = || {
        let mut names: Vec<String> = documents
            .iter()
            .filter_map(|(_, node)| node.value.clone().into_object())
            .flat_map(|members| members.into_iter().map(|(key, _)| key))
            .collect();
        names.sort();
        names.dedup();
        names
    };
    let any_of = |should: Vec<Query>| Query::Bool {
        must: Vec::new(),
        filter: Vec::new(),
        should,
        must_not: Vec::new(),
        minimum_should_match: 1,
    };
    match query {
        Query::Match { fields, terms, all } if fields.iter().any(|field| field == "*") => Query::Match {
            fields: names(),
            terms,
            all,
        },
        Query::MatchPhrase { field, terms } if field == "*" => any_of(
            names()
                .into_iter()
                .map(|field| Query::MatchPhrase {
                    field,
                    terms: terms.clone(),
                })
                .collect(),
        ),
        Query::Prefix { field, prefix } if field == "*" => any_of(
            names()
                .into_iter()
                .map(|field| Query::Prefix {
                    field,
                    prefix: prefix.clone(),
                })
                .collect(),
        ),
        Query::Range { field, bounds } if field == "*" => any_of(
            names()
                .into_iter()
                .map(|field| Query::Range {
                    field,
                    bounds: bounds.clone(),
                })
                .collect(),
        ),
        Query::Bool {
            must,
            filter,
            should,
            must_not,
            minimum_should_match,
        } => {
            let expand = |queries: Vec<Query>| -> Vec<Query> {
                queries
                    .into_iter()
                    .map(|query| expand_wildcard_fields(query, documents))
                    .collect()
            };
            Query::Bool {
                must: expand(must),
                filter: expand(filter),
                should: expand(should),
                must_not: expand(must_not),
                minimum_should_match,
            }
        }
        other => other,
    }
}

/// How a search lays out its hits: their order (`sort`, empty for relevance), the page after
/// which they start (`search_after`), which part of each source they show (`_source`) and
/// whether a sorted search still reports scores (`track_scores`).
#[derive(Debug, Default)]
struct HitPresentation {
    sort: Sort,
    search_after: Option<Vec<Node>>,
    source_filter: SourceFilter,
    track_scores: bool,
}

fn presentation_of(
    sort: Option<Node>,
    search_after: Option<Node>,
    source: Option<Node>,
    track_scores: Option<Node>,
    request: &Request,
    from: usize,
) -> Result<HitPresentation, Response> {
    let search_after = match search_after.map(|node| node.value) {
        None | Some(Json::Null) => None,
        Some(Json::Array(values)) if values.is_empty() => None,
        Some(Json::Array(values)) => Some(values),
        Some(_) => {
            return Err(error(
                400,
                "parsing_exception",
                "[search_after] must be an array of sort values",
            ));
        }
    };
    if search_after.is_some() && from > 0 {
        return Err(error(
            400,
            "illegal_argument_exception",
            "`from` parameter must be set to 0 when `search_after` is used.",
        ));
    }
    let sort = match (sort, query_flag(request, "sort")) {
        (Some(node), _) => Sort::from_node(&node),
        (None, Some(text)) => Sort::from_flag(&text),
        (None, None) => Ok(Sort::default()),
    }
    .map_err(|reason| error(400, "parsing_exception", &reason))?;
    if search_after.is_some() && sort.is_empty() {
        return Err(error(
            400,
            "illegal_argument_exception",
            "Sort must contain at least one field.",
        ));
    }
    let source_filter = match source {
        Some(node) => SourceFilter::from_node(&node)
            .map_err(|reason| error(400, "parsing_exception", &reason))?,
        None => multi::flag_filter(request),
    };
    let track_scores = match track_scores.map(|node| node.value) {
        None | Some(Json::Null) => false,
        Some(Json::Boolean(value)) => value,
        Some(_) => {
            return Err(error(
                400,
                "parsing_exception",
                "[track_scores] must be a boolean",
            ));
        }
    };
    Ok(HitPresentation {
        sort,
        search_after,
        source_filter,
        track_scores,
    })
}

fn aggregate(out: &mut Out, aggregations: &Node, matched: &[&Node]) {
    crate::aggregations::write(out, aggregations, matched);
}

fn script_error(reason: &str) -> Response {
    error(400, "script_exception", reason)
}

fn scripted(
    script: &Script,
    index: &str,
    id: &str,
    version: u64,
    source: &str,
) -> Result<(Operation, String), String> {
    let parsed = parse_document(source).map_err(|error| error.message)?;
    ran(
        script,
        Context {
            source: parsed.value,
            operation: Operation::Index,
            index,
            id,
            version,
            upsert: false,
        },
    )
}

fn scripted_upsert(
    script: &Script,
    index: &str,
    id: &str,
    upsert: &Node,
) -> Result<(Operation, String), String> {
    ran(
        script,
        Context {
            source: upsert.value.clone(),
            operation: Operation::Create,
            index,
            id,
            version: 0,
            upsert: true,
        },
    )
}

fn ran(script: &Script, mut context: Context<'_>) -> Result<(Operation, String), String> {
    script.run(&mut context)?;
    let changed = written(&detached(context.source));
    Ok((context.operation, changed))
}

fn detached(value: Json) -> Node {
    Node {
        value,
        start: 0,
        end: 0,
    }
}

enum Modification {
    Doc(Vec<(String, Node)>),
    Script(Script),
}

/// An `_update` request's body: the partial document or script, the document to insert when
/// the target is missing (`upsert`, or `doc` itself with `doc_as_upsert`), whether the script
/// builds that document (`scripted_upsert`), whether an unchanged `doc` is a `noop`
/// (`detect_noop`, true unless turned off) and which part of the new source the answer's `get`
/// returns (`_source`).
struct UpdateRequest {
    modification: Modification,
    upsert: Option<Node>,
    scripted_upsert: bool,
    detect_noop: bool,
    returned_source: Option<SourceFilter>,
}

enum UpdateOutcome {
    Created { source: String },
    Updated { version: u64, source: String },
    Noop { version: u64, source: String },
    SkippedUpsert,
    Deleted { version: u64 },
    Missing,
    ScriptFailed(String),
}

fn update_flag(name: &str, value: &Node) -> Result<bool, Response> {
    match &value.value {
        Json::Boolean(flag) => Ok(*flag),
        Json::String(text) if text == "true" => Ok(true),
        Json::String(text) if text == "false" => Ok(false),
        _ => Err(error(
            400,
            "x_content_parse_exception",
            &format!("[UpdateRequest] failed to parse field [{name}]: it must be a boolean"),
        )),
    }
}

fn update_object(name: &str, value: &Node) -> Result<Option<Node>, Response> {
    match &value.value {
        Json::Object(_) => Ok(Some(value.clone())),
        Json::Null => Ok(None),
        _ => Err(error(
            400,
            "x_content_parse_exception",
            &format!("[UpdateRequest] failed to parse field [{name}]: it must be an object"),
        )),
    }
}

fn returned_source_of(
    request: &Request,
    body: Option<&Node>,
) -> Result<Option<SourceFilter>, Response> {
    let filter = match body {
        Some(node) => SourceFilter::from_node(node)
            .map_err(|reason| error(400, "x_content_parse_exception", &reason))?,
        None => {
            let asked = [
                "_source",
                "_source_includes",
                "_source_include",
                "_source_excludes",
                "_source_exclude",
            ]
            .iter()
            .any(|name| query_flag(request, name).is_some());
            if !asked {
                return Ok(None);
            }
            multi::flag_filter(request)
        }
    };
    Ok((filter != SourceFilter::Hidden).then_some(filter))
}

fn update_request_of(parsed: &Node, request: &Request) -> Result<UpdateRequest, Response> {
    let Json::Object(members) = &parsed.value else {
        return Err(error(
            400,
            "x_content_parse_exception",
            "[UpdateRequest] the request body must be a JSON object",
        ));
    };
    let mut doc = None;
    let mut script = None;
    let mut upsert = None;
    let mut doc_as_upsert = false;
    let mut scripted_upsert = false;
    let mut detect_noop = true;
    let mut source_member = None;
    for (key, value) in members {
        match key.as_str() {
            "doc" => doc = update_object(key, value)?,
            "upsert" => upsert = update_object(key, value)?,
            "script" => script = Some(value),
            "doc_as_upsert" => doc_as_upsert = update_flag(key, value)?,
            "scripted_upsert" => scripted_upsert = update_flag(key, value)?,
            "detect_noop" => detect_noop = update_flag(key, value)?,
            "_source" => source_member = Some(value),
            _ => {}
        }
    }
    let mut problems = Vec::new();
    if script.is_none() && doc.is_none() {
        problems.push("script or doc is missing".to_owned());
    }
    if script.is_some() && doc.is_some() {
        problems.push("can't provide both script and doc".to_owned());
    }
    if doc.is_none() && doc_as_upsert {
        problems.push("doc must be specified if doc_as_upsert is enabled".to_owned());
    }
    if !problems.is_empty() {
        return Err(validation(&problems));
    }
    let returned_source = returned_source_of(request, source_member)?;
    let modification = match (doc, script) {
        (Some(doc), _) => {
            if doc_as_upsert {
                upsert = Some(doc.clone());
            }
            match doc.value {
                Json::Object(changes) => Modification::Doc(changes),
                _ => Modification::Doc(Vec::new()),
            }
        }
        (None, Some(script)) => match Script::from_node(script) {
            Ok(script) => Modification::Script(script),
            Err(reason) => return Err(script_error(&format!("compile error: {reason}"))),
        },
        (None, None) => return Err(validation(&["script or doc is missing".to_owned()])),
    };
    Ok(UpdateRequest {
        modification,
        upsert,
        scripted_upsert,
        detect_noop,
        returned_source,
    })
}

fn planned_update(
    update: &UpdateRequest,
    index: &str,
    id: &str,
    current: Option<(u64, String)>,
) -> Result<UpdateOutcome, String> {
    let Some((version, source)) = current else {
        return Ok(planned_upsert(update, index, id));
    };
    match &update.modification {
        Modification::Script(script) => Ok(match scripted(script, index, id, version, &source) {
            Ok((Operation::Index, changed)) => UpdateOutcome::Updated {
                version: version + 1,
                source: changed,
            },
            Ok((Operation::Delete, _)) => UpdateOutcome::Deleted {
                version: version + 1,
            },
            Ok((Operation::Noop | Operation::Create, _)) => UpdateOutcome::Noop { version, source },
            Err(reason) => UpdateOutcome::ScriptFailed(reason),
        }),
        Modification::Doc(changes) => {
            let current = parse_document(&source).map_err(|error| error.message)?;
            let Json::Object(mut members) = current.value else {
                return Ok(UpdateOutcome::Missing);
            };
            let differs = merged_into(&mut members, changes);
            Ok(if differs || !update.detect_noop {
                UpdateOutcome::Updated {
                    version: version + 1,
                    source: written(&detached(Json::Object(members))),
                }
            } else {
                UpdateOutcome::Noop { version, source }
            })
        }
    }
}

fn planned_upsert(update: &UpdateRequest, index: &str, id: &str) -> UpdateOutcome {
    let Some(upsert) = &update.upsert else {
        return UpdateOutcome::Missing;
    };
    match &update.modification {
        Modification::Script(script) if update.scripted_upsert => {
            match scripted_upsert(script, index, id, upsert) {
                Ok((Operation::Create, created)) => UpdateOutcome::Created { source: created },
                Ok(_) => UpdateOutcome::SkippedUpsert,
                Err(reason) => UpdateOutcome::ScriptFailed(reason),
            }
        }
        _ => UpdateOutcome::Created {
            source: written(upsert),
        },
    }
}

/// Merges a partial document into a source as Elasticsearch does: objects present on both
/// sides merge member by member, anything else is replaced. Answers whether the source changed.
fn merged_into(members: &mut Vec<(String, Node)>, changes: &[(String, Node)]) -> bool {
    let mut differs = false;
    for (name, value) in changes {
        let Some((_, existing)) = members.iter_mut().find(|(key, _)| key == name) else {
            members.push((name.clone(), value.clone()));
            differs = true;
            continue;
        };
        if let (Json::Object(inner), Json::Object(inner_changes)) =
            (&mut existing.value, &value.value)
        {
            differs |= merged_into(inner, inner_changes);
            continue;
        }
        differs |= !same_json(&existing.value, &value.value);
        *existing = value.clone();
    }
    differs
}

fn same_number(left: &str, right: &str) -> bool {
    let integral = |text: &str| !text.contains(['.', 'e', 'E']);
    match (integral(left), integral(right)) {
        (true, true) => left == right,
        (false, false) => match (left.parse::<f64>(), right.parse::<f64>()) {
            (Ok(left), Ok(right)) => left == right,
            _ => left == right,
        },
        _ => false,
    }
}

fn sorted_members(members: &[(String, Node)]) -> Vec<&(String, Node)> {
    let mut sorted: Vec<&(String, Node)> = members.iter().collect();
    sorted.sort_by(|left, right| left.0.cmp(&right.0));
    sorted
}

/// Whether two JSON values are equal as Elasticsearch compares parsed sources: object members
/// in any order, numbers by value within integers or within decimals.
fn same_json(left: &Json, right: &Json) -> bool {
    match (left, right) {
        (Json::Null, Json::Null) => true,
        (Json::Boolean(left), Json::Boolean(right)) => left == right,
        (Json::String(left), Json::String(right)) => left == right,
        (Json::Number(left), Json::Number(right)) => same_number(left, right),
        (Json::Array(left), Json::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_json(&left.value, &right.value))
        }
        (Json::Object(left), Json::Object(right)) => {
            left.len() == right.len()
                && sorted_members(left)
                    .into_iter()
                    .zip(sorted_members(right))
                    .all(|(left, right)| {
                        left.0 == right.0 && same_json(&left.1.value, &right.1.value)
                    })
        }
        _ => false,
    }
}

fn update_response(
    index: &str,
    id: &str,
    outcome: UpdateOutcome,
    returned_source: Option<&SourceFilter>,
) -> Response {
    let (status, version, result, shards, source) = match outcome {
        UpdateOutcome::Created { source } => (201, Some(1), "created", 1, Some(source)),
        UpdateOutcome::Updated { version, source } => {
            (200, Some(version), "updated", 1, Some(source))
        }
        UpdateOutcome::Noop { version, source } => (200, Some(version), "noop", 0, Some(source)),
        UpdateOutcome::SkippedUpsert => (200, None, "noop", 0, None),
        UpdateOutcome::Deleted { version } => (200, Some(version), "deleted", 1, None),
        UpdateOutcome::Missing => {
            return error(
                404,
                "document_missing_exception",
                &format!("[{id}]: document missing"),
            );
        }
        UpdateOutcome::ScriptFailed(reason) => {
            return script_error(&format!("runtime error: {reason}"));
        }
    };
    let mut out = Out::default();
    out.begin_object()
        .key("_index")
        .string(index)
        .key("_id")
        .string(id)
        .key("_version");
    match version {
        Some(version) => out.integer(version),
        None => out.raw("-1"),
    };
    out.key("result")
        .string(result)
        .key("_shards")
        .begin_object()
        .key("total")
        .integer(shards)
        .key("successful")
        .integer(shards)
        .key("failed")
        .integer(0)
        .end_object();
    if let Some(version) = version {
        out.key("_seq_no")
            .integer(version.saturating_sub(1))
            .key("_primary_term")
            .integer(1);
        if let Some(filtered) = returned_source
            .zip(source.as_deref())
            .and_then(|(filter, source)| filter.apply(source))
        {
            out.key("get")
                .begin_object()
                .key("_seq_no")
                .integer(version.saturating_sub(1))
                .key("_primary_term")
                .integer(1)
                .key("found")
                .boolean(true)
                .key("_source")
                .raw(&filtered)
                .end_object();
        }
    }
    out.end_object();
    json(status, out.finish())
}

fn refreshed() -> Response {
    json(
        200,
        "{\"_shards\":{\"total\":1,\"successful\":1,\"failed\":0}}".to_owned(),
    )
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

fn epoch_and_clock() -> (u64, String) {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let within = seconds % 86_400;
    (
        seconds,
        format!(
            "{:02}:{:02}:{:02}",
            within / 3600,
            within / 60 % 60,
            within % 60
        ),
    )
}

fn cat_health() -> Response {
    let (epoch, clock) = epoch_and_clock();
    json(
        200,
        format!(
            "[{{\"epoch\":\"{epoch}\",\"timestamp\":\"{clock}\",\"cluster\":\"eggdb\",\"status\":\"green\",\"node.total\":\"1\",\"node.data\":\"1\",\"shards\":\"0\",\"pri\":\"0\",\"relo\":\"0\",\"init\":\"0\",\"unassign\":\"0\",\"pending_tasks\":\"0\",\"max_task_wait_time\":\"-\",\"active_shards_percent\":\"100.0%\"}}]"
        ),
    )
}

fn missing_index(index: &str) -> Response {
    error(
        404,
        "index_not_found_exception",
        &format!("no such index [{index}]"),
    )
}

fn write_result(index: &str, id: &str, version: u64, result: &str) -> String {
    format!(
        "{{\"_index\":{},\"_id\":{},\"_version\":{version},\"result\":\"{result}\",\"_shards\":{{\"total\":1,\"successful\":1,\"failed\":0}},\"_seq_no\":{},\"_primary_term\":1}}",
        escaped(index),
        escaped(id),
        version.saturating_sub(1)
    )
}

trait IntoObject {
    fn into_object(self) -> Option<Vec<(String, Node)>>;
}

impl IntoObject for Json {
    fn into_object(self) -> Option<Vec<(String, Node)>> {
        match self {
            Json::Object(members) => Some(members),
            _ => None,
        }
    }
}
