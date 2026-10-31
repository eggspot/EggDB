use crate::line_protocol::{self, precision_ns};
use crate::promql::{self, Data, Labels};
use crate::store::{self, Series};
use eggdb_http::{Request, Response};
use eggdb_sql::{Dialect, KeyValueStore, Session, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

/// The largest request body the endpoint accepts.
pub const MAX_REQUEST_BYTES: usize = 16 << 20;
const MAX_POINTS_PER_SERIES: i64 = 11_000;
const VERSION: &str = "2.53.0";

/// The metrics endpoint: InfluxDB writes, Prometheus remote write and the Prometheus query API.
pub struct Endpoint<'s, S: KeyValueStore> {
    store: &'s S,
    user: String,
    password_sha256: [u8; 32],
    database: String,
}

impl<S: KeyValueStore> std::fmt::Debug for Endpoint<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("user", &self.user).finish_non_exhaustive()
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn json(status: u16, body: String) -> Response {
    Response { status, content_type: "application/json".to_owned(), body: body.into_bytes(), headers: Vec::new() }
}

fn success(data: &str) -> Response {
    json(200, format!("{{\"status\":\"success\",\"data\":{data}}}"))
}

fn bad_data(message: &str) -> Response {
    json(400, format!("{{\"status\":\"error\",\"errorType\":\"bad_data\",\"error\":{}}}", json_string(message)))
}

fn no_content() -> Response {
    let mut response = json(204, String::new());
    response.headers.push(("X-Influxdb-Version".to_owned(), "eggdb".to_owned()));
    response
}

fn number_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "+Inf".to_owned() } else { "-Inf".to_owned() }
    } else {
        format!("{value}")
    }
}

fn seconds_text(ms: i64) -> String {
    format!("{}", ms as f64 / 1000.0)
}

fn labels_json(labels: &Labels) -> String {
    let fields: Vec<String> = labels.iter().map(|(name, value)| format!("{}:{}", json_string(name), json_string(value))).collect();
    format!("{{{}}}", fields.join(","))
}

fn strings_json(values: &BTreeSet<String>) -> String {
    let items: Vec<String> = values.iter().map(|value| json_string(value)).collect();
    format!("[{}]", items.join(","))
}

fn matrix_json<'m>(series: impl Iterator<Item = (&'m Labels, &'m Vec<(i64, f64)>)>) -> String {
    let items: Vec<String> = series
        .map(|(labels, samples)| {
            let points: Vec<String> = samples.iter().map(|(time, value)| format!("[{},\"{}\"]", seconds_text(*time), number_text(*value))).collect();
            format!("{{\"metric\":{},\"values\":[{}]}}", labels_json(labels), points.join(","))
        })
        .collect();
    format!("[{}]", items.join(","))
}

fn percent_decode(text: &str) -> String {
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
            (percent_decode(name), percent_decode(value))
        })
        .collect()
}

fn time_param(text: Option<&str>, default: i64) -> Result<i64, String> {
    let Some(text) = text else {
        return Ok(default);
    };
    let seconds: f64 = text.parse().map_err(|_| format!("cannot parse {text:?} to a valid timestamp: use Unix seconds"))?;
    if !seconds.is_finite() {
        return Err(format!("{text} is not a time"));
    }
    Ok((seconds * 1000.0).round() as i64)
}

fn step_param(text: Option<&str>) -> Result<i64, String> {
    let text = text.ok_or("missing parameter step")?;
    let ms = match text.parse::<f64>() {
        Ok(seconds) if seconds.is_finite() => (seconds * 1000.0).round() as i64,
        _ => promql::duration_ms(text).ok_or_else(|| format!("cannot parse {text:?} to a valid duration"))?,
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
        Self { store, user: user.to_owned(), password_sha256, database: database.to_owned() }
    }

    fn session(&self) -> Session<'s, S> {
        let mut session = Session::with_dialect(self.store, Dialect::PostgreSql);
        session.identify(&self.user, &self.database);
        session
    }

    fn password_ok(&self, password: &[u8]) -> bool {
        eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(password), &self.password_sha256)
    }

    fn authorized(&self, request: &Request, parameters: &[(String, String)]) -> bool {
        if let Some(header) = request.header("authorization") {
            if let Some(token) = header.strip_prefix("Token ").or_else(|| header.strip_prefix("Bearer ")) {
                return self.password_ok(token.trim().as_bytes());
            }
            if let Some(encoded) = header.strip_prefix("Basic ") {
                let Ok(decoded) = eggdb_crypto::base64_decode(encoded.trim()) else {
                    return false;
                };
                let Some(split) = decoded.iter().position(|&byte| byte == b':') else {
                    return false;
                };
                return eggdb_crypto::constant_time_eq(&decoded[..split], self.user.as_bytes()) && self.password_ok(&decoded[split + 1..]);
            }
            return false;
        }
        let value = |name: &str| parameters.iter().find(|(field, _)| field == name).map(|(_, value)| value.as_str());
        match (value("u"), value("p")) {
            (Some(user), Some(password)) => eggdb_crypto::constant_time_eq(user.as_bytes(), self.user.as_bytes()) && self.password_ok(password.as_bytes()),
            _ => false,
        }
    }

    /// Answers one HTTP request.
    pub fn answer(&self, request: &Request) -> Response {
        let mut parameters = form(request.query.as_deref().unwrap_or_default());
        if request.method == "POST" && request.header("content-type").is_some_and(|kind| kind.starts_with("application/x-www-form-urlencoded")) {
            parameters.extend(form(&String::from_utf8_lossy(&request.body)));
        }
        match (request.method.as_str(), request.path.as_str()) {
            ("GET" | "HEAD", "/ping") => return no_content(),
            ("GET", "/health" | "/-/healthy" | "/-/ready") => return json(200, "{\"name\":\"eggdb\",\"status\":\"pass\"}".to_owned()),
            _ => {}
        }
        if !self.authorized(request, &parameters) {
            let mut response = json(401, "{\"code\":\"unauthorized\",\"message\":\"unauthorized access\"}".to_owned());
            response.headers.push(("WWW-Authenticate".to_owned(), "Basic realm=\"eggdb\"".to_owned()));
            return response;
        }
        let value = |name: &str| parameters.iter().find(|(field, _)| field == name).map(|(_, value)| value.as_str());
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/write" | "/api/v2/write") => self.write_lines(request, value("precision").unwrap_or("ns")),
            ("POST", "/api/v1/write") => self.remote_write(request),
            ("GET" | "POST", "/api/v1/query") => self.query(value("query"), value("time")),
            ("GET" | "POST", "/api/v1/query_range") => self.query_range(value("query"), value("start"), value("end"), value("step")),
            ("GET" | "POST", "/api/v1/labels") => self.labels(),
            ("GET" | "POST", "/api/v1/series") => {
                let matches: Vec<&str> = parameters.iter().filter(|(name, _)| name == "match[]").map(|(_, value)| value.as_str()).collect();
                self.series(&matches, value("start"), value("end"))
            }
            ("GET", "/api/v1/status/buildinfo") => success(&format!("{{\"version\":\"{VERSION}\",\"revision\":\"eggdb\",\"branch\":\"\",\"buildUser\":\"\",\"buildDate\":\"\",\"goVersion\":\"\"}}")),
            ("GET", path) if path.starts_with("/api/v1/label/") && path.ends_with("/values") => {
                let name = percent_decode(&path["/api/v1/label/".len()..path.len() - "/values".len()]);
                self.label_values(&name)
            }
            _ => json(404, "{\"status\":\"error\",\"errorType\":\"not_found\",\"error\":\"not found\"}".to_owned()),
        }
    }

    fn write_lines(&self, request: &Request, precision: &str) -> Response {
        if request.header("content-encoding").is_some_and(|encoding| !encoding.eq_ignore_ascii_case("identity")) {
            return json(415, "{\"code\":\"unsupported media type\",\"message\":\"compressed line protocol is not supported yet\"}".to_owned());
        }
        let Some(unit) = precision_ns(precision) else {
            return json(400, format!("{{\"code\":\"invalid\",\"message\":{}}}", json_string(&format!("invalid precision {precision}"))));
        };
        let Ok(body) = std::str::from_utf8(&request.body) else {
            return json(400, "{\"code\":\"invalid\",\"message\":\"the body is not UTF-8\"}".to_owned());
        };
        let written = line_protocol::parse(body, unit, now_ms().saturating_mul(1_000_000)).and_then(|points| store::write(&mut self.session(), &points));
        match written {
            Ok(_) => no_content(),
            Err(message) => json(400, format!("{{\"code\":\"invalid\",\"message\":{},\"error\":{}}}", json_string(&message), json_string(&message))),
        }
    }

    fn remote_write(&self, request: &Request) -> Response {
        match crate::remote_write::decode(&request.body).and_then(|points| store::write(&mut self.session(), &points)) {
            Ok(_) => no_content(),
            Err(message) => json(400, format!("{{\"error\":{}}}", json_string(&message))),
        }
    }

    fn fetch(&self, expr: &promql::Expr, start: i64, end: i64) -> Result<BTreeMap<String, Vec<Series>>, String> {
        let mut wanted = Vec::new();
        promql::selectors(expr, &mut wanted);
        let mut data = BTreeMap::new();
        if wanted.is_empty() {
            return Ok(data);
        }
        let mut session = self.session();
        let tables = store::catalog(&mut session)?;
        let mut reach: BTreeMap<String, i64> = BTreeMap::new();
        for (name, before) in wanted {
            let entry = reach.entry(name).or_insert(0);
            *entry = (*entry).max(before);
        }
        for (name, before) in reach {
            let series = store::read(&mut session, &tables, &name, start - before, end)?;
            data.insert(name, series);
        }
        Ok(data)
    }

    fn instant(&self, query: Option<&str>, time: Option<&str>) -> Result<(Data, i64), String> {
        let expr = promql::parse(query.ok_or("missing parameter query")?)?;
        let time = time_param(time, now_ms())?;
        let data = self.fetch(&expr, time, time)?;
        Ok((promql::evaluate(&expr, time, &data)?, time))
    }

    fn query(&self, query: Option<&str>, time: Option<&str>) -> Response {
        match self.instant(query, time) {
            Ok((Data::Scalar(value), time)) => success(&format!("{{\"resultType\":\"scalar\",\"result\":[{},\"{}\"]}}", seconds_text(time), number_text(value))),
            Ok((Data::Vector(vector), time)) => {
                let items: Vec<String> = vector.iter().map(|(labels, value)| format!("{{\"metric\":{},\"value\":[{},\"{}\"]}}", labels_json(labels), seconds_text(time), number_text(*value))).collect();
                success(&format!("{{\"resultType\":\"vector\",\"result\":[{}]}}", items.join(",")))
            }
            Ok((Data::Matrix(matrix), _)) => success(&format!("{{\"resultType\":\"matrix\",\"result\":{}}}", matrix_json(matrix.iter().map(|(labels, samples)| (labels, samples))))),
            Err(message) => bad_data(&message),
        }
    }

    fn range(&self, query: Option<&str>, start: Option<&str>, end: Option<&str>, step: Option<&str>) -> Result<BTreeMap<Labels, Vec<(i64, f64)>>, String> {
        let expr = promql::parse(query.ok_or("missing parameter query")?)?;
        let start = time_param(Some(start.ok_or("missing parameter start")?), 0)?;
        let end = time_param(Some(end.ok_or("missing parameter end")?), 0)?;
        let step = step_param(step)?;
        if end < start {
            return Err("end timestamp must not be before start time".to_owned());
        }
        if (end - start) / step > MAX_POINTS_PER_SERIES {
            return Err("exceeded maximum resolution of 11,000 points per timeseries. Try decreasing the query resolution (?step=XX)".to_owned());
        }
        let data = self.fetch(&expr, start, end)?;
        let mut series: BTreeMap<Labels, Vec<(i64, f64)>> = BTreeMap::new();
        let mut time = start;
        while time <= end {
            match promql::evaluate(&expr, time, &data)? {
                Data::Scalar(value) => series.entry(Vec::new()).or_default().push((time, value)),
                Data::Vector(vector) => {
                    for (labels, value) in vector {
                        series.entry(labels).or_default().push((time, value));
                    }
                }
                Data::Matrix(_) => return Err("a range query needs an instant vector or scalar expression".to_owned()),
            }
            time += step;
        }
        Ok(series)
    }

    fn query_range(&self, query: Option<&str>, start: Option<&str>, end: Option<&str>, step: Option<&str>) -> Response {
        match self.range(query, start, end, step) {
            Ok(series) => success(&format!("{{\"resultType\":\"matrix\",\"result\":{}}}", matrix_json(series.iter()))),
            Err(message) => bad_data(&message),
        }
    }

    fn labels(&self) -> Response {
        match store::catalog(&mut self.session()) {
            Ok(tables) => {
                let mut names: BTreeSet<String> = BTreeSet::from(["__name__".to_owned()]);
                for columns in tables.values() {
                    names.extend(columns.iter().filter(|(name, data_type)| data_type == "text" && name != "time").map(|(name, _)| name.clone()));
                }
                success(&strings_json(&names))
            }
            Err(message) => bad_data(&message),
        }
    }

    fn values_of(&self, name: &str) -> Result<BTreeSet<String>, String> {
        let mut session = self.session();
        let tables = store::catalog(&mut session)?;
        let mut values = BTreeSet::new();
        for (table, columns) in &tables {
            if name == "__name__" {
                for (column, data_type) in columns {
                    if column == "value" {
                        values.insert(table.clone());
                    } else if column != "time" && data_type != "text" {
                        values.insert(format!("{table}_{column}"));
                    }
                }
            } else if columns.iter().any(|(column, data_type)| column == name && data_type == "text") {
                let sql = format!("SELECT DISTINCT {} FROM {}.{} WHERE {} IS NOT NULL", store::identifier(name), store::SCHEMA, store::identifier(table), store::identifier(name));
                for result in store::run(&mut session, &sql)? {
                    values.extend(result.rows.iter().filter_map(|row| row.first().and_then(Value::to_text)));
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

    fn matching_series(&self, matches: &[&str], start: Option<&str>, end: Option<&str>) -> Result<BTreeSet<Labels>, String> {
        if matches.is_empty() {
            return Err("no match[] parameter provided".to_owned());
        }
        let end = time_param(end, now_ms())?;
        let start = time_param(start, end - promql::LOOKBACK_MS)?;
        let mut found = BTreeSet::new();
        for text in matches {
            let expr = promql::parse(text)?;
            let promql::Expr::Selector(selector) = &expr else {
                return Err("match[] must be a series selector".to_owned());
            };
            let data = self.fetch(&expr, start, end)?;
            for series in data.get(&selector.name).into_iter().flatten() {
                let inside = series.samples.iter().any(|(time, _)| *time >= start && *time <= end);
                if inside && selector.matchers.iter().all(|matcher| matcher.matches(&series.labels)) {
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
            Err(message) => bad_data(&message),
        }
    }
}
