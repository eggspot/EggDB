use crate::messages::{
    ATTENTION, BULK_LOAD, LOGIN7, Login7, PRELOGIN, PreLogin, RESET_CONNECTION,
    RESET_CONNECTION_SKIP_TRANSACTION, RPC, SQL_BATCH, TABULAR_RESULT, TDS_7_4,
    TRANSACTION_MANAGER, TransactionRequest, parse_login7, parse_prelogin, parse_sql_batch,
    parse_transaction_manager_request, prelogin_response,
};
use crate::bulk::{BulkInput, BulkLoadError, BulkReader};
use crate::procedures::PreparedBatches;
use crate::server::{MAX_UNAUTHENTICATED_CONNECTIONS, REFUSAL_TIMEOUT, Shared, take_slot};
use crate::tokens::{
    self, DONE, DONE_ATTENTION, DONE_COUNT, DONE_ERROR, DONE_IN_TRANSACTION, DONE_MORE,
    ENV_COMMIT_TRANSACTION, ENV_DATABASE, ENV_LANGUAGE, ENV_PACKET_SIZE, ENV_ROLLBACK_TRANSACTION,
    ServerMessage,
};
use crate::types::{declared_wire_columns, push_value, wire_columns};
use crate::wire::{DEFAULT_PACKET_SIZE, Message, POLL_INTERVAL, Packet, Wire, WireError};
use eggdb_sql::{
    BatchStep, BulkRows, Dialect, InfoMessage, KeyValueStore, NamedParameter, QueryResult,
    SQLSERVER_COMPATIBLE_VERSION, Session, SqlError, SqlResult, SqlState, Value,
};
use std::time::{Duration, Instant};

/// The largest PRELOGIN or LOGIN7 message accepted before a client has logged in.
pub const MAX_LOGIN_BYTES: usize = 64 * 1024;
/// The largest SQL batch accepted from a logged-in client, as MySQL's server allows per packet
/// (D106, D122).
pub const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;

const SMALLEST_PACKET_SIZE: u32 = 512;
const LARGEST_PACKET_SIZE: u32 = 32_767;
const SELECT_COMMAND: u16 = 0xC1;
const INSERT_COMMAND: u16 = 0xC3;
const DELETE_COMMAND: u16 = 0xC4;
const UPDATE_COMMAND: u16 = 0xC5;
const MERGE_COMMAND: u16 = 0x117;
const LOGIN_FAILED: u32 = 18_456;
const CANNOT_OPEN_DATABASE: u32 = 4060;
const TOO_MANY_CONNECTIONS: u32 = 17_809;
pub(crate) const NOT_SUPPORTED: u32 = 40_517;
const PROTOCOL_ERROR: u32 = 4002;
pub(crate) const LOGGED_BATCH_CHARACTERS: usize = 200;
const MAX_RESULT_COLUMNS: usize = 4096;
const TOO_MANY_RESULT_COLUMNS: u32 = 1056;
const BULK_DATA_INCORRECT: u16 = 4804;
const BULK_COLUMN_LENGTH: u16 = 4815;
const BULK_COLUMN_TYPE: u16 = 4816;

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
                    || matches!(
                        special,
                        '\u{200e}'
                            | '\u{200f}'
                            | '\u{061c}'
                            | '\u{2028}'..='\u{202e}'
                            | '\u{2066}'..='\u{2069}'
                    ) =>
            {
                shown.push_str(&format!("\\u{{{:x}}}", u32::from(special)));
            }
            other => shown.push(other),
        }
    }
    shown
}

fn bracketed(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn changed_rows(tag: &str) -> Option<(u16, u64)> {
    let mut words = tag.split_whitespace();
    let command = match words.next()? {
        "INSERT" => INSERT_COMMAND,
        "UPDATE" => UPDATE_COMMAND,
        "DELETE" => DELETE_COMMAND,
        "MERGE" => MERGE_COMMAND,
        _ => return None,
    };
    Some((command, words.last()?.parse().ok()?))
}

fn version_bytes() -> [u8; 4] {
    let mut parts = SQLSERVER_COMPATIBLE_VERSION
        .split('.')
        .map(|part| part.parse::<u16>().unwrap_or(0));
    let major = parts.next().unwrap_or(0) as u8;
    let minor = parts.next().unwrap_or(0) as u8;
    let build = parts.next().unwrap_or(0).to_be_bytes();
    [major, minor, build[0], build[1]]
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

fn newlines(text: &str, from: usize, to: usize) -> usize {
    text.as_bytes()
        .get(from..to)
        .map_or(0, |part| part.iter().filter(|&&byte| byte == b'\n').count())
}

struct LineCursor {
    offset: usize,
    line: usize,
}

impl LineCursor {
    fn new() -> Self {
        Self { offset: 0, line: 1 }
    }

    fn line_at(&mut self, text: &str, offset: usize) -> u32 {
        let offset = offset.min(text.len());
        if offset < self.offset {
            *self = Self::new();
        }
        self.line += newlines(text, self.offset, offset);
        self.offset = offset;
        u32::try_from(self.line).unwrap_or(u32::MAX)
    }
}

fn send_errors(wire: &mut Wire<'_>, errors: &[(u32, u8, &str)]) {
    let mut body = Vec::new();
    for (number, severity, text) in errors {
        body.extend(tokens::error(&ServerMessage {
            number: *number,
            severity: *severity,
            text,
            line: 1,
        }));
    }
    body.extend(tokens::done(DONE, DONE_ERROR, 0, 0));
    let _ = wire.send_message(TABULAR_RESULT, &body);
}

fn refuse(wire: &mut Wire<'_>, errors: &[(u32, u8, &str)]) {
    send_errors(wire, errors);
    wire.close_after_refusal();
}

fn login_failed(user: &str) -> String {
    format!("Login failed for user '{user}'.")
}

pub(crate) fn report_wire_error(shared: &Shared<'_>, wire: &mut Wire<'_>, error: &WireError) {
    match error {
        WireError::Tls => (shared.log)(&format!(
            "{}  client requires TLS (TDS 8 / encrypt=strict); EggDB has no TLS yet",
            wire.peer()
        )),
        WireError::Malformed(reason) => {
            (shared.log)(&format!("{}  protocol error: {reason}", wire.peer()));
            refuse(
                wire,
                &[(
                    PROTOCOL_ERROR,
                    16,
                    "The incoming tabular data stream (TDS) protocol stream is incorrect.",
                )],
            );
        }
        WireError::TooLarge => {
            (shared.log)(&format!("{}  message too large", wire.peer()));
            refuse(
                wire,
                &[(
                    PROTOCOL_ERROR,
                    16,
                    "The incoming tabular data stream (TDS) message is larger than EggDB accepts.",
                )],
            );
        }
        WireError::TimedOut => {
            (shared.log)(&format!("{}  login timed out", wire.peer()));
        }
        WireError::Closed | WireError::Stopping | WireError::Io => {}
    }
}

fn read_prelogin(shared: &Shared<'_>, wire: &mut Wire<'_>) -> Option<PreLogin> {
    let message = match wire.read_message(MAX_LOGIN_BYTES) {
        Ok(message) => message,
        Err(error) => {
            report_wire_error(shared, wire, &error);
            return None;
        }
    };
    if message.kind != PRELOGIN {
        (shared.log)(&format!(
            "{}  protocol error: the first message is not PRELOGIN",
            wire.peer()
        ));
        return None;
    }
    let prelogin = match parse_prelogin(&message.body) {
        Ok(prelogin) => prelogin,
        Err(error) => {
            (shared.log)(&format!("{}  protocol error: {error}", wire.peer()));
            return None;
        }
    };
    let answer = prelogin_response(&prelogin, version_bytes());
    if wire.send_message(TABULAR_RESULT, &answer).is_err() {
        return None;
    }
    if prelogin.wants_encryption() {
        (shared.log)(&format!(
            "{}  client requires encryption; EggDB has no TLS yet (connect with encrypt=false or sqlcmd -N disable)",
            wire.peer()
        ));
        wire.close_after_refusal();
        return None;
    }
    Some(prelogin)
}

pub(crate) fn refuse_too_many(shared: &Shared<'_>, wire: &mut Wire<'_>) {
    if read_prelogin(shared, wire).is_none() {
        return;
    }
    let Ok(mut message) = wire.read_message(MAX_LOGIN_BYTES) else {
        return;
    };
    message.body.fill(0);
    std::hint::black_box(&message.body);
    let text = format!(
        "Could not connect because the maximum number of '{}' user connections has already been reached. The connection has been closed.",
        shared.config.max_connections
    );
    refuse(wire, &[(TOO_MANY_CONNECTIONS, 20, &text)]);
}

enum Login {
    Accepted { user: String, packet_size: usize },
    Refused,
}

fn login_refusal(shared: &Shared<'_>, wire: &mut Wire<'_>, user: &str, reason: &str) {
    (shared.log)(&format!(
        "{}  login refused for user \"{}\": {reason}",
        wire.peer(),
        printable(user)
    ));
    refuse(wire, &[(LOGIN_FAILED, 14, &login_failed(user))]);
}

fn log_in(shared: &Shared<'_>, wire: &mut Wire<'_>) -> Login {
    let mut message = match wire.read_message(MAX_LOGIN_BYTES) {
        Ok(message) => message,
        Err(error) => {
            report_wire_error(shared, wire, &error);
            return Login::Refused;
        }
    };
    if message.kind != LOGIN7 {
        message.body.fill(0);
        std::hint::black_box(&message.body);
        (shared.log)(&format!(
            "{}  protocol error: the message after PRELOGIN is not LOGIN7",
            wire.peer()
        ));
        refuse(
            wire,
            &[(
                PROTOCOL_ERROR,
                16,
                "The incoming tabular data stream (TDS) protocol stream is incorrect.",
            )],
        );
        return Login::Refused;
    }
    let parsed = parse_login7(&message.body);
    message.body.fill(0);
    std::hint::black_box(&message.body);
    let login = match parsed {
        Ok(login) => login,
        Err(error) => {
            (shared.log)(&format!("{}  protocol error: {error}", wire.peer()));
            refuse(wire, &[(LOGIN_FAILED, 14, &login_failed(""))]);
            return Login::Refused;
        }
    };
    check_login(shared, wire, login)
}

fn check_login(shared: &Shared<'_>, wire: &mut Wire<'_>, login: Login7) -> Login {
    let user = login.user_name.clone();
    if login.tds_version < TDS_7_4 {
        let reason = format!(
            "the client speaks TDS {:#010x}; EggDB needs TDS 7.4",
            login.tds_version
        );
        login_refusal(shared, wire, &user, &reason);
        return Login::Refused;
    }
    if login.integrated_security || login.sspi_bytes > 0 {
        login_refusal(
            shared,
            wire,
            &user,
            "integrated (SSPI) authentication is not supported; use a SQL login",
        );
        return Login::Refused;
    }
    if login.federated_authentication {
        login_refusal(
            shared,
            wire,
            &user,
            "federated authentication is not supported; use a SQL login",
        );
        return Login::Refused;
    }
    if login.change_password {
        login_refusal(
            shared,
            wire,
            &user,
            "changing the password at login is not supported",
        );
        return Login::Refused;
    }
    let config = shared.config;
    let credentials = if user == config.user {
        &config.credentials
    } else {
        &shared.unknown_user
    };
    let accepted = match &login.password {
        Some(password) => credentials.verify(password.bytes()),
        None => {
            let _ = credentials.verify(&[]);
            false
        }
    };
    if !accepted || user != config.user {
        (shared.log)(&format!(
            "{}  password authentication failed for user \"{}\"",
            wire.peer(),
            printable(&user)
        ));
        sleep_unless_stopped(shared.throttle.delay_after_failure(), shared.stop);
        refuse(wire, &[(LOGIN_FAILED, 14, &login_failed(&user))]);
        return Login::Refused;
    }
    if !login.database.is_empty() && !login.database.eq_ignore_ascii_case(&config.database) {
        (shared.log)(&format!(
            "{}  unknown database \"{}\"",
            wire.peer(),
            printable(&login.database)
        ));
        let cannot_open = format!(
            "Cannot open database \"{}\" requested by the login. The login failed.",
            login.database
        );
        refuse(
            wire,
            &[
                (CANNOT_OPEN_DATABASE, 11, &cannot_open),
                (LOGIN_FAILED, 14, &login_failed(&user)),
            ],
        );
        return Login::Refused;
    }
    let packet_size = match login.packet_size {
        0 => DEFAULT_PACKET_SIZE as u32,
        size => size.clamp(SMALLEST_PACKET_SIZE, LARGEST_PACKET_SIZE),
    };
    Login::Accepted {
        user,
        packet_size: packet_size as usize,
    }
}

fn login_response(database: &str, packet_size: usize) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(tokens::env_change_text(ENV_DATABASE, database, "master"));
    body.extend(tokens::info(&ServerMessage {
        number: 5701,
        severity: 0,
        text: &format!("Changed database context to '{database}'."),
        line: 0,
    }));
    body.extend(tokens::env_change_text(ENV_LANGUAGE, "us_english", ""));
    body.extend(tokens::info(&ServerMessage {
        number: 5703,
        severity: 0,
        text: "Changed language setting to us_english.",
        line: 0,
    }));
    body.extend(tokens::env_change_collation());
    body.extend(tokens::env_change_text(
        ENV_PACKET_SIZE,
        &packet_size.to_string(),
        &DEFAULT_PACKET_SIZE.to_string(),
    ));
    body.extend(tokens::login_ack(TDS_7_4, "EggDB", version_bytes()));
    body.extend(tokens::done(DONE, 0, 0, 0));
    body
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
        refuse_too_many(shared, &mut wire);
        return;
    };
    if read_prelogin(shared, &mut wire).is_none() {
        return;
    }
    let Login::Accepted { user, packet_size } = log_in(shared, &mut wire) else {
        return;
    };
    drop(login_slot);
    wire.deadline = None;
    wire.spid = connection_id as u16;
    let config = shared.config;
    if wire
        .send_message(
            TABULAR_RESULT,
            &login_response(&config.database, packet_size),
        )
        .is_err()
    {
        return;
    }
    wire.packet_size = packet_size;
    let mut session = Session::with_dialect(store, Dialect::SqlServer);
    session.identify(&user, &config.database);
    session.set_connection_id(connection_id);
    shared.register(connection_id, session.cancel_signal());
    let mut connection = Connection {
        wire,
        shared,
        next_transaction: 1,
        transaction: None,
        prepared: PreparedBatches::default(),
        acknowledge_reset: false,
    };
    connection.run(&mut session);
    shared.unregister(connection_id);
    let _ = session.close();
}

pub(crate) struct Connection<'a> {
    pub(crate) wire: Wire<'a>,
    pub(crate) shared: &'a Shared<'a>,
    next_transaction: u64,
    transaction: Option<u64>,
    pub(crate) prepared: PreparedBatches,
    acknowledge_reset: bool,
}

pub(crate) struct PendingDone {
    pub(crate) status: u16,
    pub(crate) command: u16,
    pub(crate) rows: u64,
}

pub(crate) struct Ran {
    pub(crate) last: Option<PendingDone>,
    pub(crate) wrote_error: bool,
    pub(crate) failed_write: bool,
}

impl Connection<'_> {
    fn run<S: KeyValueStore>(&mut self, session: &mut Session<'_, S>) {
        loop {
            let message = match self.next_message() {
                Ok(Incoming::Message(message)) => message,
                Ok(Incoming::BulkLoad { first, body }) => {
                    if !self.bulk_load(session, first, body) {
                        return;
                    }
                    continue;
                }
                Err(WireError::Stopping) => {
                    self.shared
                        .ended_by_stop
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    return;
                }
                Err(error) => {
                    report_wire_error(self.shared, &mut self.wire, &error);
                    return;
                }
            };
            if matches!(message.kind, SQL_BATCH | RPC | TRANSACTION_MANAGER)
                && message.status & (RESET_CONNECTION | RESET_CONNECTION_SKIP_TRANSACTION) != 0
            {
                self.reset(session, message.status & RESET_CONNECTION == 0);
            }
            let keep_going = match message.kind {
                SQL_BATCH => self.batch(session, &message.body),
                RPC => self.rpc(session, &message.body),
                TRANSACTION_MANAGER => self.transaction_request(session, &message.body),
                ATTENTION => self.send_done(DONE_ATTENTION),
                _ => {
                    report_wire_error(
                        self.shared,
                        &mut self.wire,
                        &WireError::Malformed("an unknown packet type"),
                    );
                    false
                }
            };
            if !keep_going {
                return;
            }
        }
    }

    fn next_message(&mut self) -> Result<Incoming, WireError> {
        let mut body = Vec::new();
        let first = self.wire.read_first_packet(&mut body, MAX_BATCH_BYTES)?;
        if first.kind == BULK_LOAD {
            return Ok(Incoming::BulkLoad { first, body });
        }
        if let Err(error) = self
            .wire
            .read_rest_of_message(&first, &mut body, MAX_BATCH_BYTES)
        {
            body.fill(0);
            std::hint::black_box(&body);
            return Err(error);
        }
        Ok(Incoming::Message(Message {
            kind: first.kind,
            status: first.status,
            body,
        }))
    }

    fn bulk_load<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        first: Packet,
        body: Vec<u8>,
    ) -> bool {
        let pending = session.bulk_load_pending();
        let mut steps: Vec<BatchStep> = Vec::new();
        let (outcome, refused, wire_failure) = {
            let mut input = WireBulkInput {
                wire: &mut self.wire,
                ended: first.last,
                failure: None,
            };
            let mut rows = TdsBulkRows {
                reader: BulkReader::new(body, &mut input),
                started: false,
                refused: None,
            };
            let outcome = if pending {
                session.execute_bulk_load(&mut rows, &mut |step| {
                    steps.push(step);
                    true
                })
            } else {
                Ok(())
            };
            let refused = rows.refused.take();
            drop(rows);
            input.discard_rest();
            (outcome, refused, input.failure.take())
        };
        if let Some(error) = wire_failure {
            report_wire_error(self.shared, &mut self.wire, &error);
            return false;
        }
        if !pending {
            (self.shared.log)(&format!(
                "{}  protocol error: bulk load data without an INSERT BULK before it",
                self.wire.peer()
            ));
            return self.send_error(
                PROTOCOL_ERROR,
                16,
                "Bulk load data was sent without an INSERT BULK statement before it.",
            );
        }
        if let Some(refused) = refused {
            (self.shared.log)(&format!(
                "{}  bulk load data refused: {refused:?}",
                self.wire.peer()
            ));
        }
        if self.shared.config.log_batches {
            (self.shared.log)(&format!("{}  bulk load data", self.wire.peer()));
        }
        self.send_steps(session, steps, outcome)
    }

    fn send_steps<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        steps: Vec<BatchStep>,
        outcome: SqlResult<()>,
    ) -> bool {
        if !self.begin_response() {
            return false;
        }
        let database = session.database().to_owned();
        let ran = self.write_steps("", database, DONE, |on_step| {
            for step in steps {
                if !on_step(step) {
                    break;
                }
            }
            outcome
        });
        self.finish_batch(session, ran)
    }

    fn reset<S: KeyValueStore>(&mut self, session: &mut Session<'_, S>, keep_transaction: bool) {
        let keep_transaction = keep_transaction && self.transaction.is_some();
        if let Err(error) = session.reset_connection(keep_transaction) {
            (self.shared.log)(&format!(
                "{}  the connection reset could not roll back: {}",
                self.wire.peer(),
                error.message()
            ));
        }
        if !keep_transaction {
            self.transaction = None;
        }
        self.acknowledge_reset = true;
    }

    pub(crate) fn begin_response(&mut self) -> bool {
        self.wire.begin_message(TABULAR_RESULT);
        if std::mem::take(&mut self.acknowledge_reset) {
            return self
                .wire
                .write(&tokens::env_change_reset_acknowledged())
                .is_ok();
        }
        true
    }

    fn send_response(&mut self, body: &[u8]) -> bool {
        self.begin_response() && self.wire.write(body).is_ok() && self.wire.end_message().is_ok()
    }

    fn send_done(&mut self, status: u16) -> bool {
        self.send_response(&tokens::done(DONE, status, 0, 0))
    }

    pub(crate) fn send_error(&mut self, number: u32, severity: u8, text: &str) -> bool {
        let mut body = tokens::error(&ServerMessage {
            number,
            severity,
            text,
            line: 1,
        });
        body.extend(tokens::done(DONE, DONE_ERROR, 0, 0));
        self.send_response(&body)
    }

    fn send_not_supported(&mut self, what: &str) -> bool {
        let text = format!("{what} are not supported in this version of EggDB.");
        self.send_error(NOT_SUPPORTED, 16, &text)
    }

    fn transaction_request<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        body: &[u8],
    ) -> bool {
        let request = match parse_transaction_manager_request(body) {
            Ok(request) => request,
            Err(error) => {
                report_wire_error(
                    self.shared,
                    &mut self.wire,
                    &WireError::Malformed(error.reason),
                );
                return false;
            }
        };
        let text = match &request.request {
            TransactionRequest::Begin { .. } => "BEGIN TRANSACTION".to_owned(),
            TransactionRequest::Commit {
                begin_next: None, ..
            } => "COMMIT TRANSACTION".to_owned(),
            TransactionRequest::Commit {
                begin_next: Some(_),
                ..
            } => "COMMIT TRANSACTION BEGIN TRANSACTION".to_owned(),
            TransactionRequest::Rollback { name, begin_next } => {
                let mut text = "ROLLBACK TRANSACTION".to_owned();
                if !name.is_empty() {
                    text.push(' ');
                    text.push_str(&bracketed(name));
                }
                if begin_next.is_some() {
                    text.push_str(" BEGIN TRANSACTION");
                }
                text
            }
            TransactionRequest::Save { name } => {
                format!("SAVE TRANSACTION {}", bracketed(name))
            }
            TransactionRequest::Distributed(_) => {
                return self.send_not_supported("Distributed transactions");
            }
        };
        if self.shared.config.log_batches {
            (self.shared.log)(&format!(
                "{}  transaction manager: {text}",
                self.wire.peer()
            ));
        }
        session.in_connection_dialect(|session| self.run_batch_text(session, &text))
    }

    fn batch<S: KeyValueStore>(&mut self, session: &mut Session<'_, S>, body: &[u8]) -> bool {
        let batch = match parse_sql_batch(body) {
            Ok(batch) => batch,
            Err(error) => {
                report_wire_error(
                    self.shared,
                    &mut self.wire,
                    &WireError::Malformed(error.reason),
                );
                return false;
            }
        };
        if self.shared.config.log_batches {
            let shown: String = batch.text.chars().take(LOGGED_BATCH_CHARACTERS).collect();
            (self.shared.log)(&format!(
                "{}  batch: {}",
                self.wire.peer(),
                printable(&shown)
            ));
        }
        self.run_batch_text(session, &batch.text)
    }

    pub(crate) fn run_statements<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        text: &str,
        parameters: Option<&[NamedParameter]>,
        done_token: u8,
    ) -> Ran {
        let database = session.database().to_owned();
        let ran = self.write_steps(text, database, done_token, |on_step| match parameters {
            None => session.execute_batch(text, on_step),
            Some(parameters) => session.execute_parameterized_batch(text, parameters, on_step),
        });
        self.forget_unprepared(session);
        ran
    }

    fn write_steps(
        &mut self,
        text: &str,
        database: String,
        done_token: u8,
        execute: impl FnOnce(&mut dyn FnMut(BatchStep) -> bool) -> SqlResult<()>,
    ) -> Ran {
        let mut pending: Option<PendingDone> = None;
        let mut failed_write = false;
        let mut writer = StepWriter {
            connection: self,
            text,
            lines: LineCursor::new(),
            database,
            pending: &mut pending,
            failed_write: &mut failed_write,
            done_token,
            wrote_error: false,
        };
        let outcome = execute(&mut |step| writer.step(step));
        if let Err(error) = outcome {
            writer.error(&error, 0);
        }
        let wrote_error = writer.wrote_error;
        Ran {
            last: pending,
            wrote_error,
            failed_write,
        }
    }

    fn run_batch_text<S: KeyValueStore>(
        &mut self,
        session: &mut Session<'_, S>,
        text: &str,
    ) -> bool {
        if !self.begin_response() {
            return false;
        }
        let ran = self.run_statements(session, text, None, DONE);
        self.finish_batch(session, ran)
    }

    fn finish_batch<S: KeyValueStore>(&mut self, session: &Session<'_, S>, ran: Ran) -> bool {
        let in_transaction = session.transaction_status() != eggdb_sql::TransactionStatus::Idle;
        if ran.failed_write {
            return false;
        }
        let last = ran.last.unwrap_or(PendingDone {
            status: if in_transaction {
                DONE_IN_TRANSACTION
            } else {
                0
            },
            command: 0,
            rows: 0,
        });
        let status = (last.status & !DONE_MORE)
            | if in_transaction {
                DONE_IN_TRANSACTION
            } else {
                0
            };
        if self
            .wire
            .write(&tokens::done(DONE, status, last.command, last.rows))
            .is_err()
        {
            return false;
        }
        self.wire.end_message().is_ok()
    }
}

enum Incoming {
    Message(Message),
    BulkLoad { first: Packet, body: Vec<u8> },
}

struct WireBulkInput<'w, 'a> {
    wire: &'w mut Wire<'a>,
    ended: bool,
    failure: Option<WireError>,
}

impl WireBulkInput<'_, '_> {
    fn read_packet(&mut self, buffer: &mut Vec<u8>) -> bool {
        if self.ended {
            return false;
        }
        let before = buffer.len();
        match self.wire.read_packet(buffer, usize::MAX, false) {
            Ok(packet) if packet.kind == BULK_LOAD => {
                self.ended = packet.last;
                true
            }
            Ok(_) => {
                buffer.truncate(before);
                self.failure = Some(WireError::Malformed(
                    "the packets of one message have different types",
                ));
                self.ended = true;
                false
            }
            Err(error) => {
                buffer.truncate(before);
                self.failure = Some(error);
                self.ended = true;
                false
            }
        }
    }

    fn discard_rest(&mut self) {
        let mut scratch = Vec::new();
        while !self.ended {
            scratch.clear();
            self.read_packet(&mut scratch);
        }
    }
}

impl BulkInput for WireBulkInput<'_, '_> {
    fn read_more(&mut self, buffer: &mut Vec<u8>) -> bool {
        self.read_packet(buffer)
    }
}

fn bulk_error(error: BulkLoadError) -> SqlError {
    match error {
        BulkLoadError::Malformed(malformed) => SqlError::new(
            SqlState::PROTOCOL_VIOLATION,
            format!("The bulk load data is incorrect: {}.", malformed.reason),
        )
        .with_sqlserver_code(BULK_DATA_INCORRECT),
        BulkLoadError::UnsupportedType(kind) => SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!(
                "Invalid column type from bcp client: EggDB does not read TDS type 0x{kind:02X} yet."
            ),
        )
        .with_sqlserver_code(BULK_COLUMN_TYPE),
        BulkLoadError::EncryptedColumn => SqlError::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "Always Encrypted columns are not supported in this version of EggDB.",
        )
        .with_sqlserver_code(NOT_SUPPORTED as u16),
        BulkLoadError::RowTooLarge => SqlError::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!(
                "A bulk load row is larger than the {} bytes EggDB accepts.",
                crate::bulk::MAX_BULK_ROW_BYTES
            ),
        )
        .with_sqlserver_code(BULK_COLUMN_LENGTH),
    }
}

struct TdsBulkRows<I: BulkInput> {
    reader: BulkReader<I>,
    started: bool,
    refused: Option<BulkLoadError>,
}

impl<I: BulkInput> TdsBulkRows<I> {
    fn refuse(&mut self, error: BulkLoadError) -> SqlError {
        self.refused = Some(error);
        bulk_error(error)
    }
}

impl<I: BulkInput> BulkRows for TdsBulkRows<I> {
    fn next_row(&mut self, columns: usize, row: &mut Vec<Value>) -> SqlResult<bool> {
        if !self.started {
            self.started = true;
            let described = self.reader.read_metadata().map(<[_]>::len);
            let count = match described {
                Ok(count) => count,
                Err(error) => return Err(self.refuse(error)),
            };
            if count != columns {
                return Err(SqlError::new(
                    SqlState::BAD_COPY_FILE_FORMAT,
                    format!(
                        "The bulk load data describes {count} columns, but the INSERT BULK statement takes {columns}."
                    ),
                )
                .with_sqlserver_code(BULK_COLUMN_LENGTH));
            }
        }
        match self.reader.next_row(row) {
            Ok(more) => Ok(more),
            Err(error) => Err(self.refuse(error)),
        }
    }
}

struct StepWriter<'w, 'a> {
    connection: &'w mut Connection<'a>,
    text: &'w str,
    lines: LineCursor,
    database: String,
    pending: &'w mut Option<PendingDone>,
    failed_write: &'w mut bool,
    done_token: u8,
    wrote_error: bool,
}

impl StepWriter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> bool {
        if self.connection.wire.write(bytes).is_err() {
            *self.failed_write = true;
            return false;
        }
        true
    }

    fn flush_pending(&mut self) -> bool {
        let Some(pending) = self.pending.take() else {
            return true;
        };
        let status = pending.status | DONE_MORE;
        self.write(&tokens::done(
            self.done_token,
            status,
            pending.command,
            pending.rows,
        ))
    }

    fn transaction_change(&mut self, in_transaction: bool, committed: bool) -> bool {
        match (self.connection.transaction, in_transaction) {
            (None, true) => {
                let descriptor = self.connection.next_transaction;
                self.connection.next_transaction += 1;
                self.connection.transaction = Some(descriptor);
                self.write(&tokens::env_change_begin(descriptor))
            }
            (Some(descriptor), false) => {
                self.connection.transaction = None;
                let kind = if committed {
                    ENV_COMMIT_TRANSACTION
                } else {
                    ENV_ROLLBACK_TRANSACTION
                };
                self.write(&tokens::env_change_end(kind, descriptor))
            }
            _ => true,
        }
    }

    fn error(&mut self, error: &SqlError, statement_start: usize) {
        let line = self
            .lines
            .line_at(self.text, error.position().unwrap_or(statement_start));
        let number = error.sqlserver_number().unwrap_or(50_000);
        let severity = error.severity().unwrap_or(16);
        let token = tokens::error_in_state(
            &ServerMessage {
                number,
                severity,
                text: error.message(),
                line,
            },
            error.sqlserver_state().unwrap_or(1),
        );
        if self.write(&token) {
            self.wrote_error = true;
            *self.pending = Some(PendingDone {
                status: DONE_ERROR,
                command: 0,
                rows: 0,
            });
        }
    }

    fn step(&mut self, step: BatchStep) -> bool {
        if !self.flush_pending() {
            return false;
        }
        let committed = matches!(&step.outcome, Ok(result) if result.tag == "COMMIT");
        let line = self.lines.line_at(self.text, step.statement_start);
        let written = match step.outcome {
            Ok(result) => {
                self.messages(&result.info_messages, line)
                    && self.transaction_change(step.in_transaction, committed)
                    && self.result(&result, step.no_count, step.format_only)
            }
            Err(error) => {
                self.error(&error, step.statement_start);
                !*self.failed_write && self.transaction_change(step.in_transaction, committed)
            }
        };
        if step.in_transaction
            && let Some(pending) = self.pending.as_mut()
        {
            pending.status |= DONE_IN_TRANSACTION;
        }
        written
    }

    fn messages(&mut self, messages: &[InfoMessage], line: u32) -> bool {
        let mut body = Vec::new();
        for message in messages {
            body.extend(tokens::info_in_state(
                &ServerMessage {
                    number: message.number,
                    severity: message.severity,
                    text: &message.text,
                    line,
                },
                message.state,
            ));
        }
        body.is_empty() || self.write(&body)
    }

    fn result(&mut self, result: &QueryResult, no_count: bool, format_only: bool) -> bool {
        let count = if no_count { 0 } else { DONE_COUNT };
        if let Some(database) = result.tag.strip_prefix("USE ") {
            let old = std::mem::replace(&mut self.database, database.to_owned());
            let mut body = tokens::env_change_text(ENV_DATABASE, database, &old);
            body.extend(tokens::info(&ServerMessage {
                number: 5701,
                severity: 0,
                text: &format!("Changed database context to '{database}'."),
                line: 1,
            }));
            if !self.write(&body) {
                return false;
            }
            *self.pending = Some(PendingDone {
                status: 0,
                command: 0,
                rows: 0,
            });
            return true;
        }
        if let Some(columns) = &result.columns {
            if columns.len() > MAX_RESULT_COLUMNS {
                let token = tokens::error(&ServerMessage {
                    number: TOO_MANY_RESULT_COLUMNS,
                    severity: 15,
                    text: "The number of elements in the select list exceeds the maximum allowed number of 4096 elements.",
                    line: 1,
                });
                if !self.write(&token) {
                    return false;
                }
                *self.pending = Some(PendingDone {
                    status: DONE_ERROR,
                    command: 0,
                    rows: 0,
                });
                return true;
            }
            let wire_columns = if format_only {
                declared_wire_columns(columns)
            } else {
                wire_columns(columns, &result.rows)
            };
            if !self.write(&tokens::column_metadata(&wire_columns)) {
                return false;
            }
            let mut row_bytes = Vec::new();
            for row in &result.rows {
                row_bytes.clear();
                row_bytes.push(tokens::row_start());
                for (column, value) in wire_columns.iter().zip(row) {
                    push_value(&mut row_bytes, column, value);
                }
                if !self.write(&row_bytes) {
                    return false;
                }
            }
            let (command, rows) = match changed_rows(&result.tag) {
                Some(changed) => changed,
                None => (SELECT_COMMAND, result.rows.len() as u64),
            };
            *self.pending = Some(PendingDone {
                status: count,
                command,
                rows,
            });
            return true;
        }
        let mut words = result.tag.split_whitespace();
        let command = match words.next() {
            Some("INSERT") => INSERT_COMMAND,
            Some("UPDATE") => UPDATE_COMMAND,
            Some("DELETE") => DELETE_COMMAND,
            Some("MERGE") => MERGE_COMMAND,
            _ => 0,
        };
        let rows = if command == 0 {
            None
        } else {
            result
                .tag
                .rsplit(' ')
                .next()
                .and_then(|count| count.parse::<u64>().ok())
        };
        *self.pending = Some(match rows {
            Some(rows) => PendingDone {
                status: count,
                command,
                rows,
            },
            None => PendingDone {
                status: 0,
                command,
                rows: 0,
            },
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{LineCursor, printable, version_bytes};

    #[test]
    fn the_version_is_sql_server_2022_build_1000() {
        assert_eq!(version_bytes(), [16, 0, 0x03, 0xE8]);
    }

    #[test]
    fn lines_count_from_one_and_log_names_escape_line_breaks() {
        let text = "SELECT 1\nSELECT x\n\nSELECT y";
        let mut lines = LineCursor::new();
        assert_eq!(lines.line_at(text, 0), 1);
        assert_eq!(lines.line_at(text, 9), 2);
        assert_eq!(lines.line_at(text, 19), 4);
        assert_eq!(lines.line_at(text, 9), 2);
        assert_eq!(LineCursor::new().line_at("SELECT 1", 100), 1);
        assert_eq!(printable("\u{2066}\u{200f}"), "\\u{2066}\\u{200f}");
        assert_eq!(printable("a\nb\u{202e}"), "a\\nb\\u{202e}");
    }
}
