use super::{
    Answer, Api, Failure, document_key, error, error_cause, index_key, indices_of, json, member_of,
    over_memory_budget, query_flag, read, unstored, valid_index, validation,
};
use crate::json_out::escaped;
use crate::source_filter::{SourceFilter, matches};
use eggdb_http::{Request, Response};
use eggdb_sql::json::{Json, Node, parse_document};

const MAX_SEARCHES_PER_MSEARCH: usize = 1000;

struct Wanted {
    index: String,
    id: String,
    filter: SourceFilter,
}

enum Fetched {
    Document(u64, String),
    Missing,
    Refused(&'static str, String),
}

struct Header {
    indices: Vec<String>,
    ignore_unavailable: bool,
}

fn text_of(node: &Node) -> Option<String> {
    match &node.value {
        Json::String(text) | Json::Number(text) => Some(text.clone()),
        _ => None,
    }
}

pub(super) fn flag_filter(request: &Request) -> SourceFilter {
    let includes =
        query_flag(request, "_source_includes").or_else(|| query_flag(request, "_source_include"));
    let excludes =
        query_flag(request, "_source_excludes").or_else(|| query_flag(request, "_source_exclude"));
    SourceFilter::from_flags(
        query_flag(request, "_source").as_deref(),
        includes.as_deref(),
        excludes.as_deref(),
    )
}

fn header_of(line: &str) -> Result<Header, Response> {
    let mut header = Header {
        indices: Vec::new(),
        ignore_unavailable: false,
    };
    if line.trim().is_empty() {
        return Ok(header);
    }
    let parsed = parse_document(line).map_err(|syntax| {
        error(
            400,
            "parse_exception",
            &format!(
                "failed to parse an msearch header: {} at byte {}",
                syntax.message, syntax.position
            ),
        )
    })?;
    let Json::Object(members) = &parsed.value else {
        return Err(error(
            400,
            "illegal_argument_exception",
            "an msearch header must be a JSON object",
        ));
    };
    for (key, value) in members {
        match (key.as_str(), &value.value) {
            ("index", Json::String(names)) => {
                header.indices = names
                    .split(',')
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
                    .collect();
            }
            ("index", Json::Array(items)) => {
                header.indices = items.iter().filter_map(text_of).collect();
            }
            ("ignore_unavailable", Json::Boolean(flag)) => header.ignore_unavailable = *flag,
            ("ignore_unavailable", Json::String(flag)) => {
                header.ignore_unavailable = flag == "true";
            }
            _ => {}
        }
    }
    Ok(header)
}

impl Api<'_> {
    pub(super) fn mget(&self, default_index: Option<&str>, request: &Request) -> Answer {
        let body = String::from_utf8_lossy(&request.body).into_owned();
        if body.trim().is_empty() {
            return Ok(validation(&["no documents to get".to_owned()]));
        }
        let parsed = parse_document(&body)?;
        let defaults = flag_filter(request);
        let mut wanted = Vec::new();
        let mut problems = Vec::new();
        if let Some(docs) = member_of(&parsed, "docs") {
            let Json::Array(items) = &docs.value else {
                return Ok(error(400, "parse_exception", "[docs] must be an array"));
            };
            for (at, item) in items.iter().enumerate() {
                if !matches!(item.value, Json::Object(_)) {
                    return Ok(error(
                        400,
                        "parse_exception",
                        "each entry of [docs] must be an object",
                    ));
                }
                let index = member_of(item, "_index")
                    .as_ref()
                    .and_then(text_of)
                    .or_else(|| default_index.map(str::to_owned));
                let id = member_of(item, "_id").as_ref().and_then(text_of);
                let filter = match member_of(item, "_source") {
                    Some(node) => match SourceFilter::from_node(&node) {
                        Ok(filter) => filter,
                        Err(reason) => return Ok(error(400, "parse_exception", &reason)),
                    },
                    None => defaults.clone(),
                };
                match (index, id) {
                    (Some(index), Some(id)) => wanted.push(Wanted { index, id, filter }),
                    (index, id) => {
                        if index.is_none() {
                            problems.push(format!("index is missing for doc {at}"));
                        }
                        if id.is_none() {
                            problems.push(format!("id is missing for doc {at}"));
                        }
                    }
                }
            }
        } else if let Some(ids) = member_of(&parsed, "ids") {
            let Json::Array(items) = &ids.value else {
                return Ok(error(400, "parse_exception", "[ids] must be an array"));
            };
            for (at, item) in items.iter().enumerate() {
                let Some(id) = text_of(item) else {
                    return Ok(error(
                        400,
                        "parse_exception",
                        "each entry of [ids] must be a string",
                    ));
                };
                match default_index {
                    Some(index) => wanted.push(Wanted {
                        index: index.to_owned(),
                        id,
                        filter: defaults.clone(),
                    }),
                    None => problems.push(format!("index is missing for doc {at}")),
                }
            }
        }
        if wanted.is_empty() && problems.is_empty() {
            problems.push("no documents to get".to_owned());
        }
        if !problems.is_empty() {
            return Ok(validation(&problems));
        }
        let mut reservation = self.memory_budget.empty_reservation();
        let mut over_budget = false;
        let fetched = read(self.keyspace, |keys| {
            let mut fetched = Vec::with_capacity(wanted.len());
            let mut held = 0usize;
            for item in &wanted {
                if !valid_index(&item.index) {
                    fetched.push(Fetched::Refused(
                        "invalid_index_name_exception",
                        format!("Invalid index name [{}]", item.index),
                    ));
                    continue;
                }
                if keys.get(&index_key(&item.index))?.is_none() {
                    fetched.push(Fetched::Refused(
                        "index_not_found_exception",
                        format!("no such index [{}]", item.index),
                    ));
                    continue;
                }
                match keys
                    .get(&document_key(&item.index, &item.id))?
                    .as_deref()
                    .and_then(unstored)
                    .filter(|(_, source)| crate::permissions::source_visible(source))
                {
                    Some((version, source)) => {
                        held = held.saturating_add(source.len());
                        if !reservation.hold_if_large(held) {
                            over_budget = true;
                            return Ok(Vec::new());
                        }
                        fetched.push(Fetched::Document(version, source));
                    }
                    None => fetched.push(Fetched::Missing),
                }
            }
            Ok(fetched)
        })?;
        if over_budget {
            return Ok(over_memory_budget());
        }
        let mut out = String::from("{\"docs\":[");
        for (at, (item, found)) in wanted.iter().zip(fetched).enumerate() {
            if at > 0 {
                out.push(',');
            }
            let named = format!(
                "{{\"_index\":{},\"_id\":{}",
                escaped(&item.index),
                escaped(&item.id)
            );
            out.push_str(&named);
            match found {
                Fetched::Document(version, source) => {
                    out.push_str(&format!(
                        ",\"_version\":{version},\"_seq_no\":{},\"_primary_term\":1,\"found\":true",
                        version.saturating_sub(1)
                    ));
                    if let Some(filtered) = item.filter.apply(&source) {
                        out.push_str(",\"_source\":");
                        out.push_str(&filtered);
                    }
                }
                Fetched::Missing => out.push_str(",\"found\":false"),
                Fetched::Refused(kind, reason) => {
                    out.push_str(",\"error\":");
                    out.push_str(&error_cause(kind, &reason));
                }
            }
            out.push('}');
        }
        out.push_str("]}");
        Ok(json(200, out))
    }

    pub(super) fn msearch(&self, default_index: Option<&str>, request: &Request) -> Answer {
        self.msearch_of(default_index, request, false)
    }

    pub(super) fn msearch_template(&self, default_index: Option<&str>, request: &Request) -> Answer {
        self.msearch_of(default_index, request, true)
    }

    fn msearch_of(&self, default_index: Option<&str>, request: &Request, templated: bool) -> Answer {
        let started = std::time::Instant::now();
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let mut lines: Vec<&str> = body
            .split('\n')
            .map(|line| line.trim_end_matches('\r'))
            .collect();
        while lines.last().is_some_and(|line| line.trim().is_empty()) {
            lines.pop();
        }
        if lines.is_empty() {
            return Ok(validation(&["no requests added".to_owned()]));
        }
        if lines.len() / 2 > MAX_SEARCHES_PER_MSEARCH {
            return Ok(error(
                400,
                "illegal_argument_exception",
                &format!(
                    "an msearch request holds at most {MAX_SEARCHES_PER_MSEARCH} searches; split it into several requests"
                ),
            ));
        }
        if lines.len() % 2 == 1 {
            return Ok(error(
                400,
                "illegal_argument_exception",
                "the last msearch header has no search body line after it",
            ));
        }
        let mut searches = Vec::with_capacity(lines.len() / 2);
        for pair in lines.chunks(2) {
            let [header_line, search_line] = pair else {
                continue;
            };
            let header = match header_of(header_line) {
                Ok(header) => header,
                Err(response) => return Ok(response),
            };
            if !search_line.trim().is_empty() {
                parse_document(search_line)?;
            }
            let search = if templated {
                let mut inner = request.clone();
                inner.body = search_line.as_bytes().to_vec();
                self.rendered_template(&inner)
            } else {
                Ok((*search_line).to_owned())
            };
            searches.push((header, search));
        }
        let mut reservation = self.memory_budget.empty_reservation();
        let mut held = 0usize;
        let mut responses = Vec::with_capacity(searches.len());
        for (header, search_line) in searches {
            let indices = if header.indices.is_empty() {
                default_index
                    .map(|index| vec![index.to_owned()])
                    .unwrap_or_default()
            } else {
                header.indices
            };
            let answer = match &search_line {
                Ok(line) => self.one_search(&indices, header.ignore_unavailable, line),
                Err(refusal) => refusal.clone(),
            };
            let text = String::from_utf8_lossy(&answer.body).into_owned();
            let item = if answer.status < 300 {
                format!(
                    "{},\"status\":{}}}",
                    text.strip_suffix('}').unwrap_or(&text),
                    answer.status
                )
            } else {
                text
            };
            held = held.saturating_add(item.len());
            if !reservation.hold_if_large(held) {
                return Ok(over_memory_budget());
            }
            responses.push(item);
        }
        let took = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        Ok(json(
            200,
            format!(
                "{{\"took\":{took},\"responses\":[{}]}}",
                responses.join(",")
            ),
        ))
    }

    fn one_search(&self, indices: &[String], ignore_unavailable: bool, body: &str) -> Response {
        if let Some(invalid) = indices
            .iter()
            .find(|index| *index != "_all" && !valid_index(&index.replace('*', "x")))
        {
            return error(
                400,
                "invalid_index_name_exception",
                &format!("Invalid index name [{invalid}]"),
            );
        }
        let everything =
            indices.is_empty() || indices.iter().any(|index| index == "_all" || index == "*");
        let names = if everything {
            Vec::new()
        } else {
            match self.expanded(indices) {
                Ok(names) => names,
                Err(reason) => return error(500, "eggdb_exception", &reason),
            }
        };
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let search = Request {
            method: "POST".to_owned(),
            path: String::new(),
            query: ignore_unavailable.then(|| "ignore_unavailable=true".to_owned()),
            headers: Vec::new(),
            body: body.as_bytes().to_vec(),
        };
        let scope = (!everything).then_some(names.as_slice());
        match self.search(scope, &search, false) {
            Ok(response) => response,
            Err(Failure::BadRequest(reason)) => error(400, "parse_exception", &reason),
            Err(Failure::Storage(reason)) => error(500, "eggdb_exception", &reason),
        }
    }

    fn expanded(&self, indices: &[String]) -> Result<Vec<String>, String> {
        if !indices.iter().any(|index| index.contains('*')) {
            return Ok(indices.to_vec());
        }
        let existing = read(self.keyspace, |keys| indices_of(keys))?;
        let mut names: Vec<String> = Vec::new();
        for index in indices {
            if index.contains('*') {
                for name in existing.iter().filter(|name| matches(index, name)) {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
            } else if !names.contains(index) {
                names.push(index.clone());
            }
        }
        Ok(names)
    }
}
