//! A gRPC API generated from the schema (D1177): for each table of the served schema a service
//! `eggdb.v1.<Table>Service` with `List` (server streaming), `Get`, `Insert`, `Update` and
//! `Delete`, over messages whose fields are the table's columns, numbered by their position.
//! The `.proto` file is served at `GET /grpc/eggdb.proto` on the REST listener and printed by
//! [`Endpoint::grpc_proto`]. Calls run as SQL in the caller's session, signed in with the same
//! `authorization` metadata the REST API takes. gRPC is a trademark of The Linux Foundation;
//! EggDB is compatible with its protocol.

use crate::request::{identifier, literal};
use crate::{Caller, Endpoint};
use eggdb_http2::{GrpcStatus, Http2Request, Http2Response, grpc_message, grpc_messages};
use eggdb_sql::{KeyValueStore, QueryResult, SqlError, Value};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};

const PACKAGE: &str = "eggdb.v1";
const MOST_ROWS: u64 = 100_000;
const ACCEPT_POLL: std::time::Duration = std::time::Duration::from_millis(50);
const LIMIT_FIELD: u32 = 1;
const OFFSET_FIELD: u32 = 2;
const ORDER_BY_FIELD: u32 = 3;
const WHERE_FIELD: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtoKind {
    Int32,
    Int64,
    Float,
    Double,
    Bool,
    Bytes,
    Text,
}

impl ProtoKind {
    fn of(data_type: &str) -> Self {
        match data_type {
            "smallint" | "integer" => Self::Int32,
            "bigint" => Self::Int64,
            "real" => Self::Float,
            "double precision" => Self::Double,
            "boolean" => Self::Bool,
            "bytea" => Self::Bytes,
            _ => Self::Text,
        }
    }

    fn proto_name(self) -> &'static str {
        match self {
            Self::Int32 => "int32",
            Self::Int64 => "int64",
            Self::Float => "float",
            Self::Double => "double",
            Self::Bool => "bool",
            Self::Bytes => "bytes",
            Self::Text => "string",
        }
    }
}

struct Column {
    name: String,
    kind: ProtoKind,
    key: bool,
}

struct Table {
    name: String,
    message: String,
    columns: Vec<Column>,
}

impl Table {
    fn column(&self, number: u32) -> Option<&Column> {
        usize::try_from(number).ok()?.checked_sub(1).and_then(|at| self.columns.get(at))
    }

    fn keys(&self) -> impl Iterator<Item = (usize, &Column)> {
        self.columns.iter().enumerate().filter(|(_, column)| column.key)
    }
}

enum Wire {
    Varint(u64),
    Fixed64([u8; 8]),
    Fixed32([u8; 4]),
    Bytes(Vec<u8>),
}

struct Failure {
    status: GrpcStatus,
    message: String,
}

impl Failure {
    fn new(status: GrpcStatus, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(GrpcStatus::InvalidArgument, message)
    }
}

fn from_sql(error: &SqlError) -> Failure {
    let code = error.state().code();
    let status = if code == "42501" {
        GrpcStatus::PermissionDenied
    } else if code.starts_with("23") {
        GrpcStatus::FailedPrecondition
    } else if code.starts_with("42") || code.starts_with("22") {
        GrpcStatus::InvalidArgument
    } else {
        GrpcStatus::Internal
    };
    Failure::new(status, error.to_string())
}

fn proto_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    characters.next().is_some_and(|first| first.is_ascii_alphabetic())
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn pascal_case(name: &str) -> String {
    name.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut characters = part.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_ascii_uppercase().to_string() + characters.as_str()
            })
        })
        .collect()
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64, Failure> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let Some(&byte) = bytes.get(*at) else {
            return Err(Failure::invalid("a message ends inside a varint"));
        };
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Failure::invalid("a varint is longer than ten bytes"))
}

fn take<'b>(bytes: &'b [u8], at: &mut usize, length: usize) -> Result<&'b [u8], Failure> {
    let end = at.checked_add(length).filter(|end| *end <= bytes.len()).ok_or_else(|| Failure::invalid("a message ends inside a field"))?;
    let taken = &bytes[*at..end];
    *at = end;
    Ok(taken)
}

fn fields(message: &[u8]) -> Result<Vec<(u32, Wire)>, Failure> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < message.len() {
        let tag = varint(message, &mut at)?;
        let number = u32::try_from(tag >> 3).map_err(|_| Failure::invalid("a field number is too large"))?;
        let wire = match tag & 7 {
            0 => Wire::Varint(varint(message, &mut at)?),
            1 => Wire::Fixed64(take(message, &mut at, 8)?.try_into().map_err(|_| Failure::invalid("a fixed64 field is short"))?),
            2 => {
                let length = usize::try_from(varint(message, &mut at)?).map_err(|_| Failure::invalid("a field is too long"))?;
                Wire::Bytes(take(message, &mut at, length)?.to_vec())
            }
            5 => Wire::Fixed32(take(message, &mut at, 4)?.try_into().map_err(|_| Failure::invalid("a fixed32 field is short"))?),
            other => return Err(Failure::invalid(format!("wire type {other} is not supported"))),
        };
        found.push((number, wire));
    }
    Ok(found)
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_tag(out: &mut Vec<u8>, number: u32, wire: u8) {
    put_varint(out, (u64::from(number) << 3) | u64::from(wire));
}

fn put_value(out: &mut Vec<u8>, number: u32, kind: ProtoKind, value: &Value) {
    if value.is_null() {
        return;
    }
    let text = value.to_text().unwrap_or_default();
    match kind {
        ProtoKind::Int32 | ProtoKind::Int64 => {
            let number_value = value.as_integer().or_else(|| text.parse().ok()).unwrap_or(0);
            put_tag(out, number, 0);
            put_varint(out, number_value as u64);
        }
        ProtoKind::Bool => {
            let truth = matches!(value, Value::Boolean(true)) || text == "t" || text == "true";
            put_tag(out, number, 0);
            put_varint(out, u64::from(truth));
        }
        ProtoKind::Float => {
            put_tag(out, number, 5);
            out.extend_from_slice(&text.parse::<f32>().unwrap_or(f32::NAN).to_le_bytes());
        }
        ProtoKind::Double => {
            put_tag(out, number, 1);
            out.extend_from_slice(&text.parse::<f64>().unwrap_or(f64::NAN).to_le_bytes());
        }
        ProtoKind::Bytes => {
            let bytes = match value {
                Value::Bytea(bytes) => bytes.clone(),
                _ => text.into_bytes(),
            };
            put_tag(out, number, 2);
            put_varint(out, bytes.len() as u64);
            out.extend_from_slice(&bytes);
        }
        ProtoKind::Text => {
            put_tag(out, number, 2);
            put_varint(out, text.len() as u64);
            out.extend_from_slice(text.as_bytes());
        }
    }
}

fn float_literal(text: String, sql_type: &str) -> String {
    match text.as_str() {
        "NaN" | "inf" | "-inf" => {
            let shown = match text.as_str() {
                "inf" => "Infinity",
                "-inf" => "-Infinity",
                other => other,
            };
            format!("'{shown}'::{sql_type}")
        }
        _ => format!("{text}::{sql_type}"),
    }
}

fn sql_value(column: &Column, wire: &Wire) -> Result<String, Failure> {
    let mismatch = || Failure::invalid(format!("field '{}' has the wrong wire type", column.name));
    Ok(match (column.kind, wire) {
        (ProtoKind::Int32, Wire::Varint(raw)) => (*raw as u32 as i32).to_string(),
        (ProtoKind::Int64, Wire::Varint(raw)) => (*raw as i64).to_string(),
        (ProtoKind::Bool, Wire::Varint(raw)) => if *raw == 0 { "FALSE" } else { "TRUE" }.to_owned(),
        (ProtoKind::Float, Wire::Fixed32(bytes)) => float_literal(f32::from_le_bytes(*bytes).to_string(), "real"),
        (ProtoKind::Double, Wire::Fixed64(bytes)) => float_literal(f64::from_le_bytes(*bytes).to_string(), "double precision"),
        (ProtoKind::Bytes, Wire::Bytes(bytes)) => {
            let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("'\\x{hex}'::bytea")
        }
        (ProtoKind::Text, Wire::Bytes(bytes)) => {
            let text = std::str::from_utf8(bytes).map_err(|_| Failure::invalid(format!("field '{}' is not UTF-8", column.name)))?;
            literal(text)
        }
        _ => return Err(mismatch()),
    })
}

fn row_assignments(table: &Table, message: &[u8]) -> Result<Vec<(usize, String)>, Failure> {
    let mut assigned: Vec<(usize, String)> = Vec::new();
    for (number, wire) in fields(message)? {
        let Some(column) = table.column(number) else {
            continue;
        };
        let at = (number - 1) as usize;
        let value = sql_value(column, &wire)?;
        assigned.retain(|(known, _)| *known != at);
        assigned.push((at, value));
    }
    Ok(assigned)
}

fn quoted(table: &Table, at: usize) -> Result<String, Failure> {
    table
        .columns
        .get(at)
        .and_then(|column| identifier(&column.name))
        .ok_or_else(|| Failure::invalid("a column cannot be named in SQL"))
}

fn key_condition(table: &Table, assigned: &[(usize, String)]) -> Result<String, Failure> {
    let mut parts = Vec::new();
    for (at, column) in table.keys() {
        let Some((_, value)) = assigned.iter().find(|(known, _)| *known == at) else {
            return Err(Failure::invalid(format!("the key field '{}' is missing", column.name)));
        };
        parts.push(format!("{} = {value}", quoted(table, at)?));
    }
    if parts.is_empty() {
        return Err(Failure::new(GrpcStatus::FailedPrecondition, format!("'{}' has no primary key", table.name)));
    }
    Ok(parts.join(" AND "))
}

fn proto_text(tables: &[Table]) -> String {
    let mut text = format!("syntax = \"proto3\";\n\npackage {PACKAGE};\n\nmessage DeleteResult {{\n  int64 deleted = 1;\n}}\n");
    for table in tables {
        let message = &table.message;
        text.push_str(&format!("\nmessage {message} {{\n"));
        for (at, column) in table.columns.iter().enumerate() {
            text.push_str(&format!("  optional {} {} = {};\n", column.kind.proto_name(), column.name, at + 1));
        }
        text.push_str(&format!("}}\n\nmessage {message}Key {{\n"));
        for (at, column) in table.keys() {
            text.push_str(&format!("  {} {} = {};\n", column.kind.proto_name(), column.name, at + 1));
        }
        text.push_str(&format!(
            "}}\n\nmessage {message}ListRequest {{\n  optional int64 limit = {LIMIT_FIELD};\n  optional int64 offset = {OFFSET_FIELD};\n  optional string order_by = {ORDER_BY_FIELD};\n  {message} where = {WHERE_FIELD};\n}}\n"
        ));
        text.push_str(&format!(
            "\nservice {message}Service {{\n  rpc List({message}ListRequest) returns (stream {message});\n  rpc Get({message}Key) returns ({message});\n  rpc Insert({message}) returns ({message});\n  rpc Update({message}) returns ({message});\n  rpc Delete({message}Key) returns (DeleteResult);\n}}\n"
        ));
    }
    text
}

fn rows_of(results: &[QueryResult]) -> Vec<Vec<Value>> {
    results
        .iter()
        .rev()
        .find(|result| result.columns.is_some())
        .map(|result| result.rows.clone())
        .unwrap_or_default()
}

fn encoded_row(table: &Table, row: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for (at, (column, value)) in table.columns.iter().zip(row).enumerate() {
        put_value(&mut out, (at + 1) as u32, column.kind, value);
    }
    out
}

fn grpc_message_text(message: &str) -> String {
    message
        .bytes()
        .map(|byte| {
            if (byte.is_ascii_graphic() && byte != b'%') || byte == b' ' {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn answer(outcome: Result<Vec<Vec<u8>>, Failure>) -> Http2Response {
    let (body, status, message) = match outcome {
        Ok(messages) => (messages.iter().map(Vec::as_slice).map(grpc_message).collect(), GrpcStatus::Ok, String::new()),
        Err(failure) => (Vec::new(), failure.status, failure.message),
    };
    let mut trailers = vec![("grpc-status".to_owned(), status.code())];
    if !message.is_empty() {
        trailers.push(("grpc-message".to_owned(), grpc_message_text(&message)));
    }
    Http2Response {
        headers: vec![
            (":status".to_owned(), "200".to_owned()),
            ("content-type".to_owned(), "application/grpc".to_owned()),
        ],
        body,
        trailers,
    }
}

impl<S: KeyValueStore> Endpoint<'_, S> {
    fn grpc_tables(&self) -> Result<Vec<Table>, Failure> {
        let sql = format!(
            "SELECT c.table_name, c.column_name, c.data_type, k.column_name IS NOT NULL \
             FROM information_schema.columns c \
             LEFT JOIN information_schema.table_constraints t ON t.table_schema = c.table_schema AND t.table_name = c.table_name AND t.constraint_type = 'PRIMARY KEY' \
             LEFT JOIN information_schema.key_column_usage k ON k.constraint_schema = t.constraint_schema AND k.constraint_name = t.constraint_name AND k.table_name = c.table_name AND k.column_name = c.column_name \
             WHERE c.table_schema = {} ORDER BY c.table_name, c.ordinal_position",
            literal(&self.schema)
        );
        let results = self.run(&sql).map_err(|error| from_sql(&error))?;
        let mut tables: Vec<Table> = Vec::new();
        let mut unusable: Vec<String> = Vec::new();
        for row in rows_of(&results) {
            let text = |at: usize| row.get(at).and_then(Value::to_text).unwrap_or_default();
            let (table, column, data_type) = (text(0), text(1), text(2));
            let key = matches!(row.get(3), Some(Value::Boolean(true)));
            if !proto_identifier(&table) || !proto_identifier(&column) {
                unusable.push(table);
                continue;
            }
            if tables.last().is_none_or(|last| last.name != table) {
                tables.push(Table {
                    message: pascal_case(&table),
                    name: table.clone(),
                    columns: Vec::new(),
                });
            }
            if let Some(last) = tables.last_mut() {
                last.columns.push(Column {
                    name: column,
                    kind: ProtoKind::of(&data_type),
                    key,
                });
            }
        }
        tables.retain(|table| !unusable.contains(&table.name));
        let mut seen: Vec<String> = Vec::new();
        tables.retain(|table| {
            let fresh = !seen.contains(&table.message);
            seen.push(table.message.clone());
            fresh
        });
        Ok(tables)
    }

    /// The `.proto` file of the gRPC API over the served schema's tables.
    pub fn grpc_proto(&self) -> Result<String, SqlError> {
        match self.grpc_tables() {
            Ok(tables) => Ok(proto_text(&tables)),
            Err(failure) => Err(SqlError::new(eggdb_sql::SqlState::INTERNAL_ERROR, failure.message)),
        }
    }

    fn grpc_target(&self, table: &Table) -> Result<String, Failure> {
        let schema = identifier(&self.schema).ok_or_else(|| Failure::invalid("the schema cannot be named in SQL"))?;
        let name = identifier(&table.name).ok_or_else(|| Failure::invalid("the table cannot be named in SQL"))?;
        Ok(format!("{schema}.{name}"))
    }

    fn grpc_columns(table: &Table) -> Result<String, Failure> {
        let quoted: Result<Vec<String>, Failure> = (0..table.columns.len()).map(|at| quoted(table, at)).collect();
        Ok(quoted?.join(", "))
    }

    fn grpc_call(&self, path: &str, body: &[u8]) -> Result<Vec<Vec<u8>>, Failure> {
        let (service, method) = path
            .trim_start_matches('/')
            .split_once('/')
            .ok_or_else(|| Failure::new(GrpcStatus::Unimplemented, format!("'{path}' names no method")))?;
        let message = service
            .strip_prefix(PACKAGE)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| rest.strip_suffix("Service"))
            .ok_or_else(|| Failure::new(GrpcStatus::Unimplemented, format!("no service '{service}'")))?;
        let tables = self.grpc_tables()?;
        let table = tables
            .iter()
            .find(|table| table.message == message)
            .ok_or_else(|| Failure::new(GrpcStatus::Unimplemented, format!("no service '{service}'")))?;
        let messages = grpc_messages(body).map_err(|error| Failure::invalid(error.to_string()))?;
        let request = match messages.as_slice() {
            [only] => only.as_slice(),
            [] => &[],
            _ => return Err(Failure::invalid("a unary or server-streaming call sends one message")),
        };
        let target = self.grpc_target(table)?;
        let columns = Self::grpc_columns(table)?;
        match method {
            "List" => self.grpc_list(table, &target, &columns, request),
            "Get" => {
                let condition = key_condition(table, &row_assignments(table, request)?)?;
                let rows = self.grpc_rows(&format!("SELECT {columns} FROM {target} WHERE {condition}"))?;
                let row = rows.first().ok_or_else(|| Failure::new(GrpcStatus::NotFound, "no row has that key"))?;
                Ok(vec![encoded_row(table, row)])
            }
            "Insert" => {
                let assigned = row_assignments(table, request)?;
                let sql = if assigned.is_empty() {
                    format!("INSERT INTO {target} DEFAULT VALUES RETURNING {columns}")
                } else {
                    let names: Result<Vec<String>, Failure> = assigned.iter().map(|(at, _)| quoted(table, *at)).collect();
                    let values: Vec<&str> = assigned.iter().map(|(_, value)| value.as_str()).collect();
                    format!("INSERT INTO {target} ({}) VALUES ({}) RETURNING {columns}", names?.join(", "), values.join(", "))
                };
                let rows = self.grpc_rows(&sql)?;
                let row = rows.first().ok_or_else(|| Failure::new(GrpcStatus::Internal, "the insert returned no row"))?;
                Ok(vec![encoded_row(table, row)])
            }
            "Update" => {
                let assigned = row_assignments(table, request)?;
                let condition = key_condition(table, &assigned)?;
                let changes: Result<Vec<String>, Failure> = assigned
                    .iter()
                    .filter(|(at, _)| !table.columns[*at].key)
                    .map(|(at, value)| Ok(format!("{} = {value}", quoted(table, *at)?)))
                    .collect();
                let changes = changes?;
                if changes.is_empty() {
                    return Err(Failure::invalid("the message changes no column"));
                }
                let rows = self.grpc_rows(&format!("UPDATE {target} SET {} WHERE {condition} RETURNING {columns}", changes.join(", ")))?;
                let row = rows.first().ok_or_else(|| Failure::new(GrpcStatus::NotFound, "no row has that key"))?;
                Ok(vec![encoded_row(table, row)])
            }
            "Delete" => {
                let condition = key_condition(table, &row_assignments(table, request)?)?;
                let rows = self.grpc_rows(&format!("DELETE FROM {target} WHERE {condition} RETURNING 1"))?;
                let mut out = Vec::new();
                put_tag(&mut out, 1, 0);
                put_varint(&mut out, rows.len() as u64);
                Ok(vec![out])
            }
            other => Err(Failure::new(GrpcStatus::Unimplemented, format!("no method '{other}' on '{service}'"))),
        }
    }

    fn grpc_rows(&self, sql: &str) -> Result<Vec<Vec<Value>>, Failure> {
        self.run(sql).map(|results| rows_of(&results)).map_err(|error| from_sql(&error))
    }

    fn grpc_list(&self, table: &Table, target: &str, columns: &str, request: &[u8]) -> Result<Vec<Vec<u8>>, Failure> {
        let mut limit = MOST_ROWS;
        let mut offset = 0u64;
        let mut order = String::new();
        let mut conditions = Vec::new();
        for (number, wire) in fields(request)? {
            match (number, wire) {
                (LIMIT_FIELD, Wire::Varint(value)) => limit = value.min(MOST_ROWS),
                (OFFSET_FIELD, Wire::Varint(value)) => offset = value,
                (ORDER_BY_FIELD, Wire::Bytes(bytes)) => {
                    let text = String::from_utf8(bytes).map_err(|_| Failure::invalid("order_by is not UTF-8"))?;
                    let mut keys = Vec::new();
                    for item in text.split(',').map(str::trim).filter(|item| !item.is_empty()) {
                        let (name, direction) = match item.strip_prefix('-') {
                            Some(name) => (name, "DESC"),
                            None => (item, "ASC"),
                        };
                        let at = table
                            .columns
                            .iter()
                            .position(|column| column.name == name)
                            .ok_or_else(|| Failure::invalid(format!("order_by names no column '{name}'")))?;
                        keys.push(format!("{} {direction}", quoted(table, at)?));
                    }
                    if !keys.is_empty() {
                        order = format!(" ORDER BY {}", keys.join(", "));
                    }
                }
                (WHERE_FIELD, Wire::Bytes(bytes)) => {
                    for (at, value) in row_assignments(table, &bytes)? {
                        conditions.push(format!("{} = {value}", quoted(table, at)?));
                    }
                }
                _ => return Err(Failure::invalid(format!("field {number} of the list request is not understood"))),
            }
        }
        if order.is_empty() {
            let keys: Result<Vec<String>, Failure> = table.keys().map(|(at, _)| quoted(table, at)).collect();
            let keys = keys?;
            if !keys.is_empty() {
                order = format!(" ORDER BY {}", keys.join(", "));
            }
        }
        let filter = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let rows = self.grpc_rows(&format!("SELECT {columns} FROM {target}{filter}{order} LIMIT {limit} OFFSET {offset}"))?;
        Ok(rows.iter().map(|row| encoded_row(table, row)).collect())
    }

    fn grpc_caller(&self, request: &Http2Request) -> Result<Self, Failure> {
        let http = eggdb_http::Request {
            method: "POST".to_owned(),
            path: request.path().to_owned(),
            query: None,
            headers: request
                .headers
                .iter()
                .filter(|(name, _)| !name.starts_with(':'))
                .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
                .collect(),
            body: Vec::new(),
        };
        match self.caller(&http) {
            Caller::Server => Ok(self.copy()),
            Caller::Role(role, claims) => Ok(self.acting_as(role, claims)),
            Caller::Refused(why) => Err(Failure::new(GrpcStatus::Unauthenticated, why)),
        }
    }

    fn grpc_handle(&self, request: &Http2Request) -> Http2Response {
        if request.header(":method") != Some("POST") {
            return answer(Err(Failure::new(GrpcStatus::Unimplemented, "gRPC calls are POST requests")));
        }
        if !request
            .header("content-type")
            .is_some_and(|content_type| content_type.starts_with("application/grpc"))
        {
            return answer(Err(Failure::invalid("the content type is not application/grpc")));
        }
        let outcome = self
            .grpc_caller(request)
            .and_then(|caller| caller.grpc_call(request.path(), &request.body));
        answer(outcome)
    }
}

/// The gRPC API of an [`Endpoint`], as the HTTP/2 server's handler.
pub struct GrpcApi<'e, 's, S: KeyValueStore> {
    endpoint: &'e Endpoint<'s, S>,
}

impl<'e, 's, S: KeyValueStore> GrpcApi<'e, 's, S> {
    /// The gRPC API over `endpoint`'s schema.
    pub fn new(endpoint: &'e Endpoint<'s, S>) -> Self {
        Self { endpoint }
    }
}

impl<S: KeyValueStore> eggdb_http2::Handler for GrpcApi<'_, '_, S> {
    fn handle(&self, request: &Http2Request) -> Http2Response {
        self.endpoint.grpc_handle(request)
    }
}

/// Accepts gRPC connections on `listener` until `stop` answers true, at most
/// `max_connections` at a time, each on its own thread over EggDB's HTTP/2.
pub fn serve_grpc<S: KeyValueStore + Sync>(
    endpoint: &Endpoint<'_, S>,
    listener: &TcpListener,
    max_connections: usize,
    stop: &(dyn Fn() -> bool + Sync),
) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let active = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        while !stop() {
            let mut stream = match eggdb_platform::accept_tcp(listener) {
                Ok((stream, _)) => stream,
                Err(_) => {
                    std::thread::sleep(ACCEPT_POLL);
                    continue;
                }
            };
            if stream.set_nonblocking(false).is_err() || stream.set_nodelay(true).is_err() {
                continue;
            }
            if active.fetch_add(1, Ordering::AcqRel) >= max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let active = &active;
            let api = GrpcApi::new(endpoint);
            let spawned = std::thread::Builder::new()
                .name("eggdb-grpc".to_owned())
                .spawn_scoped(scope, move || {
                    let _ = eggdb_http2::serve_connection(&mut stream, &api);
                    active.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    Ok(())
}
