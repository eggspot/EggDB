//! The audit file of `eggdb mcp`: one JSON object per line in `<database>.mcp-audit.jsonl`
//! next to the database, for every call an agent made, written before the call's result is
//! sent. [`AuditLog`] appends to it; [`read_audit`] reads it back for `eggdb audit`.

use crate::json_out::JsonOut;
use eggdb_platform::Clock;
use eggdb_sql::json::{Json, Node, parse_document};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What `eggdb mcp` adds to a database's file name for its audit file.
pub const AUDIT_FILE_SUFFIX: &str = ".mcp-audit.jsonl";

/// The size past which the audit file takes no more records, and `eggdb mcp` refuses calls
/// until it is archived or removed: 1 GiB.
pub const MAX_AUDIT_BYTES: u64 = 1 << 30;

const LOCK_WAIT: Duration = Duration::from_secs(1);
const LOCK_POLL: Duration = Duration::from_millis(10);

/// The audit file of the database at `database`: its path with [`AUDIT_FILE_SUFFIX`] added.
pub fn audit_path_for(database: &Path) -> PathBuf {
    let mut path = OsString::from(database.as_os_str());
    path.push(AUDIT_FILE_SUFFIX);
    PathBuf::from(path)
}

/// Why a record could not be added to the audit file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFailure {
    /// What went wrong, such as `permission denied`.
    pub reason: String,
}

impl std::fmt::Display for AuditFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

/// An audit file that records are appended to. Each record opens the file anew (refusing a
/// symbolic link, creating it readable by its owner only), takes its exclusive lock, writes
/// one whole line and syncs it to the disk, so servers sharing the file never interleave and
/// the file can be moved aside between records.
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// The audit log kept at `path`.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Where the audit file is.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `record`, one JSON object without a line break, and syncs it to the disk.
    pub fn append(&self, record: &str) -> Result<(), AuditFailure> {
        let failure = |error: io::Error| AuditFailure {
            reason: describe(&error),
        };
        let mut file = eggdb_platform::user_files::open_private_append_no_follow(&self.path)
            .map_err(failure)?;
        lock(&file).map_err(failure)?;
        let size = file.metadata().map_err(failure)?.len();
        if size.saturating_add(record.len() as u64 + 1) > MAX_AUDIT_BYTES {
            return Err(AuditFailure {
                reason: "the audit file has reached its 1 GiB limit".to_owned(),
            });
        }
        let mut line = Vec::with_capacity(record.len() + 1);
        line.extend_from_slice(record.as_bytes());
        line.push(b'\n');
        file.write_all(&line).map_err(failure)?;
        file.sync_data().map_err(failure)
    }
}

fn lock(file: &File) -> io::Result<()> {
    let give_up = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < give_up => {
                std::thread::sleep(LOCK_POLL);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another eggdb mcp held the audit file's lock for over a second",
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

fn describe(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        io::ErrorKind::NotFound => "its directory does not exist".to_owned(),
        _ => error.to_string(),
    }
}

pub(crate) fn now_rfc3339() -> String {
    let nanos = eggdb_platform::SystemClock::new().unix_time_nanos();
    rfc3339(nanos / 1000)
}

pub(crate) fn rfc3339(unix_micros: u64) -> String {
    let seconds = unix_micros / 1_000_000;
    let micros = unix_micros % 1_000_000;
    let (year, month, day) = civil_from_days(seconds / 86_400);
    let of_day = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{micros:06}Z",
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60
    )
}

fn civil_from_days(days_since_epoch: u64) -> (u64, u64, u64) {
    let shifted = days_since_epoch + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

pub(crate) struct ServerStarted<'a> {
    pub(crate) version: &'a str,
    pub(crate) database: &'a str,
    pub(crate) time_limit: Duration,
}

pub(crate) fn server_started_record(started: &ServerStarted<'_>) -> String {
    let mut out = JsonOut::new();
    out.begin_object()
        .key("event")
        .string("server_started")
        .key("time")
        .string(&now_rfc3339())
        .key("version")
        .string(started.version)
        .key("pid")
        .unsigned(u64::from(std::process::id()))
        .key("database")
        .string(started.database)
        .key("read_only")
        .boolean(true)
        .key("time_limit_ms")
        .unsigned(duration_ms(started.time_limit))
        .end_object();
    out.finish()
}

pub(crate) fn time_limit_exceeded_record(tool: &str, time_limit: Duration) -> String {
    let mut out = JsonOut::new();
    out.begin_object()
        .key("event")
        .string("time_limit_exceeded")
        .key("time")
        .string(&now_rfc3339())
        .key("tool")
        .string(tool)
        .key("time_limit_ms")
        .unsigned(duration_ms(time_limit))
        .key("pid")
        .unsigned(u64::from(std::process::id()))
        .end_object();
    out.finish()
}

pub(crate) fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(crate) const MAX_ARGUMENT_BYTES: usize = 64 * 1024;

pub(crate) struct CallRecord<'a> {
    pub(crate) started: &'a str,
    pub(crate) client_name: Option<&'a str>,
    pub(crate) client_version: Option<&'a str>,
    pub(crate) protocol: Option<&'a str>,
    pub(crate) tool: &'a str,
    pub(crate) arguments: &'a str,
    pub(crate) error_code: Option<&'a str>,
    pub(crate) sqlstate: Option<&'a str>,
    pub(crate) result_bytes: u64,
    pub(crate) rows: Option<u64>,
    pub(crate) truncated: bool,
    pub(crate) duration: Duration,
}

pub(crate) fn call_record(call: &CallRecord<'_>) -> String {
    let mut out = JsonOut::new();
    out.begin_object()
        .key("event")
        .string("call")
        .key("time")
        .string(call.started);
    if call.client_name.is_some() || call.client_version.is_some() {
        out.key("client")
            .begin_object()
            .optional_string("name", call.client_name)
            .optional_string("version", call.client_version)
            .end_object();
    }
    out.optional_string("protocol", call.protocol)
        .key("tool")
        .string(call.tool);
    if call.arguments.len() <= MAX_ARGUMENT_BYTES {
        out.key("arguments").encoded(call.arguments);
    } else {
        let mut cut = MAX_ARGUMENT_BYTES;
        while !call.arguments.is_char_boundary(cut) {
            cut -= 1;
        }
        out.key("arguments_cut")
            .string(&call.arguments[..cut])
            .key("arguments_bytes")
            .unsigned(call.arguments.len() as u64);
    }
    out.key("outcome")
        .string(if call.error_code.is_some() {
            "error"
        } else {
            "ok"
        })
        .optional_string("error_code", call.error_code)
        .optional_string("sqlstate", call.sqlstate)
        .key("result_bytes")
        .unsigned(call.result_bytes);
    if let Some(rows) = call.rows {
        out.key("rows").unsigned(rows);
    }
    out.key("truncated")
        .boolean(call.truncated)
        .key("duration_ms")
        .unsigned(duration_ms(call.duration))
        .end_object();
    out.finish()
}

/// What kind of record a line of the audit file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditEvent {
    /// `eggdb mcp` started.
    ServerStarted,
    /// An agent's call.
    Call,
    /// A call ran past its time limit and the server exited.
    TimeLimitExceeded,
}

/// One record of the audit file, with the fields `eggdb audit` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// What kind of record it is.
    pub event: AuditEvent,
    /// When it was written, in RFC 3339 UTC with microseconds.
    pub time: String,
    /// The client's name and version, as the client stated them.
    pub client: Option<String>,
    /// The tool called, or `resources/read`.
    pub tool: Option<String>,
    /// The call's arguments for reading: a SQL statement as sent, a resource's URI, or the
    /// arguments as compact JSON.
    pub arguments: Option<String>,
    /// The error code, when the call failed.
    pub error_code: Option<String>,
    /// The SQLSTATE of a failed statement.
    pub sqlstate: Option<String>,
    /// How many rows or entries came back.
    pub rows: Option<u64>,
    /// Whether the result was cut.
    pub truncated: bool,
    /// How long the call took.
    pub duration_ms: Option<u64>,
    /// The server's version, for a server start.
    pub version: Option<String>,
    /// The server's process id.
    pub pid: Option<u64>,
    /// The time limit per call.
    pub time_limit_ms: Option<u64>,
}

/// One line of the audit file as [`read_audit`] found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditLine {
    /// A record: its line number from 1, the line as stored, and its fields.
    Record {
        /// The line number, from 1.
        number: u64,
        /// The line as stored.
        raw: String,
        /// The record's fields.
        entry: Box<AuditEntry>,
    },
    /// A line that is not a record of the audit file.
    Unreadable {
        /// The line number, from 1.
        number: u64,
    },
}

/// The audit file as [`read_audit`] read it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditListing {
    /// The lines kept: all of them, or the last ones when a limit was given.
    pub lines: Vec<AuditLine>,
    /// Every call in the file.
    pub calls: u64,
    /// Every call that failed.
    pub errors: u64,
    /// Every line that is not a record.
    pub unreadable: u64,
    /// The client names, in the order they first appear.
    pub clients: Vec<String>,
}

const SHARING_VIOLATION: i32 = 32;
const LOCK_VIOLATION: i32 = 33;

/// Reads the audit file at `path`, keeping only its last `limit` lines when a limit is
/// given; `None` when there is no audit file. On Windows, where a server's lock blocks reading
/// for the moment it writes, it retries for up to a second.
pub fn read_audit(path: &Path, limit: Option<usize>) -> io::Result<Option<AuditListing>> {
    let give_up = Instant::now() + LOCK_WAIT;
    loop {
        match read_audit_once(path, limit) {
            Err(error)
                if cfg!(windows)
                    && matches!(
                        error.raw_os_error(),
                        Some(SHARING_VIOLATION | LOCK_VIOLATION)
                    )
                    && Instant::now() < give_up =>
            {
                std::thread::sleep(LOCK_POLL);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            other => return other.map(Some),
        }
    }
}

fn read_audit_once(path: &Path, limit: Option<usize>) -> io::Result<AuditListing> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut listing = AuditListing::default();
    let mut kept = VecDeque::new();
    let mut bytes = Vec::new();
    let mut number = 0;
    loop {
        bytes.clear();
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            break;
        }
        number += 1;
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        let line = match std::str::from_utf8(&bytes).ok().and_then(entry_of) {
            Some(entry) => {
                if entry.event == AuditEvent::Call {
                    listing.calls += 1;
                    if entry.error_code.is_some() {
                        listing.errors += 1;
                    }
                    if let Some(client) = &entry.client
                        && !listing.clients.contains(client)
                    {
                        listing.clients.push(client.clone());
                    }
                }
                AuditLine::Record {
                    number,
                    raw: String::from_utf8_lossy(&bytes).into_owned(),
                    entry: Box::new(entry),
                }
            }
            None => {
                listing.unreadable += 1;
                AuditLine::Unreadable { number }
            }
        };
        kept.push_back(line);
        if limit.is_some_and(|limit| kept.len() > limit) {
            kept.pop_front();
        }
    }
    listing.lines = kept.into();
    Ok(listing)
}

fn member<'n>(members: &'n [(String, Node)], name: &str) -> Option<&'n Json> {
    members
        .iter()
        .find(|(member, _)| member == name)
        .map(|(_, node)| &node.value)
}

fn text_of(members: &[(String, Node)], name: &str) -> Option<String> {
    match member(members, name) {
        Some(Json::String(text)) => Some(text.clone()),
        _ => None,
    }
}

fn number_of(members: &[(String, Node)], name: &str) -> Option<u64> {
    match member(members, name) {
        Some(Json::Number(text)) => text.parse().ok(),
        _ => None,
    }
}

fn entry_of(line: &str) -> Option<AuditEntry> {
    let Json::Object(members) = parse_document(line).ok()?.value else {
        return None;
    };
    let event = match text_of(&members, "event")?.as_str() {
        "server_started" => AuditEvent::ServerStarted,
        "call" => AuditEvent::Call,
        "time_limit_exceeded" => AuditEvent::TimeLimitExceeded,
        _ => return None,
    };
    let client = match member(&members, "client") {
        Some(Json::Object(client)) => {
            let name = text_of(client, "name");
            let version = text_of(client, "version");
            match (name, version) {
                (Some(name), Some(version)) => Some(format!("{name} {version}")),
                (Some(name), None) => Some(name),
                (None, Some(version)) => Some(version),
                (None, None) => None,
            }
        }
        _ => None,
    };
    let tool = text_of(&members, "tool");
    Some(AuditEntry {
        event,
        time: text_of(&members, "time")?,
        client,
        arguments: arguments_shown(&members, tool.as_deref()),
        tool,
        error_code: text_of(&members, "error_code"),
        sqlstate: text_of(&members, "sqlstate"),
        rows: number_of(&members, "rows"),
        truncated: matches!(member(&members, "truncated"), Some(Json::Boolean(true))),
        duration_ms: number_of(&members, "duration_ms"),
        version: text_of(&members, "version"),
        pid: number_of(&members, "pid"),
        time_limit_ms: number_of(&members, "time_limit_ms"),
    })
}

fn arguments_shown(members: &[(String, Node)], tool: Option<&str>) -> Option<String> {
    if let Some(cut) = text_of(members, "arguments_cut") {
        return Some(cut);
    }
    let arguments = members
        .iter()
        .find(|(name, _)| name == "arguments")
        .map(|(_, node)| node)?;
    if let Json::Object(fields) = &arguments.value {
        let shown_field = match tool {
            Some("run_sql" | "explain_query") => Some("sql"),
            Some("resources/read") => Some("uri"),
            _ => None,
        };
        if let Some(text) = shown_field.and_then(|field| text_of(fields, field)) {
            return Some(text);
        }
    }
    let mut out = JsonOut::new();
    out.node(arguments);
    Some(out.finish())
}

#[cfg(test)]
mod tests {
    use super::{
        AuditEvent, AuditLine, AuditLog, CallRecord, audit_path_for, call_record, read_audit,
        rfc3339,
    };
    use std::path::Path;
    use std::time::Duration;

    fn temporary(name: &str) -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("eggdb-mcp-audit-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("app.db.mcp-audit.jsonl");
        let _ = std::fs::remove_file(&path);
        path
    }

    fn call(tool: &str, arguments: &str, error_code: Option<&str>) -> String {
        call_record(&CallRecord {
            started: "2026-10-28T09:14:07.123456Z",
            client_name: Some("claude-code"),
            client_version: Some("2.1.0"),
            protocol: Some("2025-11-25"),
            tool,
            arguments,
            error_code,
            sqlstate: None,
            result_bytes: 10,
            rows: Some(2),
            truncated: false,
            duration: Duration::from_millis(12),
        })
    }

    #[test]
    fn the_audit_file_sits_next_to_the_database() {
        assert_eq!(
            audit_path_for(Path::new("dir/app.db")),
            Path::new("dir/app.db.mcp-audit.jsonl")
        );
    }

    #[test]
    fn times_are_rfc_3339_in_utc_with_microseconds() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(
            rfc3339(1_793_178_847_123_456),
            "2026-10-28T09:14:07.123456Z"
        );
    }

    #[test]
    fn records_append_as_whole_lines_and_read_back_with_counts_and_a_limit() {
        let path = temporary("append");
        let log = AuditLog::new(path.clone());
        log.append(&call("run_sql", r#"{"sql":"SELECT 1\nFROM t"}"#, None))
            .unwrap();
        log.append(&call("list_tables", "{}", Some("database_busy")))
            .unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, b"not json\n"))
            .unwrap();
        log.append(&call("kv_get", r#"{"key":"a"}"#, None)).unwrap();
        let listing = read_audit(&path, None).unwrap().unwrap();
        assert_eq!(
            (listing.calls, listing.errors, listing.unreadable),
            (3, 1, 1)
        );
        assert_eq!(listing.clients, ["claude-code 2.1.0"]);
        let AuditLine::Record { entry, .. } = &listing.lines[0] else {
            panic!("{:?}", listing.lines[0]);
        };
        assert_eq!(entry.event, AuditEvent::Call);
        assert_eq!(entry.arguments.as_deref(), Some("SELECT 1\nFROM t"));
        assert_eq!(entry.rows, Some(2));
        assert_eq!(listing.lines[2], AuditLine::Unreadable { number: 3 });
        let last = read_audit(&path, Some(1)).unwrap().unwrap();
        assert_eq!(last.lines.len(), 1);
        assert_eq!(last.calls, 3);
        assert!(
            read_audit(&path.with_extension("missing"), None)
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn long_arguments_are_cut_and_their_size_kept() {
        let long = format!("{{\"sql\":\"{}\"}}", "x".repeat(70_000));
        let record = call("run_sql", &long, None);
        assert!(record.contains("\"arguments_cut\""));
        assert!(record.contains(&format!("\"arguments_bytes\":{}", long.len())));
        assert!(record.len() < 70_000);
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symbolic_link_is_refused_and_its_target_untouched() {
        let path = temporary("link");
        let target = path.with_file_name("target");
        std::fs::write(&target, b"kept").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(AuditLog::new(path.clone()).append("{}").is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"kept");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&target);
    }

    #[test]
    fn a_file_moved_aside_between_records_is_started_again() {
        let path = temporary("rotate");
        let log = AuditLog::new(path.clone());
        log.append(&call("list_tables", "{}", None)).unwrap();
        let archived = path.with_file_name("archived.jsonl");
        std::fs::rename(&path, &archived).unwrap();
        log.append(&call("list_tables", "{}", None)).unwrap();
        assert_eq!(read_audit(&path, None).unwrap().unwrap().calls, 1);
        assert_eq!(read_audit(&archived, None).unwrap().unwrap().calls, 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&archived);
    }
}
