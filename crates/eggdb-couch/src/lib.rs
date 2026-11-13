//! A CouchDB-compatible HTTP API over EggDB's storage, enough for CouchDB's replication protocol
//! (D48, D1109), so CouchDB, PouchDB and other replicators sync with EggDB both ways: databases,
//! documents with revision trees and deterministic winners, `_changes` (normal and longpoll),
//! `_revs_diff`, `_bulk_docs` (with `new_edits: false`), `_bulk_get`, `_all_docs`, open
//! revisions, and `_local` checkpoint documents, behind HTTP Basic authentication.
//! Attachments are not supported yet and are refused.

pub mod revs;
pub mod store;

use eggdb_http::{Request, Response};
use eggdb_sql::json::{Json, Node, parse_document};
use revs::{Rev, path_to, winner};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use store::Store;

const LONGPOLL_MOST: Duration = Duration::from_secs(60);
const LONGPOLL_STEP: Duration = Duration::from_millis(200);
const CHANGES_MOST: usize = 10_000;
const MAX_ID_BYTES: usize = 1_024;
/// The CouchDB version the API answers to, the one whose replication protocol it follows.
pub const COMPATIBLE_VERSION: &str = "3.3.3";

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
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn json(status: u16, body: String) -> Response {
    let mut response = Response::ok("application/json", body.into_bytes());
    response.status = status;
    response
}

fn error(status: u16, error: &str, reason: &str) -> Response {
    json(status, format!("{{\"error\":{},\"reason\":{}}}", json_string(error), json_string(reason)))
}

fn storage(message: String) -> Response {
    error(500, "internal_server_error", &message)
}

/// A document as a client sent it: its special `_` members apart from its body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SentDoc {
    /// `_id`.
    pub id: Option<String>,
    /// `_rev`.
    pub rev: Option<Rev>,
    /// `_deleted`.
    pub deleted: bool,
    /// `_revisions`: the newest revision's generation and the hashes from it back.
    pub revisions: Option<(u64, Vec<String>)>,
    /// Whether it carries `_attachments`.
    pub attachments: bool,
    /// Every other member, as a JSON object.
    pub body: String,
}

fn text_of(node: &Node) -> Option<&str> {
    match &node.value {
        Json::String(text) => Some(text),
        _ => None,
    }
}

/// Reads a document object from `text` (whose node `node` is).
pub fn read_doc(text: &str, node: &Node) -> Option<SentDoc> {
    let Json::Object(members) = &node.value else {
        return None;
    };
    let mut doc = SentDoc::default();
    let mut body = Vec::new();
    for (name, value) in members {
        match name.as_str() {
            "_id" => doc.id = Some(text_of(value)?.to_owned()),
            "_rev" => doc.rev = Some(Rev::parse(text_of(value)?)?),
            "_deleted" => doc.deleted = matches!(value.value, Json::Boolean(true)),
            "_attachments" => doc.attachments = true,
            "_revisions" => {
                let Json::Object(fields) = &value.value else {
                    return None;
                };
                let mut start = None;
                let mut ids = Vec::new();
                for (field, inner) in fields {
                    match (field.as_str(), &inner.value) {
                        ("start", Json::Number(number)) => start = number.parse::<u64>().ok(),
                        ("ids", Json::Array(items)) => {
                            for item in items {
                                ids.push(text_of(item)?.to_owned());
                            }
                        }
                        _ => {}
                    }
                }
                doc.revisions = Some((start?, ids));
            }
            special if special.starts_with('_') => {}
            _ => body.push(format!("{}:{}", json_string(name), text.get(value.start..value.end)?)),
        }
    }
    doc.body = format!("{{{}}}", body.join(","));
    Some(doc)
}

fn render_doc(id: &str, rev: &Rev, deleted: bool, body: &str, revisions: Option<&[Rev]>) -> String {
    let mut out = format!("{{\"_id\":{},\"_rev\":{}", json_string(id), json_string(&rev.text()));
    if deleted {
        out.push_str(",\"_deleted\":true");
    }
    if let Some(path) = revisions {
        let ids: Vec<String> = path.iter().map(|rev| json_string(&rev.hash)).collect();
        out.push_str(&format!(",\"_revisions\":{{\"start\":{},\"ids\":[{}]}}", rev.generation, ids.join(",")));
    }
    let inner = body.trim().trim_start_matches('{').trim_end_matches('}').trim();
    if !inner.is_empty() {
        out.push(',');
        out.push_str(inner);
    }
    out.push('}');
    out
}

fn query_map(query: Option<&str>) -> BTreeMap<String, String> {
    query
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((eggdb_http::percent_decoded(name)?, eggdb_http::percent_decoded(&value.replace('+', " "))?))
        })
        .collect()
}

fn valid_database_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(|first| first.is_ascii_lowercase())
        && name.len() <= 238
        && characters.all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || "_$()+-/".contains(character))
}

fn valid_doc_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.contains('\0') && (!id.starts_with('_') || id.starts_with("_design/"))
}

/// The CouchDB-compatible API of one EggDB server.
pub struct CouchEndpoint<'k> {
    store: Store<'k>,
    authenticate: &'k (dyn Fn(&str, &str) -> bool + Sync),
    uuid: String,
}

impl std::fmt::Debug for CouchEndpoint<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CouchEndpoint").field("uuid", &self.uuid).finish_non_exhaustive()
    }
}

impl<'k> CouchEndpoint<'k> {
    /// The API over the databases kept in `keyspace`, letting in the requests whose Basic
    /// credentials `authenticate` accepts; `uuid` names this server to replicators.
    pub fn new(keyspace: &'k (dyn eggdb_keyspace::Keyspace + Sync), authenticate: &'k (dyn Fn(&str, &str) -> bool + Sync), uuid: &str) -> Self {
        Self { store: Store::new(keyspace), authenticate, uuid: uuid.to_owned() }
    }

    fn signed_in(&self, request: &Request) -> bool {
        let Some(encoded) = request.header("authorization").and_then(|value| value.strip_prefix("Basic ")) else {
            return false;
        };
        let Ok(decoded) = eggdb_crypto::base64_decode(encoded.trim()) else {
            return false;
        };
        let Ok(text) = String::from_utf8(decoded) else {
            return false;
        };
        text.split_once(':').is_some_and(|(user, password)| (self.authenticate)(user, password))
    }

    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        if !self.signed_in(request) {
            return error(401, "unauthorized", "Name or password is incorrect.").with_header("WWW-Authenticate", "Basic realm=\"eggdb\"");
        }
        let segments: Option<Vec<String>> = request
            .path
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(eggdb_http::percent_decoded)
            .collect();
        let Some(segments) = segments else {
            return error(400, "bad_request", "the path is not valid percent-encoded text");
        };
        let query = query_map(request.query.as_deref());
        let method = request.method.as_str();
        let parts: Vec<&str> = segments.iter().map(String::as_str).collect();
        match parts.as_slice() {
            [] => json(200, format!(
                "{{\"couchdb\":\"Welcome\",\"version\":\"{COMPATIBLE_VERSION}\",\"uuid\":{},\"vendor\":{{\"name\":\"EggDB\"}},\"features\":[]}}",
                json_string(&self.uuid)
            )),
            ["_up"] => json(200, "{\"status\":\"ok\"}".to_owned()),
            ["_all_dbs"] => match self.store.databases() {
                Ok(names) => json(200, format!("[{}]", names.iter().map(|name| json_string(name)).collect::<Vec<_>>().join(","))),
                Err(message) => storage(message),
            },
            ["_uuids"] => {
                let count = query.get("count").and_then(|count| count.parse::<usize>().ok()).unwrap_or(1).clamp(1, 1_000);
                let mut ids = Vec::with_capacity(count);
                for _ in 0..count {
                    let Some(id) = random_id() else {
                        return error(500, "internal_server_error", "no random source");
                    };
                    ids.push(json_string(&id));
                }
                json(200, format!("{{\"uuids\":[{}]}}", ids.join(",")))
            }
            [name] => self.database_request(method, name, request),
            [name, rest @ ..] => {
                let Some(db) = (match self.store.database(name) {
                    Ok(db) => db,
                    Err(message) => return storage(message),
                }) else {
                    return error(404, "not_found", "Database does not exist.");
                };
                match rest {
                    ["_changes"] => self.changes(db, &query, request),
                    ["_revs_diff"] if method == "POST" => self.revs_diff(db, request),
                    ["_bulk_docs"] if method == "POST" => self.bulk_docs(db, request),
                    ["_bulk_get"] if method == "POST" => self.bulk_get(db, &query, request),
                    ["_ensure_full_commit"] if method == "POST" => json(201, "{\"ok\":true,\"instance_start_time\":\"0\"}".to_owned()),
                    ["_all_docs"] => self.all_docs(db, &query),
                    ["_local", id] => self.local(db, method, id, request),
                    ["_design", design] => self.document(db, method, &format!("_design/{design}"), &query, request),
                    [id] => self.document(db, method, id, &query, request),
                    _ => error(404, "not_found", "missing"),
                }
            }
        }
    }

    fn database_request(&self, method: &str, name: &str, request: &Request) -> Response {
        match method {
            "PUT" => {
                if !valid_database_name(name) {
                    return error(400, "illegal_database_name", "Name must begin with a letter and hold only lowercase letters, digits and _$()+-/.");
                }
                match self.store.create_database(name) {
                    Ok(true) => json(201, "{\"ok\":true}".to_owned()),
                    Ok(false) => error(412, "file_exists", "The database could not be created, the file already exists."),
                    Err(message) => storage(message),
                }
            }
            "DELETE" => match self.store.delete_database(name) {
                Ok(true) => json(200, "{\"ok\":true}".to_owned()),
                Ok(false) => error(404, "not_found", "Database does not exist."),
                Err(message) => storage(message),
            },
            "GET" | "HEAD" => match self.store.database(name) {
                Ok(Some(db)) => match self.store.info(db) {
                    Ok(info) => json(200, format!(
                        "{{\"db_name\":{},\"doc_count\":{},\"doc_del_count\":{},\"update_seq\":\"{}\",\"instance_start_time\":\"0\",\"purge_seq\":\"0\"}}",
                        json_string(name),
                        info.doc_count,
                        info.doc_del_count,
                        info.update_seq
                    )),
                    Err(message) => storage(message),
                },
                Ok(None) => error(404, "not_found", "Database does not exist."),
                Err(message) => storage(message),
            },
            "POST" => match self.store.database(name) {
                Ok(Some(db)) => {
                    let Some(text) = std::str::from_utf8(&request.body).ok() else {
                        return error(400, "bad_request", "the body is not UTF-8");
                    };
                    let Some(mut doc) = parse_document(text).ok().and_then(|node| read_doc(text, &node)) else {
                        return error(400, "bad_request", "the body is not a JSON document object");
                    };
                    let Some(id) = doc.id.take().or_else(random_id) else {
                        return error(500, "internal_server_error", "no random source");
                    };
                    self.edit(db, &id, &doc)
                }
                Ok(None) => error(404, "not_found", "Database does not exist."),
                Err(message) => storage(message),
            },
            _ => error(405, "method_not_allowed", "Only GET, HEAD, PUT, POST and DELETE are allowed."),
        }
    }

    fn edit_rev(&self, db: u32, id: &str, doc: &SentDoc) -> Result<Rev, Response> {
        if doc.attachments {
            return Err(error(415, "bad_content_type", "Attachments are not supported yet."));
        }
        if !valid_doc_id(id) {
            return Err(error(400, "illegal_docid", "Only reserved document ids may start with underscore."));
        }
        let info = self.store.doc(db, id).map_err(storage)?;
        let current = info.as_ref().and_then(|info| winner(&info.revs).cloned());
        let parent = match (&current, &doc.rev) {
            (Some(current), Some(given)) if &current.rev == given => Some(given.clone()),
            (Some(current), None) if current.deleted => Some(current.rev.clone()),
            (None, None) => None,
            _ => return Err(error(409, "conflict", "Document update conflict.")),
        };
        let rev = Rev {
            generation: parent.as_ref().map_or(1, |parent| parent.generation + 1),
            hash: revs::new_hash(parent.as_ref(), doc.deleted, &doc.body),
        };
        let mut path = vec![rev.clone()];
        path.extend(parent);
        self.store.add_path(db, id, &path, doc.deleted, &doc.body).map_err(storage)?;
        Ok(rev)
    }

    fn edit(&self, db: u32, id: &str, doc: &SentDoc) -> Response {
        match self.edit_rev(db, id, doc) {
            Ok(rev) => json(201, format!("{{\"ok\":true,\"id\":{},\"rev\":{}}}", json_string(id), json_string(&rev.text()))),
            Err(response) => response,
        }
    }

    fn replicate(&self, db: u32, id: &str, doc: &SentDoc) -> Result<(), Response> {
        if doc.attachments {
            return Err(error(415, "bad_content_type", "Attachments are not supported yet."));
        }
        if !valid_doc_id(id) {
            return Err(error(400, "illegal_docid", "Only reserved document ids may start with underscore."));
        }
        let path: Vec<Rev> = match (&doc.revisions, &doc.rev) {
            (Some((start, ids)), _) => ids
                .iter()
                .enumerate()
                .map(|(at, hash)| Rev::parse(&format!("{}-{hash}", start.saturating_sub(at as u64))))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| error(400, "bad_request", "_revisions holds an invalid revision"))?,
            (None, Some(rev)) => vec![rev.clone()],
            (None, None) => return Err(error(400, "bad_request", "a replicated document needs _rev")),
        };
        self.store.add_path(db, id, &path, doc.deleted, &doc.body).map(|_| ()).map_err(storage)
    }

    fn document(&self, db: u32, method: &str, id: &str, query: &BTreeMap<String, String>, request: &Request) -> Response {
        match method {
            "GET" | "HEAD" => self.read_document(db, id, query),
            "PUT" => {
                let Some(text) = std::str::from_utf8(&request.body).ok() else {
                    return error(400, "bad_request", "the body is not UTF-8");
                };
                let Some(mut doc) = parse_document(text).ok().and_then(|node| read_doc(text, &node)) else {
                    return error(400, "bad_request", "the body is not a JSON document object");
                };
                if query.get("new_edits").is_some_and(|value| value == "false") {
                    return match self.replicate(db, id, &doc) {
                        Ok(()) => json(201, format!("{{\"ok\":true,\"id\":{}}}", json_string(id))),
                        Err(response) => response,
                    };
                }
                if doc.rev.is_none() {
                    doc.rev = query.get("rev").and_then(|rev| Rev::parse(rev));
                }
                self.edit(db, id, &doc)
            }
            "DELETE" => {
                let doc = SentDoc { rev: query.get("rev").and_then(|rev| Rev::parse(rev)), deleted: true, body: "{}".to_owned(), ..SentDoc::default() };
                if doc.rev.is_none() {
                    return error(409, "conflict", "Document update conflict.");
                }
                match self.edit_rev(db, id, &doc) {
                    Ok(rev) => json(200, format!("{{\"ok\":true,\"id\":{},\"rev\":{}}}", json_string(id), json_string(&rev.text()))),
                    Err(response) => response,
                }
            }
            _ => error(405, "method_not_allowed", "Only GET, HEAD, PUT and DELETE are allowed."),
        }
    }

    fn rendered(&self, db: u32, id: &str, info: &store::DocInfo, rev: &Rev, revisions: bool) -> Result<Option<String>, String> {
        let Some(node) = info.revs.iter().find(|node| &node.rev == rev) else {
            return Ok(None);
        };
        let Some(body) = self.store.body(db, id, rev)? else {
            return Ok(None);
        };
        let path = revisions.then(|| path_to(&info.revs, rev));
        Ok(Some(render_doc(id, rev, node.deleted, &body, path.as_deref())))
    }

    fn read_document(&self, db: u32, id: &str, query: &BTreeMap<String, String>) -> Response {
        let info = match self.store.doc(db, id) {
            Ok(Some(info)) => info,
            Ok(None) => return error(404, "not_found", "missing"),
            Err(message) => return storage(message),
        };
        let revisions = query.get("revs").is_some_and(|value| value == "true");
        if let Some(open) = query.get("open_revs") {
            let wanted: Vec<String> = if open == "all" {
                revs::leaves(&info.revs).into_iter().map(|node| node.rev.text()).collect()
            } else {
                let Some(node) = parse_document(open).ok() else {
                    return error(400, "bad_request", "open_revs must be all or a JSON array of revisions");
                };
                let Json::Array(items) = node.value else {
                    return error(400, "bad_request", "open_revs must be all or a JSON array of revisions");
                };
                items.iter().filter_map(|item| text_of(item).map(str::to_owned)).collect()
            };
            let mut answers = Vec::new();
            for text in wanted {
                let rendered = Rev::parse(&text).map(|rev| self.rendered(db, id, &info, &rev, revisions)).transpose();
                match rendered {
                    Ok(Some(Some(doc))) => answers.push(format!("{{\"ok\":{doc}}}")),
                    Ok(_) => answers.push(format!("{{\"missing\":{}}}", json_string(&text))),
                    Err(message) => return storage(message),
                }
            }
            return json(200, format!("[{}]", answers.join(",")));
        }
        let rev = match query.get("rev") {
            Some(text) => match Rev::parse(text) {
                Some(rev) => rev,
                None => return error(400, "bad_request", "Invalid rev format"),
            },
            None => match winner(&info.revs) {
                Some(node) if !node.deleted => node.rev.clone(),
                _ => return error(404, "not_found", "deleted"),
            },
        };
        match self.rendered(db, id, &info, &rev, revisions) {
            Ok(Some(doc)) => json(200, doc),
            Ok(None) => error(404, "not_found", "missing"),
            Err(message) => storage(message),
        }
    }

    fn changes(&self, db: u32, query: &BTreeMap<String, String>, request: &Request) -> Response {
        let since = match query.get("since").map(String::as_str) {
            Some("now") => match self.store.info(db) {
                Ok(info) => info.update_seq,
                Err(message) => return storage(message),
            },
            Some(text) => text.split('-').next().and_then(|number| number.parse::<u64>().ok()).unwrap_or(0),
            None => 0,
        };
        let limit = query.get("limit").and_then(|limit| limit.parse::<usize>().ok()).unwrap_or(CHANGES_MOST).clamp(1, CHANGES_MOST);
        let all_leaves = query.get("style").is_some_and(|style| style == "all_docs");
        let include_docs = query.get("include_docs").is_some_and(|value| value == "true");
        let longpoll = matches!(query.get("feed").map(String::as_str), Some("longpoll" | "continuous"));
        let wait = query
            .get("timeout")
            .and_then(|timeout| timeout.parse::<u64>().ok())
            .map_or(LONGPOLL_MOST, Duration::from_millis)
            .min(LONGPOLL_MOST);
        let filter_ids: Option<Vec<String>> = (request.method == "POST")
            .then(|| std::str::from_utf8(&request.body).ok())
            .flatten()
            .and_then(|text| parse_document(text).ok())
            .and_then(|node| match node.value {
                Json::Object(members) => members.into_iter().find(|(name, _)| name == "doc_ids").map(|(_, ids)| ids),
                _ => None,
            })
            .and_then(|ids| match ids.value {
                Json::Array(items) => Some(items.iter().filter_map(|item| text_of(item).map(str::to_owned)).collect()),
                _ => None,
            });
        let started = Instant::now();
        let found = loop {
            let found = match self.store.changes(db, since, limit) {
                Ok(found) => found,
                Err(message) => return storage(message),
            };
            let found: Vec<(u64, String)> = match &filter_ids {
                Some(ids) => found.into_iter().filter(|(_, id)| ids.contains(id)).collect(),
                None => found,
            };
            if !found.is_empty() || !longpoll || started.elapsed() >= wait {
                break found;
            }
            std::thread::sleep(LONGPOLL_STEP);
        };
        let mut results = Vec::with_capacity(found.len());
        let mut last_seq = since;
        for (seq, id) in &found {
            last_seq = *seq;
            let info = match self.store.doc(db, id) {
                Ok(Some(info)) => info,
                Ok(None) => continue,
                Err(message) => return storage(message),
            };
            let Some(current) = winner(&info.revs) else {
                continue;
            };
            let listed: Vec<String> = if all_leaves {
                revs::leaves(&info.revs).into_iter().map(|node| format!("{{\"rev\":{}}}", json_string(&node.rev.text()))).collect()
            } else {
                vec![format!("{{\"rev\":{}}}", json_string(&current.rev.text()))]
            };
            let mut entry = format!("{{\"seq\":\"{seq}\",\"id\":{},\"changes\":[{}]", json_string(id), listed.join(","));
            if current.deleted {
                entry.push_str(",\"deleted\":true");
            }
            if include_docs {
                match self.rendered(db, id, &info, &current.rev, false) {
                    Ok(Some(doc)) => entry.push_str(&format!(",\"doc\":{doc}")),
                    Ok(None) => {}
                    Err(message) => return storage(message),
                }
            }
            entry.push('}');
            results.push(entry);
        }
        let pending = match self.store.changes(db, last_seq, CHANGES_MOST) {
            Ok(rest) => rest.len(),
            Err(message) => return storage(message),
        };
        json(200, format!("{{\"results\":[{}],\"last_seq\":\"{last_seq}\",\"pending\":{pending}}}", results.join(",")))
    }

    fn revs_diff(&self, db: u32, request: &Request) -> Response {
        let Some(text) = std::str::from_utf8(&request.body).ok() else {
            return error(400, "bad_request", "the body is not UTF-8");
        };
        let Some(Json::Object(members)) = parse_document(text).ok().map(|node| node.value) else {
            return error(400, "bad_request", "the body must be an object of document ids and revisions");
        };
        let mut answers = Vec::new();
        for (id, listed) in members {
            let Json::Array(items) = listed.value else {
                return error(400, "bad_request", "each document id needs an array of revisions");
            };
            let known = match self.store.doc(db, &id) {
                Ok(info) => info.map(|info| info.revs).unwrap_or_default(),
                Err(message) => return storage(message),
            };
            let missing: Vec<String> = items
                .iter()
                .filter_map(|item| text_of(item).map(str::to_owned))
                .filter(|text| !Rev::parse(text).is_some_and(|rev| known.iter().any(|node| node.rev == rev)))
                .collect();
            if !missing.is_empty() {
                let quoted: Vec<String> = missing.iter().map(|rev| json_string(rev)).collect();
                answers.push(format!("{}:{{\"missing\":[{}]}}", json_string(&id), quoted.join(",")));
            }
        }
        json(200, format!("{{{}}}", answers.join(",")))
    }

    fn bulk_docs(&self, db: u32, request: &Request) -> Response {
        let Some(text) = std::str::from_utf8(&request.body).ok() else {
            return error(400, "bad_request", "the body is not UTF-8");
        };
        let Some(Json::Object(members)) = parse_document(text).ok().map(|node| node.value) else {
            return error(400, "bad_request", "the body must be an object with docs");
        };
        let new_edits = !members.iter().any(|(name, value)| name == "new_edits" && matches!(value.value, Json::Boolean(false)));
        let Some(docs) = members.iter().find(|(name, _)| name == "docs").map(|(_, docs)| docs) else {
            return error(400, "bad_request", "POST body must include `docs` parameter.");
        };
        let Json::Array(items) = &docs.value else {
            return error(400, "bad_request", "`docs` parameter must be an array.");
        };
        let mut answers = Vec::new();
        for item in items {
            let Some(mut doc) = read_doc(text, item) else {
                answers.push("{\"error\":\"bad_request\",\"reason\":\"Document must be a JSON object\"}".to_owned());
                continue;
            };
            if new_edits {
                let Some(id) = doc.id.take().or_else(random_id) else {
                    return error(500, "internal_server_error", "no random source");
                };
                match self.edit_rev(db, &id, &doc) {
                    Ok(rev) => answers.push(format!("{{\"ok\":true,\"id\":{},\"rev\":{}}}", json_string(&id), json_string(&rev.text()))),
                    Err(response) => answers.push(format!(
                        "{{\"id\":{},\"error\":{}}}",
                        json_string(&id),
                        if response.status == 409 { "\"conflict\"" } else { "\"forbidden\"" }
                    )),
                }
            } else {
                let Some(id) = doc.id.clone() else {
                    answers.push("{\"error\":\"bad_request\",\"reason\":\"a replicated document needs _id\"}".to_owned());
                    continue;
                };
                if self.replicate(db, &id, &doc).is_err() {
                    answers.push(format!("{{\"id\":{},\"error\":\"forbidden\"}}", json_string(&id)));
                }
            }
        }
        json(201, format!("[{}]", answers.join(",")))
    }

    fn bulk_get(&self, db: u32, query: &BTreeMap<String, String>, request: &Request) -> Response {
        let Some(text) = std::str::from_utf8(&request.body).ok() else {
            return error(400, "bad_request", "the body is not UTF-8");
        };
        let Some(Json::Object(members)) = parse_document(text).ok().map(|node| node.value) else {
            return error(400, "bad_request", "the body must be an object with docs");
        };
        let Some(Json::Array(items)) = members.into_iter().find(|(name, _)| name == "docs").map(|(_, docs)| docs.value) else {
            return error(400, "bad_request", "`docs` parameter must be an array.");
        };
        let revisions = query.get("revs").is_some_and(|value| value == "true");
        let mut results = Vec::new();
        for item in items {
            let Json::Object(fields) = item.value else {
                continue;
            };
            let id = fields.iter().find(|(name, _)| name == "id").and_then(|(_, value)| text_of(value)).map(str::to_owned).unwrap_or_default();
            let wanted = fields.iter().find(|(name, _)| name == "rev").and_then(|(_, value)| text_of(value)).and_then(Rev::parse);
            let answer = match self.store.doc(db, &id) {
                Ok(Some(info)) => {
                    let rev = wanted.or_else(|| winner(&info.revs).map(|node| node.rev.clone()));
                    match rev.map(|rev| self.rendered(db, &id, &info, &rev, revisions)) {
                        Some(Ok(Some(doc))) => format!("{{\"ok\":{doc}}}"),
                        Some(Err(message)) => return storage(message),
                        _ => format!("{{\"error\":{{\"id\":{},\"error\":\"not_found\",\"reason\":\"missing\"}}}}", json_string(&id)),
                    }
                }
                Ok(None) => format!("{{\"error\":{{\"id\":{},\"error\":\"not_found\",\"reason\":\"missing\"}}}}", json_string(&id)),
                Err(message) => return storage(message),
            };
            results.push(format!("{{\"id\":{},\"docs\":[{answer}]}}", json_string(&id)));
        }
        json(200, format!("{{\"results\":[{}]}}", results.join(",")))
    }

    fn all_docs(&self, db: u32, query: &BTreeMap<String, String>) -> Response {
        let include_docs = query.get("include_docs").is_some_and(|value| value == "true");
        let ids = match self.store.doc_ids(db) {
            Ok(ids) => ids,
            Err(message) => return storage(message),
        };
        let mut rows = Vec::new();
        for id in ids {
            let info = match self.store.doc(db, &id) {
                Ok(Some(info)) => info,
                Ok(None) => continue,
                Err(message) => return storage(message),
            };
            let Some(current) = winner(&info.revs).filter(|node| !node.deleted) else {
                continue;
            };
            let mut row = format!("{{\"id\":{0},\"key\":{0},\"value\":{{\"rev\":{1}}}", json_string(&id), json_string(&current.rev.text()));
            if include_docs {
                match self.rendered(db, &id, &info, &current.rev, false) {
                    Ok(Some(doc)) => row.push_str(&format!(",\"doc\":{doc}")),
                    Ok(None) => {}
                    Err(message) => return storage(message),
                }
            }
            row.push('}');
            rows.push(row);
        }
        json(200, format!("{{\"total_rows\":{},\"offset\":0,\"rows\":[{}]}}", rows.len(), rows.join(",")))
    }

    fn local(&self, db: u32, method: &str, id: &str, request: &Request) -> Response {
        let full_id = format!("_local/{id}");
        match method {
            "GET" => match self.store.local(db, id) {
                Ok(Some(body)) => json(200, render_doc(&full_id, &Rev { generation: 0, hash: "1".to_owned() }, false, &body, None)),
                Ok(None) => error(404, "not_found", "missing"),
                Err(message) => storage(message),
            },
            "PUT" => {
                let Some(text) = std::str::from_utf8(&request.body).ok() else {
                    return error(400, "bad_request", "the body is not UTF-8");
                };
                let Some(doc) = parse_document(text).ok().and_then(|node| read_doc(text, &node)) else {
                    return error(400, "bad_request", "the body is not a JSON document object");
                };
                match self.store.set_local(db, id, Some(&doc.body)) {
                    Ok(()) => json(201, format!("{{\"ok\":true,\"id\":{},\"rev\":\"0-1\"}}", json_string(&full_id))),
                    Err(message) => storage(message),
                }
            }
            "DELETE" => match self.store.set_local(db, id, None) {
                Ok(()) => json(200, format!("{{\"ok\":true,\"id\":{},\"rev\":\"0-0\"}}", json_string(&full_id))),
                Err(message) => storage(message),
            },
            _ => error(405, "method_not_allowed", "Only GET, PUT and DELETE are allowed."),
        }
    }
}

fn random_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    eggdb_platform::secure_random::fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests;
