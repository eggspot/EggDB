use crate::line_protocol::{self, precision_ns};
use crate::promql::{self, Data, Evaluator, Failure, Fetched, Labels, Work};
use crate::store;
use eggdb_http::{Reply, Request, Response, StreamedResponse};
use eggdb_platform::connection_limits::MemoryBudget;
use eggdb_sql::{Dialect, KeyValueStore, Session, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// The largest request body the endpoint accepts.
pub const MAX_REQUEST_BYTES: usize = 16 << 20;
const MAX_POINTS_PER_SERIES: i64 = 11_000;
const VERSION: &str = "2.53.0";
const REMOTE_WRITE_PATH: &str = "/api/v1/write";
/// The most samples one PromQL query may hold at once, as Prometheus's `--query.max-samples`
/// defaults to.
pub const DEFAULT_MAX_QUERY_SAMPLES: usize = 50_000_000;

/// Whether the HTTP server decodes `request`'s `Content-Encoding` (gzip or deflate) before
/// [`Endpoint::answer`] sees it: for every route but Prometheus remote write, whose body is
/// snappy-compressed and decoded by the endpoint itself (D343).
pub fn wants_content_decoded(request: &Request) -> bool {
    request.path != REMOTE_WRITE_PATH
        && !(request.path == crate::loki::PUSH_PATH
            && request
                .header("content-type")
                .is_some_and(|kind| kind.starts_with("application/x-protobuf")))
}

/// The metrics endpoint: InfluxDB writes, Prometheus remote write and the Prometheus query API.
pub struct Endpoint<'s, S: KeyValueStore> {
    store: &'s S,
    user: String,
    password_sha256: [u8; 32],
    database: String,
    memory_budget: Arc<MemoryBudget>,
    max_samples: usize,
}

impl<S: KeyValueStore> std::fmt::Debug for Endpoint<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

pub(crate) fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32))
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn json(status: u16, body: String) -> Response {
    Response {
        status,
        content_type: "application/json".to_owned(),
        body: body.into_bytes(),
        headers: Vec::new(),
    }
}

fn success(data: &str) -> Response {
    json(200, format!("{{\"status\":\"success\",\"data\":{data}}}"))
}

fn error_json(status: u16, error_type: &str, message: &str) -> Response {
    json(
        status,
        format!(
            "{{\"status\":\"error\",\"errorType\":\"{error_type}\",\"error\":{}}}",
            json_string(message)
        ),
    )
}

fn bad_data(message: &str) -> Response {
    error_json(400, "bad_data", message)
}

fn failed(failure: &Failure) -> Response {
    let message = failure.message();
    match failure {
        Failure::BadData(_) => bad_data(message),
        Failure::Execution(_) => error_json(422, "execution", message),
        Failure::Unavailable(_) => {
            let mut response = error_json(503, "unavailable", message);
            response
                .headers
                .push(("Retry-After".to_owned(), "1".to_owned()));
            response
        }
    }
}

fn no_content() -> Response {
    let mut response = json(204, String::new());
    response
        .headers
        .push(("X-Influxdb-Version".to_owned(), "eggdb".to_owned()));
    response
}

fn number_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 {
            "+Inf".to_owned()
        } else {
            "-Inf".to_owned()
        }
    } else {
        format!("{value}")
    }
}

fn seconds_text(ms: i64) -> String {
    format!("{}", ms as f64 / 1000.0)
}

fn labels_json(labels: &Labels) -> String {
    let fields: Vec<String> = labels
        .iter()
        .map(|(name, value)| format!("{}:{}", json_string(name), json_string(value)))
        .collect();
    format!("{{{}}}", fields.join(","))
}

fn strings_json(values: &BTreeSet<String>) -> String {
    let items: Vec<String> = values.iter().map(|value| json_string(value)).collect();
    format!("[{}]", items.join(","))
}

fn series_json(labels: &Labels, samples: &[(i64, f64)]) -> String {
    let points: Vec<String> = samples
        .iter()
        .map(|(time, value)| format!("[{},\"{}\"]", seconds_text(*time), number_text(*value)))
        .collect();
    format!(
        "{{\"metric\":{},\"values\":[{}]}}",
        labels_json(labels),
        points.join(",")
    )
}

fn matrix_json<'m>(series: impl Iterator<Item = (&'m Labels, &'m Vec<(i64, f64)>)>) -> String {
    let items: Vec<String> = series
        .map(|(labels, samples)| series_json(labels, samples))
        .collect();
    format!("[{}]", items.join(","))
}

fn parameters_of(request: &Request) -> Vec<(String, String)> {
    let mut parameters = form(request.query.as_deref().unwrap_or_default());
    if request.method == "POST"
        && request
            .header("content-type")
            .is_some_and(|kind| kind.starts_with("application/x-www-form-urlencoded"))
    {
        parameters.extend(form(&String::from_utf8_lossy(&request.body)));
    }
    parameters
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' => match text
                .get(at + 1..at + 3)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
            {
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
            (percent_decode(name), percent_decode(value))
        })
        .collect()
}

const MAX_LOG_ROWS_READ: usize = 1_000_000;
const MAX_LOG_LIMIT: usize = 5000;
const UNIX_EPOCH_FROM_2000_MICROS: i64 = 946_684_800_000_000;

fn loki_time(text: Option<&str>, default_ns: i64) -> Result<i64, String> {
    let Some(text) = text else {
        return Ok(default_ns);
    };
    if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) && text.len() >= 16 {
        return text
            .parse()
            .map_err(|_| format!("cannot parse {text:?} to a valid timestamp"));
    }
    let seconds: f64 = text
        .parse()
        .map_err(|_| format!("cannot parse {text:?} to a valid timestamp: use Unix nanoseconds or seconds"))?;
    if !seconds.is_finite() {
        return Err(format!("{text} is not a time"));
    }
    Ok((seconds * 1e9).round() as i64)
}

fn time_param(text: Option<&str>, default: i64) -> Result<i64, String> {
    let Some(text) = text else {
        return Ok(default);
    };
    let seconds: f64 = text
        .parse()
        .map_err(|_| format!("cannot parse {text:?} to a valid timestamp: use Unix seconds"))?;
    if !seconds.is_finite() {
        return Err(format!("{text} is not a time"));
    }
    Ok((seconds * 1000.0).round() as i64)
}

fn required<'t>(value: Option<&'t str>, name: &str) -> Result<&'t str, Failure> {
    value.ok_or_else(|| Failure::BadData(format!("missing parameter {name}")))
}

fn label_in_values_path(path: &str) -> Option<&str> {
    path.strip_prefix("/api/v1/label/")
        .and_then(|rest| rest.strip_suffix("/values"))
}

fn step_param(text: Option<&str>) -> Result<i64, String> {
    let text = text.ok_or("missing parameter step")?;
    let ms = match text.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() => (seconds * 1000.0).round() as i64,
        _ => promql::duration_ms(text)
            .ok_or_else(|| format!("cannot parse {text:?} to a valid duration"))?,
    };
    if ms <= 0 {
        return Err("zero or negative query resolution step widths are not accepted".to_owned());
    }
    Ok(ms)
}

impl<'s, S: KeyValueStore> Endpoint<'s, S> {
    /// An endpoint writing to and reading from `store`'s `metrics` schema as `user` in
    /// `database`, for clients that sign in with `user` and the password whose SHA-256 is
    /// `password_sha256`.
    pub fn new(store: &'s S, user: &str, password_sha256: [u8; 32], database: &str) -> Self {
        Self {
            store,
            user: user.to_owned(),
            password_sha256,
            database: database.to_owned(),
            memory_budget: Arc::new(MemoryBudget::default()),
            max_samples: DEFAULT_MAX_QUERY_SAMPLES,
        }
    }

    /// Holds what PromQL queries read and build against `budget`, the server's one connection
    /// memory budget (D307), instead of a budget of its own.
    pub fn with_memory_budget(mut self, budget: Arc<MemoryBudget>) -> Self {
        self.memory_budget = budget;
        self
    }

    /// Lets one PromQL query hold at most `samples` samples at once (by default
    /// [`DEFAULT_MAX_QUERY_SAMPLES`]).
    pub fn with_max_samples(mut self, samples: usize) -> Self {
        self.max_samples = samples;
        self
    }

    fn work(&self) -> Work<'_> {
        Work::new(&self.memory_budget, self.max_samples)
    }

    fn session(&self) -> Session<'s> {
        let mut session = Session::with_dialect(self.store, Dialect::PostgreSql);
        session.identify(&self.user, &self.database);
        session.set_protocol("observability");
        session
    }

    fn password_ok(&self, password: &[u8]) -> bool {
        eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(password), &self.password_sha256)
    }

    fn authorized(&self, request: &Request, parameters: &[(String, String)]) -> bool {
        if let Some(header) = request.header("authorization") {
            if let Some(token) = header
                .strip_prefix("Token ")
                .or_else(|| header.strip_prefix("Bearer "))
            {
                return self.password_ok(token.trim().as_bytes());
            }
            if let Some(encoded) = header.strip_prefix("Basic ") {
                let Ok(decoded) = eggdb_crypto::base64_decode(encoded.trim().as_bytes()) else {
                    return false;
                };
                let Some(split) = decoded.iter().position(|&byte| byte == b':') else {
                    return false;
                };
                return eggdb_crypto::constant_time_eq(&decoded[..split], self.user.as_bytes())
                    && self.password_ok(&decoded[split + 1..]);
            }
            return false;
        }
        let value = |name: &str| {
            parameters
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.as_str())
        };
        match (value("u"), value("p")) {
            (Some(user), Some(password)) => {
                eggdb_crypto::constant_time_eq(user.as_bytes(), self.user.as_bytes())
                    && self.password_ok(password.as_bytes())
            }
            _ => false,
        }
    }

    /// Answers one HTTP request; the matrix of a `query_range` is a [`StreamedResponse`],
    /// one series rendered at a time as it is sent.
    pub fn reply(&self, request: &Request) -> Reply<'static> {
        let ranged = matches!(request.method.as_str(), "GET" | "POST")
            && request.path == "/api/v1/query_range";
        if !ranged {
            return Reply::Whole(self.answer(request));
        }
        let parameters = parameters_of(request);
        if !self.authorized(request, &parameters) {
            return Reply::Whole(self.answer(request));
        }
        let value = |name: &str| {
            parameters
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.as_str())
        };
        match self.range(value("query"), value("start"), value("end"), value("step")) {
            Ok(series) => Reply::Streamed(StreamedResponse::new(
                200,
                "application/json",
                move |out| {
                    out.write_all(b"{\"status\":\"success\",\"data\":{\"resultType\":\"matrix\",\"result\":[")?;
                    for (at, (labels, samples)) in series.iter().enumerate() {
                        if at > 0 {
                            out.write_all(b",")?;
                        }
                        out.write_all(series_json(labels, samples).as_bytes())?;
                    }
                    out.write_all(b"]}}")
                },
            )),
            Err(failure) => Reply::Whole(failed(&failure)),
        }
    }

    /// Answers one HTTP request with its whole response in memory.
    pub fn answer(&self, request: &Request) -> Response {
        let parameters = parameters_of(request);
        match (request.method.as_str(), request.path.as_str()) {
            ("GET" | "HEAD", "/ping") => return no_content(),
            ("GET", "/health" | "/-/healthy" | "/-/ready") => {
                return json(200, "{\"name\":\"eggdb\",\"status\":\"pass\"}".to_owned());
            }
            _ => {}
        }
        if !self.authorized(request, &parameters) {
            let mut response = json(
                401,
                "{\"code\":\"unauthorized\",\"message\":\"unauthorized access\"}".to_owned(),
            );
            response.headers.push((
                "WWW-Authenticate".to_owned(),
                "Basic realm=\"eggdb\"".to_owned(),
            ));
            return response;
        }
        let value = |name: &str| {
            parameters
                .iter()
                .find(|(field, _)| field == name)
                .map(|(_, value)| value.as_str())
        };
        match (request.method.as_str(), request.path.as_str()) {
            ("GET" | "POST", "/query") => match value("q") {
                Some(text) => {
                    let database = value("db").unwrap_or(&self.database).to_owned();
                    let body = crate::influxql::answer(&mut self.session(), text, &database, value("epoch"));
                    json(200, body)
                }
                None => json(400, "{\"error\":\"missing required parameter q\"}".to_owned()),
            },
            ("POST", "/write" | "/api/v2/write") => {
                self.write_lines(request, value("precision").unwrap_or("ns"))
            }
            ("POST", REMOTE_WRITE_PATH) => self.remote_write(request),
            ("POST", crate::loki::PUSH_PATH) => self.loki_push(request),
            ("POST", crate::otlp::LOGS_PATH | crate::otlp::TRACES_PATH | crate::otlp::METRICS_PATH) => {
                let protobuf = request
                    .header("content-type")
                    .is_some_and(|kind| kind.starts_with("application/x-protobuf"));
                let decoded = match (request.path.as_str(), protobuf) {
                    (crate::otlp::LOGS_PATH, false) => crate::otlp::logs(&request.body),
                    (crate::otlp::TRACES_PATH, false) => crate::otlp::traces(&request.body),
                    (_, false) => crate::otlp::metrics(&request.body),
                    (crate::otlp::LOGS_PATH, true) => crate::otlp_protobuf::logs(&request.body),
                    (crate::otlp::TRACES_PATH, true) => crate::otlp_protobuf::traces(&request.body),
                    (_, true) => crate::otlp_protobuf::metrics(&request.body),
                };
                match decoded.and_then(|points| store::write(&mut self.session(), &points)) {
                    Ok(_) if protobuf => Response {
                        status: 200,
                        content_type: "application/x-protobuf".to_owned(),
                        body: Vec::new(),
                        headers: Vec::new(),
                    },
                    Ok(_) => json(200, "{}".to_owned()),
                    Err(message) => json(
                        400,
                        format!("{{\"code\":3,\"message\":{}}}", json_string(&message)),
                    ),
                }
            }
            ("POST", crate::zipkin::SPANS_PATH) => {
                match crate::zipkin::decode(&request.body)
                    .and_then(|points| store::write(&mut self.session(), &points))
                {
                    Ok(_) => json(202, String::new()),
                    Err(message) => json(400, format!("{{\"message\":{}}}", json_string(&message))),
                }
            }
            ("GET", "/api/v2/services") => self.zipkin_names("service", None),
            ("GET", crate::zipkin::SPANS_PATH) => match value("serviceName") {
                Some(service) => self.zipkin_names("name", Some(service)),
                None => json(400, "{\"message\":\"serviceName is required\"}".to_owned()),
            },
            ("GET", path) if path.starts_with("/api/v2/trace/") => {
                self.zipkin_trace(path.trim_start_matches("/api/v2/trace/"))
            }
            ("GET" | "POST", "/loki/api/v1/query_range") => self.loki_query(
                value("query"),
                (value("start"), value("end")),
                value("limit"),
                value("direction"),
            ),
            ("GET" | "POST", "/loki/api/v1/labels") => self.loki_labels(None),
            ("GET" | "POST", path)
                if path.starts_with("/loki/api/v1/label/") && path.ends_with("/values") =>
            {
                let name = path
                    .trim_start_matches("/loki/api/v1/label/")
                    .trim_end_matches("/values");
                self.loki_labels(Some(&percent_decode(name)))
            }
            ("GET" | "POST", "/api/v1/query") => self.query(value("query"), value("time")),
            ("GET" | "POST", "/api/v1/query_range") => {
                self.query_range(value("query"), value("start"), value("end"), value("step"))
            }
            ("GET" | "POST", "/api/v1/labels") => self.labels(),
            ("GET" | "POST", "/api/v1/series") => {
                let matches: Vec<&str> = parameters
                    .iter()
                    .filter(|(name, _)| name == "match[]")
                    .map(|(_, value)| value.as_str())
                    .collect();
                self.series(&matches, value("start"), value("end"))
            }
            ("GET", "/api/v1/status/buildinfo") => success(&format!(
                "{{\"version\":\"{VERSION}\",\"revision\":\"eggdb\",\"branch\":\"\",\"buildUser\":\"\",\"buildDate\":\"\",\"goVersion\":\"\"}}"
            )),
            ("GET", path) if label_in_values_path(path).is_some() => {
                self.label_values(&percent_decode(label_in_values_path(path).unwrap_or("")))
            }
            _ => json(
                404,
                "{\"status\":\"error\",\"errorType\":\"not_found\",\"error\":\"not found\"}"
                    .to_owned(),
            ),
        }
    }

    fn write_lines(&self, request: &Request, precision: &str) -> Response {
        let Some(unit) = precision_ns(precision) else {
            return json(
                400,
                format!(
                    "{{\"code\":\"invalid\",\"message\":{}}}",
                    json_string(&format!("invalid precision {precision}"))
                ),
            );
        };
        let Ok(body) = std::str::from_utf8(&request.body) else {
            return json(
                400,
                "{\"code\":\"invalid\",\"message\":\"the body is not UTF-8\"}".to_owned(),
            );
        };
        let written = line_protocol::parse(body, unit, now_ms().saturating_mul(1_000_000))
            .and_then(|points| store::write(&mut self.session(), &points));
        match written {
            Ok(_) => no_content(),
            Err(message) => json(
                400,
                format!(
                    "{{\"code\":\"invalid\",\"message\":{},\"error\":{}}}",
                    json_string(&message),
                    json_string(&message)
                ),
            ),
        }
    }

    fn log_rows(&self, from_ns: i64, to_ns: i64, forward: bool) -> Result<Vec<(Vec<(String, String)>, i64, String)>, String> {
        let sql = format!(
            "SELECT * FROM {}.logs WHERE time >= {} AND time <= {} ORDER BY time {} LIMIT {MAX_LOG_ROWS_READ}",
            store::SCHEMA,
            store::timestamp_literal(from_ns.div_euclid(1000)),
            store::timestamp_literal(to_ns.div_euclid(1000)),
            if forward { "ASC" } else { "DESC" }
        );
        let results = match store::run(&mut self.session(), &sql) {
            Ok(results) => results,
            Err(message) if message.contains("does not exist") => return Ok(Vec::new()),
            Err(message) => return Err(message),
        };
        let Some(found) = results.into_iter().rev().find(|result| result.columns.is_some()) else {
            return Ok(Vec::new());
        };
        let names: Vec<String> = found
            .columns
            .iter()
            .flatten()
            .map(|column| column.name.clone())
            .collect();
        let mut rows = Vec::with_capacity(found.rows.len());
        for row in found.rows {
            let mut labels = Vec::new();
            let mut line = String::new();
            let mut time_ns = 0i64;
            for (name, value) in names.iter().zip(row) {
                match (name.as_str(), value) {
                    ("time", Value::TimestampTz(micros)) => {
                        time_ns = micros
                            .saturating_add(UNIX_EPOCH_FROM_2000_MICROS)
                            .saturating_mul(1000);
                    }
                    ("line", Value::Text(text)) => line = text,
                    (_, Value::Text(text)) if !text.is_empty() => labels.push((name.clone(), text)),
                    _ => {}
                }
            }
            labels.sort();
            rows.push((labels, time_ns, line));
        }
        Ok(rows)
    }

    fn loki_query(
        &self,
        query: Option<&str>,
        (start, end): (Option<&str>, Option<&str>),
        limit: Option<&str>,
        direction: Option<&str>,
    ) -> Response {
        let Some(query) = query else {
            return bad_data("missing parameter query");
        };
        let parsed = match crate::logql::parse(query) {
            Ok(parsed) => parsed,
            Err(message) => return bad_data(&message),
        };
        let now_ns = now_ms().saturating_mul(1_000_000);
        let to_ns = match loki_time(end, now_ns) {
            Ok(value) => value,
            Err(message) => return bad_data(&message),
        };
        let from_ns = match loki_time(start, to_ns.saturating_sub(3_600_000_000_000)) {
            Ok(value) => value,
            Err(message) => return bad_data(&message),
        };
        let limit = match limit.map(str::parse::<usize>) {
            None => 100,
            Some(Ok(limit)) if limit > 0 => limit.min(MAX_LOG_LIMIT),
            Some(_) => return bad_data("limit must be a positive whole number"),
        };
        let forward = direction.is_some_and(|direction| direction.eq_ignore_ascii_case("forward"));
        let rows = match self.log_rows(from_ns, to_ns, forward) {
            Ok(rows) => rows,
            Err(message) => return bad_data(&message),
        };
        let mut streams: BTreeMap<Vec<(String, String)>, Vec<(i64, String)>> = BTreeMap::new();
        let mut kept = 0usize;
        for (labels, time_ns, line) in rows {
            if kept >= limit {
                break;
            }
            if parsed.keeps(&labels, &line) {
                streams.entry(labels).or_default().push((time_ns, line));
                kept += 1;
            }
        }
        let result: Vec<String> = streams
            .into_iter()
            .map(|(labels, values)| {
                let entries: Vec<String> = values
                    .iter()
                    .map(|(time_ns, line)| format!("[\"{time_ns}\",{}]", json_string(line)))
                    .collect();
                format!(
                    "{{\"stream\":{},\"values\":[{}]}}",
                    labels_json(&labels),
                    entries.join(",")
                )
            })
            .collect();
        success(&format!(
            "{{\"resultType\":\"streams\",\"result\":[{}],\"stats\":{{}}}}",
            result.join(",")
        ))
    }

    fn loki_labels(&self, name: Option<&str>) -> Response {
        let now_ns = now_ms().saturating_mul(1_000_000);
        let rows = match self.log_rows(now_ns.saturating_sub(6 * 3_600_000_000_000), now_ns, false) {
            Ok(rows) => rows,
            Err(message) => return bad_data(&message),
        };
        let mut found: BTreeSet<String> = BTreeSet::new();
        for (labels, _, _) in rows {
            for (label, value) in labels {
                match name {
                    None => {
                        found.insert(label);
                    }
                    Some(wanted) if wanted == label => {
                        found.insert(value);
                    }
                    Some(_) => {}
                }
            }
        }
        success(&strings_json(&found))
    }

    fn span_rows(&self, filter: &str) -> Result<Vec<Vec<(String, Value)>>, String> {
        let sql = format!(
            "SELECT * FROM {}.{} WHERE {filter} ORDER BY time LIMIT {MAX_LOG_ROWS_READ}",
            store::SCHEMA,
            crate::zipkin::MEASUREMENT
        );
        let results = match store::run(&mut self.session(), &sql) {
            Ok(results) => results,
            Err(message) if message.contains("does not exist") => return Ok(Vec::new()),
            Err(message) => return Err(message),
        };
        let Some(found) = results.into_iter().rev().find(|result| result.columns.is_some()) else {
            return Ok(Vec::new());
        };
        let names: Vec<String> = found
            .columns
            .iter()
            .flatten()
            .map(|column| column.name.clone())
            .collect();
        Ok(found
            .rows
            .into_iter()
            .map(|row| names.iter().cloned().zip(row).collect())
            .collect())
    }

    fn zipkin_names(&self, column: &str, service: Option<&str>) -> Response {
        let filter = match service {
            Some(service) => format!("service = {}", store::literal(service)),
            None => "true".to_owned(),
        };
        let rows = match self.span_rows(&filter) {
            Ok(rows) => rows,
            Err(message) => return json(400, format!("{{\"message\":{}}}", json_string(&message))),
        };
        let names: BTreeSet<String> = rows
            .into_iter()
            .flat_map(|row| row.into_iter())
            .filter_map(|(name, value)| match value {
                Value::Text(text) if name == column => Some(text),
                _ => None,
            })
            .collect();
        json(200, strings_json(&names))
    }

    fn zipkin_trace(&self, trace_id: &str) -> Response {
        let trace_id = trace_id.to_ascii_lowercase();
        if trace_id.is_empty() || trace_id.len() > 32 || !trace_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return json(400, "{\"message\":\"a trace id is 16 or 32 hex digits\"}".to_owned());
        }
        let rows = match self.span_rows(&format!("trace_id = {}", store::literal(&trace_id))) {
            Ok(rows) => rows,
            Err(message) => return json(400, format!("{{\"message\":{}}}", json_string(&message))),
        };
        if rows.is_empty() {
            return json(404, format!("{{\"message\":\"trace {trace_id} not found\"}}"));
        }
        let spans: Vec<String> = rows
            .into_iter()
            .map(|row| {
                let mut fields = Vec::new();
                for (name, value) in row {
                    let key = match name.as_str() {
                        "trace_id" => "traceId",
                        "span_id" => "id",
                        "parent_id" => "parentId",
                        "name" => "name",
                        "kind" => "kind",
                        "duration_us" => "duration",
                        "time" => "timestamp",
                        "service" => "localEndpoint",
                        "remote_service" => "remoteEndpoint",
                        "tags" => "tags",
                        _ => continue,
                    };
                    let rendered = match (key, value) {
                        ("timestamp", Value::TimestampTz(micros)) => {
                            micros.saturating_add(UNIX_EPOCH_FROM_2000_MICROS).to_string()
                        }
                        ("duration", Value::BigInt(duration)) => duration.to_string(),
                        ("tags", Value::Text(text)) => text,
                        ("localEndpoint" | "remoteEndpoint", Value::Text(service)) => {
                            format!("{{\"serviceName\":{}}}", json_string(&service))
                        }
                        (_, Value::Text(text)) => json_string(&text),
                        _ => continue,
                    };
                    fields.push(format!("{}:{rendered}", json_string(key)));
                }
                format!("{{{}}}", fields.join(","))
            })
            .collect();
        json(200, format!("[{}]", spans.join(",")))
    }

    fn loki_push(&self, request: &Request) -> Response {
        let content_type = request.header("content-type").unwrap_or("application/json");
        match crate::loki::decode(content_type, &request.body)
            .and_then(|points| store::write(&mut self.session(), &points))
        {
            Ok(_) => no_content(),
            Err(message) => json(400, format!("{{\"message\":{}}}", json_string(&message))),
        }
    }

    fn remote_write(&self, request: &Request) -> Response {
        match crate::remote_write::decode(&request.body)
            .and_then(|points| store::write(&mut self.session(), &points))
        {
            Ok(_) => no_content(),
            Err(message) => json(400, format!("{{\"error\":{}}}", json_string(&message))),
        }
    }

    fn fetch(
        &self,
        expr: &promql::Expr,
        start: i64,
        end: i64,
        work: &mut Work<'_>,
    ) -> Result<Fetched, Failure> {
        let mut wanted = Vec::new();
        promql::selectors(expr, 0, &mut wanted);
        let mut data = BTreeMap::new();
        if wanted.is_empty() {
            return Ok(data);
        }
        let mut session = self.session();
        let tables = store::catalog(&mut session)?;
        let metrics = store::metrics(&tables);
        let mut reach: BTreeMap<String, i64> = BTreeMap::new();
        for (selector, before) in wanted {
            let names: Vec<&String> = match &selector.name {
                Some(name) => vec![name],
                None => metrics
                    .iter()
                    .filter(|(name, tags)| selector.could_match(name, tags))
                    .map(|(name, _)| name)
                    .collect(),
            };
            for name in names {
                let entry = reach.entry(name.clone()).or_insert(0);
                *entry = (*entry).max(before);
            }
        }
        for (name, before) in reach {
            let series = store::read(
                &mut session,
                &tables,
                &name,
                start.saturating_sub(before),
                end,
                work.remaining_samples().saturating_add(1),
            )?;
            work.keep(series.iter().map(|series| series.samples.len()).sum())?;
            data.insert(name, series);
        }
        Ok(data)
    }

    fn instant(&self, query: Option<&str>, time: Option<&str>) -> Result<(Data, i64), Failure> {
        let expr = promql::parse(required(query, "query")?).map_err(Failure::BadData)?;
        let time = time_param(time, now_ms()).map_err(Failure::BadData)?;
        let mut work = self.work();
        let data = self.fetch(&expr, time, time, &mut work)?;
        let result = Evaluator::new(&data, work).evaluate(&expr, time)?;
        Ok((result, time))
    }

    fn query(&self, query: Option<&str>, time: Option<&str>) -> Response {
        match self.instant(query, time) {
            Ok((Data::Scalar(value), time)) => success(&format!(
                "{{\"resultType\":\"scalar\",\"result\":[{},\"{}\"]}}",
                seconds_text(time),
                number_text(value)
            )),
            Ok((Data::Vector(vector), time)) => {
                let items: Vec<String> = vector
                    .iter()
                    .map(|(labels, value)| {
                        format!(
                            "{{\"metric\":{},\"value\":[{},\"{}\"]}}",
                            labels_json(labels),
                            seconds_text(time),
                            number_text(*value)
                        )
                    })
                    .collect();
                success(&format!(
                    "{{\"resultType\":\"vector\",\"result\":[{}]}}",
                    items.join(",")
                ))
            }
            Ok((Data::Matrix(matrix), _)) => success(&format!(
                "{{\"resultType\":\"matrix\",\"result\":{}}}",
                matrix_json(matrix.iter().map(|(labels, samples)| (labels, samples)))
            )),
            Err(failure) => failed(&failure),
        }
    }

    fn range(
        &self,
        query: Option<&str>,
        start: Option<&str>,
        end: Option<&str>,
        step: Option<&str>,
    ) -> Result<BTreeMap<Labels, Vec<(i64, f64)>>, Failure> {
        let expr = promql::parse(required(query, "query")?).map_err(Failure::BadData)?;
        let start = time_param(Some(required(start, "start")?), 0).map_err(Failure::BadData)?;
        let end = time_param(Some(required(end, "end")?), 0).map_err(Failure::BadData)?;
        let step = step_param(step).map_err(Failure::BadData)?;
        if end < start {
            return Err(Failure::BadData(
                "end timestamp must not be before start time".to_owned(),
            ));
        }
        if end.saturating_sub(start) / step > MAX_POINTS_PER_SERIES {
            return Err(Failure::BadData("exceeded maximum resolution of 11,000 points per timeseries. Try decreasing the query resolution (?step=XX)".to_owned()));
        }
        let mut work = self.work();
        let data = self.fetch(&expr, start, end, &mut work)?;
        let mut evaluator = Evaluator::new(&data, work);
        let mut series: BTreeMap<Labels, Vec<(i64, f64)>> = BTreeMap::new();
        let mut time = start;
        while time <= end {
            let result = evaluator.evaluate(&expr, time)?;
            evaluator.work.finish_step();
            match result {
                Data::Scalar(value) => {
                    evaluator.work.keep(1)?;
                    series.entry(Vec::new()).or_default().push((time, value));
                }
                Data::Vector(vector) => {
                    evaluator.work.keep(vector.len())?;
                    for (labels, value) in vector {
                        series.entry(labels).or_default().push((time, value));
                    }
                }
                Data::Matrix(_) => {
                    return Err(Failure::BadData(
                        "a range query needs an instant vector or scalar expression".to_owned(),
                    ));
                }
            }
            let Some(next) = time.checked_add(step) else {
                break;
            };
            time = next;
        }
        Ok(series)
    }

    fn query_range(
        &self,
        query: Option<&str>,
        start: Option<&str>,
        end: Option<&str>,
        step: Option<&str>,
    ) -> Response {
        match self.range(query, start, end, step) {
            Ok(series) => success(&format!(
                "{{\"resultType\":\"matrix\",\"result\":{}}}",
                matrix_json(series.iter())
            )),
            Err(failure) => failed(&failure),
        }
    }

    fn labels(&self) -> Response {
        match store::catalog(&mut self.session()) {
            Ok(tables) => {
                let mut names: BTreeSet<String> = BTreeSet::from(["__name__".to_owned()]);
                for columns in tables.values() {
                    names.extend(
                        columns
                            .iter()
                            .filter(|(name, data_type)| data_type == "text" && name != "time")
                            .map(|(name, _)| name.clone()),
                    );
                }
                success(&strings_json(&names))
            }
            Err(message) => bad_data(&message),
        }
    }

    fn values_of(&self, name: &str) -> Result<BTreeSet<String>, String> {
        let mut session = self.session();
        let tables = store::catalog(&mut session)?;
        if name == "__name__" {
            return Ok(store::metrics(&tables)
                .into_iter()
                .map(|(metric, _)| metric)
                .collect());
        }
        let mut values = BTreeSet::new();
        for (table, columns) in &tables {
            if columns
                .iter()
                .any(|(column, data_type)| column == name && data_type == "text")
            {
                let sql = format!(
                    "SELECT DISTINCT {} FROM {}.{} WHERE {} IS NOT NULL",
                    store::identifier(name),
                    store::SCHEMA,
                    store::identifier(table),
                    store::identifier(name)
                );
                for result in store::run(&mut session, &sql)? {
                    values.extend(
                        result
                            .rows
                            .iter()
                            .filter_map(|row| row.first().and_then(Value::to_text)),
                    );
                }
            }
        }
        Ok(values)
    }

    fn label_values(&self, name: &str) -> Response {
        match self.values_of(name) {
            Ok(values) => success(&strings_json(&values)),
            Err(message) => bad_data(&message),
        }
    }

    fn matching_series(
        &self,
        matches: &[&str],
        start: Option<&str>,
        end: Option<&str>,
    ) -> Result<BTreeSet<Labels>, Failure> {
        if matches.is_empty() {
            return Err(Failure::BadData("no match[] parameter provided".to_owned()));
        }
        let end = time_param(end, now_ms()).map_err(Failure::BadData)?;
        let start =
            time_param(start, end.saturating_sub(promql::LOOKBACK_MS)).map_err(Failure::BadData)?;
        let mut found = BTreeSet::new();
        for text in matches {
            let expr = promql::parse(text).map_err(Failure::BadData)?;
            let promql::Expr::Selector(selector) = &expr else {
                return Err(Failure::BadData(
                    "match[] must be a series selector".to_owned(),
                ));
            };
            let mut work = self.work();
            let data = self.fetch(&expr, start, end, &mut work)?;
            for series in promql::chosen(selector, &data) {
                if series
                    .samples
                    .iter()
                    .any(|(time, _)| *time >= start && *time <= end)
                {
                    found.insert(series.labels.clone());
                }
            }
        }
        Ok(found)
    }

    fn series(&self, matches: &[&str], start: Option<&str>, end: Option<&str>) -> Response {
        match self.matching_series(matches, start, end) {
            Ok(found) => {
                let items: Vec<String> = found.iter().map(labels_json).collect();
                success(&format!("[{}]", items.join(",")))
            }
            Err(failure) => failed(&failure),
        }
    }
}
