//! EggDB's REST API generated from the schema (D235), following the URL conventions PostgREST
//! documents so its clients and examples work: `GET /<table>` with `select`, column filters
//! (`eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `like`, `ilike`, `is`, `in`, and `not.` before any),
//! `order`, `limit`, `offset` and `Prefer: count=exact`; `POST` to insert one object or an array;
//! `PATCH` and `DELETE` with filters; `Prefer: return=representation` to get the rows back; and
//! `GET /` listing the tables and their columns. Every request runs as SQL in a session of the
//! signed-in user, so roles and row security apply unchanged. PostgREST is a trademark of its
//! owners; EggDB is compatible with its URL conventions.

mod request;

use eggdb_http::{Request, Response};
use eggdb_sql::json::{Json, Node, parse_document};
use eggdb_sql::{Dialect, KeyValueStore, QueryResult, Session, Value};
use request::{Query, identifier, literal};

/// The largest request body the API accepts.
pub const MAX_REQUEST_BYTES: usize = 8 << 20;

/// The REST API over one database.
pub struct Endpoint<'s, S: KeyValueStore> {
    store: &'s S,
    user: String,
    password_sha256: [u8; 32],
    database: String,
    schema: String,
}

impl<S: KeyValueStore> std::fmt::Debug for Endpoint<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("user", &self.user).field("schema", &self.schema).finish_non_exhaustive()
    }
}

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
    Response { status, content_type: "application/json; charset=utf-8".to_owned(), body: body.into_bytes(), headers: Vec::new() }
}

fn failure(status: u16, code: &str, message: &str) -> Response {
    json(status, format!("{{\"code\":{},\"message\":{},\"details\":null,\"hint\":null}}", json_string(code), json_string(message)))
}

fn value_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Boolean(flag) => flag.to_string(),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) | Value::Numeric(_) => value.to_text().unwrap_or_default(),
        Value::Double(number) if number.is_finite() => format!("{number}"),
        Value::Real(number) if number.is_finite() => format!("{number}"),
        Value::Json(text) | Value::Jsonb(text) => text.clone(),
        other => json_string(&other.to_text().unwrap_or_default()),
    }
}

fn rows_json(result: &QueryResult) -> String {
    let names: Vec<String> = result.columns.iter().flatten().map(|column| json_string(&column.name)).collect();
    let rows: Vec<String> = result
        .rows
        .iter()
        .map(|row| {
            let fields: Vec<String> = names.iter().zip(row).map(|(name, value)| format!("{name}:{}", value_json(value))).collect();
            format!("{{{}}}", fields.join(","))
        })
        .collect();
    format!("[{}]", rows.join(","))
}

fn json_literal(node: &Node, text: &str) -> Result<String, String> {
    Ok(match &node.value {
        Json::Null => "NULL".to_owned(),
        Json::Boolean(flag) => if *flag { "TRUE" } else { "FALSE" }.to_owned(),
        Json::Number(number) => {
            if !number.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E')) {
                return Err(format!("{number} is not a number"));
            }
            number.clone()
        }
        Json::String(value) => literal(value),
        Json::Array(_) | Json::Object(_) => format!("{}::jsonb", literal(text.get(node.start..node.end).unwrap_or_default())),
    })
}

fn sql_failure(error: &eggdb_sql::SqlError) -> Response {
    let code = error.state().code().to_owned();
    let status = match code.as_str() {
        "42P01" | "42703" => 404,
        "42501" => 403,
        "23505" => 409,
        "23502" | "23503" | "23514" | "22P02" | "22023" | "42601" => 400,
        _ => 400,
    };
    failure(status, &code, error.message())
}

impl<'s, S: KeyValueStore> Endpoint<'s, S> {
    /// The API over `store`'s tables in `schema`, run as `user` in `database`, for clients
    /// that sign in with `user` and the password whose SHA-256 is `password_sha256`.
    pub fn new(store: &'s S, user: &str, password_sha256: [u8; 32], database: &str, schema: &str) -> Self {
        Self { store, user: user.to_owned(), password_sha256, database: database.to_owned(), schema: schema.to_owned() }
    }

    fn authorized(&self, request: &Request) -> bool {
        let Some(encoded) = request.header("authorization").and_then(|header| header.strip_prefix("Basic ")) else {
            return false;
        };
        let Ok(decoded) = eggdb_crypto::base64_decode(encoded.trim()) else {
            return false;
        };
        let Some(split) = decoded.iter().position(|&byte| byte == b':') else {
            return false;
        };
        eggdb_crypto::constant_time_eq(&decoded[..split], self.user.as_bytes()) && eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(&decoded[split + 1..]), &self.password_sha256)
    }

    fn run(&self, sql: &str) -> Result<Vec<QueryResult>, eggdb_sql::SqlError> {
        let mut session = Session::with_dialect(self.store, Dialect::PostgreSql);
        session.identify(&self.user, &self.database);
        let mut results = Vec::new();
        session.execute(sql, &mut |result| results.push(result))?;
        Ok(results)
    }

    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        if !self.authorized(request) {
            let mut response = failure(401, "PGRST301", "sign in with the server's user and password (HTTP Basic)");
            response.headers.push(("WWW-Authenticate".to_owned(), "Basic realm=\"eggdb\"".to_owned()));
            return response;
        }
        let path = request.path.trim_matches('/');
        if path.is_empty() {
            return self.describe();
        }
        let Some(table) = identifier(path) else {
            return failure(404, "PGRST205", &format!("no table named {path}"));
        };
        let query = match Query::parse(request.query.as_deref().unwrap_or_default()) {
            Ok(query) => query,
            Err(message) => return failure(400, "PGRST100", &message),
        };
        let target = format!("{}.{table}", identifier(&self.schema).unwrap_or_else(|| "public".to_owned()));
        let prefer = request.header("prefer").unwrap_or_default().to_ascii_lowercase();
        let representation = prefer.contains("return=representation");
        match request.method.as_str() {
            "GET" | "HEAD" => self.read(&target, &query, prefer.contains("count=exact")),
            "POST" => self.insert(&target, &request.body, representation, &query),
            "PATCH" => self.update(&target, &request.body, &query, representation),
            "DELETE" => self.delete(&target, &query, representation),
            _ => failure(405, "PGRST117", "use GET, POST, PATCH or DELETE"),
        }
    }

    fn describe(&self) -> Response {
        let sql = format!(
            "SELECT table_name, column_name, data_type FROM information_schema.columns WHERE table_schema = {} ORDER BY table_name, ordinal_position",
            literal(&self.schema)
        );
        match self.run(&sql) {
            Ok(results) => {
                let mut tables: Vec<(String, Vec<String>)> = Vec::new();
                for row in results.iter().filter(|result| result.columns.is_some()).flat_map(|result| result.rows.iter()) {
                    let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
                    let column = format!("{}:{{\"type\":{}}}", json_string(&text(1)), json_string(&text(2)));
                    match tables.last_mut() {
                        Some((name, columns)) if *name == text(0) => columns.push(column),
                        _ => tables.push((text(0), vec![column])),
                    }
                }
                let described: Vec<String> = tables.iter().map(|(name, columns)| format!("{}:{{\"columns\":{{{}}}}}", json_string(name), columns.join(","))).collect();
                json(200, format!("{{\"schema\":{},\"tables\":{{{}}}}}", json_string(&self.schema), described.join(",")))
            }
            Err(error) => sql_failure(&error),
        }
    }

    fn read(&self, target: &str, query: &Query, count: bool) -> Response {
        let sql = format!("SELECT {} FROM {target}{}{}{}", query.projection(), query.where_clause(), query.order_clause(), query.page_clause());
        let rows = match self.run(&sql) {
            Ok(results) => results.into_iter().rev().find(|result| result.columns.is_some()),
            Err(error) => return sql_failure(&error),
        };
        let Some(rows) = rows else {
            return json(200, "[]".to_owned());
        };
        let mut response = json(200, rows_json(&rows));
        let first = query.offset.unwrap_or(0);
        let shown = rows.rows.len() as u64;
        let range = if shown == 0 { "*".to_owned() } else { format!("{first}-{}", first + shown - 1) };
        let total = if count {
            match self.run(&format!("SELECT count(*) FROM {target}{}", query.where_clause())) {
                Ok(results) => results.iter().rev().find_map(|result| result.rows.first().and_then(|row| row.first()).and_then(Value::to_text)).unwrap_or_else(|| "*".to_owned()),
                Err(error) => return sql_failure(&error),
            }
        } else {
            "*".to_owned()
        };
        response.headers.push(("Content-Range".to_owned(), format!("{range}/{total}")));
        response
    }

    fn insert(&self, target: &str, body: &[u8], representation: bool, query: &Query) -> Response {
        let text = String::from_utf8_lossy(body).into_owned();
        let Ok(document) = parse_document(&text) else {
            return failure(400, "PGRST102", "the body is not valid JSON");
        };
        let objects: Vec<&Node> = match &document.value {
            Json::Object(_) => vec![&document],
            Json::Array(items) => items.iter().collect(),
            _ => return failure(400, "PGRST102", "send a JSON object or an array of objects"),
        };
        let mut columns: Vec<String> = Vec::new();
        for object in &objects {
            let Json::Object(members) = &object.value else {
                return failure(400, "PGRST102", "every element must be a JSON object");
            };
            for (name, _) in members {
                if !columns.contains(name) {
                    columns.push(name.clone());
                }
            }
        }
        if columns.is_empty() {
            return failure(400, "PGRST102", "the objects have no columns");
        }
        let mut quoted = Vec::with_capacity(columns.len());
        for column in &columns {
            match identifier(column) {
                Some(name) => quoted.push(name),
                None => return failure(400, "PGRST204", &format!("{column} is not a column name")),
            }
        }
        let mut rows = Vec::with_capacity(objects.len());
        for object in &objects {
            let Json::Object(members) = &object.value else {
                continue;
            };
            let mut cells = Vec::with_capacity(columns.len());
            for column in &columns {
                let cell = match members.iter().find(|(name, _)| name == column) {
                    Some((_, node)) => match json_literal(node, &text) {
                        Ok(cell) => cell,
                        Err(message) => return failure(400, "PGRST102", &message),
                    },
                    None => "DEFAULT".to_owned(),
                };
                cells.push(cell);
            }
            rows.push(format!("({})", cells.join(", ")));
        }
        let returning = if representation { format!(" RETURNING {}", query.projection()) } else { String::new() };
        let sql = format!("INSERT INTO {target} ({}) VALUES {}{returning}", quoted.join(", "), rows.join(", "));
        self.changed(&sql, representation, 201)
    }

    fn update(&self, target: &str, body: &[u8], query: &Query, representation: bool) -> Response {
        let text = String::from_utf8_lossy(body).into_owned();
        let Ok(document) = parse_document(&text) else {
            return failure(400, "PGRST102", "the body is not valid JSON");
        };
        let Json::Object(members) = &document.value else {
            return failure(400, "PGRST102", "send a JSON object of the columns to change");
        };
        let mut assignments = Vec::with_capacity(members.len());
        for (name, node) in members {
            let Some(column) = identifier(name) else {
                return failure(400, "PGRST204", &format!("{name} is not a column name"));
            };
            match json_literal(node, &text) {
                Ok(value) => assignments.push(format!("{column} = {value}")),
                Err(message) => return failure(400, "PGRST102", &message),
            }
        }
        if assignments.is_empty() {
            return failure(400, "PGRST102", "the object has no columns");
        }
        let returning = if representation { format!(" RETURNING {}", query.projection()) } else { String::new() };
        self.changed(&format!("UPDATE {target} SET {}{}{returning}", assignments.join(", "), query.where_clause()), representation, 200)
    }

    fn delete(&self, target: &str, query: &Query, representation: bool) -> Response {
        let returning = if representation { format!(" RETURNING {}", query.projection()) } else { String::new() };
        self.changed(&format!("DELETE FROM {target}{}{returning}", query.where_clause()), representation, 200)
    }

    fn changed(&self, sql: &str, representation: bool, status: u16) -> Response {
        match self.run(sql) {
            Ok(results) => {
                if representation {
                    let rows = results.iter().rev().find(|result| result.columns.is_some());
                    json(status, rows.map_or_else(|| "[]".to_owned(), rows_json))
                } else {
                    json(if status == 201 { 201 } else { 204 }, String::new())
                }
            }
            Err(error) => sql_failure(&error),
        }
    }
}

#[cfg(test)]
mod tests;
