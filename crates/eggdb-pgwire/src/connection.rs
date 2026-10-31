use crate::messages::{
    ColumnDescription, Format, FrontendMessage, ReportField, StartupRequest, Target, TooLarge,
    parse_message, parse_sasl_initial_response, parse_startup,
};
use crate::backend_keys::SECRET_KEY_BYTES;
use crate::scram::{ScramExchange, ScramFailure, ScramVerifier};
use crate::server::{MAX_UNAUTHENTICATED_CONNECTIONS, REFUSAL_TIMEOUT, Shared, take_slot};
use crate::types::{
    TEXT_OID, UNKNOWN_OID, decode_parameter, describe_columns, encode_value, format_at, oid_of,
    sql_type_of,
};
use crate::wire::{POLL_INTERVAL, Wire, WireError};
use eggdb_sql::{
    CancelSignal, CopyChannel, CopyFormat, CopyStart, KeyValueStore, OutputColumn, Parameter,
    QueryResult, Session, SqlError, SqlResult, SqlState, SqlType, TransactionStatus,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The largest message accepted before authentication succeeds.
pub const MAX_AUTHENTICATION_MESSAGE_BYTES: usize = 16 * 1024;
/// The largest message accepted from an authenticated client: 1 GiB, the largest value a
/// database stores, plus room for the rest of the message.
pub const MAX_MESSAGE_BYTES: usize = (1 << 30) + (1 << 20);
const MAX_ENCRYPTION_REQUESTS: usize = 2;
const SERVER_NONCE_BYTES: usize = 18;

enum Severity {
    Error,
    Fatal,
}

impl Severity {
    fn name(&self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Fatal => "FATAL",
        }
    }
}

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

fn too_large(error: TooLarge) -> SqlError {
    let state = match error {
        TooLarge::Columns(_) => SqlState::TOO_MANY_COLUMNS,
        TooLarge::Parameters(_) | TooLarge::ValueBytes(_) => SqlState::PROGRAM_LIMIT_EXCEEDED,
    };
    SqlError::new(state, error.to_string())
}

fn character_position(text: &str, byte_offset: usize) -> usize {
    let end = byte_offset.min(text.len());
    let mut boundary = end;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text[..boundary].chars().count() + 1
}

fn report_error(wire: &mut Wire<'_>, severity: Severity, error: &SqlError, text: Option<&str>) {
    let position = match (error.position(), text) {
        (Some(offset), Some(text)) => Some(character_position(text, offset).to_string()),
        _ => None,
    };
    let state = error.state();
    let mut fields = vec![
        (ReportField::Severity, severity.name()),
        (ReportField::SeverityName, severity.name()),
        (ReportField::Code, state.code()),
        (ReportField::Message, error.message()),
    ];
    if let Some(detail) = error.detail() {
        fields.push((ReportField::Detail, detail));
    }
    if let Some(hint) = error.hint() {
        fields.push((ReportField::Hint, hint));
    }
    if let Some(position) = &position {
        fields.push((ReportField::Position, position));
    }
    if let Some(context) = error.context() {
        fields.push((ReportField::Where, context));
    }
    wire.outbox.report(false, &fields);
}

fn fatal(wire: &mut Wire<'_>, state: SqlState, message: &str) {
    report_error(wire, Severity::Fatal, &SqlError::new(state, message), None);
    let _ = wire.flush();
    wire.close_after_refusal();
}

fn send_notice(wire: &mut Wire<'_>, notice: &str) {
    let (severity, message) = notice.split_once(":  ").unwrap_or(("NOTICE", notice));
    let code = if message.contains("there is no transaction in progress") {
        "25P01"
    } else if message.contains("there is already a transaction in progress") {
        "25001"
    } else {
        "01000"
    };
    wire.outbox.report(
        true,
        &[
            (ReportField::Severity, severity),
            (ReportField::SeverityName, severity),
            (ReportField::Code, code),
            (ReportField::Message, message),
        ],
    );
}

fn is_transaction_exit(text: &str) -> bool {
    let first_word: String = text
        .trim_start_matches(|c: char| c.is_whitespace() || c == ';' || c == '(')
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(first_word.as_str(), "commit" | "rollback" | "end" | "abort")
}

fn aborted() -> SqlError {
    SqlError::new(
        SqlState::IN_FAILED_SQL_TRANSACTION,
        "current transaction is aborted, commands ignored until end of transaction block",
    )
}

fn protocol_violation(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::PROTOCOL_VIOLATION, message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lost {
    Closed,
    Stopping,
    TooLarge(usize),
}

struct WireCopy<'r, 'w, 'a> {
    wire: &'r RefCell<&'w mut Wire<'a>>,
    lost: Option<Lost>,
}

impl WireCopy<'_, '_, '_> {
    fn lose(&mut self, lost: Lost) -> SqlError {
        self.lost = Some(lost);
        match lost {
            Lost::Stopping => SqlError::new(
                SqlState::ADMIN_SHUTDOWN,
                "terminating connection due to administrator command",
            ),
            Lost::TooLarge(length) => {
                protocol_violation(format!("invalid message length {length}"))
            }
            Lost::Closed => SqlError::new(
                SqlState::CONNECTION_FAILURE,
                "unexpected EOF on client connection with an open transaction",
            ),
        }
    }

    fn sent(&mut self, written: std::io::Result<()>) -> SqlResult<()> {
        written.map_err(|_| self.lose(Lost::Closed))
    }
}

fn copy_format(start: &CopyStart) -> Format {
    match start.format {
        CopyFormat::Binary => Format::Binary,
        CopyFormat::Text | CopyFormat::Csv => Format::Text,
    }
}

impl CopyChannel for WireCopy<'_, '_, '_> {
    fn start_copy_in(&mut self, start: &CopyStart) -> SqlResult<()> {
        let flushed = {
            let mut wire = self.wire.borrow_mut();
            wire.outbox
                .copy_in_response(start.columns, copy_format(start))
                .map_err(too_large)?;
            wire.flush()
        };
        self.sent(flushed)
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        loop {
            let read = self.wire.borrow_mut().read_message(MAX_MESSAGE_BYTES);
            let (tag, body) = match read {
                Ok(message) => message,
                Err(WireError::Stopping) => return Err(self.lose(Lost::Stopping)),
                Err(WireError::TooLarge(length)) => return Err(self.lose(Lost::TooLarge(length))),
                Err(WireError::Closed | WireError::TimedOut | WireError::Io) => {
                    return Err(self.lose(Lost::Closed));
                }
            };
            match tag {
                b'd' => {
                    data.extend_from_slice(&body);
                    return Ok(true);
                }
                b'c' => return Ok(false),
                b'f' => {
                    let reason = match parse_message(tag, &body) {
                        Ok(FrontendMessage::CopyFail(reason)) => reason,
                        _ => String::new(),
                    };
                    return Err(SqlError::new(
                        SqlState::QUERY_CANCELED,
                        format!("COPY from stdin failed: {reason}"),
                    ));
                }
                b'H' | b'S' => {}
                other => {
                    return Err(protocol_violation(format!(
                        "unexpected message type 0x{other:02X} during COPY from stdin"
                    )));
                }
            }
        }
    }

    fn start_copy_out(&mut self, start: &CopyStart) -> SqlResult<()> {
        self.wire
            .borrow_mut()
            .outbox
            .copy_out_response(start.columns, copy_format(start))
            .map_err(too_large)
    }

    fn write_copy_data(&mut self, data: &[u8]) -> SqlResult<()> {
        let flushed = {
            let mut wire = self.wire.borrow_mut();
            wire.outbox.copy_data(data).map_err(too_large)?;
            wire.flush_if_large()
        };
        self.sent(flushed)
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        self.wire.borrow_mut().outbox.copy_done();
        Ok(())
    }
}

const DIALECT_PARAMETER: &str = "eggdb.dialect";
const DISCARD_ALL_TAG: &str = "DISCARD ALL";

pub(crate) struct Startup {
    user: String,
    database: String,
    minor: u16,
    parameters: Vec<(String, String)>,
}

fn options_parameters(options: &str) -> Vec<(String, String)> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut escaped = false;
    for character in options.chars() {
        if escaped {
            word.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(character);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    let mut parameters = Vec::new();
    let mut words = words.into_iter();
    while let Some(word) = words.next() {
        let setting = if word == "-c" {
            words.next()
        } else if let Some(rest) = word.strip_prefix("-c") {
            Some(rest.to_owned())
        } else {
            word.strip_prefix("--").map(str::to_owned)
        };
        if let Some((name, value)) = setting
            .as_deref()
            .and_then(|setting| setting.split_once('='))
        {
            parameters.push((name.replace('-', "_"), value.to_owned()));
        }
    }
    parameters
}

fn cancel_running_statement(
    wire: &Wire<'_>,
    shared: &Shared<'_>,
    process_id: i32,
    secret_key: &[u8],
) {
    if !shared.backend_keys.cancel(process_id, secret_key) {
        (shared.log)(&format!(
            "{}  cancel request for process {process_id} ignored: no such process, or a wrong secret key",
            wire.peer()
        ));
    }
}

const TLS_RECORD_HANDSHAKE: u8 = 0x16;
const POSTGRESQL_APPLICATION_PROTOCOL: &[u8] = b"postgresql";

fn start_tls(wire: &mut Wire<'_>, shared: &Shared<'_>, config: &eggdb_tls::ServerConfig) -> bool {
    match wire.start_tls(config) {
        Ok(()) => true,
        Err(error) => {
            (shared.log)(&format!("{}  TLS handshake failed: {error}", wire.peer()));
            false
        }
    }
}

fn start_direct_tls(wire: &mut Wire<'_>, shared: &Shared<'_>) -> Option<bool> {
    if wire.is_local() {
        return Some(true);
    }
    let config = shared.config.tls.as_deref()?;
    match wire.peek_byte() {
        Ok(TLS_RECORD_HANDSHAKE) => {}
        Ok(_) => return Some(true),
        Err(_) => return Some(false),
    }
    if !start_tls(wire, shared, config) {
        return Some(false);
    }
    let negotiated = wire.tls().and_then(|info| info.application_protocol.as_deref());
    if negotiated != Some(POSTGRESQL_APPLICATION_PROTOCOL) {
        (shared.log)(&format!(
            "{}  direct TLS connection refused: the client did not ask for the postgresql protocol with ALPN",
            wire.peer()
        ));
        return Some(false);
    }
    Some(true)
}

fn read_startup(wire: &mut Wire<'_>, shared: &Shared<'_>) -> Option<Startup> {
    if start_direct_tls(wire, shared) == Some(false) {
        return None;
    }
    for _ in 0..=MAX_ENCRYPTION_REQUESTS {
        let body = match wire.read_startup_packet() {
            Ok(body) => body,
            Err(WireError::TooLarge(_)) => {
                fatal(
                    wire,
                    SqlState::PROTOCOL_VIOLATION,
                    "invalid length of startup packet",
                );
                return None;
            }
            Err(_) => return None,
        };
        match parse_startup(&body) {
            Ok(StartupRequest::Ssl) if wire.tls().is_some() => {
                fatal(
                    wire,
                    SqlState::PROTOCOL_VIOLATION,
                    "SSLRequest on a connection that already uses TLS",
                );
                return None;
            }
            Ok(StartupRequest::Ssl) if shared.config.tls.is_some() && !wire.is_local() => {
                wire.outbox.accept_encryption();
                if wire.flush().is_err() {
                    return None;
                }
                let config = shared.config.tls.as_deref()?;
                if !start_tls(wire, shared, config) {
                    return None;
                }
            }
            Ok(StartupRequest::Ssl | StartupRequest::GssEncryption) => {
                wire.outbox.refuse_encryption();
                if wire.flush().is_err() {
                    return None;
                }
            }
            Ok(StartupRequest::Cancel {
                process_id,
                secret_key,
            }) => {
                cancel_running_statement(wire, shared, process_id, &secret_key);
                return None;
            }
            Ok(StartupRequest::Startup { major, minor, .. }) if major != 3 => {
                fatal(
                    wire,
                    SqlState::FEATURE_NOT_SUPPORTED,
                    &format!(
                        "unsupported frontend protocol {major}.{minor}: server supports 3.0 to 3.0"
                    ),
                );
                return None;
            }
            Ok(StartupRequest::Startup {
                minor, parameters, ..
            }) => {
                let value_of = |name: &str| {
                    parameters
                        .iter()
                        .find(|(key, _)| key == name)
                        .map(|(_, value)| value.clone())
                };
                if wire.tls().is_none() && !wire.peer_is_loopback() {
                    fatal(
                        wire,
                        SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
                        "EggDB accepts connections from other machines only over TLS: connect with sslmode=require or stronger",
                    );
                    return None;
                }
                let Some(user) = value_of("user").filter(|user| !user.is_empty()) else {
                    fatal(
                        wire,
                        SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
                        "no PostgreSQL user name specified in startup packet",
                    );
                    return None;
                };
                let database = value_of("database")
                    .filter(|database| !database.is_empty())
                    .unwrap_or_else(|| user.clone());
                return Some(Startup {
                    user,
                    database,
                    minor,
                    parameters,
                });
            }
            Err(error) => {
                fatal(
                    wire,
                    SqlState::PROTOCOL_VIOLATION,
                    &format!("invalid startup packet layout: {error}"),
                );
                return None;
            }
        }
    }
    fatal(
        wire,
        SqlState::PROTOCOL_VIOLATION,
        "too many encryption requests before the startup packet",
    );
    None
}

pub(crate) fn refuse_too_many(wire: &mut Wire<'_>, shared: &Shared<'_>) {
    if read_startup(wire, shared).is_some() {
        fatal(
            wire,
            SqlState::TOO_MANY_CONNECTIONS,
            "sorry, too many clients already",
        );
    }
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

enum AuthenticationOutcome {
    Authenticated,
    Refused,
}

fn read_sasl_message(wire: &mut Wire<'_>) -> Option<Vec<u8>> {
    match wire.read_message(crate::connection::MAX_AUTHENTICATION_MESSAGE_BYTES) {
        Ok((b'p', body)) => Some(body),
        Ok((tag, _)) => {
            fatal(
                wire,
                SqlState::PROTOCOL_VIOLATION,
                &format!(
                    "expected SASL response, got message type {}",
                    char::from(tag).escape_default()
                ),
            );
            None
        }
        Err(WireError::TooLarge(_)) => {
            fatal(wire, SqlState::PROTOCOL_VIOLATION, "invalid message length");
            None
        }
        Err(WireError::TimedOut) => {
            fatal(
                wire,
                SqlState::CONNECTION_FAILURE,
                "canceling authentication due to timeout",
            );
            None
        }
        Err(_) => None,
    }
}

fn stored_role_verifier<S: KeyValueStore>(store: &S, user: &str) -> Option<ScramVerifier> {
    let reader = store.begin_read().ok()?;
    let text = eggdb_sql::login_verifier(&reader, user).ok()??;
    ScramVerifier::from_stored(&text)
}

fn refuse_login(wire: &mut Wire<'_>, shared: &Shared<'_>, user: &str, why: &str) -> AuthenticationOutcome {
    (shared.log)(&format!(
        "{}  authentication failed for user \"{}\": {why}",
        wire.peer(),
        printable(user)
    ));
    sleep_unless_stopped(shared.throttle.delay_after_failure(), shared.stop);
    fatal(
        wire,
        SqlState::INVALID_PASSWORD,
        &format!("password authentication failed for user \"{user}\""),
    );
    AuthenticationOutcome::Refused
}

fn may_log_in<S: KeyValueStore>(store: &S, user: &str) -> bool {
    store
        .begin_read()
        .ok()
        .and_then(|reader| eggdb_sql::stored_role(&reader, user).ok().flatten())
        .is_some_and(|role| role.login)
}

fn authenticate_externally<S: KeyValueStore>(
    wire: &mut Wire<'_>,
    shared: &Shared<'_>,
    user: &str,
    method: &eggdb_auth::AuthMethod,
    store: &S,
) -> AuthenticationOutcome {
    if matches!(method, eggdb_auth::AuthMethod::Reject) {
        return refuse_login(wire, shared, user, "the rule file rejects the user");
    }
    if method.sends_secret_in_clear() && wire.tls().is_none() && !wire.is_local() {
        fatal(
            wire,
            SqlState::INVALID_AUTHORIZATION_SPECIFICATION,
            &format!(
                "user \"{user}\" logs in with a password checked elsewhere, which needs an encrypted connection (sslmode=require) or the server's local socket"
            ),
        );
        return AuthenticationOutcome::Refused;
    }
    wire.outbox.authentication_cleartext_password();
    if wire.flush().is_err() {
        return AuthenticationOutcome::Refused;
    }
    let Some(mut body) = read_sasl_message(wire) else {
        return AuthenticationOutcome::Refused;
    };
    if body.last() == Some(&0) {
        body.pop();
    }
    let Ok(secret) = String::from_utf8(body) else {
        return refuse_login(wire, shared, user, "the password is not UTF-8");
    };
    let checked = match method {
        eggdb_auth::AuthMethod::Ldap(config) => match config.authenticate(user, &secret) {
            Ok(true) => Ok(()),
            Ok(false) => Err("the LDAP server refused the password".to_owned()),
            Err(error) => Err(error.to_string()),
        },
        eggdb_auth::AuthMethod::Jwt { keys, policy } => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX));
            match eggdb_auth::jwt::verify(&secret, keys, policy, now) {
                Ok(named) if named == user => Ok(()),
                Ok(named) => Err(format!("the token names user \"{}\"", printable(&named))),
                Err(error) => Err(error.to_string()),
            }
        }
        eggdb_auth::AuthMethod::ScramSha256 | eggdb_auth::AuthMethod::Reject => {
            Err("no external method".to_owned())
        }
    };
    if let Err(why) = checked {
        return refuse_login(wire, shared, user, &why);
    }
    if !may_log_in(store, user) {
        return refuse_login(wire, shared, user, "no stored role with LOGIN has this name");
    }
    wire.outbox.authentication_ok();
    AuthenticationOutcome::Authenticated
}

fn authenticate(
    wire: &mut Wire<'_>,
    shared: &Shared<'_>,
    user: &str,
    stored: Option<ScramVerifier>,
) -> AuthenticationOutcome {
    let config = shared.config;
    let verifier = if user == config.user {
        config.verifier.clone()
    } else if let Some(stored) = stored {
        stored
    } else {
        ScramVerifier::for_unknown_user(&shared.secret, user, config.verifier_iterations())
    };
    let mut nonce = [0u8; SERVER_NONCE_BYTES];
    if eggdb_platform::secure_random::fill(&mut nonce).is_err() {
        fatal(
            wire,
            SqlState::INTERNAL_ERROR,
            "could not generate a random nonce",
        );
        return AuthenticationOutcome::Refused;
    }
    let mut exchange = ScramExchange::new(verifier, &nonce);
    if let Some(binding) = wire.tls().and_then(|info| info.tls_server_end_point()) {
        exchange = exchange.with_channel_binding(binding);
    }
    wire.outbox.authentication_sasl(exchange.mechanisms());
    if wire.flush().is_err() {
        return AuthenticationOutcome::Refused;
    }
    let Some(body) = read_sasl_message(wire) else {
        return AuthenticationOutcome::Refused;
    };
    let initial = match parse_sasl_initial_response(&body) {
        Ok(initial) => initial,
        Err(error) => {
            fatal(wire, SqlState::PROTOCOL_VIOLATION, &error.0);
            return AuthenticationOutcome::Refused;
        }
    };
    if exchange.select_mechanism(&initial.mechanism).is_err() {
        fatal(
            wire,
            SqlState::PROTOCOL_VIOLATION,
            "client selected an invalid SASL authentication mechanism",
        );
        return AuthenticationOutcome::Refused;
    }
    let refuse = |wire: &mut Wire<'_>, failure: ScramFailure| {
        match failure {
            ScramFailure::Malformed(message) => {
                fatal(
                    wire,
                    SqlState::PROTOCOL_VIOLATION,
                    &format!("malformed SCRAM message: {message}"),
                );
            }
            ScramFailure::Unsupported(message) => {
                fatal(wire, SqlState::FEATURE_NOT_SUPPORTED, &message);
            }
            ScramFailure::WrongProof => {
                (shared.log)(&format!(
                    "{}  password authentication failed for user \"{}\"",
                    wire.peer(),
                    printable(user)
                ));
                sleep_unless_stopped(shared.throttle.delay_after_failure(), shared.stop);
                fatal(
                    wire,
                    SqlState::INVALID_PASSWORD,
                    &format!("password authentication failed for user \"{user}\""),
                );
            }
        }
        AuthenticationOutcome::Refused
    };
    let server_first = match exchange.client_first(&initial.data) {
        Ok(server_first) => server_first,
        Err(failure) => return refuse(wire, failure),
    };
    wire.outbox
        .authentication_sasl_continue(server_first.as_bytes());
    if wire.flush().is_err() {
        return AuthenticationOutcome::Refused;
    }
    let Some(body) = read_sasl_message(wire) else {
        return AuthenticationOutcome::Refused;
    };
    match exchange.client_final(&body) {
        Ok(server_final) => {
            wire.outbox
                .authentication_sasl_final(server_final.as_bytes());
            wire.outbox.authentication_ok();
            AuthenticationOutcome::Authenticated
        }
        Err(failure) => refuse(wire, failure),
    }
}

struct PreparedEntry {
    text: String,
    declared_oids: Vec<u32>,
    declared_types: Vec<Option<SqlType>>,
    parameter_count: usize,
}

struct Portal {
    text: String,
    declared_types: Vec<Option<SqlType>>,
    parameters: Vec<Parameter>,
    result_formats: Vec<Format>,
    described: Option<Vec<ColumnDescription>>,
    pending: Option<PendingRows>,
    run: PortalRun,
}

enum PortalRun {
    NotYet,
    Failed,
    Finished(String),
}

struct PendingRows {
    columns: Vec<ColumnDescription>,
    rows: std::vec::IntoIter<Vec<eggdb_sql::Value>>,
    tag: String,
}

fn tag_with_count(tag: &str, count: usize) -> String {
    match tag.rsplit_once(' ') {
        Some((command, last)) if last.bytes().all(|byte| byte.is_ascii_digit()) => {
            format!("{command} {count}")
        }
        _ => tag.to_owned(),
    }
}

pub(crate) struct Connection<'a, 's, S: KeyValueStore + 's> {
    wire: Wire<'a>,
    shared: &'a Shared<'a>,
    session: Session<'s, S>,
    reported: Vec<(&'static str, String)>,
    statements: HashMap<String, PreparedEntry>,
    portals: HashMap<String, Portal>,
    skipping_to_sync: bool,
    lost: Option<Lost>,
    cancel: CancelSignal,
    idle: bool,
    process_id: i32,
    listening: bool,
}

pub(crate) fn serve_connection<'a, 's, S: KeyValueStore + 's>(
    mut wire: Wire<'a>,
    shared: &'a Shared<'a>,
    databases: &[(&str, &'s S)],
    process_id: i32,
) {
    let Some(login_slot) = take_slot(&shared.unauthenticated, MAX_UNAUTHENTICATED_CONNECTIONS)
    else {
        (shared.log)(&format!(
            "{}  connection refused: {MAX_UNAUTHENTICATED_CONNECTIONS} connections are still logging in",
            wire.peer()
        ));
        wire.deadline = Some(Instant::now() + REFUSAL_TIMEOUT);
        refuse_too_many(&mut wire, shared);
        return;
    };
    let Some(startup) = read_startup(&mut wire, shared) else {
        return;
    };
    let chosen = databases
        .iter()
        .find(|(name, _)| *name == startup.database)
        .map(|(_, store)| *store);
    let Some(&(_, first)) = databases.first() else {
        return;
    };
    let own_user = startup.user == shared.config.user;
    let method = match (&shared.config.auth_rules, own_user) {
        (Some(rules), false) => rules.method_for(&startup.user).clone(),
        _ => eggdb_auth::AuthMethod::ScramSha256,
    };
    let is_stored_role = !own_user;
    let outcome = match method {
        eggdb_auth::AuthMethod::ScramSha256 => {
            let stored_role = (!own_user)
                .then(|| stored_role_verifier(chosen.unwrap_or(first), &startup.user))
                .flatten();
            authenticate(&mut wire, shared, &startup.user, stored_role)
        }
        external => authenticate_externally(
            &mut wire,
            shared,
            &startup.user,
            &external,
            chosen.unwrap_or(first),
        ),
    };
    match outcome {
        AuthenticationOutcome::Authenticated => {}
        AuthenticationOutcome::Refused => return,
    }
    drop(login_slot);
    wire.deadline = None;
    let Some(store) = chosen else {
        (shared.log)(&format!(
            "{}  database \"{}\" does not exist",
            wire.peer(),
            printable(&startup.database)
        ));
        let served: Vec<String> = databases.iter().map(|(name, _)| format!("\"{name}\"")).collect();
        fatal(
            &mut wire,
            SqlState::INVALID_CATALOG_NAME,
            &format!(
                "database \"{}\" does not exist; this server serves {}",
                startup.database,
                served.join(", ")
            ),
        );
        return;
    };
    let mut session = Session::new(store);
    session.set_database_names(databases.iter().map(|(name, _)| (*name).to_owned()).collect());
    if is_stored_role {
        if let Err(error) = session.identify_role(&startup.user, &startup.database) {
            fatal(&mut wire, error.state(), error.message());
            return;
        }
    } else {
        session.identify(&startup.user, &startup.database);
    }
    let mut unrecognized_options = Vec::new();
    let mut settings = Vec::new();
    for (name, value) in &startup.parameters {
        match name.as_str() {
            "user" | "database" => {}
            "options" => settings.extend(options_parameters(value)),
            "replication" => {
                if !matches!(value.as_str(), "false" | "0" | "off" | "no") {
                    fatal(
                        &mut wire,
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "replication connections are not supported yet",
                    );
                    return;
                }
            }
            protocol if protocol.starts_with("_pq_.") => {
                unrecognized_options.push(protocol.to_owned());
            }
            _ => settings.push((name.clone(), value.clone())),
        }
    }
    for (name, value) in settings {
        let applied = session.set_parameter(&name, &value);
        if let Err(error) = applied
            && name.eq_ignore_ascii_case(DIALECT_PARAMETER)
        {
            fatal(&mut wire, error.state(), error.message());
            return;
        }
    }
    session.remember_connection_settings();
    if startup.minor > 0 || !unrecognized_options.is_empty() {
        wire.outbox
            .negotiate_protocol_version(0, &unrecognized_options);
    }
    let reported = session.reported_parameters();
    for (name, value) in &reported {
        wire.outbox.parameter_status(name, value);
    }
    let cancel = session.cancel_signal();
    let mut secret_key = [0u8; SECRET_KEY_BYTES];
    let keyed = eggdb_platform::secure_random::fill(&mut secret_key).is_ok();
    wire.outbox
        .backend_key_data(process_id, i32::from_be_bytes(secret_key));
    let _registration =
        keyed.then(|| shared.backend_keys.register(process_id, secret_key, cancel.clone()));
    let mut connection = Connection {
        wire,
        shared,
        session,
        reported,
        statements: HashMap::new(),
        portals: HashMap::new(),
        skipping_to_sync: false,
        lost: None,
        cancel,
        idle: true,
        process_id,
        listening: false,
    };
    connection.ready_for_query();
    if connection.wire.flush().is_ok() {
        connection.run();
    }
    shared.notifications.remove(process_id);
    let _ = connection.session.close();
}

impl<'a, 's, S: KeyValueStore + 's> Connection<'a, 's, S> {
    fn exchange_notifications(&mut self) {
        let channels = self.session.listening_channels();
        self.listening = !channels.is_empty();
        let sent = self.session.take_notifications();
        if !self.listening && sent.is_empty() {
            return;
        }
        let delivered = self
            .shared
            .notifications
            .exchange(self.process_id, channels, sent);
        self.write_notifications(delivered);
    }

    fn write_notifications(&mut self, delivered: Vec<crate::notifications::Delivered>) {
        for notification in delivered {
            self.wire.outbox.notification_response(
                notification.sender,
                &notification.channel,
                &notification.payload,
            );
        }
    }

    fn deliver_while_idle(&mut self) {
        let delivered = self.shared.notifications.waiting(self.process_id);
        if !delivered.is_empty() {
            self.write_notifications(delivered);
            let _ = self.wire.flush();
        }
    }

    fn ready_for_query(&mut self) {
        self.exchange_notifications();
        self.idle = true;
        let status = match self.session.transaction_status() {
            TransactionStatus::Idle => b'I',
            TransactionStatus::InBlock => b'T',
            TransactionStatus::Failed => b'E',
        };
        self.wire.outbox.ready_for_query(status);
    }

    fn run(&mut self) {
        loop {
            if self.idle && self.listening && matches!(self.wire.wait_for_message(), Ok(false)) {
                self.deliver_while_idle();
                continue;
            }
            let (tag, body) = match self.wire.read_message(MAX_MESSAGE_BYTES) {
                Ok(message) => message,
                Err(WireError::Stopping) => {
                    self.shared
                        .ended_by_stop
                        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    fatal(
                        &mut self.wire,
                        SqlState::ADMIN_SHUTDOWN,
                        "terminating connection due to administrator command",
                    );
                    return;
                }
                Err(WireError::TooLarge(length)) => {
                    fatal(
                        &mut self.wire,
                        SqlState::PROTOCOL_VIOLATION,
                        &format!("invalid message length {length}"),
                    );
                    return;
                }
                Err(WireError::Closed | WireError::TimedOut | WireError::Io) => return,
            };
            let message = match parse_message(tag, &body) {
                Ok(message) => message,
                Err(error) => {
                    fatal(&mut self.wire, SqlState::PROTOCOL_VIOLATION, &error.0);
                    return;
                }
            };
            drop(body);
            if self.idle
                && !matches!(
                    message,
                    FrontendMessage::Sync | FrontendMessage::Flush | FrontendMessage::Terminate
                )
            {
                self.idle = false;
                self.cancel.clear();
            }
            if self.skipping_to_sync
                && !matches!(message, FrontendMessage::Sync | FrontendMessage::Terminate)
            {
                continue;
            }
            let keep_going = match message {
                FrontendMessage::Query(text) => self.simple_query(&text),
                FrontendMessage::Parse {
                    name,
                    query,
                    parameter_types,
                } => self.extended(|connection| connection.parse(name, query, parameter_types)),
                FrontendMessage::Bind {
                    portal,
                    statement,
                    parameter_formats,
                    parameters,
                    result_formats,
                } => self.extended(|connection| {
                    connection.bind(
                        portal,
                        &statement,
                        &parameter_formats,
                        parameters,
                        result_formats,
                    )
                }),
                FrontendMessage::Describe(target, name) => {
                    self.extended(|connection| connection.describe(target, &name))
                }
                FrontendMessage::Execute { portal, max_rows } => {
                    self.extended(|connection| connection.execute(&portal, max_rows))
                }
                FrontendMessage::Close(target, name) => {
                    match target {
                        Target::Statement => self.statements.remove(&name).map(|_| ()),
                        Target::Portal => self.portals.remove(&name).map(|_| ()),
                    };
                    self.wire.outbox.close_complete();
                    true
                }
                FrontendMessage::Sync => {
                    let failed = std::mem::take(&mut self.skipping_to_sync);
                    self.finish_implicit_transaction(!failed);
                    self.ready_for_query();
                    self.wire.flush().is_ok()
                }
                FrontendMessage::Flush => self.wire.flush().is_ok(),
                FrontendMessage::Terminate => false,
                FrontendMessage::CopyData
                | FrontendMessage::CopyDone
                | FrontendMessage::CopyFail(_) => true,
                FrontendMessage::Other(b'F') => {
                    report_error(
                        &mut self.wire,
                        Severity::Error,
                        &SqlError::new(
                            SqlState::FEATURE_NOT_SUPPORTED,
                            "the function call protocol is not supported",
                        ),
                        None,
                    );
                    self.finish_implicit_transaction(false);
                    self.ready_for_query();
                    self.wire.flush().is_ok()
                }
                FrontendMessage::AuthenticationResponse(_) | FrontendMessage::Other(_) => {
                    fatal(
                        &mut self.wire,
                        SqlState::PROTOCOL_VIOLATION,
                        &format!(
                            "invalid frontend message type {}",
                            char::from(tag).escape_default()
                        ),
                    );
                    false
                }
            };
            if !keep_going || !self.still_connected() {
                return;
            }
        }
    }

    fn still_connected(&mut self) -> bool {
        match self.lost.take() {
            None => true,
            Some(Lost::Closed) => false,
            Some(Lost::Stopping) => {
                self.shared
                    .ended_by_stop
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                fatal(
                    &mut self.wire,
                    SqlState::ADMIN_SHUTDOWN,
                    "terminating connection due to administrator command",
                );
                false
            }
            Some(Lost::TooLarge(length)) => {
                fatal(
                    &mut self.wire,
                    SqlState::PROTOCOL_VIOLATION,
                    &format!("invalid message length {length}"),
                );
                false
            }
        }
    }

    fn finish_implicit_transaction(&mut self, succeeded: bool) {
        let ended = if succeeded {
            self.session.commit_implicit_transaction()
        } else {
            self.session.roll_back_implicit_transaction()
        };
        if let Err(error) = ended {
            report_error(&mut self.wire, Severity::Error, &error, None);
        }
        if self.session.transaction_status() == TransactionStatus::Idle {
            self.portals.clear();
        }
    }

    fn discard_all(&mut self) {
        self.statements.clear();
        self.portals.clear();
    }

    fn extended(
        &mut self,
        step: impl FnOnce(&mut Self) -> Result<(), (SqlError, Option<String>)>,
    ) -> bool {
        self.session.start_implicit_transaction();
        if let Err((error, text)) = step(self) {
            if self.lost.is_none() {
                report_error(&mut self.wire, Severity::Error, &error, text.as_deref());
            }
            self.skipping_to_sync = true;
        }
        self.wire.flush_if_large().is_ok()
    }

    fn report_changed_parameters(&mut self) {
        let now = self.session.reported_parameters();
        for (name, value) in &now {
            let before = self
                .reported
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value);
            if before != Some(value) {
                self.wire.outbox.parameter_status(name, value);
            }
        }
        self.reported = now;
    }

    fn log_if_slow(&self, started: Instant, text: &str) {
        let Some(threshold) = self.session.slow_statement_threshold() else {
            return;
        };
        let took = started.elapsed();
        if took < threshold {
            return;
        }
        let shown: String = text.chars().take(1024).collect();
        (self.shared.log)(&format!(
            "{}  duration: {:.3} ms  statement: {}",
            self.wire.peer(),
            took.as_secs_f64() * 1000.0,
            printable(&shown)
        ));
    }

    fn simple_query(&mut self, text: &str) -> bool {
        let started = Instant::now();
        let keep_going = self.simple_query_timed(text);
        self.log_if_slow(started, text);
        keep_going
    }

    fn simple_query_timed(&mut self, text: &str) -> bool {
        self.session.start_implicit_transaction();
        let mut results = 0usize;
        let mut sent = true;
        let mut unsendable = None;
        let mut discarded_all = false;
        let wire = RefCell::new(&mut self.wire);
        let mut copy = WireCopy {
            wire: &wire,
            lost: None,
        };
        let outcome = self.session.execute_with_copy(
            text,
            &mut |result| {
                results += 1;
                discarded_all |= result.tag == DISCARD_ALL_TAG;
                let mut wire = wire.borrow_mut();
                for notice in &result.notices {
                    send_notice(&mut wire, notice);
                }
                if let Some(columns) = &result.columns {
                    let description = describe_columns(columns, &result.rows, &[]);
                    if let Err(error) = wire.outbox.row_description(&description) {
                        unsendable = Some(too_large(error));
                        return false;
                    }
                    for row in &result.rows {
                        let values: Vec<Option<Vec<u8>>> = row
                            .iter()
                            .map(|value| value.to_text().map(String::into_bytes))
                            .collect();
                        if let Err(error) = wire.outbox.data_row(&values) {
                            unsendable = Some(too_large(error));
                            return false;
                        }
                        if wire.flush_if_large().is_err() {
                            sent = false;
                            return false;
                        }
                    }
                }
                wire.outbox.command_complete(&result.tag);
                sent = wire.flush_if_large().is_ok();
                sent
            },
            &mut copy,
        );
        self.lost = copy.lost;
        if !sent || self.lost == Some(Lost::Closed) {
            return false;
        }
        let outcome = match unsendable {
            Some(error) => Err(error),
            None => outcome,
        };
        let succeeded = outcome.is_ok();
        match outcome {
            Ok(()) if results == 0 => self.wire.outbox.empty_query_response(),
            Ok(()) => {}
            Err(_) if self.lost.is_some() => return true,
            Err(error) => report_error(&mut self.wire, Severity::Error, &error, Some(text)),
        }
        if discarded_all {
            self.discard_all();
        }
        self.finish_implicit_transaction(succeeded);
        self.report_changed_parameters();
        self.ready_for_query();
        self.wire.flush().is_ok()
    }

    fn refuse_in_failed_block(&self, text: &str) -> Result<(), (SqlError, Option<String>)> {
        if self.session.transaction_status() == TransactionStatus::Failed
            && !is_transaction_exit(text)
        {
            return Err((aborted(), None));
        }
        Ok(())
    }

    fn parse(
        &mut self,
        name: String,
        query: String,
        parameter_oids: Vec<u32>,
    ) -> Result<(), (SqlError, Option<String>)> {
        if !name.is_empty() && self.statements.contains_key(&name) {
            return Err((
                SqlError::new(
                    SqlState::DUPLICATE_PREPARED_STATEMENT,
                    format!("prepared statement \"{name}\" already exists"),
                ),
                None,
            ));
        }
        self.refuse_in_failed_block(&query)?;
        let declared_types: Vec<Option<SqlType>> =
            parameter_oids.iter().map(|&oid| sql_type_of(oid)).collect();
        let prepared = self
            .session
            .prepare(&query, &declared_types)
            .map_err(|error| (error, Some(query.clone())))?;
        self.statements.insert(
            name,
            PreparedEntry {
                text: query,
                declared_oids: parameter_oids,
                declared_types,
                parameter_count: prepared.parameter_count,
            },
        );
        self.wire.outbox.parse_complete();
        Ok(())
    }

    fn bind(
        &mut self,
        portal_name: String,
        statement_name: &str,
        parameter_formats: &[Format],
        values: Vec<Option<Vec<u8>>>,
        result_formats: Vec<Format>,
    ) -> Result<(), (SqlError, Option<String>)> {
        let Some(statement) = self.statements.get(statement_name) else {
            return Err((
                SqlError::new(
                    SqlState::INVALID_SQL_STATEMENT_NAME,
                    format!("prepared statement \"{statement_name}\" does not exist"),
                ),
                None,
            ));
        };
        if !portal_name.is_empty() && self.portals.contains_key(&portal_name) {
            return Err((
                SqlError::new(
                    SqlState::DUPLICATE_CURSOR,
                    format!("portal \"{portal_name}\" already exists"),
                ),
                None,
            ));
        }
        if parameter_formats.len() > 1 && parameter_formats.len() != values.len() {
            return Err((
                protocol_violation(format!(
                    "bind message has {} parameter formats but {} parameters",
                    parameter_formats.len(),
                    values.len()
                )),
                None,
            ));
        }
        if values.len() != statement.parameter_count {
            return Err((
                protocol_violation(format!(
                    "bind message supplies {} parameters, but prepared statement \"{statement_name}\" requires {}",
                    values.len(),
                    statement.parameter_count
                )),
                None,
            ));
        }
        self.refuse_in_failed_block(&statement.text)?;
        let mut parameters = Vec::with_capacity(values.len());
        for (index, value) in values.into_iter().enumerate() {
            let oid = statement.declared_oids.get(index).copied().unwrap_or(0);
            let value = value
                .map(|bytes| {
                    decode_parameter(&bytes, oid, format_at(parameter_formats, index), index + 1)
                })
                .transpose()
                .map_err(|error| (error, None))?;
            parameters.push(Parameter {
                declared_type: statement.declared_types.get(index).copied().flatten(),
                value,
            });
        }
        let portal = Portal {
            text: statement.text.clone(),
            declared_types: statement.declared_types.clone(),
            parameters,
            result_formats,
            described: None,
            pending: None,
            run: PortalRun::NotYet,
        };
        self.portals.insert(portal_name, portal);
        self.wire.outbox.bind_complete();
        Ok(())
    }

    fn describe(&mut self, target: Target, name: &str) -> Result<(), (SqlError, Option<String>)> {
        match target {
            Target::Statement => {
                let Some(statement) = self.statements.get(name) else {
                    return Err((
                        SqlError::new(
                            SqlState::INVALID_SQL_STATEMENT_NAME,
                            format!("prepared statement \"{name}\" does not exist"),
                        ),
                        None,
                    ));
                };
                let text = statement.text.clone();
                let declared_oids = statement.declared_oids.clone();
                let declared_types = statement.declared_types.clone();
                let description = self
                    .session
                    .describe(&text, &declared_types)
                    .map_err(|error| (error, Some(text.clone())))?;
                let oids: Vec<u32> = description
                    .parameter_types
                    .iter()
                    .enumerate()
                    .map(|(index, sql_type)| {
                        match declared_oids
                            .get(index)
                            .copied()
                            .filter(|&oid| oid != 0 && oid != UNKNOWN_OID)
                        {
                            Some(oid) => oid,
                            None => sql_type.map_or(TEXT_OID, oid_of),
                        }
                    })
                    .collect();
                self.wire
                    .outbox
                    .parameter_description(&oids)
                    .map_err(|error| (too_large(error), None))?;
                self.send_row_description(description.columns.as_deref(), &[])?;
            }
            Target::Portal => {
                let Some(portal) = self.portals.get(name) else {
                    return Err((
                        SqlError::new(
                            SqlState::INVALID_CURSOR_NAME,
                            format!("portal \"{name}\" does not exist"),
                        ),
                        None,
                    ));
                };
                if let Some(pending) = &portal.pending {
                    let columns = pending.columns.clone();
                    self.wire
                        .outbox
                        .row_description(&columns)
                        .map_err(|error| (too_large(error), None))?;
                    return Ok(());
                }
                let text = portal.text.clone();
                let declared_types = portal.declared_types.clone();
                let formats = portal.result_formats.clone();
                let description = self
                    .session
                    .describe(&text, &declared_types)
                    .map_err(|error| (error, Some(text.clone())))?;
                let described =
                    self.send_row_description(description.columns.as_deref(), &formats)?;
                if let Some(portal) = self.portals.get_mut(name) {
                    portal.described = described;
                }
            }
        }
        Ok(())
    }

    fn send_row_description(
        &mut self,
        columns: Option<&[OutputColumn]>,
        formats: &[Format],
    ) -> Result<Option<Vec<ColumnDescription>>, (SqlError, Option<String>)> {
        match columns {
            Some(columns) => {
                let description = describe_columns(columns, &[], formats);
                self.wire
                    .outbox
                    .row_description(&description)
                    .map_err(|error| (too_large(error), None))?;
                Ok(Some(description))
            }
            None => {
                self.wire.outbox.no_data();
                Ok(None)
            }
        }
    }

    fn execute(&mut self, name: &str, max_rows: i32) -> Result<(), (SqlError, Option<String>)> {
        let Some(portal) = self.portals.get_mut(name) else {
            return Err((
                SqlError::new(
                    SqlState::INVALID_CURSOR_NAME,
                    format!("portal \"{name}\" does not exist"),
                ),
                None,
            ));
        };
        let limit = usize::try_from(max_rows)
            .ok()
            .filter(|&limit| limit > 0)
            .unwrap_or(usize::MAX);
        if let Some(pending) = portal.pending.take() {
            return self.send_rows(name, pending, limit);
        }
        match &portal.run {
            PortalRun::NotYet => {}
            PortalRun::Finished(tag) => {
                let tag = tag_with_count(tag, 0);
                self.wire.outbox.command_complete(&tag);
                return Ok(());
            }
            PortalRun::Failed => {
                return Err((
                    SqlError::new(
                        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                        format!("portal \"{name}\" cannot be run"),
                    ),
                    None,
                ));
            }
        }
        portal.run = PortalRun::Failed;
        let text = portal.text.clone();
        let parameters = portal.parameters.clone();
        let declared_types = portal.declared_types.clone();
        let formats = portal.result_formats.clone();
        let described = portal.described.clone();
        if self.session.transaction_status() == TransactionStatus::Failed
            && !is_transaction_exit(&text)
        {
            return Err((aborted(), None));
        }
        let described = match described {
            Some(described) => Some(described),
            None => self
                .session
                .describe(&text, &declared_types)
                .map_err(|error| (error, Some(text.clone())))?
                .columns
                .map(|columns| describe_columns(&columns, &[], &formats)),
        };
        let mut results: Vec<QueryResult> = Vec::new();
        let wire = RefCell::new(&mut self.wire);
        let mut copy = WireCopy {
            wire: &wire,
            lost: None,
        };
        let outcome = self.session.execute_prepared_with_copy(
            &text,
            &parameters,
            &mut |result| {
                results.push(result);
                true
            },
            &mut copy,
        );
        self.lost = copy.lost;
        outcome.map_err(|error| (error, Some(text.clone())))?;
        self.report_changed_parameters();
        let Some(result) = results.pop() else {
            if let Some(portal) = self.portals.get_mut(name) {
                portal.run = PortalRun::NotYet;
            }
            self.wire.outbox.empty_query_response();
            return Ok(());
        };
        if result.tag == DISCARD_ALL_TAG {
            self.discard_all();
        }
        if let Some(portal) = self.portals.get_mut(name) {
            portal.run = PortalRun::Finished(result.tag.clone());
        }
        for notice in &result.notices {
            send_notice(&mut self.wire, notice);
        }
        match result.columns {
            Some(columns) => {
                let description = described
                    .filter(|described| described.len() == columns.len())
                    .unwrap_or_else(|| describe_columns(&columns, &[], &formats));
                let pending = PendingRows {
                    columns: description,
                    rows: result.rows.into_iter(),
                    tag: result.tag,
                };
                self.send_rows(name, pending, limit)
            }
            None => {
                self.wire.outbox.command_complete(&result.tag);
                Ok(())
            }
        }
    }

    fn send_rows(
        &mut self,
        name: &str,
        mut pending: PendingRows,
        limit: usize,
    ) -> Result<(), (SqlError, Option<String>)> {
        let mut sent = 0usize;
        let zone = self.session.time_zone();
        while sent < limit {
            let Some(row) = pending.rows.next() else {
                break;
            };
            let mut values = Vec::with_capacity(row.len());
            for (value, column) in row.iter().zip(&pending.columns) {
                values.push(
                    encode_value(value, column.type_oid, column.format, zone)
                        .map_err(|error| (error, None))?,
                );
            }
            self.wire
                .outbox
                .data_row(&values)
                .map_err(|error| (too_large(error), None))?;
            sent += 1;
            if self.wire.flush_if_large().is_err() {
                return Ok(());
            }
        }
        if pending.rows.len() > 0 {
            if let Some(portal) = self.portals.get_mut(name) {
                portal.pending = Some(pending);
            }
            self.wire.outbox.portal_suspended();
        } else {
            self.wire
                .outbox
                .command_complete(&tag_with_count(&pending.tag, sent));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{character_position, is_transaction_exit, options_parameters};

    #[test]
    fn client_values_are_escaped_onto_one_line() {
        assert_eq!(super::printable("ada"), "ada");
        assert_eq!(
            super::printable("a\nb\r\"c\\\u{1b}[31m\u{85}"),
            "a\\nb\\r\\\"c\\\\\\u{1b}[31m\\u{85}"
        );
        assert_eq!(
            super::printable("x\u{2028}y\u{2029}z\u{202e}"),
            "x\\u{2028}y\\u{2029}z\\u{202e}"
        );
    }

    #[test]
    fn positions_count_characters_from_one() {
        assert_eq!(character_position("SELECT x", 7), 8);
        assert_eq!(character_position("SELECT 'é', x", 13), 13);
        assert_eq!(character_position("é", 1), 1);
        assert_eq!(character_position("abc", 99), 4);
    }

    #[test]
    fn only_commit_and_rollback_leave_a_failed_block() {
        for text in ["COMMIT", "  rollback;", "END", "abort work"] {
            assert!(is_transaction_exit(text), "{text}");
        }
        for text in ["SELECT 1", "BEGIN", "", "commitx"] {
            assert!(!is_transaction_exit(text), "{text}");
        }
    }

    #[test]
    fn suspended_portals_count_the_rows_of_their_last_execute() {
        assert_eq!(super::tag_with_count("SELECT 10", 3), "SELECT 3");
        assert_eq!(super::tag_with_count("INSERT 0 10", 3), "INSERT 0 3");
        assert_eq!(super::tag_with_count("SHOW", 1), "SHOW");
    }

    #[test]
    fn startup_options_carry_settings() {
        assert_eq!(
            options_parameters("-c search_path=app --statement-timeout=5 -cDateStyle=ISO -x y"),
            [
                ("search_path".to_owned(), "app".to_owned()),
                ("statement_timeout".to_owned(), "5".to_owned()),
                ("DateStyle".to_owned(), "ISO".to_owned()),
            ]
        );
        assert_eq!(
            options_parameters("-c application_name=a\\ b"),
            [("application_name".to_owned(), "a b".to_owned())]
        );
    }
}
