use crate::columns::{columns_of, text_columns};
use crate::proto::{Message, any, bytes_field, repeated_bytes, text_field, unpack_any};
use eggdb_http2::{GrpcStatus, Handler, Http2Request, Http2Response, grpc_message, grpc_messages};
use eggdb_parquet::Column;
use eggdb_sql::{Dialect, KeyValueStore, QueryResult, Session};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SERVICE: &str = "/arrow.flight.protocol.FlightService/";
const KEPT_RESULTS: usize = 64;
const RESULT_LIFETIME: Duration = Duration::from_secs(600);
const MOST_PREPARED: usize = 1024;

struct KeptResult {
    handle: Vec<u8>,
    columns: Vec<Column>,
    kept_at: Instant,
}

/// The Arrow Flight SQL service over a store, for clients that sign in as the server's user
/// (HTTP basic authentication, or the bearer token `Handshake` returns).
pub struct Endpoint<'s, S: KeyValueStore + Sync> {
    store: &'s S,
    user: String,
    password_sha256: [u8; 32],
    database: String,
    tokens: Mutex<VecDeque<String>>,
    results: Mutex<VecDeque<KeptResult>>,
    prepared: Mutex<HashMap<Vec<u8>, String>>,
    next: AtomicU64,
}

impl<S: KeyValueStore + Sync> std::fmt::Debug for Endpoint<'_, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint").field("user", &self.user).finish_non_exhaustive()
    }
}

fn grpc_text(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for byte in message.bytes() {
        if (0x20..0x7F).contains(&byte) && byte != b'%' {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn failure(status: GrpcStatus, message: &str) -> Http2Response {
    Http2Response {
        headers: vec![
            (":status".to_owned(), "200".to_owned()),
            ("content-type".to_owned(), "application/grpc".to_owned()),
            ("grpc-status".to_owned(), status.code()),
            ("grpc-message".to_owned(), grpc_text(message)),
        ],
        body: Vec::new(),
        trailers: Vec::new(),
    }
}

fn answer(messages: Vec<Vec<u8>>, extra_headers: Vec<(String, String)>) -> Http2Response {
    let mut headers = vec![
        (":status".to_owned(), "200".to_owned()),
        ("content-type".to_owned(), "application/grpc".to_owned()),
    ];
    headers.extend(extra_headers);
    Http2Response {
        headers,
        body: messages.iter().map(|message| grpc_message(message)).collect(),
        trailers: vec![("grpc-status".to_owned(), GrpcStatus::Ok.code())],
    }
}

fn like_pattern(pattern: &str) -> String {
    format!("'{}'", pattern.replace('\'', "''"))
}

fn rows_text(result: &QueryResult) -> Vec<Vec<Option<String>>> {
    result
        .rows
        .iter()
        .map(|row| row.iter().map(|value| value.to_text()).collect())
        .collect()
}

impl<'s, S: KeyValueStore + Sync> Endpoint<'s, S> {
    /// The service over `store`, signing in `user` with the password whose SHA-256 is given,
    /// running statements in `database`.
    pub fn new(store: &'s S, user: &str, password_sha256: [u8; 32], database: &str) -> Self {
        Self {
            store,
            user: user.to_owned(),
            password_sha256,
            database: database.to_owned(),
            tokens: Mutex::new(VecDeque::new()),
            results: Mutex::new(VecDeque::new()),
            prepared: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }

    fn basic_matches(&self, encoded: &str) -> bool {
        let Ok(decoded) = eggdb_crypto::base64_decode(encoded.trim().as_bytes()) else {
            return false;
        };
        let Ok(text) = String::from_utf8(decoded) else {
            return false;
        };
        let Some((user, password)) = text.split_once(':') else {
            return false;
        };
        user == self.user && eggdb_crypto::constant_time_eq(&eggdb_crypto::sha256(password.as_bytes()), &self.password_sha256)
    }

    fn authorized(&self, request: &Http2Request) -> bool {
        match request.header("authorization") {
            Some(value) => match value.split_once(' ') {
                Some(("Basic", encoded)) => self.basic_matches(encoded),
                Some(("Bearer", token)) => {
                    let tokens = self.tokens.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    tokens.iter().any(|known| eggdb_crypto::constant_time_eq(known.as_bytes(), token.trim().as_bytes()))
                }
                _ => false,
            },
            None => false,
        }
    }

    fn new_handle(&self) -> Vec<u8> {
        let number = self.next.fetch_add(1, Ordering::Relaxed);
        let seed = format!("{number}{:?}{}", Instant::now(), self.user);
        let digest = eggdb_crypto::sha256(seed.as_bytes());
        digest[..16].to_vec()
    }

    fn run(&self, sql: &str) -> Result<QueryResult, String> {
        let mut session = Session::with_dialect(self.store, Dialect::PostgreSql);
        session.identify(&self.user, &self.database);
        session.set_protocol("flight-sql");
        let mut results = Vec::new();
        let ran = session.execute(sql, &mut |result| results.push(result));
        let _ = session.close();
        ran.map_err(|error| error.message().to_owned())?;
        results.into_iter().last().ok_or_else(|| "the statement returned nothing".to_owned())
    }

    fn keep(&self, columns: Vec<Column>) -> Vec<u8> {
        let handle = self.new_handle();
        let mut results = self.results.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        results.retain(|kept| kept.kept_at.elapsed() < RESULT_LIFETIME);
        while results.len() >= KEPT_RESULTS {
            results.pop_front();
        }
        results.push_back(KeptResult { handle: handle.clone(), columns, kept_at: Instant::now() });
        handle
    }

    fn take(&self, handle: &[u8]) -> Option<Vec<Column>> {
        let mut results = self.results.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let at = results.iter().position(|kept| kept.handle == handle)?;
        results.remove(at).map(|kept| kept.columns)
    }

    fn catalog_query(&self, command: &str, value: &[u8]) -> Result<Option<Vec<Column>>, String> {
        Ok(Some(match command {
            "CommandGetCatalogs" => text_columns(&["catalog_name"], &[vec![Some(self.database.clone())]]),
            "CommandGetTableTypes" => text_columns(&["table_type"], &[vec![Some("TABLE".to_owned())], vec![Some("VIEW".to_owned())]]),
            "CommandGetDbSchemas" => {
                let pattern = text_field(value, 2)?;
                let filter = if pattern.is_empty() { String::new() } else { format!(" WHERE schema_name LIKE {}", like_pattern(&pattern)) };
                let result = self.run(&format!("SELECT schema_name FROM information_schema.schemata{filter} ORDER BY schema_name"))?;
                let rows: Vec<Vec<Option<String>>> = rows_text(&result)
                    .into_iter()
                    .map(|row| vec![Some(self.database.clone()), row.into_iter().next().flatten()])
                    .collect();
                text_columns(&["catalog_name", "db_schema_name"], &rows)
            }
            "CommandGetTables" => {
                let mut conditions = Vec::new();
                let schema = text_field(value, 2)?;
                if !schema.is_empty() {
                    conditions.push(format!("table_schema LIKE {}", like_pattern(&schema)));
                }
                let table = text_field(value, 3)?;
                if !table.is_empty() {
                    conditions.push(format!("table_name LIKE {}", like_pattern(&table)));
                }
                let types: Vec<String> = repeated_bytes(value, 4)?
                    .into_iter()
                    .map(|kind| String::from_utf8_lossy(kind).into_owned())
                    .collect();
                let filter = if conditions.is_empty() { String::new() } else { format!(" WHERE {}", conditions.join(" AND ")) };
                let result = self.run(&format!(
                    "SELECT table_schema, table_name, CASE table_type WHEN 'BASE TABLE' THEN 'TABLE' ELSE table_type END FROM information_schema.tables{filter} ORDER BY table_schema, table_name"
                ))?;
                let rows: Vec<Vec<Option<String>>> = rows_text(&result)
                    .into_iter()
                    .filter(|row| types.is_empty() || row.get(2).cloned().flatten().is_some_and(|kind| types.contains(&kind)))
                    .map(|row| {
                        let mut full = vec![Some(self.database.clone())];
                        full.extend(row);
                        full
                    })
                    .collect();
                text_columns(&["catalog_name", "db_schema_name", "table_name", "table_type"], &rows)
            }
            _ => return Ok(None),
        }))
    }

    fn flight_info(&self, descriptor: &[u8]) -> Result<Vec<u8>, (GrpcStatus, String)> {
        let invalid = |message: String| (GrpcStatus::InvalidArgument, message);
        let command = bytes_field(descriptor, 2).map_err(invalid)?;
        let (name, value) = unpack_any(command).map_err(invalid)?;
        let columns = match name.as_str() {
            "CommandStatementQuery" => {
                let query = text_field(&value, 1).map_err(invalid)?;
                columns_of(self.run(&query).map_err(invalid)?)
            }
            "CommandPreparedStatementQuery" => {
                let handle = bytes_field(&value, 1).map_err(invalid)?.to_vec();
                let query = self
                    .prepared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&handle)
                    .cloned()
                    .ok_or_else(|| (GrpcStatus::NotFound, "no such prepared statement".to_owned()))?;
                columns_of(self.run(&query).map_err(invalid)?)
            }
            other => match self.catalog_query(other, &value).map_err(invalid)? {
                Some(columns) => columns,
                None => return Err((GrpcStatus::Unimplemented, format!("{other} is not supported yet"))),
            },
        };
        let rows = columns.first().map_or(0, |column| column.cells.len());
        let schema = eggdb_arrow::encapsulated_schema(&columns);
        let handle = self.keep(columns);
        let ticket = Message::new()
            .bytes(1, &any("TicketStatementQuery", &Message::new().bytes(1, &handle).finish()))
            .finish();
        let endpoint = Message::new().bytes(1, &ticket).finish();
        Ok(Message::new()
            .bytes(1, &schema)
            .bytes(2, descriptor)
            .bytes(3, &endpoint)
            .int64(4, i64::try_from(rows).unwrap_or(-1))
            .int64(5, -1)
            .finish())
    }

    fn do_get(&self, ticket: &[u8]) -> Result<Vec<Vec<u8>>, (GrpcStatus, String)> {
        let invalid = |message: String| (GrpcStatus::InvalidArgument, message);
        let packed = bytes_field(ticket, 1).map_err(invalid)?;
        let (_, value) = unpack_any(packed).map_err(invalid)?;
        let handle = bytes_field(&value, 1).map_err(invalid)?;
        let columns = self
            .take(handle)
            .ok_or_else(|| (GrpcStatus::NotFound, "the ticket is unknown or was already read".to_owned()))?;
        let schema = Message::new().bytes(2, &eggdb_arrow::schema_message_bytes(&columns)).finish();
        let (header, body) = eggdb_arrow::record_batch_message(&columns).map_err(|message| (GrpcStatus::Internal, message))?;
        let batch = Message::new().bytes(2, &header).bytes(1000, &body).finish();
        Ok(vec![schema, batch])
    }

    fn do_put(&self, body: &[u8]) -> Result<Vec<Vec<u8>>, (GrpcStatus, String)> {
        let invalid = |message: String| (GrpcStatus::InvalidArgument, message);
        let messages = grpc_messages(body).map_err(|error| invalid(error.message))?;
        let first = messages.first().ok_or_else(|| invalid("DoPut sent no message".to_owned()))?;
        let descriptor = bytes_field(first, 1).map_err(invalid)?;
        let command = bytes_field(descriptor, 2).map_err(invalid)?;
        let (name, value) = unpack_any(command).map_err(invalid)?;
        let statement = match name.as_str() {
            "CommandStatementUpdate" => text_field(&value, 1).map_err(invalid)?,
            "CommandPreparedStatementUpdate" => {
                let handle = bytes_field(&value, 1).map_err(invalid)?.to_vec();
                self.prepared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&handle)
                    .cloned()
                    .ok_or_else(|| (GrpcStatus::NotFound, "no such prepared statement".to_owned()))?
            }
            other => return Err((GrpcStatus::Unimplemented, format!("{other} through DoPut is not supported yet"))),
        };
        let result = self.run(&statement).map_err(invalid)?;
        let count = result.tag.rsplit(' ').next().and_then(|count| count.parse::<i64>().ok()).unwrap_or(0);
        let update = Message::new().int64(1, count).finish();
        Ok(vec![Message::new().bytes(1, &update).finish()])
    }

    fn do_action(&self, body: &[u8]) -> Result<Vec<Vec<u8>>, (GrpcStatus, String)> {
        let invalid = |message: String| (GrpcStatus::InvalidArgument, message);
        let messages = grpc_messages(body).map_err(|error| invalid(error.message))?;
        let action = messages.first().ok_or_else(|| invalid("DoAction sent no action".to_owned()))?;
        let kind = text_field(action, 1).map_err(invalid)?;
        let (_, value) = unpack_any(bytes_field(action, 2).map_err(invalid)?).map_err(invalid)?;
        let mut prepared = self.prepared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match kind.as_str() {
            "CreatePreparedStatement" => {
                if prepared.len() >= MOST_PREPARED {
                    return Err((GrpcStatus::ResourceExhausted, "too many prepared statements".to_owned()));
                }
                let query = text_field(&value, 1).map_err(invalid)?;
                let handle = self.new_handle();
                prepared.insert(handle.clone(), query);
                let created = Message::new().bytes(1, &handle).finish();
                Ok(vec![Message::new().bytes(1, &any("ActionCreatePreparedStatementResult", &created)).finish()])
            }
            "ClosePreparedStatement" => {
                prepared.remove(bytes_field(&value, 1).map_err(invalid)?);
                Ok(Vec::new())
            }
            other => Err((GrpcStatus::Unimplemented, format!("the action {other} is not supported"))),
        }
    }

    fn list_actions() -> Vec<Vec<u8>> {
        ["CreatePreparedStatement", "ClosePreparedStatement"]
            .iter()
            .map(|kind| Message::new().text(1, kind).finish())
            .collect()
    }
}

impl<S: KeyValueStore + Sync> Handler for Endpoint<'_, S> {
    fn handle(&self, request: &Http2Request) -> Http2Response {
        let Some(method) = request.path().strip_prefix(SERVICE) else {
            return failure(GrpcStatus::Unimplemented, "only the Arrow Flight service is served here");
        };
        if !self.authorized(request) {
            return failure(GrpcStatus::Unauthenticated, "sign in with the server's user and password");
        }
        let messages = match grpc_messages(&request.body) {
            Ok(messages) => messages,
            Err(error) => return failure(GrpcStatus::InvalidArgument, &error.message),
        };
        let first = messages.first().cloned().unwrap_or_default();
        let outcome = match method {
            "Handshake" => {
                let token = eggdb_crypto::base64_encode(&self.new_handle());
                let mut tokens = self.tokens.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                while tokens.len() >= KEPT_RESULTS {
                    tokens.pop_front();
                }
                tokens.push_back(token.clone());
                return answer(vec![Message::new().finish()], vec![("authorization".to_owned(), format!("Bearer {token}"))]);
            }
            "GetFlightInfo" => self.flight_info(&first).map(|info| vec![info]),
            "DoGet" => self.do_get(&first),
            "DoPut" => self.do_put(&request.body),
            "DoAction" => self.do_action(&request.body),
            "ListActions" => Ok(Self::list_actions()),
            other => Err((GrpcStatus::Unimplemented, format!("{other} is not supported yet"))),
        };
        match outcome {
            Ok(messages) => answer(messages, Vec::new()),
            Err((status, message)) => failure(status, &message),
        }
    }
}
