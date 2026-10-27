use crate::auth::{CACHING_SHA2_PASSWORD, MYSQL_NATIVE_PASSWORD, NONCE_BYTES};
use crate::packets::{
    BoundValue, CLIENT_MULTI_STATEMENTS, CLIENT_PLUGIN_AUTH, ColumnDefinition, Command,
    ParameterType, Payload, SERVER_CAPABILITIES, STATUS_AUTOCOMMIT, STATUS_IN_TRANSACTION,
    STATUS_MORE_RESULTS, TYPE_DATE, TYPE_DATETIME, TYPE_DOUBLE, TYPE_FLOAT, TYPE_JSON, TYPE_LONG,
    TYPE_LONGLONG, TYPE_NEWDECIMAL, TYPE_SHORT, TYPE_TIMESTAMP, TYPE_TINY, TYPE_VAR_STRING,
    UTF8MB4_0900_AI_CI, column_definition_packet, eof_packet, err_packet, ok_packet, parse_command,
    parse_execute, parse_handshake_response,
};
use crate::server::{MAX_UNAUTHENTICATED_CONNECTIONS, REFUSAL_TIMEOUT, Shared, take_slot};
use crate::wire::{POLL_INTERVAL, Wire, WireError};
use eggdb_sql::{
    Dialect, KeyValueStore, MYSQL_COMPATIBLE_VERSION, OutputColumn, Parameter, QueryResult,
    Session, SqlError, SqlType, TransactionStatus, Value,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The largest packet accepted before authentication succeeds.
pub const MAX_AUTHENTICATION_PACKET_BYTES: usize = 16 * 1024;
/// The largest packet accepted from an authenticated client: `@@max_allowed_packet`, 64 MiB.
pub const MAX_PACKET_BYTES: usize = 64 * 1024 * 1024;
const MAX_PREPARED_STATEMENTS: usize = 4096;
const MAX_PLACEHOLDERS: usize = 65_535;
const MAX_HELD_BYTES: usize = MAX_PACKET_BYTES;

pub(crate) fn printable(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => shown.push_str("\\\\"),
            '"' => shown.push_str("\\\""),
            '\n' => shown.push_str("\\n"),
            '\r' => shown.push_str("\\r"),
            '\t' => shown.push_str("\\t"),
            special
                if special.is_control()
                    || matches!(special, '\u{2028}' | '\u{2029}' | '\u{202e}') =>
            {
                shown.push_str(&format!("\\u{{{:x}}}", u32::from(special)));
            }
            other => shown.push(other),
        }
    }
    shown
}

fn fresh_nonce() -> Option<[u8; NONCE_BYTES]> {
    let mut nonce = [0u8; NONCE_BYTES];
    eggdb_platform::secure_random::fill(&mut nonce).ok()?;
    for byte in &mut nonce {
        *byte = *byte % 127 + 1;
    }
    Some(nonce)
}

fn handshake_packet(connection_id: u32, nonce: &[u8; NONCE_BYTES]) -> Vec<u8> {
    let mut payload = Payload::new();
    payload
        .u8(10)
        .null_terminated(MYSQL_COMPATIBLE_VERSION.as_bytes())
        .u32(connection_id)
        .bytes(&nonce[..8])
        .u8(0)
        .u16(SERVER_CAPABILITIES as u16)
        .u8(UTF8MB4_0900_AI_CI)
        .u16(STATUS_AUTOCOMMIT)
        .u16((SERVER_CAPABILITIES >> 16) as u16)
        .u8(NONCE_BYTES as u8 + 1)
        .bytes(&[0; 10])
        .null_terminated(&nonce[8..])
        .null_terminated(CACHING_SHA2_PASSWORD.as_bytes());
    payload.0
}

fn send_error(wire: &mut Wire<'_>, code: u16, state: &str, message: &str) {
    wire.send(&err_packet(code, state, message));
    let _ = wire.flush();
}

fn send_sql_error(wire: &mut Wire<'_>, error: &SqlError) {
    let (code, state) = match error.vendor_code() {
        Some(code) => (code, error.state().code().to_owned()),
        None => (1105, "HY000".to_owned()),
    };
    wire.send(&err_packet(code, &state, error.message()));
}

fn refuse(wire: &mut Wire<'_>, code: u16, state: &str, message: &str) {
    send_error(wire, code, state, message);
    wire.close_after_refusal();
}

pub(crate) fn refuse_too_many(wire: &mut Wire<'_>) {
    wire.sequence = 0;
    refuse(wire, 1040, "08004", "Too many connections");
}

fn sleep_unless_stopped(duration: Duration, stop: &(dyn Fn() -> bool + Sync)) {
    let until = Instant::now() + duration;
    while !stop() {
        let now = Instant::now();
        if now >= until {
            return;
        }
        std::thread::sleep((until - now).min(POLL_INTERVAL));
    }
}

fn report_wire_error(wire: &mut Wire<'_>, error: &WireError) {
    match error {
        WireError::TooLarge => refuse(
            wire,
            1153,
            "08S01",
            "Got a packet bigger than 'max_allowed_packet' bytes",
        ),
        WireError::OutOfOrder => refuse(wire, 1156, "08S01", "Got packets out of order"),
        WireError::TimedOut => refuse(wire, 1043, "08S01", "Bad handshake: timed out"),
        WireError::Closed | WireError::Stopping | WireError::Io => {}
    }
}

enum Outcome {
    Accepted,
    Refused,
}

fn authenticate(
    wire: &mut Wire<'_>,
    shared: &Shared<'_>,
    nonce: &[u8; NONCE_BYTES],
    response: &crate::packets::HandshakeResponse,
) -> Outcome {
    let config = shared.config;
    let credentials = if response.user == config.user {
        config.credentials.clone()
    } else {
        shared.unknown_user.clone()
    };
    let named_plugin = if response.capabilities & CLIENT_PLUGIN_AUTH != 0 {
        response.auth_plugin.as_deref()
    } else {
        Some(MYSQL_NATIVE_PASSWORD)
    };
    let (accepted, with_password, fast) = match named_plugin {
        Some(CACHING_SHA2_PASSWORD) => (
            credentials.check_caching_sha2(nonce, &response.auth_response),
            !response.auth_response.is_empty(),
            true,
        ),
        Some(MYSQL_NATIVE_PASSWORD) => (
            credentials.check_native(nonce, &response.auth_response),
            !response.auth_response.is_empty(),
            false,
        ),
        _ => {
            let Some(switch_nonce) = fresh_nonce() else {
                refuse(wire, 1105, "HY000", "could not generate a random nonce");
                return Outcome::Refused;
            };
            let mut switch = Payload::new();
            switch
                .u8(0xFE)
                .null_terminated(MYSQL_NATIVE_PASSWORD.as_bytes())
                .null_terminated(&switch_nonce);
            wire.send(&switch.0);
            if wire.flush().is_err() {
                return Outcome::Refused;
            }
            let expected = wire.sequence;
            let answer = match wire.read_payload(expected, MAX_AUTHENTICATION_PACKET_BYTES) {
                Ok(answer) => answer,
                Err(error) => {
                    report_wire_error(wire, &error);
                    return Outcome::Refused;
                }
            };
            (
                credentials.check_native(&switch_nonce, &answer),
                !answer.is_empty(),
                false,
            )
        }
    };
    if !accepted {
        (shared.log)(&format!(
            "{}  password authentication failed for user \"{}\"",
            wire.peer(),
            printable(&response.user)
        ));
        sleep_unless_stopped(shared.throttle.delay_after_failure(), shared.stop);
        let using = if with_password { "YES" } else { "NO" };
        refuse(
            wire,
            1045,
            "28000",
            &format!(
                "Access denied for user '{}'@'localhost' (using password: {using})",
                response.user
            ),
        );
        return Outcome::Refused;
    }
    if fast {
        wire.send(&[0x01, 0x03]);
    }
    Outcome::Accepted
}

struct Prepared {
    text: String,
    parameter_count: usize,
    types: Vec<ParameterType>,
    long_data: Vec<Option<Vec<u8>>>,
    long_data_bytes: usize,
    long_data_overflowed: bool,
}

struct Connection<'a, 's, S: KeyValueStore + 's> {
    wire: Wire<'a>,
    shared: &'a Shared<'a>,
    store: &'s S,
    session: Session<'s, S>,
    user: String,
    connection_id: u32,
    capabilities: u32,
    multi_statements: bool,
    statements: HashMap<u32, Prepared>,
    next_statement: u32,
}

fn new_session<'s, S: KeyValueStore + 's>(
    store: &'s S,
    user: &str,
    database: &str,
    connection_id: u32,
) -> Session<'s, S> {
    let mut session = Session::with_dialect(store, Dialect::MySql);
    session.identify(user, database);
    session.set_connection_id(connection_id);
    session
}

pub(crate) fn serve_connection<'a, 's, S: KeyValueStore + 's>(
    mut wire: Wire<'a>,
    shared: &'a Shared<'a>,
    store: &'s S,
    connection_id: u32,
) {
    let Some(login_slot) = take_slot(&shared.unauthenticated, MAX_UNAUTHENTICATED_CONNECTIONS)
    else {
        (shared.log)(&format!(
            "{}  connection refused: {MAX_UNAUTHENTICATED_CONNECTIONS} connections are still logging in",
            wire.peer()
        ));
        wire.deadline = Some(Instant::now() + REFUSAL_TIMEOUT);
        refuse_too_many(&mut wire);
        return;
    };
    let Some(nonce) = fresh_nonce() else {
        refuse(
            &mut wire,
            1105,
            "HY000",
            "could not generate a random nonce",
        );
        return;
    };
    wire.sequence = 0;
    wire.send(&handshake_packet(connection_id, &nonce));
    if wire.flush().is_err() {
        return;
    }
    let body = match wire.read_payload(1, MAX_AUTHENTICATION_PACKET_BYTES) {
        Ok(body) => body,
        Err(error) => {
            report_wire_error(&mut wire, &error);
            return;
        }
    };
    let response = match parse_handshake_response(&body) {
        Ok(response) => response,
        Err(error) => {
            refuse(&mut wire, 1043, "08S01", &format!("Bad handshake: {error}"));
            return;
        }
    };
    if let Outcome::Refused = authenticate(&mut wire, shared, &nonce, &response) {
        return;
    }
    let config = shared.config;
    if let Some(database) = &response.database
        && !database.eq_ignore_ascii_case(&config.database)
    {
        (shared.log)(&format!(
            "{}  unknown database \"{}\"",
            wire.peer(),
            printable(database)
        ));
        refuse(
            &mut wire,
            1049,
            "42000",
            &format!("Unknown database '{database}'"),
        );
        return;
    }
    drop(login_slot);
    wire.deadline = None;
    let capabilities = response.capabilities & SERVER_CAPABILITIES;
    let session = new_session(store, &response.user, &config.database, connection_id);
    let mut connection = Connection {
        wire,
        shared,
        store,
        session,
        user: response.user,
        connection_id,
        capabilities,
        multi_statements: capabilities & CLIENT_MULTI_STATEMENTS != 0,
        statements: HashMap::new(),
        next_statement: 1,
    };
    let status = connection.status();
    connection.wire.send(&ok_packet(0, 0, status, 0));
    if connection.wire.flush().is_ok() {
        connection.run();
    }
    let _ = connection.session.close();
}

fn affected_rows(tag: &str) -> u64 {
    match tag.split_whitespace().next() {
        Some("INSERT" | "UPDATE" | "DELETE") => tag
            .rsplit(' ')
            .next()
            .and_then(|count| count.parse().ok())
            .unwrap_or(0),
        _ => 0,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueClass {
    Integer,
    Float,
    Decimal,
    Boolean,
    Text,
    Date,
    DateTime,
    Timestamp,
    Json,
}

fn class_of_type(sql_type: SqlType) -> ValueClass {
    match sql_type {
        SqlType::SmallInt | SqlType::Integer | SqlType::BigInt => ValueClass::Integer,
        SqlType::Real | SqlType::Double => ValueClass::Float,
        SqlType::Numeric(_) => ValueClass::Decimal,
        SqlType::Boolean => ValueClass::Boolean,
        SqlType::Text | SqlType::Varchar(_) => ValueClass::Text,
        SqlType::Date => ValueClass::Date,
        SqlType::Timestamp(_) => ValueClass::DateTime,
        SqlType::TimestampTz(_) => ValueClass::Timestamp,
        SqlType::Json | SqlType::Jsonb => ValueClass::Json,
    }
}

fn temporal_length(precision: Option<u8>) -> u32 {
    match precision {
        Some(digits @ 1..=6) => 20 + u32::from(digits),
        _ => 19,
    }
}

fn wire_columns(columns: &[OutputColumn], rows: &[Vec<Value>]) -> Vec<ColumnDefinition> {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let mut values = rows.iter().filter_map(|row| row.get(index));
            let declared = column
                .sql_type
                .or_else(|| values.clone().find_map(Value::sql_type));
            let consistent = declared.is_some_and(|declared| {
                let class = class_of_type(declared);
                values.all(|value| {
                    value
                        .sql_type()
                        .is_none_or(|found| class_of_type(found) == class)
                })
            });
            let sql_type = if consistent { declared } else { None };
            column_for(&column.name, sql_type, rows, index)
        })
        .collect()
}

fn column_for(
    name: &str,
    sql_type: Option<SqlType>,
    rows: &[Vec<Value>],
    index: usize,
) -> ColumnDefinition {
    let (kind, length, decimals) = match sql_type {
        Some(SqlType::Boolean) => (TYPE_TINY, 1, 0),
        Some(SqlType::Real) => (TYPE_FLOAT, 12, 31),
        Some(SqlType::Double) => (TYPE_DOUBLE, 22, 31),
        Some(integer @ (SqlType::SmallInt | SqlType::Integer | SqlType::BigInt)) => {
            let widest = rows
                .iter()
                .filter_map(|row| row.get(index).and_then(Value::as_integer))
                .fold(integer, |widest, value| {
                    if i32::try_from(value).is_err() {
                        SqlType::BigInt
                    } else if i16::try_from(value).is_err() && widest == SqlType::SmallInt {
                        SqlType::Integer
                    } else {
                        widest
                    }
                });
            match widest {
                SqlType::SmallInt => (TYPE_SHORT, 6, 0),
                SqlType::Integer => (TYPE_LONG, 11, 0),
                _ => (TYPE_LONGLONG, 20, 0),
            }
        }
        Some(SqlType::Varchar(Some(length))) => (TYPE_VAR_STRING, length.saturating_mul(4), 0),
        Some(SqlType::Numeric(modifier)) => {
            let (precision, scale) = modifier.map_or((65, 30), |modifier| {
                (
                    u32::from(modifier.precision),
                    u8::try_from(modifier.scale.max(0)).unwrap_or(30),
                )
            });
            (TYPE_NEWDECIMAL, precision + 2, scale)
        }
        Some(SqlType::Date) => (TYPE_DATE, 10, 0),
        Some(SqlType::Timestamp(precision)) => (
            TYPE_DATETIME,
            temporal_length(precision),
            precision.unwrap_or(0),
        ),
        Some(SqlType::TimestampTz(precision)) => (
            TYPE_TIMESTAMP,
            temporal_length(precision),
            precision.unwrap_or(0),
        ),
        Some(SqlType::Json | SqlType::Jsonb) => (TYPE_JSON, u32::MAX, 0),
        _ => (TYPE_VAR_STRING, 262_140, 0),
    };
    ColumnDefinition {
        name: name.to_owned(),
        kind,
        length,
        decimals,
    }
}

fn binary_row(columns: &[ColumnDefinition], row: &[Value]) -> Result<Vec<u8>, SqlError> {
    let mut bitmap = vec![0u8; (columns.len() + 9) / 8];
    let mut values = Payload::new();
    for (index, column) in columns.iter().enumerate() {
        let value = row.get(index).unwrap_or(&Value::Null);
        if value.is_null() {
            let bit = index + 2;
            bitmap[bit / 8] |= 1 << (bit % 8);
            continue;
        }
        match column.kind {
            TYPE_TINY => {
                let number = match value {
                    Value::Boolean(flag) => i64::from(*flag),
                    other => other.as_integer().unwrap_or(0),
                };
                values.u8(number as u8);
            }
            TYPE_SHORT => {
                values.u16(value.as_integer().unwrap_or(0) as i16 as u16);
            }
            TYPE_LONG => {
                values.u32(value.as_integer().unwrap_or(0) as i32 as u32);
            }
            TYPE_LONGLONG => {
                values.bytes(&value.as_integer().unwrap_or(0).to_le_bytes());
            }
            TYPE_FLOAT => {
                let number = match value {
                    Value::Real(number) => *number,
                    other => other.as_float().unwrap_or(0.0) as f32,
                };
                values.bytes(&number.to_le_bytes());
            }
            TYPE_DOUBLE => {
                values.bytes(&value.as_float().unwrap_or(0.0).to_le_bytes());
            }
            TYPE_DATE | TYPE_DATETIME | TYPE_TIMESTAMP => {
                let text = Dialect::MySql.value_text(value).unwrap_or_default();
                values.bytes(&packed_temporal(&text)?);
            }
            _ => {
                let text = Dialect::MySql.value_text(value).unwrap_or_default();
                values.length_encoded_bytes(text.as_bytes());
            }
        }
    }
    let mut payload = Payload::new();
    payload.u8(0x00).bytes(&bitmap).bytes(&values.0);
    Ok(payload.0)
}

fn packed_temporal(text: &str) -> Result<Vec<u8>, SqlError> {
    let in_mysql_range = text.len() >= 10
        && text.as_bytes()[4] == b'-'
        && !text.ends_with(" BC")
        && text
            .get(..4)
            .is_some_and(|year| year.bytes().all(|byte| byte.is_ascii_digit()));
    if !in_mysql_range {
        return Err(SqlError::new(
            eggdb_sql::SqlState::INVALID_DATETIME_FORMAT,
            format!("Incorrect datetime value: '{text}'"),
        )
        .with_vendor_code(1292));
    }
    let number = |range: std::ops::Range<usize>| -> u32 {
        text.get(range)
            .and_then(|digits| digits.parse().ok())
            .unwrap_or(0)
    };
    let year = number(0..4) as u16;
    let (month, day) = (number(5..7) as u8, number(8..10) as u8);
    let (hour, minute, second) = (
        number(11..13) as u8,
        number(14..16) as u8,
        number(17..19) as u8,
    );
    let fraction = text.get(20..).unwrap_or("");
    let micros = if fraction.is_empty() {
        0
    } else {
        let padded = format!("{fraction:0<6}");
        padded
            .get(..6)
            .and_then(|digits| digits.parse().ok())
            .unwrap_or(0u32)
    };
    let mut out = vec![0u8];
    if (year, month, day) != (0, 0, 0) || hour != 0 || minute != 0 || second != 0 || micros != 0 {
        out.extend_from_slice(&year.to_le_bytes());
        out.push(month);
        out.push(day);
    }
    if hour != 0 || minute != 0 || second != 0 || micros != 0 {
        out.extend_from_slice(&[hour, minute, second]);
    }
    if micros != 0 {
        out.extend_from_slice(&micros.to_le_bytes());
    }
    out[0] = (out.len() - 1) as u8;
    Ok(out)
}

fn text_row(row: &[Value]) -> Vec<u8> {
    let mut payload = Payload::new();
    for value in row {
        match Dialect::MySql.value_text(value) {
            Some(text) => {
                payload.length_encoded_bytes(text.as_bytes());
            }
            None => {
                payload.u8(0xFB);
            }
        }
    }
    payload.0
}

fn parameter_of(value: BoundValue) -> Result<Parameter, SqlError> {
    let (declared_type, value) = match value {
        BoundValue::Null => (None, None),
        BoundValue::Integer(number) => (Some(SqlType::BigInt), Some(number.to_string())),
        BoundValue::Unsigned(number) => (None, Some(number.to_string())),
        BoundValue::Float(number) => (Some(SqlType::Double), Some(float_text(number))),
        BoundValue::Bytes(bytes) => (
            None,
            Some(String::from_utf8(bytes).map_err(|_| {
                SqlError::new(
                    eggdb_sql::SqlState::MYSQL_GENERAL,
                    "Incorrect string value: a parameter is not UTF-8",
                )
                .with_vendor_code(1366)
            })?),
        ),
    };
    Ok(Parameter {
        declared_type,
        value,
    })
}

fn float_text(number: f64) -> String {
    if number.is_nan() {
        "NaN".into()
    } else if number.is_infinite() {
        if number > 0.0 {
            "Infinity"
        } else {
            "-Infinity"
        }
        .into()
    } else {
        format!("{number:?}")
    }
}

enum Encoding {
    Text,
    Binary,
}

impl<'a, 's, S: KeyValueStore + 's> Connection<'a, 's, S> {
    fn status(&self) -> u16 {
        let mut status = 0;
        if self.session.autocommit() {
            status |= STATUS_AUTOCOMMIT;
        }
        if self.session.transaction_status() != TransactionStatus::Idle {
            status |= STATUS_IN_TRANSACTION;
        }
        status
    }

    fn run(&mut self) {
        loop {
            let body = match self.wire.read_payload(0, MAX_PACKET_BYTES) {
                Ok(body) => body,
                Err(WireError::Stopping) => {
                    self.shared
                        .ended_by_stop
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    self.wire.sequence = 1;
                    refuse(&mut self.wire, 1053, "08S01", "Server shutdown in progress");
                    return;
                }
                Err(error) => {
                    report_wire_error(&mut self.wire, &error);
                    return;
                }
            };
            let command = match parse_command(&body) {
                Ok(command) => command,
                Err(_) => {
                    send_error(
                        &mut self.wire,
                        1835,
                        "HY000",
                        "Malformed communication packet.",
                    );
                    continue;
                }
            };
            drop(body);
            let keep_going = match command {
                Command::Quit => false,
                Command::Ping => self.ok(0, 0, 0),
                Command::InitDb(database) => self.init_db(&database),
                Command::Query(text) => self.query(&text),
                Command::FieldList(table) => self.field_list(&table),
                Command::Prepare(text) => self.prepare(text),
                Command::Execute { statement, body } => self.execute(statement, &body),
                Command::SendLongData {
                    statement,
                    parameter,
                    data,
                } => {
                    self.long_data(statement, parameter, &data);
                    true
                }
                Command::Close(statement) => {
                    self.statements.remove(&statement);
                    true
                }
                Command::Reset(statement) => match self.statements.get_mut(&statement) {
                    Some(prepared) => {
                        prepared.long_data.iter_mut().for_each(|data| *data = None);
                        prepared.long_data_bytes = 0;
                        prepared.long_data_overflowed = false;
                        self.ok(0, 0, 0)
                    }
                    None => self.unknown_statement(statement, "mysqld_stmt_reset"),
                },
                Command::SetOption(option) => match option {
                    0 | 1 => {
                        self.multi_statements = option == 0;
                        let status = self.status();
                        self.wire.send(&eof_packet(0, status));
                        self.wire.flush().is_ok()
                    }
                    _ => self.error(1047, "08S01", "Unknown command"),
                },
                Command::ResetConnection => {
                    let fresh = new_session(
                        self.store,
                        &self.user,
                        &self.shared.config.database,
                        self.connection_id,
                    );
                    let old = std::mem::replace(&mut self.session, fresh);
                    let _ = old.close();
                    self.statements.clear();
                    self.multi_statements = self.capabilities & CLIENT_MULTI_STATEMENTS != 0;
                    self.ok(0, 0, 0)
                }
                Command::Other(_) => self.error(1047, "08S01", "Unknown command"),
            };
            if !keep_going {
                return;
            }
        }
    }

    fn ok(&mut self, affected: u64, last_insert_id: u64, warnings: u16) -> bool {
        let status = self.status();
        self.wire
            .send(&ok_packet(affected, last_insert_id, status, warnings));
        self.wire.flush().is_ok()
    }

    fn error(&mut self, code: u16, state: &str, message: &str) -> bool {
        self.wire.send(&err_packet(code, state, message));
        self.wire.flush().is_ok()
    }

    fn sql_error(&mut self, error: &SqlError) -> bool {
        send_sql_error(&mut self.wire, error);
        self.wire.flush().is_ok()
    }

    fn query_was_empty(&mut self) -> bool {
        self.error(1065, "42000", "Query was empty")
    }

    fn too_many_placeholders(&mut self) -> bool {
        self.error(
            1390,
            "HY000",
            "Prepared statement contains too many placeholders",
        )
    }

    fn unknown_statement(&mut self, statement: u32, command: &str) -> bool {
        self.error(
            1243,
            "HY000",
            &format!("Unknown prepared statement handler ({statement}) given to {command}"),
        )
    }

    fn init_db(&mut self, database: &str) -> bool {
        if database.eq_ignore_ascii_case(&self.shared.config.database) {
            return self.ok(0, 0, 0);
        }
        self.error(1049, "42000", &format!("Unknown database '{database}'"))
    }

    fn field_list(&mut self, table: &str) -> bool {
        let tables = match self.session.tables() {
            Ok(tables) => tables,
            Err(error) => return self.sql_error(&error),
        };
        let wanted = table.to_lowercase();
        let Some(schema) = tables.into_iter().find(|schema| schema.name == wanted) else {
            let database = self.shared.config.database.clone();
            return self.error(
                1146,
                "42S02",
                &format!("Table '{database}.{table}' doesn't exist"),
            );
        };
        let database = self.shared.config.database.clone();
        for column in &schema.columns {
            let definition = column_for(&column.name, Some(column.sql_type), &[], 0);
            let mut packet = column_definition_packet(&definition, &database);
            packet.push(0xFB);
            self.wire.send(&packet);
        }
        let status = self.status();
        self.wire.send(&eof_packet(0, status));
        self.wire.flush().is_ok()
    }

    fn send_result(&mut self, result: &QueryResult, more: bool, encoding: &Encoding) {
        let mut status = self.status();
        if more {
            status |= STATUS_MORE_RESULTS;
        }
        let warnings = u16::try_from(result.warnings.len()).unwrap_or(u16::MAX);
        let Some(columns) = &result.columns else {
            self.wire.send(&ok_packet(
                affected_rows(&result.tag),
                result.last_insert_id.unwrap_or(0),
                status,
                warnings,
            ));
            return;
        };
        let definitions = wire_columns(columns, &result.rows);
        let mut count = Payload::new();
        count.length_encoded(definitions.len() as u64);
        self.wire.send(&count.0);
        let database = self.shared.config.database.clone();
        for definition in &definitions {
            self.wire
                .send(&column_definition_packet(definition, &database));
        }
        self.wire
            .send(&eof_packet(warnings, status & !STATUS_MORE_RESULTS));
        for row in &result.rows {
            match encoding {
                Encoding::Text => self.wire.send(&text_row(row)),
                Encoding::Binary => match binary_row(&definitions, row) {
                    Ok(bytes) => self.wire.send(&bytes),
                    Err(error) => {
                        send_sql_error(&mut self.wire, &error);
                        return;
                    }
                },
            }
            if self.wire.flush_if_large().is_err() {
                return;
            }
        }
        self.wire.send(&eof_packet(warnings, status));
    }

    fn run_statements(
        &mut self,
        text: &str,
        parameters: Option<&[Parameter]>,
        encoding: &Encoding,
    ) -> bool {
        let mut results = Vec::new();
        let outcome = match parameters {
            Some(parameters) => self
                .session
                .execute_prepared(text, parameters, &mut |result| {
                    results.push(result);
                    true
                }),
            None => self.session.execute_while(text, &mut |result| {
                results.push(result);
                true
            }),
        };
        let count = results.len();
        for (index, result) in results.iter().enumerate() {
            let more = index + 1 < count || outcome.is_err();
            self.send_result(result, more, encoding);
        }
        match outcome {
            Ok(()) if count == 0 && parameters.is_none() => self.query_was_empty(),
            Ok(()) if count == 0 => self.ok(0, 0, 0),
            Ok(()) => self.wire.flush().is_ok(),
            Err(error) => self.sql_error(&error),
        }
    }

    fn query(&mut self, text: &str) -> bool {
        if !self.multi_statements
            && let Err(error) = self.session.prepare(text, &[])
        {
            return self.sql_error(&error);
        }
        self.run_statements(text, None, &Encoding::Text)
    }

    fn prepare(&mut self, text: String) -> bool {
        if self.statements.len() >= MAX_PREPARED_STATEMENTS
            || self.held_bytes().saturating_add(text.len()) > MAX_HELD_BYTES
        {
            return self.error(
                1461,
                "42000",
                "Can't create more than max_prepared_stmt_count statements",
            );
        }
        let prepared = match self.session.prepare(&text, &[]) {
            Ok(prepared) => prepared,
            Err(error) => return self.sql_error(&error),
        };
        if prepared.is_empty {
            return self.query_was_empty();
        }
        let count = prepared.parameter_count;
        if count > MAX_PLACEHOLDERS {
            return self.too_many_placeholders();
        }
        let description = match self.session.describe(&text, &vec![None; count]) {
            Ok(description) => description,
            Err(error) => return self.sql_error(&error),
        };
        let id = self.next_statement;
        self.next_statement = self.next_statement.wrapping_add(1).max(1);
        let columns = description.columns.unwrap_or_default();
        let mut ok = Payload::new();
        ok.u8(0x00)
            .u32(id)
            .u16(u16::try_from(columns.len()).unwrap_or(u16::MAX))
            .u16(u16::try_from(count).unwrap_or(u16::MAX))
            .u8(0)
            .u16(0);
        self.wire.send(&ok.0);
        let database = self.shared.config.database.clone();
        let status = self.status();
        if count > 0 {
            let parameter = column_for("?", None, &[], 0);
            for _ in 0..count {
                self.wire
                    .send(&column_definition_packet(&parameter, &database));
            }
            self.wire.send(&eof_packet(0, status));
        }
        if !columns.is_empty() {
            for definition in wire_columns(&columns, &[]) {
                self.wire
                    .send(&column_definition_packet(&definition, &database));
            }
            self.wire.send(&eof_packet(0, status));
        }
        self.statements.insert(
            id,
            Prepared {
                text,
                parameter_count: count,
                types: Vec::new(),
                long_data: vec![None; count],
                long_data_bytes: 0,
                long_data_overflowed: false,
            },
        );
        self.wire.flush().is_ok()
    }

    fn held_bytes(&self) -> usize {
        self.statements
            .values()
            .map(|prepared| prepared.text.len().saturating_add(prepared.long_data_bytes))
            .fold(0, usize::saturating_add)
    }

    fn long_data(&mut self, statement: u32, parameter: u16, data: &[u8]) {
        let fits = self.held_bytes().saturating_add(data.len()) <= MAX_HELD_BYTES;
        let Some(prepared) = self.statements.get_mut(&statement) else {
            return;
        };
        let Some(slot) = prepared.long_data.get_mut(usize::from(parameter)) else {
            return;
        };
        if fits {
            prepared.long_data_bytes += data.len();
            slot.get_or_insert_with(Vec::new).extend_from_slice(data);
        } else {
            prepared.long_data_overflowed = true;
        }
    }

    fn execute(&mut self, statement: u32, body: &[u8]) -> bool {
        let Some(prepared) = self.statements.get_mut(&statement) else {
            return self.unknown_statement(statement, "mysqld_stmt_execute");
        };
        let sent_long: Vec<bool> = prepared.long_data.iter().map(Option::is_some).collect();
        let parsed = parse_execute(body, prepared.parameter_count, &prepared.types, &sent_long);
        let too_long = std::mem::take(&mut prepared.long_data_overflowed);
        let long_data: Vec<Option<Vec<u8>>> =
            prepared.long_data.iter_mut().map(Option::take).collect();
        prepared.long_data_bytes = 0;
        let text = prepared.text.clone();
        let (types, values) = match parsed {
            Ok(parsed) => parsed,
            Err(_) => return self.error(1835, "HY000", "Malformed communication packet."),
        };
        if too_long {
            return self.error(
                1153,
                "08S01",
                "Got a packet bigger than 'max_allowed_packet' bytes",
            );
        }
        if let Some(prepared) = self.statements.get_mut(&statement) {
            prepared.types = types;
        }
        let mut parameters = Vec::with_capacity(values.len());
        for (index, value) in values.into_iter().enumerate() {
            let value = match long_data.get(index).cloned().flatten() {
                Some(bytes) if value != BoundValue::Null => BoundValue::Bytes(bytes),
                _ => value,
            };
            match parameter_of(value) {
                Ok(parameter) => parameters.push(parameter),
                Err(error) => return self.sql_error(&error),
            }
        }
        self.run_statements(&text, Some(&parameters), &Encoding::Binary)
    }
}

#[cfg(test)]
mod tests {
    use super::{affected_rows, printable};

    #[test]
    fn log_names_escape_line_breaks_and_direction_overrides() {
        assert_eq!(printable("a\nb\"c"), "a\\nb\\\"c");
        assert_eq!(printable("x\u{2028}y\u{202e}z"), "x\\u{2028}y\\u{202e}z");
    }

    #[test]
    fn affected_rows_come_from_the_command_tag() {
        assert_eq!(affected_rows("INSERT 0 3"), 3);
        assert_eq!(affected_rows("UPDATE 2"), 2);
        assert_eq!(affected_rows("CREATE TABLE"), 0);
    }
}
