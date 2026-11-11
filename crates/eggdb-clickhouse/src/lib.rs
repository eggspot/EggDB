//! EggDB's ClickHouse HTTP interface (D510): `/ping`, queries in the `query` parameter of a `GET`
//! or in a `POST` body, the `FORMAT` clause and `default_format`, results in ClickHouse's text
//! formats ([`Format`]) and `INSERT ... FORMAT` reading rows from the body. Queries run as
//! PostgreSQL-dialect SQL; `SHOW TABLES`, `SHOW DATABASES`, `DESCRIBE TABLE` and `EXISTS TABLE`
//! are answered from the catalog. Errors come back as ClickHouse's `Code: <n>. DB::Exception`
//! text with `X-ClickHouse-Exception-Code`.

mod format;

pub use format::{Field, Format, Input, json_string, parse_input, render, type_name};

use eggdb_http::{Request, Response};
use eggdb_sql::{Dialect, KeyValueStore, QueryResult, Session, SqlError};

/// The largest request body the interface reads.
pub const MAX_REQUEST_BYTES: usize = 64 << 20;

const SERVER_NAME: &str = "eggdb";
const ROWS_PER_INSERT: usize = 1000;
const READ_ONLY_STARTS: [&str; 7] = ["SELECT", "WITH", "SHOW", "DESCRIBE", "DESC", "EXISTS", "EXPLAIN"];
const SYNTAX_ERROR: u32 = 62;
const UNKNOWN_TABLE: u32 = 60;
const UNKNOWN_IDENTIFIER: u32 = 47;
const UNKNOWN_DATABASE: u32 = 81;
const UNKNOWN_FORMAT: u32 = 73;
const READONLY: u32 = 164;
const ACCESS_DENIED: u32 = 497;
const AUTHENTICATION_FAILED: u32 = 516;
const INCORRECT_DATA: u32 = 117;
const STD_EXCEPTION: u32 = 1001;

/// The interface over `store`, signing clients in as the server's user.
pub struct Endpoint<'s, S: KeyValueStore> {
    store: &'s S,
    user: String,
    password_sha256: [u8; 32],
    database: String,
}

struct Failure {
    code: u32,
    name: &'static str,
    status: u16,
    message: String,
}

impl Failure {
    fn new(code: u32, name: &'static str, status: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            name,
            status,
            message: message.into(),
        }
    }

    fn from_sql(error: &SqlError) -> Self {
        let (code, name, status) = match error.state().code() {
            "42601" => (SYNTAX_ERROR, "SYNTAX_ERROR", 400),
            "42P01" => (UNKNOWN_TABLE, "UNKNOWN_TABLE", 404),
            "42703" => (UNKNOWN_IDENTIFIER, "UNKNOWN_IDENTIFIER", 400),
            "42501" => (ACCESS_DENIED, "ACCESS_DENIED", 403),
            "22P02" | "22007" | "22003" | "23502" | "23505" | "23514" => {
                (INCORRECT_DATA, "INCORRECT_DATA", 400)
            }
            _ => (STD_EXCEPTION, "STD_EXCEPTION", 500),
        };
        Self::new(code, name, status, error.message())
    }

    fn response(&self) -> Response {
        let mut response = Response::text(
            self.status,
            &format!(
                "Code: {}. DB::Exception: {}. ({}) (version {})",
                self.code,
                self.message,
                self.name,
                env!("CARGO_PKG_VERSION")
            ),
        )
        .with_header("X-ClickHouse-Exception-Code", &self.code.to_string());
        response.content_type = "text/plain; charset=UTF-8".to_owned();
        response
    }
}

fn query_parameters(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let decode = |text: &str| eggdb_http::percent_decoded(&text.replace('+', " ")).unwrap_or_default();
            (decode(name), decode(value))
        })
        .collect()
}

fn word_at(bytes: &[u8], at: usize) -> usize {
    let mut end = at;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    end
}

struct Split<'q> {
    statement: &'q str,
    format: Option<&'q str>,
    data: &'q str,
}

fn split_format(query: &str) -> Split<'_> {
    let bytes = query.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            quote @ (b'\'' | b'"' | b'`') => {
                at += 1;
                while at < bytes.len() && bytes[at] != quote {
                    if bytes[at] == b'\\' {
                        at += 1;
                    }
                    at += 1;
                }
                at += 1;
            }
            b'-' if bytes.get(at + 1) == Some(&b'-') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at + 1 < bytes.len() && !(bytes[at] == b'*' && bytes[at + 1] == b'/') {
                    at += 1;
                }
                at += 2;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let end = word_at(bytes, at);
                if query[at..end].eq_ignore_ascii_case("FORMAT") {
                    let mut name_start = end;
                    while name_start < bytes.len() && bytes[name_start].is_ascii_whitespace() {
                        name_start += 1;
                    }
                    let name_end = word_at(bytes, name_start);
                    if name_end > name_start {
                        return Split {
                            statement: &query[..at],
                            format: Some(&query[name_start..name_end]),
                            data: &query[name_end..],
                        };
                    }
                }
                at = end;
            }
            _ => at += 1,
        }
    }
    Split {
        statement: query,
        format: None,
        data: "",
    }
}

fn first_word(text: &str) -> String {
    text.split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_end_matches(';')
        .to_ascii_uppercase()
}

fn quoted_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn identifier(text: &str) -> String {
    let trimmed = text.trim().trim_end_matches(';');
    let bare = trimmed
        .strip_prefix('`')
        .and_then(|rest| rest.strip_suffix('`'))
        .unwrap_or(trimmed);
    bare.rsplit('.').next().unwrap_or(bare).to_owned()
}

fn catalog_query(statement: &str, database: &str) -> Option<String> {
    let words: Vec<String> = statement
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let upper: Vec<String> = words.iter().map(|word| word.to_ascii_uppercase()).collect();
    let upper: Vec<&str> = upper.iter().map(String::as_str).collect();
    match upper.as_slice() {
        ["SHOW", "TABLES", ..] => Some(
            "SELECT table_name AS name FROM information_schema.tables WHERE table_schema = 'public' ORDER BY 1".to_owned(),
        ),
        ["SHOW", "DATABASES"] => Some(format!("SELECT {} AS name", quoted_literal(database))),
        ["DESCRIBE" | "DESC", "TABLE", _] | ["DESCRIBE" | "DESC", _] => {
            let table = identifier(words.last()?);
            Some(format!(
                "SELECT column_name AS name, data_type AS type, COALESCE(column_default, '') AS default_expression FROM information_schema.columns WHERE table_schema = 'public' AND table_name = {} ORDER BY ordinal_position",
                quoted_literal(&table)
            ))
        }
        ["EXISTS", "TABLE", _] | ["EXISTS", _] => {
            let table = identifier(words.last()?);
            Some(format!(
                "SELECT CASE WHEN EXISTS (SELECT 1 FROM information_schema.tables WHERE table_schema = 'public' AND table_name = {}) THEN 1 ELSE 0 END AS result",
                quoted_literal(&table)
            ))
        }
        _ => None,
    }
}

fn insert_target(statement: &str) -> Option<(String, Option<String>)> {
    let trimmed = statement.trim();
    let upper = trimmed.to_ascii_uppercase();
    let rest = upper.strip_prefix("INSERT")?.trim_start();
    let offset = trimmed.len() - rest.len();
    let rest_original = &trimmed[offset..];
    let rest_original = if rest.starts_with("INTO") {
        rest_original[4..].trim_start()
    } else {
        rest_original
    };
    let rest_original = if rest_original.to_ascii_uppercase().starts_with("TABLE ") {
        rest_original[6..].trim_start()
    } else {
        rest_original
    };
    match rest_original.find('(') {
        Some(open) => Some((
            rest_original[..open].trim().to_owned(),
            Some(rest_original[open..].trim().to_owned()),
        )),
        None => Some((rest_original.trim().to_owned(), None)),
    }
}

fn insert_statements(statement: &str, input: Input) -> Result<Vec<String>, String> {
    let (table, listed) = insert_target(statement).ok_or("an insert's data needs an INSERT INTO statement")?;
    let header = input.names.map(|names| {
        format!("({})", names.iter().map(|name| format!("\"{}\"", name.replace('"', "\"\""))).collect::<Vec<_>>().join(", "))
    });
    let mut statements = Vec::new();
    let mut batch: Vec<String> = Vec::new();
    let mut batch_columns: Option<String> = None;
    let flush = |batch: &mut Vec<String>, columns: &Option<String>, statements: &mut Vec<String>| {
        if batch.is_empty() {
            return;
        }
        let columns = columns.clone().or_else(|| listed.clone()).or_else(|| header.clone()).unwrap_or_default();
        statements.push(format!("INSERT INTO {table} {columns} VALUES {}", batch.join(", ")));
        batch.clear();
    };
    for (names, fields) in input.rows {
        let columns = names.map(|names| {
            format!("({})", names.iter().map(|name| format!("\"{}\"", name.replace('"', "\"\""))).collect::<Vec<_>>().join(", "))
        });
        if columns != batch_columns || batch.len() >= ROWS_PER_INSERT {
            flush(&mut batch, &batch_columns, &mut statements);
            batch_columns = columns;
        }
        batch.push(format!("({})", fields.iter().map(Field::sql).collect::<Vec<_>>().join(", ")));
    }
    flush(&mut batch, &batch_columns, &mut statements);
    Ok(statements)
}

impl<'s, S: KeyValueStore> Endpoint<'s, S> {
    /// The interface over `store`'s `database`, for clients that sign in as `user` with the
    /// password whose SHA-256 is `password_sha256`.
    pub fn new(store: &'s S, user: &str, password_sha256: [u8; 32], database: &str) -> Self {
        Self {
            store,
            user: user.to_owned(),
            password_sha256,
            database: database.to_owned(),
        }
    }

    fn signed_in(&self, request: &Request, parameters: &[(String, String)]) -> bool {
        let parameter = |name: &str| {
            parameters
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let (user, password) = if let Some(user) = request.header("x-clickhouse-user") {
            (user.to_owned(), request.header("x-clickhouse-key").unwrap_or_default().to_owned())
        } else if let Some(encoded) = request
            .header("authorization")
            .and_then(|header| header.strip_prefix("Basic "))
        {
            let Ok(decoded) = eggdb_crypto::base64_decode(encoded.trim().as_bytes()) else {
                return false;
            };
            let text = String::from_utf8_lossy(&decoded).into_owned();
            let (user, password) = text.split_once(':').unwrap_or((&text, ""));
            (user.to_owned(), password.to_owned())
        } else if let Some(user) = parameter("user") {
            (user, parameter("password").unwrap_or_default())
        } else {
            return false;
        };
        eggdb_crypto::constant_time_eq(user.as_bytes(), self.user.as_bytes())
            && eggdb_crypto::constant_time_eq(
                &eggdb_crypto::sha256(password.as_bytes()),
                &self.password_sha256,
            )
    }

    fn run(&self, sql: &str) -> Result<Vec<QueryResult>, Failure> {
        let mut session = Session::with_dialect(self.store, Dialect::PostgreSql);
        session.identify(&self.user, &self.database);
        session.set_protocol("clickhouse");
        let mut results = Vec::new();
        session
            .execute(sql, &mut |result| results.push(result))
            .map_err(|error| Failure::from_sql(&error))?;
        Ok(results)
    }

    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        match self.serve(request) {
            Ok(response) => response,
            Err(failure) => failure.response(),
        }
    }

    fn serve(&self, request: &Request) -> Result<Response, Failure> {
        let path = request.path.trim_end_matches('/');
        let parameters = query_parameters(request.query.as_deref().unwrap_or_default());
        let parameter = |name: &str| {
            parameters
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        if matches!(path, "/ping" | "/replicas_status")
            || (path.is_empty() && request.method == "GET" && parameter("query").is_none())
        {
            return Ok(Response::ok("text/plain; charset=UTF-8", "Ok.\n"));
        }
        if !path.is_empty() {
            return Err(Failure::new(STD_EXCEPTION, "STD_EXCEPTION", 404, format!("there is no handler for {path}")));
        }
        if !self.signed_in(request, &parameters) {
            return Err(Failure::new(
                AUTHENTICATION_FAILED,
                "AUTHENTICATION_FAILED",
                403,
                "authentication failed: password is incorrect, or there is no user with such name",
            ));
        }
        if let Some(database) = parameter("database").or_else(|| request.header("x-clickhouse-database")) {
            if database != self.database && database != "default" {
                return Err(Failure::new(UNKNOWN_DATABASE, "UNKNOWN_DATABASE", 404, format!("database {database} does not exist")));
            }
        }
        let body = String::from_utf8_lossy(&request.body);
        let text = match (parameter("query"), request.method.as_str()) {
            (Some(query), "GET" | "HEAD") => query.to_owned(),
            (Some(query), _) if body.is_empty() => query.to_owned(),
            (Some(query), _) => format!("{query}\n{body}"),
            (None, _) => body.into_owned(),
        };
        let split = split_format(&text);
        let format_name = split
            .format
            .map(str::to_owned)
            .or_else(|| parameter("default_format").map(str::to_owned))
            .or_else(|| request.header("x-clickhouse-format").map(str::to_owned))
            .unwrap_or_else(|| "TabSeparated".to_owned());
        let format = Format::named(&format_name)
            .ok_or_else(|| Failure::new(UNKNOWN_FORMAT, "UNKNOWN_FORMAT", 400, format!("unknown format {format_name}")))?;
        let verb = first_word(split.statement);
        if matches!(request.method.as_str(), "GET" | "HEAD") && !READ_ONLY_STARTS.contains(&verb.as_str()) {
            return Err(Failure::new(READONLY, "READONLY", 400, "cannot execute query in readonly mode: send it in a POST"));
        }
        let started = std::time::Instant::now();
        let (result, written) = if verb == "INSERT" && split.format.is_some() {
            let input = parse_input(format, split.data.trim_start_matches([' ', '\t', '\r', '\n']))
                .map_err(|message| Failure::new(INCORRECT_DATA, "INCORRECT_DATA", 400, message))?;
            let mut written = 0usize;
            for statement in insert_statements(split.statement, input)
                .map_err(|message| Failure::new(SYNTAX_ERROR, "SYNTAX_ERROR", 400, message))?
            {
                for result in self.run(&statement)? {
                    written += tag_count(&result.tag);
                }
            }
            (None, written)
        } else {
            let sql = catalog_query(split.statement, &self.database).unwrap_or_else(|| split.statement.to_owned());
            let results = self.run(&sql)?;
            let written = results.iter().filter(|result| result.columns.is_none()).map(|result| tag_count(&result.tag)).sum();
            (results.into_iter().rev().find(|result| result.columns.is_some()), written)
        };
        let elapsed = started.elapsed().as_secs_f64();
        let body = result.as_ref().map(|result| render(result, format, elapsed)).unwrap_or_default();
        let read_rows = result.as_ref().map_or(0, |result| result.rows.len());
        let mut response = Response::ok(format.content_type(), body)
            .with_header("X-ClickHouse-Server-Display-Name", SERVER_NAME)
            .with_header("X-ClickHouse-Format", format.name())
            .with_header("X-ClickHouse-Timezone", "UTC")
            .with_header(
                "X-ClickHouse-Summary",
                &format!(
                    "{{\"read_rows\":\"{read_rows}\",\"read_bytes\":\"0\",\"written_rows\":\"{written}\",\"written_bytes\":\"0\",\"total_rows_to_read\":\"{read_rows}\",\"result_rows\":\"{read_rows}\",\"result_bytes\":\"0\",\"elapsed_ns\":\"{}\"}}",
                    (elapsed * 1e9) as u64
                ),
            );
        if let Some(id) = parameter("query_id") {
            response = response.with_header("X-ClickHouse-Query-Id", id);
        }
        Ok(response)
    }
}

fn tag_count(tag: &str) -> usize {
    tag.rsplit(' ').next().and_then(|count| count.parse().ok()).unwrap_or(0)
}

#[cfg(test)]
mod tests;
