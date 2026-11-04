mod by_query;
mod multi;

use crate::json_out::{Out, escaped, written};
use crate::query::{Stats, numbers, parse, score, term_buckets};
use crate::script::{Context, Operation, Script};
use eggdb_http::{Request, Response};
use eggdb_keyspace::{KeysRead, KeysWrite, Keyspace};
use eggdb_platform::connection_limits::MemoryBudget;
use eggdb_sql::json::{Json, JsonSyntaxError, Node, parse_document};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const INDEX_RECORD: u8 = 1;
const DOCUMENT_RECORD: u8 = 2;
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

fn ensure_index(keys: &mut dyn KeysWrite, index: &str) -> Result<(), String> {
    if keys.get(&index_key(index))?.is_none() {
        keys.put(&index_key(index), b"{}")?;
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

    /// Answers one request.
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
            ("POST", ["_bulk"]) => self.bulk(None, request),
            ("GET" | "POST", ["_search"]) => self.search(None, request, false),
            ("GET" | "POST", ["_count"]) => self.search(None, request, true),
            ("GET" | "POST", ["_mget"]) => self.mget(None, request),
            ("GET" | "POST", ["_msearch"]) => self.msearch(None, request),
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
            ("GET" | "HEAD", [index, "_doc", id]) => self.get_document(index, id, method == "HEAD"),
            ("DELETE", [index, "_doc", id]) => self.delete_document(index, id),
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
        let mappings = if body.trim().is_empty() {
            "{}".to_owned()
        } else {
            let parsed = parse_document(&body)?;
            parsed
                .value
                .clone()
                .into_object()
                .and_then(|members| {
                    members
                        .into_iter()
                        .find(|(key, _)| key == "mappings")
                        .map(|(_, node)| written(&node))
                })
                .unwrap_or_else(|| "{}".to_owned())
        };
        let created = write(self.keyspace, |keys| {
            if keys.get(&index_key(index))?.is_some() {
                return Ok(false);
            }
            keys.put(&index_key(index), mappings.as_bytes())?;
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
        let outcome = write(self.keyspace, |keys| {
            ensure_index(keys, index)?;
            let key = document_key(index, id);
            let previous = keys
                .get(&key)?
                .as_deref()
                .and_then(unstored)
                .map(|(version, _)| version);
            if create_only && previous.is_some() {
                return Ok(None);
            }
            let version = previous.map_or(1, |version| version + 1);
            keys.put(&key, &stored(version, &source))?;
            Ok(Some((version, previous.is_none())))
        })?;
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

    fn get_document(&self, index: &str, id: &str, head: bool) -> Answer {
        let found = read(self.keyspace, |keys| {
            Ok(keys
                .get(&document_key(index, id))?
                .as_deref()
                .and_then(unstored))
        })?;
        Ok(match found {
            Some(_) if head => json(200, String::new()),
            Some((version, source)) => json(
                200,
                format!(
                    "{{\"_index\":{},\"_id\":{},\"_version\":{version},\"_seq_no\":{},\"_primary_term\":1,\"found\":true,\"_source\":{source}}}",
                    escaped(index),
                    escaped(id),
                    version.saturating_sub(1)
                ),
            ),
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

    fn delete_document(&self, index: &str, id: &str) -> Answer {
        let removed = write(self.keyspace, |keys| {
            let key = document_key(index, id);
            let version = keys
                .get(&key)?
                .as_deref()
                .and_then(unstored)
                .map(|(version, _)| version);
            if version.is_some() {
                keys.delete(&key)?;
            }
            Ok(version)
        })?;
        Ok(match removed {
            Some(version) => json(200, write_result(index, id, version + 1, "deleted")),
            None => json(404, write_result(index, id, 1, "not_found")),
        })
    }

    fn update_document(&self, index: &str, id: &str, request: &Request) -> Answer {
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let parsed = parse_document(&body)?;
        if let Some(script) = member_of(&parsed, "script") {
            if member_of(&parsed, "doc").is_some() {
                return Ok(error(
                    400,
                    "action_request_validation_exception",
                    "Validation Failed: 1: can't provide both script and doc;",
                ));
            }
            return match Script::from_node(&script) {
                Ok(script) => self.update_document_with_script(index, id, &script),
                Err(reason) => Ok(script_error(&format!("compile error: {reason}"))),
            };
        }
        let Some(Json::Object(changes)) = parsed.value.clone().into_object().and_then(|members| {
            members
                .into_iter()
                .find(|(key, _)| key == "doc")
                .map(|(_, node)| node.value)
        }) else {
            return Ok(error(
                400,
                "action_request_validation_exception",
                "Validation Failed: 1: script or doc is missing;",
            ));
        };
        let outcome = write(self.keyspace, |keys| {
            let key = document_key(index, id);
            let Some((version, source)) = keys.get(&key)?.as_deref().and_then(unstored) else {
                return Ok(None);
            };
            let current = parse_document(&source).map_err(|error| error.message)?;
            let Json::Object(mut members) = current.value else {
                return Ok(None);
            };
            for (name, value) in &changes {
                match members.iter_mut().find(|(key, _)| key == name) {
                    Some((_, existing)) => *existing = value.clone(),
                    None => members.push((name.clone(), value.clone())),
                }
            }
            let merged = written(&Node {
                value: Json::Object(members),
                start: 0,
                end: 0,
            });
            keys.put(&key, &stored(version + 1, &merged))?;
            Ok(Some(version + 1))
        })?;
        Ok(match outcome {
            Some(version) => json(200, write_result(index, id, version, "updated")),
            None => error(
                404,
                "document_missing_exception",
                &format!("[{id}]: document missing"),
            ),
        })
    }

    fn update_document_with_script(&self, index: &str, id: &str, script: &Script) -> Answer {
        let mut failure = None;
        let outcome = write(self.keyspace, |keys| {
            let key = document_key(index, id);
            let Some((version, source)) = keys.get(&key)?.as_deref().and_then(unstored) else {
                return Ok(None);
            };
            match scripted(script, index, id, version, &source) {
                Ok((Operation::Index, changed)) => {
                    keys.put(&key, &stored(version + 1, &changed))?;
                    Ok(Some((version + 1, "updated")))
                }
                Ok((Operation::Noop, _)) => Ok(Some((version, "noop"))),
                Ok((Operation::Delete, _)) => {
                    keys.delete(&key)?;
                    Ok(Some((version + 1, "deleted")))
                }
                Err(reason) => {
                    failure = Some(reason);
                    Err("the script failed".to_owned())
                }
            }
        });
        if let Some(reason) = failure {
            return Ok(script_error(&format!("runtime error: {reason}")));
        }
        Ok(match outcome? {
            Some((version, result)) => json(200, write_result(index, id, version, result)),
            None => error(
                404,
                "document_missing_exception",
                &format!("[{id}]: document missing"),
            ),
        })
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
                "delete" => self.delete_document(&index, &id)?,
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

    fn search(&self, indices: Option<&[&str]>, request: &Request, count_only: bool) -> Answer {
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
        let query = match member("query") {
            Some(node) => match parse(&node, 0) {
                Ok(query) => query,
                Err(reason) => return Ok(error(400, "parsing_exception", &reason)),
            },
            None => match query_flag(request, "q") {
                Some(text) => crate::query::Query::Match {
                    fields: vec!["*".to_owned()],
                    terms: crate::query::analyze(&text),
                    all: false,
                },
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
        let from = number("from", 0);
        if from.saturating_add(size) > MAX_RESULT_WINDOW {
            return Ok(error(
                400,
                "illegal_argument_exception",
                "Result window is too large, from + size must be less than or equal to: [10000]",
            ));
        }
        let ignore_unavailable =
            query_flag(request, "ignore_unavailable").as_deref() == Some("true");
        let gathered = read(self.keyspace, |keys| {
            let searched = match indices {
                Some(named) => {
                    let mut searched = Vec::new();
                    for index in named {
                        if keys.get(&index_key(index))?.is_some() {
                            searched.push((*index).to_owned());
                        } else if !ignore_unavailable {
                            return Ok(Err((*index).to_owned()));
                        }
                    }
                    searched
                }
                None => indices_of(keys)?,
            };
            let mut all = Vec::new();
            for index in searched {
                for (id, _, source) in documents_of(keys, &index)? {
                    all.push((index.clone(), id, source));
                }
            }
            Ok(Ok(all))
        })?;
        let gathered = match gathered {
            Ok(gathered) => gathered,
            Err(missing) => return Ok(missing_index(&missing)),
        };
        let documents: Vec<(String, Node)> = gathered
            .iter()
            .filter_map(|(_, id, source)| {
                parse_document(source).ok().map(|node| (id.clone(), node))
            })
            .collect();
        let query = expand_wildcard_fields(query, &documents);
        let stats = Stats::of(&query, &documents);
        let mut hits: Vec<(f64, usize)> = documents
            .iter()
            .enumerate()
            .filter_map(|(at, (id, source))| {
                score(&query, id, source, &stats).map(|value| (value, at))
            })
            .collect();
        if count_only {
            return Ok(json(
                200,
                format!(
                    "{{\"count\":{},\"_shards\":{{\"total\":1,\"successful\":1,\"skipped\":0,\"failed\":0}}}}",
                    hits.len()
                ),
            ));
        }
        let sort_field = member("sort").and_then(|node| sort_of(&node));
        match &sort_field {
            Some((field, descending)) => hits.sort_by(|left, right| {
                let value = |at: usize| {
                    crate::query::values_at(&documents[at].1, field)
                        .first()
                        .map(|node| node.value.clone())
                };
                let ordering = compare_values(value(left.1).as_ref(), value(right.1).as_ref());
                if *descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            }),
            None => {
                hits.sort_by(|left, right| right.0.total_cmp(&left.0).then(left.1.cmp(&right.1)))
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
        out.begin_object()
            .key("took")
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
            Some(value) if sort_field.is_none() => out.number(value),
            _ => out.null(),
        };
        out.key("hits").begin_array();
        for (value, at) in hits.iter().skip(from).take(size) {
            let (index, id, source) = &gathered[*at];
            out.begin_object()
                .key("_index")
                .string(index)
                .key("_id")
                .string(id)
                .key("_score");
            if sort_field.is_some() {
                out.null();
            } else {
                out.number(*value);
            }
            out.key("_source").raw(source).end_object();
        }
        out.end_array().end_object();
        if let Some(aggregations) = member("aggs").or_else(|| member("aggregations")) {
            let matched: Vec<&Node> = hits.iter().map(|(_, at)| &documents[*at].1).collect();
            out.key("aggregations");
            aggregate(&mut out, &aggregations, &matched);
        }
        out.end_object();
        Ok(json(200, out.finish()))
    }
}

fn expand_wildcard_fields(
    query: crate::query::Query,
    documents: &[(String, Node)],
) -> crate::query::Query {
    use crate::query::Query;
    match query {
        Query::Match { fields, terms, all } if fields.iter().any(|field| field == "*") => {
            let mut names: Vec<String> = documents
                .iter()
                .filter_map(|(_, node)| node.value.clone().into_object())
                .flat_map(|members| members.into_iter().map(|(key, _)| key))
                .collect();
            names.sort();
            names.dedup();
            Query::Match {
                fields: names,
                terms,
                all,
            }
        }
        other => other,
    }
}

fn compare_values(left: Option<&Json>, right: Option<&Json>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(Json::Number(a)), Some(Json::Number(b))) => a
            .parse::<f64>()
            .unwrap_or(0.0)
            .total_cmp(&b.parse::<f64>().unwrap_or(0.0)),
        (Some(Json::String(a)), Some(Json::String(b))) => a.cmp(b),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    }
}

fn sort_of(node: &Node) -> Option<(String, bool)> {
    let first = match &node.value {
        Json::Array(items) => items.first()?.clone(),
        _ => node.clone(),
    };
    match first.value {
        Json::String(field) => Some((field, false)),
        Json::Object(members) => {
            let (field, spec) = members.into_iter().next()?;
            let descending = match spec.value {
                Json::String(order) => order == "desc",
                Json::Object(inner) => inner.iter().any(|(key, value)| {
                    key == "order" && value.value == Json::String("desc".to_owned())
                }),
                _ => false,
            };
            Some((field, descending))
        }
        _ => None,
    }
}

fn aggregate(out: &mut Out, aggregations: &Node, matched: &[&Node]) {
    out.begin_object();
    if let Json::Object(members) = &aggregations.value {
        for (name, spec) in members {
            out.key(name).begin_object();
            if let Json::Object(kinds) = &spec.value
                && let Some((kind, settings)) = kinds.first()
            {
                let field = settings.value.clone().into_object().and_then(|members| {
                    members
                        .into_iter()
                        .find(|(key, _)| key == "field")
                        .and_then(|(_, node)| match node.value {
                            Json::String(text) => Some(text),
                            _ => None,
                        })
                });
                let field = field.unwrap_or_default();
                match kind.as_str() {
                    "terms" => {
                        let size = settings.value.clone().into_object().and_then(|members| {
                            members.into_iter().find(|(key, _)| key == "size").and_then(
                                |(_, node)| match node.value {
                                    Json::Number(text) => text.parse().ok(),
                                    _ => None,
                                },
                            )
                        });
                        out.key("doc_count_error_upper_bound")
                            .integer(0)
                            .key("sum_other_doc_count")
                            .integer(0)
                            .key("buckets")
                            .begin_array();
                        for (key, numeric, count) in
                            term_buckets(matched, &field, size.unwrap_or(10))
                        {
                            out.begin_object().key("key");
                            if numeric {
                                out.raw(&key);
                            } else {
                                out.string(&key);
                            }
                            out.key("doc_count").integer(count).end_object();
                        }
                        out.end_array();
                    }
                    "avg" | "sum" | "min" | "max" | "value_count" => {
                        let values = numbers(matched, &field);
                        let value = match kind.as_str() {
                            "sum" => Some(values.iter().sum()),
                            "avg" => (!values.is_empty())
                                .then(|| values.iter().sum::<f64>() / values.len() as f64),
                            "min" => values.iter().copied().reduce(f64::min),
                            "max" => values.iter().copied().reduce(f64::max),
                            _ => Some(values.len() as f64),
                        };
                        out.key("value");
                        match value {
                            Some(value) => out.number(value),
                            None => out.null(),
                        };
                    }
                    _ => {}
                }
            }
            out.end_object();
        }
    }
    out.end_object();
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
    let mut context = Context {
        source: parsed.value,
        operation: Operation::Index,
        index,
        id,
        version,
    };
    script.run(&mut context)?;
    let changed = written(&Node {
        value: context.source,
        start: 0,
        end: 0,
    });
    Ok((context.operation, changed))
}

fn refreshed() -> Response {
    json(
        200,
        "{\"_shards\":{\"total\":1,\"successful\":1,\"failed\":0}}".to_owned(),
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
