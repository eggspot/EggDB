use crate::compile::{Compiled, Shape, compile};
use crate::graphson::{element_map, group_count, path_of, to_json, value_map, value_of};
use crate::json::{Json, parse};
use crate::traversal::{Step, parse_bytecode, parse_script};
use crate::websocket::{OPCODE_BINARY, OPCODE_TEXT, Message, frame, handshake, read_message};
use eggdb_graph::Value;
use eggdb_keyspace::Keyspace;
use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const ACCEPT_POLL: Duration = Duration::from_millis(100);
const READ_POLL: Duration = Duration::from_millis(200);
const FAILED_LOGIN_PAUSE: Duration = Duration::from_millis(250);
const GRAPHSON_V3: &str = "application/vnd.gremlin-v3.0+json";
const DEFAULT_BATCH_SIZE: usize = 64;
const MAX_BATCH_SIZE: usize = 10_000;
const MAX_RESULTS: usize = 1_000_000;

const SUCCESS: i64 = 200;
const NO_CONTENT: i64 = 204;
const PARTIAL_CONTENT: i64 = 206;
const UNAUTHORIZED: i64 = 401;
const AUTHENTICATE: i64 = 407;
const MALFORMED_REQUEST: i64 = 498;
const INVALID_REQUEST_ARGUMENTS: i64 = 499;
const SERVER_ERROR: i64 = 500;
const SCRIPT_EVALUATION_ERROR: i64 = 597;

/// How the Gremlin server runs.
#[derive(Clone)]
pub struct ServerConfig {
    /// The one user clients authenticate as, with SASL `PLAIN`.
    pub user: String,
    /// The SHA-256 of that user's password (the password is not kept).
    pub password_sha256: [u8; 32],
    /// The most connections served at once.
    pub max_connections: usize,
}

impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig").field("user", &self.user).field("max_connections", &self.max_connections).finish_non_exhaustive()
    }
}

struct Request {
    id: String,
    op: String,
    args: Json,
}

struct Connection<'s> {
    keyspace: &'s dyn Keyspace,
    config: &'s ServerConfig,
    authenticated: bool,
    waiting: Option<Request>,
}

fn empty_map() -> Json {
    Json::typed("g:Map", Json::Array(Vec::new()))
}

fn response(id: &str, code: i64, message: &str, data: Option<Vec<Json>>) -> Json {
    Json::object(vec![
        ("requestId", Json::Text(id.to_owned())),
        (
            "status",
            Json::object(vec![("message", Json::Text(message.to_owned())), ("code", Json::Integer(code)), ("attributes", empty_map())]),
        ),
        (
            "result",
            Json::object(vec![
                ("data", data.map_or(Json::Null, |items| Json::typed("g:List", Json::Array(items)))),
                ("meta", empty_map()),
            ]),
        ),
    ])
}

fn request_id(json: &Json) -> Option<String> {
    match json {
        Json::Text(text) => Some(text.clone()),
        Json::Object(_) => json.field("@value").and_then(Json::text).map(str::to_owned),
        _ => None,
    }
}

fn body_of(message: &Message) -> Result<&[u8], String> {
    if message.opcode != OPCODE_BINARY {
        return Ok(&message.payload);
    }
    let (&length, rest) = message.payload.split_first().ok_or("an empty request")?;
    let mime = rest.get(..usize::from(length)).ok_or("the request's MIME type is cut short")?;
    let mime = std::str::from_utf8(mime).map_err(|_| "the request's MIME type is not text")?;
    if mime != GRAPHSON_V3 && mime != "application/json" {
        return Err(format!("EggDB's Gremlin server speaks {GRAPHSON_V3}, not {mime}"));
    }
    Ok(&rest[usize::from(length)..])
}

fn bindings(args: &Json) -> Result<Vec<(String, Value)>, String> {
    match args.field("bindings") {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(json) => match value_of(json, 0)? {
            Value::Map(entries) => Ok(entries),
            _ => Err("bindings are a map of names to values".into()),
        },
    }
}

fn batch_size(args: &Json) -> usize {
    match args.field("batchSize").map(|json| value_of(json, 0)) {
        Some(Ok(Value::Integer(size))) if size > 0 => usize::try_from(size).unwrap_or(MAX_BATCH_SIZE).min(MAX_BATCH_SIZE),
        _ => DEFAULT_BATCH_SIZE,
    }
}

fn shaped(compiled: &Compiled, rows: &[Vec<Value>]) -> Vec<Json> {
    let first = |row: &Vec<Value>| row.first().cloned().unwrap_or(Value::Null);
    match &compiled.shape {
        Shape::Nothing => Vec::new(),
        Shape::Plain => rows.iter().map(|row| to_json(&first(row))).collect(),
        Shape::ValueMap { tokens, keys } => rows.iter().map(|row| value_map(&first(row), *tokens, keys)).collect(),
        Shape::ElementMap { keys } => rows.iter().map(|row| element_map(&first(row), keys)).collect(),
        Shape::Path => rows.iter().map(|row| path_of(&first(row))).collect(),
        Shape::GroupCount => {
            let pairs: Vec<(Value, Value)> = rows
                .iter()
                .map(|row| (row.first().cloned().unwrap_or(Value::Null), row.get(1).cloned().unwrap_or(Value::Null)))
                .collect();
            vec![group_count(&pairs)]
        }
    }
}

impl Connection<'_> {
    fn sasl_accepted(&self, args: &Json) -> bool {
        let mechanism = args.field("saslMechanism").and_then(Json::text).unwrap_or("PLAIN");
        let Some(encoded) = args.field("sasl").and_then(Json::text) else {
            return false;
        };
        let Ok(decoded) = eggdb_crypto::base64_decode(encoded.as_bytes()) else {
            return false;
        };
        let mut parts = decoded.split(|byte| *byte == 0);
        let (Some(_), Some(user), Some(password), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        mechanism.eq_ignore_ascii_case("PLAIN")
            && user == self.config.user.as_bytes()
            && eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(password), &self.config.password_sha256)
    }

    fn steps(request: &Request) -> Result<Vec<Step>, (i64, String)> {
        let invalid = |message: String| (INVALID_REQUEST_ARGUMENTS, message);
        match request.op.as_str() {
            "eval" => {
                if let Some(language) = request.args.field("language").and_then(Json::text)
                    && !matches!(language, "gremlin-groovy" | "gremlin-lang")
                {
                    return Err(invalid(format!("EggDB runs Gremlin scripts, not {language}")));
                }
                let script = request.args.field("gremlin").and_then(Json::text).ok_or_else(|| invalid("eval needs args.gremlin".into()))?;
                let bindings = bindings(&request.args).map_err(invalid)?;
                parse_script(script, &bindings).map_err(|error| (SCRIPT_EVALUATION_ERROR, error.0))
            }
            "bytecode" => {
                let bytecode = request.args.field("gremlin").ok_or_else(|| invalid("bytecode needs args.gremlin".into()))?;
                parse_bytecode(bytecode, 0).map_err(|error| (SCRIPT_EVALUATION_ERROR, error.0))
            }
            other => Err(invalid(format!("EggDB's Gremlin server has no {other} operation"))),
        }
    }

    fn run(&self, request: &Request) -> Vec<Json> {
        if request.op == "close" {
            return vec![response(&request.id, NO_CONTENT, "", None)];
        }
        let steps = match Self::steps(request) {
            Ok(steps) => steps,
            Err((code, message)) => return vec![response(&request.id, code, &message, None)],
        };
        let compiled = match compile(&steps) {
            Ok(compiled) => compiled,
            Err(message) => return vec![response(&request.id, SCRIPT_EVALUATION_ERROR, &message, None)],
        };
        let result = match eggdb_graph::run(self.keyspace, &compiled.query, &compiled.parameters, !compiled.writes) {
            Ok(result) => result,
            Err(error) => return vec![response(&request.id, SCRIPT_EVALUATION_ERROR, &error.message, None)],
        };
        if result.rows.len() > MAX_RESULTS {
            return vec![response(&request.id, SERVER_ERROR, &format!("the traversal returned more than {MAX_RESULTS} results"), None)];
        }
        let items = shaped(&compiled, &result.rows);
        if items.is_empty() {
            return vec![response(&request.id, NO_CONTENT, "", None)];
        }
        let size = batch_size(&request.args);
        let batches: Vec<&[Json]> = items.chunks(size).collect();
        let last = batches.len().saturating_sub(1);
        batches
            .into_iter()
            .enumerate()
            .map(|(index, batch)| response(&request.id, if index == last { SUCCESS } else { PARTIAL_CONTENT }, "", Some(batch.to_vec())))
            .collect()
    }

    fn handle(&mut self, message: &Message) -> Vec<Json> {
        let body = match body_of(message) {
            Ok(body) => body,
            Err(text) => return vec![response("", MALFORMED_REQUEST, &text, None)],
        };
        let Some(json) = parse(body) else {
            return vec![response("", MALFORMED_REQUEST, "the request is not JSON", None)];
        };
        let Some(id) = json.field("requestId").and_then(request_id) else {
            return vec![response("", MALFORMED_REQUEST, "the request has no requestId", None)];
        };
        let request = Request {
            id,
            op: json.field("op").and_then(Json::text).unwrap_or_default().to_owned(),
            args: json.field("args").cloned().unwrap_or(Json::Object(Vec::new())),
        };
        if request.op == "authentication" {
            if !self.sasl_accepted(&request.args) {
                std::thread::sleep(FAILED_LOGIN_PAUSE);
                self.waiting = None;
                return vec![response(&request.id, UNAUTHORIZED, "Username and/or password are incorrect", None)];
            }
            self.authenticated = true;
            return match self.waiting.take() {
                Some(waiting) => self.run(&waiting),
                None => vec![response(&request.id, NO_CONTENT, "", None)],
            };
        }
        if !self.authenticated {
            let id = request.id.clone();
            self.waiting = Some(request);
            return vec![response(&id, AUTHENTICATE, "", None)];
        }
        self.run(&request)
    }
}

fn serve_stream(
    keyspace: &dyn Keyspace,
    config: &ServerConfig,
    mut stream: TcpStream,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    handshake(&mut stream)?;
    let mut writer = stream.try_clone()?;
    let mut connection = Connection { keyspace, config, authenticated: false, waiting: None };
    loop {
        let message = match read_message(&mut stream, &mut writer, &mut || !stop()) {
            Ok(Some(message)) => message,
            Ok(None) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(()),
            Err(error) => return Err(error),
        };
        let opcode = if message.opcode == OPCODE_BINARY { OPCODE_BINARY } else { OPCODE_TEXT };
        for reply in connection.handle(&message) {
            writer.write_all(&frame(opcode, reply.to_text().as_bytes()))?;
        }
    }
}

/// Serves Gremlin clients on `listener` from the property graph in `keyspace`, a thread per
/// connection, until `stop` turns true.
pub fn serve(
    keyspace: &(dyn Keyspace + Sync),
    listener: &TcpListener,
    config: &ServerConfig,
    stop: &(dyn Fn() -> bool + Sync),
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !stop() {
            let stream = match eggdb_platform::accept_tcp(listener) {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() || stream.set_read_timeout(Some(READ_POLL)).is_err() || stream.set_nodelay(true).is_err() {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= config.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let active = &active;
            let spawned = std::thread::Builder::new().name("eggdb-gremlin".to_owned()).spawn_scoped(scope, move || {
                let _ = serve_stream(keyspace, config, stream, stop);
                active.fetch_sub(1, Ordering::AcqRel);
            });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_keyspace::MemoryKeyspace;

    fn request(connection: &mut Connection<'_>, body: &str) -> Vec<String> {
        let message = Message { opcode: OPCODE_TEXT, payload: body.as_bytes().to_vec() };
        connection.handle(&message).iter().map(Json::to_text).collect()
    }

    #[test]
    fn a_client_authenticates_then_writes_and_reads_the_graph() {
        let keyspace = MemoryKeyspace::default();
        let config = ServerConfig { user: "ada".into(), password_sha256: eggdb_crypto::sha256(b"secret"), max_connections: 4 };
        let mut connection = Connection { keyspace: &keyspace, config: &config, authenticated: false, waiting: None };
        let first = request(&mut connection, r#"{"requestId":"1","op":"eval","processor":"","args":{"gremlin":"g.addV('person').property('name','marko').id()"}}"#);
        assert!(first[0].contains("\"code\":407"));
        let sasl = eggdb_crypto::base64_encode(b"\0ada\0secret");
        let created = request(&mut connection, &format!(r#"{{"requestId":"1","op":"authentication","args":{{"sasl":"{sasl}","saslMechanism":"PLAIN"}}}}"#));
        assert!(created[0].contains("\"code\":200"));
        let names = request(&mut connection, r#"{"requestId":"2","op":"eval","args":{"gremlin":"g.V().hasLabel(x).values('name')","bindings":{"x":"person"}}}"#);
        assert!(names[0].contains(r#""data":{"@type":"g:List","@value":["marko"]}"#));
        let none = request(&mut connection, r#"{"requestId":"3","op":"eval","args":{"gremlin":"g.V().has('name','nobody')"}}"#);
        assert!(none[0].contains("\"code\":204"));
    }
}
