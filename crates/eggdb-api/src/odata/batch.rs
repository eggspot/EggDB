//! OData's `$batch` (D1176): several requests in one `POST`, as `multipart/mixed` (OData 4.0,
//! with change sets) or as JSON (OData 4.01, with atomicity groups). Reads answer one by
//! one; the writes of a change set or an atomicity group run in one transaction, so all of
//! them happen or none does, and a later request of the set may name an entity an earlier one
//! inserted by its `Content-ID` (`$1`).

use super::{EntitySet, Resolved, odata_error, rows_of, sql_error};
use crate::{Endpoint, json_string};
use eggdb_http::{Request, Response};
use eggdb_sql::json::{Json, Node, parse_document};
use eggdb_sql::{KeyValueStore, QueryResult, SqlError};
use std::collections::HashMap;

const MOST_REQUESTS: usize = 1000;

struct Part {
    content_id: Option<String>,
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

enum Item {
    Single(Part),
    Atomic(Option<String>, Vec<Part>),
}

struct Answer {
    content_id: Option<String>,
    response: Response,
}

type Execute<'e> = dyn FnMut(&str) -> Result<Vec<QueryResult>, SqlError> + 'e;

fn bad_request(message: &str) -> Response {
    odata_error(400, "BadRequest", message)
}

fn boundary_of(content_type: &str) -> Option<String> {
    content_type.split(';').map(str::trim).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| value.trim().trim_matches('"').to_owned())
            .filter(|value| !value.is_empty())
    })
}

fn positions_of(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return Vec::new();
    }
    let mut found = Vec::new();
    let mut at = 0;
    while at + needle.len() <= haystack.len() {
        if &haystack[at..at + needle.len()] == needle {
            found.push(at);
            at += needle.len();
        } else {
            at += 1;
        }
    }
    found
}

fn without_line_breaks(mut bytes: &[u8]) -> &[u8] {
    while let Some(rest) = bytes.strip_prefix(b"\r\n").or_else(|| bytes.strip_prefix(b"\n")) {
        bytes = rest;
    }
    while let Some(rest) = bytes.strip_suffix(b"\r\n").or_else(|| bytes.strip_suffix(b"\n")) {
        bytes = rest;
    }
    bytes
}

fn parts_of<'b>(body: &'b [u8], boundary: &str) -> Result<Vec<&'b [u8]>, String> {
    let delimiter = format!("--{boundary}");
    let positions = positions_of(body, delimiter.as_bytes());
    let Some(&last) = positions.last() else {
        return Err(format!("the body has no boundary '{boundary}'"));
    };
    if !body[last + delimiter.len()..].starts_with(b"--") {
        return Err("the batch does not end with its closing boundary".to_owned());
    }
    Ok(positions
        .windows(2)
        .map(|pair| without_line_breaks(&body[pair[0] + delimiter.len()..pair[1]]))
        .collect())
}

fn head_and_body(bytes: &[u8]) -> (&[u8], &[u8]) {
    for (separator, length) in [(&b"\r\n\r\n"[..], 4), (&b"\n\n"[..], 2)] {
        if let Some(&at) = positions_of(bytes, separator).first() {
            return (&bytes[..at], &bytes[at + length..]);
        }
    }
    (bytes, &[])
}

fn header_lines(head: &[u8]) -> Result<Vec<&str>, String> {
    let text = std::str::from_utf8(head).map_err(|_| "a part's headers are not UTF-8".to_owned())?;
    Ok(text.lines().map(|line| line.trim_end_matches('\r')).filter(|line| !line.is_empty()).collect())
}

fn headers_of(lines: &[&str]) -> Vec<(String, String)> {
    lines
        .iter()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect()
}

fn header<'h>(headers: &'h [(String, String)], name: &str) -> Option<&'h str> {
    headers.iter().find(|(field, _)| field == name).map(|(_, value)| value.as_str())
}

fn http_part(part_headers: &[(String, String)], content: &[u8]) -> Result<Part, String> {
    let (head, body) = head_and_body(content);
    let lines = header_lines(head)?;
    let Some((request_line, rest)) = lines.split_first() else {
        return Err("a part has no request line".to_owned());
    };
    let mut words = request_line.split_whitespace();
    let (Some(method), Some(url)) = (words.next(), words.next()) else {
        return Err(format!("'{request_line}' is not a request line"));
    };
    let headers = headers_of(rest);
    let content_id = header(part_headers, "content-id")
        .or_else(|| header(&headers, "content-id"))
        .map(str::to_owned);
    Ok(Part {
        content_id,
        method: method.to_ascii_uppercase(),
        url: url.to_owned(),
        headers,
        body: without_line_breaks(body).to_vec(),
    })
}

fn multipart_items(body: &[u8], boundary: &str) -> Result<Vec<Item>, String> {
    let mut items = Vec::new();
    for part in parts_of(body, boundary)? {
        let (head, content) = head_and_body(part);
        let part_headers = headers_of(&header_lines(head)?);
        let content_type = header(&part_headers, "content-type").unwrap_or_default();
        if content_type.to_ascii_lowercase().starts_with("multipart/mixed") {
            let inner = boundary_of(content_type).ok_or_else(|| "a change set has no boundary".to_owned())?;
            let mut writes = Vec::new();
            for change in parts_of(content, &inner)? {
                let (change_head, change_content) = head_and_body(change);
                let change_headers = headers_of(&header_lines(change_head)?);
                writes.push(http_part(&change_headers, change_content)?);
            }
            items.push(Item::Atomic(None, writes));
        } else {
            items.push(Item::Single(http_part(&part_headers, content)?));
        }
    }
    Ok(items)
}

fn member<'n>(members: &'n [(String, Node)], name: &str) -> Option<&'n Json> {
    members.iter().find(|(key, _)| key == name).map(|(_, node)| &node.value)
}

fn text_member(members: &[(String, Node)], name: &str) -> Option<String> {
    match member(members, name)? {
        Json::String(text) => Some(text.clone()),
        Json::Number(text) => Some(text.clone()),
        _ => None,
    }
}

fn json_items(text: &str) -> Result<Vec<Item>, String> {
    let document = parse_document(text).map_err(|_| "the batch is not JSON".to_owned())?;
    let Json::Object(members) = &document.value else {
        return Err("the batch is not a JSON object".to_owned());
    };
    let Some(Json::Array(requests)) = member(members, "requests") else {
        return Err("the batch has no 'requests' array".to_owned());
    };
    let mut items: Vec<Item> = Vec::new();
    let mut groups: Vec<(String, usize)> = Vec::new();
    for node in requests {
        let Json::Object(fields) = &node.value else {
            return Err("each request is a JSON object".to_owned());
        };
        let method = text_member(fields, "method").ok_or_else(|| "a request has no 'method'".to_owned())?;
        let url = text_member(fields, "url").ok_or_else(|| "a request has no 'url'".to_owned())?;
        let headers = match member(fields, "headers") {
            Some(Json::Object(pairs)) => pairs
                .iter()
                .filter_map(|(name, value)| match &value.value {
                    Json::String(text) => Some((name.to_ascii_lowercase(), text.clone())),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let body = fields
            .iter()
            .find(|(key, _)| key == "body")
            .map(|(_, body)| match &body.value {
                Json::String(text) => text.clone().into_bytes(),
                _ => text[body.start..body.end].as_bytes().to_vec(),
            })
            .unwrap_or_default();
        let part = Part {
            content_id: text_member(fields, "id"),
            method: method.to_ascii_uppercase(),
            url,
            headers,
            body,
        };
        match text_member(fields, "atomicityGroup") {
            Some(group) => match groups.iter().find(|(known, _)| *known == group) {
                Some(&(_, at)) => {
                    if let Some(Item::Atomic(_, parts)) = items.get_mut(at) {
                        parts.push(part);
                    }
                }
                None => {
                    groups.push((group.clone(), items.len()));
                    items.push(Item::Atomic(Some(group), vec![part]));
                }
            },
            None => items.push(Item::Single(part)),
        }
    }
    Ok(items)
}

fn request_count(items: &[Item]) -> usize {
    items
        .iter()
        .map(|item| match item {
            Item::Single(_) => 1,
            Item::Atomic(_, parts) => parts.len(),
        })
        .sum()
}

fn service_relative(url: &str, base: &str, locations: &HashMap<String, String>) -> Result<(String, Option<String>), String> {
    let mut rest = url.strip_prefix(base).unwrap_or(url);
    if let Some(after_scheme) = rest.split_once("://").map(|(_, after)| after) {
        rest = after_scheme.split_once('/').map_or("", |(_, path)| path);
    }
    let rest = rest.trim_start_matches('/');
    let rest = rest.strip_prefix("odata/").unwrap_or(rest);
    let (path, query) = match rest.split_once('?') {
        Some((path, query)) => (path, Some(query.to_owned())),
        None => (rest, None),
    };
    let path = match path.strip_prefix('$').and_then(|reference| {
        let (id, tail) = reference.split_once('/').unwrap_or((reference, ""));
        locations.get(id).map(|location| (location, tail))
    }) {
        Some((location, "")) => location.clone(),
        Some((location, tail)) => format!("{location}/{tail}"),
        None => crate::request::percent_decode(path),
    };
    if path == "$batch" {
        return Err("a batch may not hold another batch".to_owned());
    }
    Ok((path, query))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        _ if status >= 500 => "Internal Server Error",
        _ => "Unknown",
    }
}

fn http_response_part(answer: &Answer, out: &mut Vec<u8>) {
    out.extend_from_slice(b"Content-Type: application/http\r\nContent-Transfer-Encoding: binary\r\n");
    if let Some(id) = &answer.content_id {
        out.extend_from_slice(format!("Content-ID: {id}\r\n").as_bytes());
    }
    let response = &answer.response;
    out.extend_from_slice(format!("\r\nHTTP/1.1 {} {}\r\n", response.status, reason(response.status)).as_bytes());
    if !response.body.is_empty() {
        out.extend_from_slice(format!("Content-Type: {}\r\n", response.content_type).as_bytes());
    }
    for (name, value) in &response.headers {
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    out.extend_from_slice(b"OData-Version: 4.0\r\n\r\n");
    out.extend_from_slice(&response.body);
    out.extend_from_slice(b"\r\n");
}

fn json_response(answer: &Answer, id: &str, group: Option<&str>) -> String {
    let response = &answer.response;
    let body = std::str::from_utf8(&response.body).unwrap_or_default();
    let body = if body.is_empty() {
        String::new()
    } else if response.content_type.starts_with("application/json") {
        format!(",\"body\":{body}")
    } else {
        format!(",\"body\":{}", json_string(body))
    };
    let mut headers = vec![format!("\"content-type\":{}", json_string(&response.content_type))];
    headers.extend(
        response
            .headers
            .iter()
            .map(|(name, value)| format!("{}:{}", json_string(&name.to_ascii_lowercase()), json_string(value))),
    );
    let group = group.map_or_else(String::new, |group| format!(",\"atomicityGroup\":{}", json_string(group)));
    format!(
        "{{\"id\":{}{group},\"status\":{},\"headers\":{{{}}}{body}}}",
        json_string(id),
        response.status,
        headers.join(",")
    )
}

impl<S: KeyValueStore> Endpoint<'_, S> {
    pub(super) fn odata_batch(&self, sets: &[EntitySet], request: &Request, base: &str) -> Response {
        let content_type = request.header("content-type").unwrap_or_default();
        let json = content_type.to_ascii_lowercase().starts_with("application/json");
        let items = if json {
            match std::str::from_utf8(&request.body) {
                Ok(text) => json_items(text),
                Err(_) => Err("the batch is not UTF-8".to_owned()),
            }
        } else {
            match boundary_of(content_type) {
                Some(boundary) => multipart_items(&request.body, &boundary),
                None => Err("a batch is multipart/mixed with a boundary, or JSON".to_owned()),
            }
        };
        let items = match items {
            Ok(items) => items,
            Err(message) => return bad_request(&message),
        };
        if request_count(&items) > MOST_REQUESTS {
            return bad_request(&format!("a batch holds at most {MOST_REQUESTS} requests"));
        }
        let answered: Vec<Result<Vec<Answer>, Answer>> = items
            .iter()
            .map(|item| match item {
                Item::Single(part) => Ok(vec![self.batch_single(request, part, base)]),
                Item::Atomic(_, parts) => self.batch_atomic(sets, parts, base),
            })
            .collect();
        if json {
            Self::json_batch_response(&items, &answered)
        } else {
            Self::multipart_batch_response(request, &items, &answered)
        }
    }

    fn batch_single(&self, outer: &Request, part: &Part, base: &str) -> Answer {
        let response = match service_relative(&part.url, base, &HashMap::new()) {
            Ok((path, query)) => {
                let mut headers = part.headers.clone();
                if let Some(authorization) = outer.header("authorization") {
                    headers.push(("authorization".to_owned(), authorization.to_owned()));
                }
                if let Some(host) = outer.header("host") {
                    headers.push(("host".to_owned(), host.to_owned()));
                }
                let sub = Request {
                    method: part.method.clone(),
                    path: format!("/odata/{path}"),
                    query,
                    headers,
                    body: part.body.clone(),
                };
                self.odata(&path, &sub)
            }
            Err(message) => bad_request(&message),
        };
        Answer {
            content_id: part.content_id.clone(),
            response,
        }
    }

    fn batch_atomic(&self, sets: &[EntitySet], parts: &[Part], base: &str) -> Result<Vec<Answer>, Answer> {
        let mut failed_id = None;
        let outcome = self.atomically(|execute| {
            let mut locations: HashMap<String, String> = HashMap::new();
            let mut answers = Vec::new();
            for part in parts {
                let response = self.batch_write(sets, part, base, &locations, execute);
                if response.status >= 400 {
                    failed_id = part.content_id.clone();
                    return Err(response);
                }
                if let Some(id) = &part.content_id
                    && let Some((_, location)) = response.headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("location"))
                {
                    locations.insert(id.clone(), location.strip_prefix(base).unwrap_or(location).to_owned());
                }
                answers.push(Answer {
                    content_id: part.content_id.clone(),
                    response,
                });
            }
            Ok(answers)
        });
        outcome.map_err(|response| Answer {
            content_id: failed_id,
            response,
        })
    }

    fn batch_write(
        &self,
        sets: &[EntitySet],
        part: &Part,
        base: &str,
        locations: &HashMap<String, String>,
        execute: &mut Execute<'_>,
    ) -> Response {
        let (path, _) = match service_relative(&part.url, base, locations) {
            Ok(relative) => relative,
            Err(message) => return bad_request(&message),
        };
        let Resolved { set, target, key, tail } = match self.resolve(sets, &path) {
            Ok(resolved) => resolved,
            Err(response) => return response,
        };
        let planned = match (key.as_deref(), tail.as_str(), part.method.as_str()) {
            (None, "", "POST") => Self::insert_statement(set, &target, &part.body).map(|sql| (sql, true)),
            (Some(key), "", "PATCH" | "PUT" | "MERGE") => Self::update_statement(set, &target, key, &part.body).map(|sql| (sql, false)),
            (Some(key), "", "DELETE") => Self::delete_statement(set, &target, key).map(|sql| (sql, false)),
            _ => Err(bad_request("a change set or atomicity group holds only inserts, updates and deletes")),
        };
        let (sql, inserting) = match planned {
            Ok(planned) => planned,
            Err(response) => return response,
        };
        match execute(&sql) {
            Ok(results) if inserting => Self::inserted(set, rows_of(&results).first(), base),
            Ok(results) => Self::changed(&results),
            Err(error) => sql_error(&error),
        }
    }

    fn multipart_batch_response(request: &Request, items: &[Item], answered: &[Result<Vec<Answer>, Answer>]) -> Response {
        let tag: String = eggdb_crypto::sha256(&request.body)[..12].iter().map(|byte| format!("{byte:02x}")).collect();
        let boundary = format!("batchresponse_{tag}");
        let mut out = Vec::new();
        for (at, (item, outcome)) in items.iter().zip(answered).enumerate() {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match (item, outcome) {
                (Item::Single(_), Ok(answers)) => {
                    if let Some(answer) = answers.first() {
                        http_response_part(answer, &mut out);
                    }
                }
                (Item::Atomic(..), Ok(answers)) => {
                    let inner = format!("changesetresponse_{tag}_{at}");
                    out.extend_from_slice(format!("Content-Type: multipart/mixed; boundary={inner}\r\n\r\n").as_bytes());
                    for answer in answers {
                        out.extend_from_slice(format!("--{inner}\r\n").as_bytes());
                        http_response_part(answer, &mut out);
                    }
                    out.extend_from_slice(format!("--{inner}--\r\n").as_bytes());
                }
                (_, Err(failed)) => http_response_part(failed, &mut out),
            }
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        Response {
            status: 200,
            content_type: format!("multipart/mixed; boundary={boundary}"),
            body: out,
            headers: vec![("OData-Version".to_owned(), "4.0".to_owned())],
        }
    }

    fn json_batch_response(items: &[Item], answered: &[Result<Vec<Answer>, Answer>]) -> Response {
        let mut responses = Vec::new();
        for (item, outcome) in items.iter().zip(answered) {
            match (item, outcome) {
                (Item::Single(part), Ok(answers)) => {
                    if let Some(answer) = answers.first() {
                        responses.push(json_response(answer, part.content_id.as_deref().unwrap_or_default(), None));
                    }
                }
                (Item::Atomic(group, parts), Ok(answers)) => {
                    for (part, answer) in parts.iter().zip(answers) {
                        responses.push(json_response(answer, part.content_id.as_deref().unwrap_or_default(), group.as_deref()));
                    }
                }
                (Item::Atomic(group, parts), Err(failed)) => {
                    for part in parts {
                        let answer = Answer {
                            content_id: part.content_id.clone(),
                            response: Response {
                                status: failed.response.status,
                                content_type: failed.response.content_type.clone(),
                                body: failed.response.body.clone(),
                                headers: Vec::new(),
                            },
                        };
                        responses.push(json_response(&answer, part.content_id.as_deref().unwrap_or_default(), group.as_deref()));
                    }
                }
                (Item::Single(_), Err(failed)) => responses.push(json_response(failed, "", None)),
            }
        }
        Response {
            status: 200,
            content_type: "application/json".to_owned(),
            body: format!("{{\"responses\":[{}]}}", responses.join(",")).into_bytes(),
            headers: vec![("OData-Version".to_owned(), "4.01".to_owned())],
        }
    }
}
