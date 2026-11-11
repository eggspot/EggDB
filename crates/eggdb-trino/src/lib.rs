//! EggDB's Trino client REST protocol (D511): `POST /v1/statement` runs a statement and answers
//! Trino's query results document, with the columns' Trino types and type signatures, rows
//! paged through `nextUri` (`GET` the next page, `DELETE` to cancel), update counts, and Trino's
//! error objects. `GET /v1/info` describes the server. Statements run as PostgreSQL-dialect SQL.

use eggdb_http::{Request, Response};
use eggdb_sql::{Dialect, KeyValueStore, QueryResult, Session, SqlError, SqlType, Value};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The largest request body the protocol reads.
pub const MAX_REQUEST_BYTES: usize = 16 << 20;

const PAGE_ROWS: usize = 10_000;
const MAX_PENDING_QUERIES: usize = 64;
const PENDING_LIFETIME: Duration = Duration::from_secs(600);
const EXECUTING_PATH: &str = "/v1/statement/executing/";
const UNBOUNDED_VARCHAR: u64 = 2_147_483_647;
const SYNTAX_ERROR: (u32, &str) = (1, "SYNTAX_ERROR");
const PERMISSION_DENIED: (u32, &str) = (4, "PERMISSION_DENIED");
const TABLE_NOT_FOUND: (u32, &str) = (46, "TABLE_NOT_FOUND");
const COLUMN_NOT_FOUND: (u32, &str) = (47, "COLUMN_NOT_FOUND");
const INVALID_CAST_ARGUMENT: (u32, &str) = (58, "INVALID_CAST_ARGUMENT");
const CONSTRAINT_VIOLATION: (u32, &str) = (125, "CONSTRAINT_VIOLATION");
const GENERIC_USER_ERROR: (u32, &str) = (0, "GENERIC_USER_ERROR");

struct Pending {
    id: String,
    token: u64,
    columns: String,
    rows: VecDeque<String>,
    created: Instant,
}

/// The protocol over `store`, for clients that sign in as the server's user.
pub struct Endpoint<'s, S: KeyValueStore> {
    store: &'s S,
    user: String,
    password_sha256: [u8; 32],
    database: String,
    pending: Mutex<Vec<Pending>>,
    queries: AtomicU64,
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

fn type_of(sql_type: Option<&SqlType>) -> (String, String) {
    let long = |value: u64| format!("{{\"kind\":\"LONG\",\"value\":{value}}}");
    let (name, raw, arguments) = match sql_type {
        Some(SqlType::SmallInt) => ("smallint".to_owned(), "smallint", Vec::new()),
        Some(SqlType::Integer) => ("integer".to_owned(), "integer", Vec::new()),
        Some(SqlType::BigInt) => ("bigint".to_owned(), "bigint", Vec::new()),
        Some(SqlType::Real) => ("real".to_owned(), "real", Vec::new()),
        Some(SqlType::Double) => ("double".to_owned(), "double", Vec::new()),
        Some(SqlType::Boolean) => ("boolean".to_owned(), "boolean", Vec::new()),
        Some(SqlType::Numeric(Some(modifier))) => {
            let precision = u64::from(modifier.precision.min(38));
            let scale = u64::try_from(modifier.scale.max(0)).unwrap_or(0).min(precision);
            (format!("decimal({precision},{scale})"), "decimal", vec![long(precision), long(scale)])
        }
        Some(SqlType::Numeric(None)) => ("decimal(38,10)".to_owned(), "decimal", vec![long(38), long(10)]),
        Some(SqlType::Date) => ("date".to_owned(), "date", Vec::new()),
        Some(SqlType::Timestamp(_)) => ("timestamp(6)".to_owned(), "timestamp", vec![long(6)]),
        Some(SqlType::TimestampTz(_)) => (
            "timestamp(6) with time zone".to_owned(),
            "timestamp with time zone",
            vec![long(6)],
        ),
        Some(SqlType::Json | SqlType::Jsonb) => ("json".to_owned(), "json", Vec::new()),
        Some(SqlType::Uuid) => ("uuid".to_owned(), "uuid", Vec::new()),
        Some(SqlType::Bytea) => ("varbinary".to_owned(), "varbinary", Vec::new()),
        _ => ("varchar".to_owned(), "varchar", vec![long(UNBOUNDED_VARCHAR)]),
    };
    let signature = format!(
        "{{\"rawType\":{},\"arguments\":[{}]}}",
        json_string(raw),
        arguments.join(",")
    );
    (name, signature)
}

fn value_json(value: &Value, typed: bool) -> String {
    match value {
        Value::Null => "null".to_owned(),
        _ if !typed => json_string(&value.to_text().unwrap_or_default()),
        Value::Boolean(flag) => flag.to_string(),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) => value.to_text().unwrap_or_default(),
        Value::Double(number) => float_json(*number),
        Value::Real(number) => float_json(f64::from(*number)),
        Value::Bytea(bytes) => json_string(&eggdb_crypto::base64_encode(bytes)),
        other => json_string(&other.to_text().unwrap_or_default()),
    }
}

fn float_json(number: f64) -> String {
    if number.is_finite() {
        number.to_string()
    } else if number.is_nan() {
        json_string("NaN")
    } else if number > 0.0 {
        json_string("Infinity")
    } else {
        json_string("-Infinity")
    }
}

fn columns_json(result: &QueryResult) -> String {
    let columns: Vec<String> = result
        .columns
        .iter()
        .flatten()
        .map(|column| {
            let (name, signature) = type_of(column.sql_type.as_ref());
            format!(
                "{{\"name\":{},\"type\":{},\"typeSignature\":{signature}}}",
                json_string(&column.name),
                json_string(&name)
            )
        })
        .collect();
    format!("[{}]", columns.join(","))
}

fn rows_json(result: &QueryResult) -> VecDeque<String> {
    let typed: Vec<bool> = result
        .columns
        .iter()
        .flatten()
        .map(|column| column.sql_type.is_some())
        .collect();
    result
        .rows
        .iter()
        .map(|row| {
            let values: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(index, value)| value_json(value, typed.get(index).copied().unwrap_or(false)))
                .collect();
            format!("[{}]", values.join(","))
        })
        .collect()
}

fn error_of(error: &SqlError) -> ((u32, &'static str), String) {
    let kind = match error.state().code() {
        "42601" => SYNTAX_ERROR,
        "42501" => PERMISSION_DENIED,
        "42P01" => TABLE_NOT_FOUND,
        "42703" => COLUMN_NOT_FOUND,
        "22P02" | "22007" | "22003" => INVALID_CAST_ARGUMENT,
        "23502" | "23505" | "23514" | "23503" => CONSTRAINT_VIOLATION,
        _ => GENERIC_USER_ERROR,
    };
    (kind, error.message().to_owned())
}

fn stats(state: &str, rows: usize) -> String {
    format!(
        "{{\"state\":{},\"queued\":false,\"scheduled\":true,\"nodes\":1,\"totalSplits\":1,\"queuedSplits\":0,\"runningSplits\":0,\"completedSplits\":1,\"cpuTimeMillis\":0,\"wallTimeMillis\":0,\"queuedTimeMillis\":0,\"elapsedTimeMillis\":0,\"processedRows\":{rows},\"processedBytes\":0,\"physicalInputBytes\":0,\"peakMemoryBytes\":0,\"spilledBytes\":0}}",
        json_string(state)
    )
}

fn base_url(request: &Request) -> String {
    let host = request
        .header("host")
        .filter(|host| host.chars().all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | ':' | '-' | '[' | ']')))
        .unwrap_or("localhost");
    format!("http://{host}")
}

fn json_response(body: String) -> Response {
    Response::ok("application/json", body)
}

impl<'s, S: KeyValueStore> Endpoint<'s, S> {
    /// The protocol over `store`'s `database`, for clients that sign in as `user` with the
    /// password whose SHA-256 is `password_sha256`.
    pub fn new(store: &'s S, user: &str, password_sha256: [u8; 32], database: &str) -> Self {
        Self {
            store,
            user: user.to_owned(),
            password_sha256,
            database: database.to_owned(),
            pending: Mutex::new(Vec::new()),
            queries: AtomicU64::new(0),
        }
    }

    fn signed_in(&self, request: &Request) -> bool {
        let Some(encoded) = request
            .header("authorization")
            .and_then(|header| header.strip_prefix("Basic "))
        else {
            return false;
        };
        let Ok(decoded) = eggdb_crypto::base64_decode(encoded.trim().as_bytes()) else {
            return false;
        };
        let Some(split) = decoded.iter().position(|&byte| byte == b':') else {
            return false;
        };
        eggdb_crypto::constant_time_eq(&decoded[..split], self.user.as_bytes())
            && eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(&decoded[split + 1..]), &self.password_sha256)
    }

    fn next_id(&self) -> String {
        let number = self.queries.fetch_add(1, Ordering::Relaxed) + 1;
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        format!("{seconds}_{number:05}_eggdb")
    }

    fn page(&self, base: &str, id: &str, token: u64, columns: &str, mut rows: VecDeque<String>) -> Response {
        let count = rows.len().min(PAGE_ROWS);
        let data: Vec<String> = rows.drain(..count).collect();
        let more = !rows.is_empty();
        let next = if more {
            let mut pending = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.retain(|query| query.id != id && query.created.elapsed() < PENDING_LIFETIME);
            if pending.len() >= MAX_PENDING_QUERIES {
                pending.remove(0);
            }
            pending.push(Pending {
                id: id.to_owned(),
                token: token + 1,
                columns: columns.to_owned(),
                rows,
                created: Instant::now(),
            });
            format!(",\"nextUri\":{}", json_string(&format!("{base}{EXECUTING_PATH}{id}/{}", token + 1)))
        } else {
            String::new()
        };
        let data = if data.is_empty() { String::new() } else { format!(",\"data\":[{}]", data.join(",")) };
        json_response(format!(
            "{{\"id\":{},\"infoUri\":{}{next},\"columns\":{columns}{data},\"stats\":{},\"warnings\":[]}}",
            json_string(id),
            json_string(&format!("{base}/ui/query.html?{id}")),
            stats(if more { "RUNNING" } else { "FINISHED" }, count)
        ))
    }

    fn failed(&self, base: &str, id: &str, (code, name): (u32, &str), message: &str) -> Response {
        json_response(format!(
            "{{\"id\":{},\"infoUri\":{},\"stats\":{},\"error\":{{\"message\":{},\"errorCode\":{code},\"errorName\":{},\"errorType\":\"USER_ERROR\",\"failureInfo\":{{\"type\":\"io.trino.spi.TrinoException\",\"message\":{},\"suppressed\":[],\"stack\":[]}}}},\"warnings\":[]}}",
            json_string(id),
            json_string(&format!("{base}/ui/query.html?{id}")),
            stats("FAILED", 0),
            json_string(message),
            json_string(name),
            json_string(message)
        ))
    }

    fn statement(&self, request: &Request) -> Response {
        let base = base_url(request);
        let id = self.next_id();
        let sql = String::from_utf8_lossy(&request.body).trim().trim_end_matches(';').to_owned();
        let mut session = Session::with_dialect(self.store, Dialect::PostgreSql);
        session.identify(&self.user, &self.database);
        session.set_protocol("trino");
        let mut results = Vec::new();
        if let Err(error) = session.execute(&sql, &mut |result| results.push(result)) {
            let (kind, message) = error_of(&error);
            return self.failed(&base, &id, kind, &message);
        }
        match results.into_iter().last() {
            Some(result) if result.columns.is_some() => {
                let columns = columns_json(&result);
                self.page(&base, &id, 0, &columns, rows_json(&result))
            }
            Some(result) => {
                let mut words = result.tag.split(' ');
                let verb = words.next().unwrap_or_default().to_owned();
                let count = result.tag.rsplit(' ').next().and_then(|count| count.parse::<u64>().ok());
                let update = match count {
                    Some(count) => format!(",\"updateType\":{},\"updateCount\":{count}", json_string(&verb)),
                    None => format!(",\"updateType\":{}", json_string(&result.tag)),
                };
                json_response(format!(
                    "{{\"id\":{},\"infoUri\":{},\"stats\":{}{update},\"warnings\":[]}}",
                    json_string(&id),
                    json_string(&format!("{base}/ui/query.html?{id}")),
                    stats("FINISHED", 0)
                ))
            }
            None => json_response(format!(
                "{{\"id\":{},\"infoUri\":{},\"stats\":{},\"warnings\":[]}}",
                json_string(&id),
                json_string(&format!("{base}/ui/query.html?{id}")),
                stats("FINISHED", 0)
            )),
        }
    }

    fn next_page(&self, request: &Request, id: &str, token: u64) -> Response {
        let taken = {
            let mut pending = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            pending
                .iter()
                .position(|query| query.id == id && query.token == token)
                .map(|at| pending.remove(at))
        };
        match taken {
            Some(query) => self.page(&base_url(request), id, token, &query.columns, query.rows),
            None => Response::text(410, "this query's results are gone or were never there"),
        }
    }

    fn cancel(&self, id: &str) -> Response {
        let mut pending = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        pending.retain(|query| query.id != id);
        Response {
            status: 204,
            content_type: "text/plain".to_owned(),
            body: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        if request.path == "/v1/info" {
            return json_response(format!(
                "{{\"nodeVersion\":{{\"version\":{}}},\"environment\":\"eggdb\",\"coordinator\":true,\"starting\":false,\"uptime\":\"0.00s\"}}",
                json_string(env!("CARGO_PKG_VERSION"))
            ));
        }
        if !self.signed_in(request) {
            return Response::text(401, "sign in with the server's user and password (HTTP Basic)")
                .with_header("WWW-Authenticate", "Basic realm=\"eggdb\"");
        }
        let executing = request.path.strip_prefix(EXECUTING_PATH).and_then(|rest| {
            let (id, token) = rest.split_once('/')?;
            Some((id.to_owned(), token.parse::<u64>().ok()?))
        });
        match (request.method.as_str(), request.path.as_str(), executing) {
            ("POST", "/v1/statement", _) => self.statement(request),
            ("GET", _, Some((id, token))) => self.next_page(request, &id, token),
            ("DELETE", _, Some((id, _))) => self.cancel(&id),
            _ => Response::text(404, "Trino's protocol has no such resource here"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_carry_signatures() {
        let (name, signature) = type_of(Some(&SqlType::Timestamp(None)));
        assert_eq!(name, "timestamp(6)");
        assert_eq!(signature, "{\"rawType\":\"timestamp\",\"arguments\":[{\"kind\":\"LONG\",\"value\":6}]}");
        let (name, _) = type_of(None);
        assert_eq!(name, "varchar");
        assert_eq!(value_json(&Value::Double(f64::NAN), true), "\"NaN\"");
        assert_eq!(value_json(&Value::BigInt(7), false), "\"7\"");
    }
}
