//! The SPARQL 1.1 Protocol and the SPARQL 1.1 Graph Store HTTP Protocol (indirect graph
//! identification) over EggDB's HTTP server (D889): `/sparql` takes queries by `GET` or
//! `POST` and updates by `POST`, and `/rdf-graphs` reads, replaces, adds to and deletes one
//! graph. Requests sign in with HTTP Basic as the server's user.

use crate::sparql_results::{ResultFormat, write_results};
use crate::syntax::Format;
use crate::term::Term;
use crate::{RdfError, RdfErrorKind, export_graph, load, query_in, update};
use eggdb_http::{Request, Response};
use eggdb_keyspace::Keyspace;

/// The largest request body the endpoint accepts.
pub const MAX_REQUEST_BYTES: usize = 64 << 20;

/// The SPARQL endpoint over one RDF database.
pub struct Endpoint<'k, K: Keyspace + ?Sized> {
    keyspace: &'k K,
    db: u32,
    user: String,
    password_sha256: [u8; 32],
    read_only: bool,
}

impl<K: Keyspace + ?Sized> std::fmt::Debug for Endpoint<'_, K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("db", &self.db)
            .field("user", &self.user)
            .field("read_only", &self.read_only)
            .finish_non_exhaustive()
    }
}

fn text(status: u16, body: &str) -> Response {
    Response {
        status,
        content_type: "text/plain; charset=utf-8".to_owned(),
        body: body.as_bytes().to_vec(),
        headers: Vec::new(),
    }
}

fn failure(error: &RdfError) -> Response {
    let status = match error.kind {
        RdfErrorKind::Syntax | RdfErrorKind::Invalid => 400,
        RdfErrorKind::NotSupported => 501,
        RdfErrorKind::Storage => 500,
    };
    text(status, &error.message)
}

fn decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' => match text.get(at + 1..at + 3).and_then(|pair| u8::from_str_radix(pair, 16).ok()) {
                Some(byte) => {
                    out.push(byte);
                    at += 2;
                }
                None => out.push(b'%'),
            },
            other => out.push(other),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn form(text: &str) -> Vec<(String, String)> {
    text.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decoded(name), decoded(value))
        })
        .collect()
}

fn values<'f>(fields: &'f [(String, String)], name: &str) -> Vec<&'f str> {
    fields.iter().filter(|(key, _)| key == name).map(|(_, value)| value.as_str()).collect()
}

impl<'k, K: Keyspace + ?Sized> Endpoint<'k, K> {
    /// The endpoint over database `db` of `keyspace`, for clients signing in as `user` with
    /// the password whose SHA-256 is `password_sha256`; with `read_only`, updates and graph
    /// changes are refused.
    pub fn new(keyspace: &'k K, db: u32, user: &str, password_sha256: [u8; 32], read_only: bool) -> Self {
        Self {
            keyspace,
            db,
            user: user.to_owned(),
            password_sha256,
            read_only,
        }
    }

    fn signed_in(&self, request: &Request) -> bool {
        let Some(encoded) = request.header("authorization").and_then(|header| header.strip_prefix("Basic ")) else {
            return false;
        };
        let Ok(decoded) = eggdb_crypto::base64_decode(encoded.trim().as_bytes()) else {
            return false;
        };
        let Ok(credentials) = String::from_utf8(decoded) else {
            return false;
        };
        let Some((user, password)) = credentials.split_once(':') else {
            return false;
        };
        user == self.user && eggdb_crypto::sha256(password.as_bytes()) == self.password_sha256
    }

    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        if !self.signed_in(request) {
            let mut response = text(401, "sign in with HTTP Basic authentication");
            response.headers.push(("WWW-Authenticate".to_owned(), "Basic realm=\"eggdb\"".to_owned()));
            return response;
        }
        match request.path.trim_end_matches('/') {
            "/sparql" | "/query" | "/update" => self.sparql(request),
            "/rdf-graphs" | "/data" => self.graph_store(request),
            _ => text(404, "the SPARQL endpoint is /sparql and the graph store /rdf-graphs"),
        }
    }

    fn sparql(&self, request: &Request) -> Response {
        let mut fields = form(request.query.as_deref().unwrap_or_default());
        let content_type = request.header("content-type").unwrap_or_default().to_ascii_lowercase();
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let (query_text, update_text) = match request.method.as_str() {
            "GET" => (values(&fields, "query").first().map(|text| (*text).to_owned()), None),
            "POST" if content_type.starts_with("application/sparql-query") => (Some(body), None),
            "POST" if content_type.starts_with("application/sparql-update") => (None, Some(body)),
            "POST" if content_type.starts_with("application/x-www-form-urlencoded") => {
                fields.extend(form(&body));
                (
                    values(&fields, "query").first().map(|text| (*text).to_owned()),
                    values(&fields, "update").first().map(|text| (*text).to_owned()),
                )
            }
            "POST" => return text(415, "send application/sparql-query, application/sparql-update or a form"),
            _ => return text(405, "use GET or POST"),
        };
        let default_graphs: Vec<String> = values(&fields, "default-graph-uri").into_iter().chain(values(&fields, "using-graph-uri")).map(str::to_owned).collect();
        let named_graphs: Vec<String> = values(&fields, "named-graph-uri").into_iter().chain(values(&fields, "using-named-graph-uri")).map(str::to_owned).collect();
        let dataset = (!default_graphs.is_empty() || !named_graphs.is_empty()).then_some((default_graphs, named_graphs));
        if let Some(update_text) = update_text {
            if self.read_only {
                return text(403, "the server is read-only");
            }
            return match update(self.keyspace, self.db, &update_text, "") {
                Ok(summary) => text(200, &format!("inserted {} quads, deleted {}\n", summary.inserted, summary.deleted)),
                Err(error) => failure(&error),
            };
        }
        let Some(query_text) = query_text else {
            return text(400, "the request has no query");
        };
        let outcome = match query_in(self.keyspace, self.db, &query_text, "", dataset) {
            Ok(outcome) => outcome,
            Err(error) => return failure(&error),
        };
        let wanted = values(&fields, "format")
            .first()
            .map(|format| (*format).to_owned())
            .or_else(|| request.header("accept").map(str::to_owned))
            .unwrap_or_default();
        let (media_type, body) = write_results(&outcome, ResultFormat::negotiated(&wanted));
        let media_type = if media_type == "application/n-triples" && wanted.contains("text/turtle") {
            "text/turtle; charset=utf-8".to_owned()
        } else {
            media_type
        };
        Response {
            status: 200,
            content_type: media_type,
            body: body.into_bytes(),
            headers: Vec::new(),
        }
    }

    fn graph_store(&self, request: &Request) -> Response {
        let fields = form(request.query.as_deref().unwrap_or_default());
        let graph = match (values(&fields, "graph").first(), fields.iter().any(|(key, _)| key == "default")) {
            (Some(iri), false) => Some(Term::Iri((*iri).to_owned())),
            (None, true) => None,
            _ => return text(400, "name the graph with ?graph=<iri> or ?default"),
        };
        let target = match &graph {
            Some(Term::Iri(iri)) => format!("GRAPH <{iri}>"),
            _ => "DEFAULT".to_owned(),
        };
        if graph.as_ref().is_some_and(|term| matches!(term, Term::Iri(iri) if iri.contains(['<', '>', ' ', '"', '{', '}']))) {
            return text(400, "the graph IRI holds characters IRIs may not hold");
        }
        match request.method.as_str() {
            "GET" | "HEAD" => match export_graph(self.keyspace, self.db, graph.as_ref()) {
                Ok(None) => text(404, "the graph does not exist"),
                Ok(Some(body)) => Response {
                    status: 200,
                    content_type: "application/n-triples".to_owned(),
                    body: body.into_bytes(),
                    headers: Vec::new(),
                },
                Err(error) => failure(&error),
            },
            "PUT" | "POST" => {
                if self.read_only {
                    return text(403, "the server is read-only");
                }
                let content_type = request.header("content-type").unwrap_or("text/turtle");
                let Some(format) = Format::named(content_type) else {
                    return text(415, "send Turtle, N-Triples, N-Quads or TriG");
                };
                let Ok(document) = std::str::from_utf8(&request.body) else {
                    return text(400, "the body is not UTF-8");
                };
                if request.method == "PUT"
                    && let Err(error) = update(self.keyspace, self.db, &format!("CLEAR SILENT {target}"), "")
                {
                    return failure(&error);
                }
                match load(self.keyspace, self.db, document, format, "", graph.as_ref()) {
                    Ok(_) => text(if request.method == "PUT" { 200 } else { 204 }, ""),
                    Err(error) => failure(&error),
                }
            }
            "DELETE" => {
                if self.read_only {
                    return text(403, "the server is read-only");
                }
                match update(self.keyspace, self.db, &format!("CLEAR {target}"), "") {
                    Ok(summary) if summary.deleted == 0 && graph.is_some() => text(404, "the graph does not exist"),
                    Ok(_) => text(204, ""),
                    Err(error) if graph.is_some() && error.kind == RdfErrorKind::Invalid => text(404, &error.message),
                    Err(error) => failure(&error),
                }
            }
            _ => text(405, "use GET, PUT, POST or DELETE"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;

    fn call(endpoint: &Endpoint<'_, MemoryKeyspace>, method: &str, path: &str, query: Option<&str>, content_type: Option<&str>, body: &str) -> Response {
        let mut headers = vec![(
            "authorization".to_owned(),
            format!("Basic {}", eggdb_crypto::base64_encode(b"eggdb:secret")),
        )];
        if let Some(content_type) = content_type {
            headers.push(("content-type".to_owned(), content_type.to_owned()));
        }
        endpoint.answer(&Request {
            method: method.to_owned(),
            path: path.to_owned(),
            query: query.map(str::to_owned),
            headers,
            body: body.as_bytes().to_vec(),
        })
    }

    #[test]
    fn the_protocols_query_update_and_manage_graphs() {
        let keyspace = MemoryKeyspace::default();
        let endpoint = Endpoint::new(&keyspace, 0, "eggdb", eggdb_crypto::sha256(b"secret"), false);
        let put = call(&endpoint, "PUT", "/rdf-graphs", Some("graph=http%3A%2F%2Fex%2Fg"), Some("text/turtle"), "<http://ex/a> <http://ex/p> 1 .");
        assert_eq!(put.status, 200, "{}", String::from_utf8_lossy(&put.body));
        let changed = call(
            &endpoint,
            "POST",
            "/sparql",
            None,
            Some("application/sparql-update"),
            "INSERT DATA { <http://ex/b> <http://ex/p> 2 }",
        );
        assert_eq!(changed.status, 200, "{}", String::from_utf8_lossy(&changed.body));
        let asked = call(
            &endpoint,
            "GET",
            "/sparql",
            Some("query=SELECT+%3Fv+WHERE+%7B+%3Fs+%3Chttp%3A%2F%2Fex%2Fp%3E+%3Fv+%7D&format=csv"),
            None,
            "",
        );
        assert_eq!(String::from_utf8_lossy(&asked.body), "v\r\n2\r\n");
        let everywhere = call(
            &endpoint,
            "POST",
            "/sparql",
            None,
            Some("application/sparql-query"),
            "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }",
        );
        assert!(String::from_utf8_lossy(&everywhere.body).contains("\"value\":\"1\""), "{}", String::from_utf8_lossy(&everywhere.body));
        let read = call(&endpoint, "GET", "/rdf-graphs", Some("graph=http://ex/g"), None, "");
        assert!(String::from_utf8_lossy(&read.body).contains("<http://ex/a>"));
        assert_eq!(call(&endpoint, "DELETE", "/rdf-graphs", Some("graph=http://ex/g"), None, "").status, 204);
        assert_eq!(call(&endpoint, "GET", "/rdf-graphs", Some("graph=http://ex/g"), None, "").status, 404);
        let refused = endpoint.answer(&Request {
            method: "GET".to_owned(),
            path: "/sparql".to_owned(),
            query: Some("query=ASK%7B%7D".to_owned()),
            headers: Vec::new(),
            body: Vec::new(),
        });
        assert_eq!(refused.status, 401);
        let broken = call(&endpoint, "POST", "/sparql", None, Some("application/sparql-query"), "SELECT WHERE");
        assert_eq!(broken.status, 400);
    }
}
