//! The audit file of `eggdb mcp`: one JSON object per line in `<database>.mcp-audit.jsonl`
//! next to the database, for every call an agent made, written before the call's result is
//! sent. [`AuditLog`] appends to it; [`read_audit`] reads it back for `eggdb audit`.

use crate::json_out::JsonOut;
use eggdb_platform::Clock;
use eggdb_sql::json::{Json, Node, parse_document};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// What `eggdb mcp` adds to a database's file name for its audit file.
pub const AUDIT_FILE_SUFFIX: &str = ".mcp-audit.jsonl";

/// The size past which the audit file takes no more records, and `eggdb mcp` refuses calls
/// until it is archived or removed: 1 GiB.
pub const MAX_AUDIT_BYTES: u64 = 1 << 30;

const LOCK_WAIT: Duration = Duration::from_secs(5);
const PROMPT_LOCK_WAIT: Duration = Duration::from_millis(250);
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

    /// Appends `record`, one JSON object without a line break, and syncs it to the disk,
    /// waiting up to five seconds for another server's lock on the file.
    pub fn append(&self, record: &str) -> Result<(), AuditFailure> {
        self.append_waiting(record, LOCK_WAIT).map(drop)
    }

    /// Appends `record` as [`AuditLog::append`] does and returns the SHA-256 (hex) of the line
    /// written, the chain's newest digest: kept outside the file, it shows later whether
    /// records were cut off the end ([`verify_chain_to_anchor`]).
    pub fn append_with_digest(&self, record: &str) -> Result<String, AuditFailure> {
        self.append_waiting(record, LOCK_WAIT)
    }

    pub(crate) fn append_promptly(&self, record: &str) -> Result<(), AuditFailure> {
        self.append_waiting(record, PROMPT_LOCK_WAIT).map(drop)
    }

    /// Appends a `chain_signed` record: the Ed25519 signature, by the private key `seed`, of
    /// the chain's newest digest, so a file rewritten whole by someone without the key no
    /// longer verifies against the key's public half ([`verify_chain_signed`]). Returns the
    /// digest it signed.
    pub fn append_signed(&self, seed: &[u8; eggdb_crypto::ED25519_SEED_BYTES]) -> Result<String, AuditFailure> {
        let public = hex(&eggdb_crypto::ed25519_public_key(seed));
        let mut signed = String::new();
        self.append_built(LOCK_WAIT, |previous| {
            signed = previous.to_owned();
            let signature = eggdb_crypto::ed25519_sign(seed, signed_message(previous).as_bytes());
            let mut out = JsonOut::new();
            out.begin_object()
                .key("event")
                .string(CHAIN_SIGNED_EVENT)
                .key("time")
                .string(&now_rfc3339())
                .key("key")
                .string(&public)
                .key("signature")
                .string(&hex(&signature))
                .key("detail")
                .string(&format!("key {}", &public[..16]));
            out.end_object();
            out.finish()
        })?;
        Ok(signed)
    }

    fn append_waiting(&self, record: &str, wait: Duration) -> Result<String, AuditFailure> {
        self.append_built(wait, |_| record.to_owned())
    }

    fn append_built(&self, wait: Duration, build: impl FnOnce(&str) -> String) -> Result<String, AuditFailure> {
        let failure = |error: io::Error| AuditFailure {
            reason: describe(&error),
        };
        let mut file = eggdb_platform::user_files::open_private_append_no_follow(&self.path)
            .map_err(failure)?;
        lock(&file, wait).map_err(failure)?;
        let size = file.metadata().map_err(failure)?.len();
        let previous = last_line_digest(&mut file, size).map_err(failure)?;
        let record = build(&previous);
        if size.saturating_add(record.len() as u64 + 1) > MAX_AUDIT_BYTES {
            return Err(AuditFailure {
                reason: "the audit file has reached its 1 GiB limit".to_owned(),
            });
        }
        let chained = match record.strip_prefix('{') {
            Some(rest) if rest.trim_start().starts_with('}') => {
                format!("{{\"prev\":\"{previous}\"}}")
            }
            Some(rest) => format!("{{\"prev\":\"{previous}\",{rest}"),
            None => {
                return Err(AuditFailure {
                    reason: "an audit record must be a JSON object".to_owned(),
                });
            }
        };
        let mut line = Vec::with_capacity(chained.len() + 1);
        line.extend_from_slice(chained.as_bytes());
        line.push(b'\n');
        file.write_all(&line).map_err(failure)?;
        file.sync_data().map_err(failure)?;
        Ok(hex(&eggdb_crypto::sha256(chained.as_bytes())))
    }
}

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
const CHAIN_SIGNED_EVENT: &str = "chain_signed";

fn signed_message(digest: &str) -> String {
    format!("eggdb audit chain {digest}")
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        *slot = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

fn signature_holds(line: &[u8], previous: &str, trusted: Option<&[u8; eggdb_crypto::ED25519_PUBLIC_KEY_BYTES]>) -> bool {
    let Ok(text) = std::str::from_utf8(line) else {
        return false;
    };
    let Ok(document) = parse_document(text) else {
        return false;
    };
    let Json::Object(members) = document.value else {
        return false;
    };
    let (Some(key), Some(signature)) = (
        text_of(&members, "key").and_then(|key| unhex::<{ eggdb_crypto::ED25519_PUBLIC_KEY_BYTES }>(&key)),
        text_of(&members, "signature")
            .and_then(|signature| unhex::<{ eggdb_crypto::ED25519_SIGNATURE_BYTES }>(&signature)),
    ) else {
        return false;
    };
    if trusted.is_some_and(|trusted| *trusted != key) {
        return false;
    }
    eggdb_crypto::ed25519_verify(&key, signed_message(previous).as_bytes(), &signature)
}

fn is_chain_signed(line: &[u8]) -> bool {
    std::str::from_utf8(line)
        .ok()
        .and_then(|text| text.get(74..))
        .is_some_and(|rest| rest.starts_with(",\"event\":\"chain_signed\""))
}
const ROTATED_EVENT_PREFIX: &str = ",\"event\":\"rotated\"";
const SECONDS_PER_DAY: u64 = 86_400;

impl AuditLog {
    /// The file's size in bytes, 0 when it does not exist yet.
    pub fn size(&self) -> u64 {
        std::fs::metadata(&self.path).map_or(0, |metadata| metadata.len())
    }

    /// Moves the audit file aside as `<file>.<unix seconds>` and starts a new one whose first
    /// record (`"event":"rotated"`) links to the moved file's newest digest and names it, so the
    /// chain goes on across files. Returns the moved file's path, or `None` when there was no
    /// file to move.
    pub fn rotate(&self) -> Result<Option<PathBuf>, AuditFailure> {
        let failure = |error: io::Error| AuditFailure {
            reason: describe(&error),
        };
        let mut file = match eggdb_platform::user_files::open_private_append_no_follow(&self.path) {
            Ok(file) => file,
            Err(error) => return Err(failure(error)),
        };
        lock(&file, LOCK_WAIT).map_err(failure)?;
        let size = file.metadata().map_err(failure)?.len();
        if size == 0 {
            return Ok(None);
        }
        let newest = last_line_digest(&mut file, size).map_err(failure)?;
        let seconds = now_unix_ms() / 1000;
        let mut archived = OsString::from(self.path.as_os_str());
        archived.push(format!(".{seconds}"));
        let archived = PathBuf::from(archived);
        if archived.exists() {
            return Err(AuditFailure {
                reason: format!("{} already exists", archived.display()),
            });
        }
        std::fs::rename(&self.path, &archived).map_err(failure)?;
        let name = archived
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut out = JsonOut::new();
        out.begin_object()
            .key("event")
            .string("rotated")
            .key("time")
            .string(&now_rfc3339())
            .key("previous_file")
            .string(&name)
            .end_object();
        let record = out.finish();
        let rest = record.strip_prefix('{').unwrap_or(&record);
        let first = format!("{{\"prev\":\"{newest}\",{rest}\n");
        let mut fresh = eggdb_platform::user_files::open_private_append_no_follow(&self.path).map_err(failure)?;
        fresh.write_all(first.as_bytes()).map_err(failure)?;
        fresh.sync_data().map_err(failure)?;
        Ok(Some(archived))
    }

    /// Removes the files [`AuditLog::rotate`] moved aside more than `keep_days` days ago,
    /// by their exact names, and returns how many it removed.
    pub fn remove_rotated_older_than(&self, keep_days: u64) -> Result<usize, AuditFailure> {
        let failure = |error: io::Error| AuditFailure {
            reason: describe(&error),
        };
        let (Some(directory), Some(stem)) = (self.path.parent(), self.path.file_name()) else {
            return Ok(0);
        };
        let directory = if directory.as_os_str().is_empty() { Path::new(".") } else { directory };
        let stem = format!("{}.", stem.to_string_lossy());
        let cutoff = (now_unix_ms() / 1000).saturating_sub(keep_days.saturating_mul(SECONDS_PER_DAY));
        let mut removed = 0;
        for entry in std::fs::read_dir(directory).map_err(failure)? {
            let entry = entry.map_err(failure)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(seconds) = name.strip_prefix(&stem).and_then(|suffix| suffix.parse::<u64>().ok()) else {
                continue;
            };
            if seconds < cutoff && entry.file_type().map_err(failure)?.is_file() {
                std::fs::remove_file(directory.join(&name)).map_err(failure)?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn starts_rotated(line: &[u8]) -> bool {
    std::str::from_utf8(line)
        .ok()
        .and_then(|text| text.get(9 + 64 + 1..))
        .is_some_and(|rest| rest.starts_with(ROTATED_EVENT_PREFIX))
}
const TAIL_BYTES: u64 = 1 << 20;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn last_line_digest(file: &mut File, size: u64) -> io::Result<String> {
    if size == 0 {
        return Ok(GENESIS.to_owned());
    }
    let start = size.saturating_sub(TAIL_BYTES);
    file.seek(io::SeekFrom::Start(start))?;
    let mut tail = Vec::with_capacity(usize::try_from(size - start).unwrap_or(0));
    file.take(size - start).read_to_end(&mut tail)?;
    let body = tail.strip_suffix(b"\n").unwrap_or(&tail);
    let line = match body.iter().rposition(|&byte| byte == b'\n') {
        Some(at) => &body[at + 1..],
        None if start == 0 => body,
        None => return Ok(GENESIS.to_owned()),
    };
    Ok(hex(&eggdb_crypto::sha256(line)))
}

/// What checking an audit file's chain found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChainReport {
    /// How many records carry a link to the one before.
    pub chained: u64,
    /// How many records at the start predate the chain (written by an older release).
    pub unchained: u64,
    /// The line number (from 1) of the first record whose link does not match the line before
    /// it, or that has no link after the chain began: the file was changed there.
    pub first_break: Option<u64>,
    /// The SHA-256 (hex) of the last line, the chain's newest digest; all zeros for an empty
    /// file. Kept elsewhere, it is an anchor for a later check.
    pub newest_digest: String,
    /// The line number of the record whose digest is the anchor checked, when one was given
    /// and found.
    pub anchor_line: Option<u64>,
    /// How many `chain_signed` records carry a good signature.
    pub signatures: u64,
    /// The last line a good signature covers: the line before the newest good
    /// `chain_signed` record.
    pub signed_through: Option<u64>,
    /// The line number of the first `chain_signed` record whose signature does not hold, or
    /// is by another key than the trusted one.
    pub bad_signature: Option<u64>,
}

fn prev_of(line: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(line).ok()?;
    let rest = text.strip_prefix("{\"prev\":\"")?;
    rest.get(..64)
        .filter(|digest| digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// Checks that every record of the audit file at `path` links to the line before it by its
/// SHA-256, so a record changed, removed or inserted before the last one shows as a break.
/// Records cut off the end, and a file rewritten whole with a new chain, still verify: compare
/// the last record's digest with a copy kept elsewhere to catch those.
pub fn verify_chain(path: &Path) -> io::Result<ChainReport> {
    verify_chain_to_anchor(path, None)
}

/// [`verify_chain`], also finding the record whose digest is `anchor` (a newest digest kept
/// from an earlier run): when the anchor is given and [`ChainReport::anchor_line`] is `None`,
/// that record and maybe others after it were cut off or the file was rewritten.
pub fn verify_chain_to_anchor(path: &Path, anchor: Option<&str>) -> io::Result<ChainReport> {
    verify_chain_signed(path, anchor, None)
}

/// [`verify_chain_to_anchor`], also checking every `chain_signed` record: its signature must
/// hold for the digest before it, and when `trusted` is given, be by that public key. The
/// first that does not is [`ChainReport::bad_signature`]; [`ChainReport::signed_through`] is
/// the last line a good signature covers.
pub fn verify_chain_signed(
    path: &Path,
    anchor: Option<&str>,
    trusted: Option<&[u8; eggdb_crypto::ED25519_PUBLIC_KEY_BYTES]>,
) -> io::Result<ChainReport> {
    let mut report = ChainReport {
        newest_digest: GENESIS.to_owned(),
        ..ChainReport::default()
    };
    let anchor = anchor.map(str::to_ascii_lowercase);
    let Some(mut reader) = open_audit(path)? else {
        return Ok(report);
    };
    let mut previous = GENESIS.to_owned();
    let mut number = 0u64;
    while let Some(bytes) = next_line(&mut reader)? {
        number += 1;
        let Some(line) = bytes else {
            report.first_break.get_or_insert(number);
            continue;
        };
        match prev_of(&line) {
            Some(link) if link == previous => report.chained += 1,
            Some(_) if number == 1 && starts_rotated(&line) => report.chained += 1,
            Some(_) => {
                report.first_break.get_or_insert(number);
            }
            None if report.chained == 0 && report.first_break.is_none() => report.unchained += 1,
            None => {
                report.first_break.get_or_insert(number);
            }
        }
        if is_chain_signed(&line) {
            if signature_holds(&line, &previous, trusted) {
                report.signatures += 1;
                report.signed_through = Some(number - 1);
            } else {
                report.bad_signature.get_or_insert(number);
            }
        }
        previous = hex(&eggdb_crypto::sha256(&line));
        if anchor.as_deref() == Some(previous.as_str()) {
            report.anchor_line.get_or_insert(number);
        }
    }
    report.newest_digest = previous;
    Ok(report)
}

fn lock(file: &File, wait: Duration) -> io::Result<()> {
    let give_up = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < give_up => {
                std::thread::sleep(LOCK_POLL);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another eggdb mcp held the audit file's lock too long",
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

pub(crate) fn now_unix_ms() -> u64 {
    eggdb_platform::SystemClock::new().unix_time_nanos() / 1_000_000
}

pub(crate) fn now_rfc3339() -> String {
    rfc3339(now_unix_micros())
}

pub(crate) fn rfc3339(unix_micros: u64) -> String {
    eggdb_commands::utc_rfc3339(unix_micros)
}

pub(crate) fn now_unix_micros() -> u64 {
    eggdb_platform::SystemClock::new().unix_time_nanos() / 1000
}

pub(crate) struct ServerStarted<'a> {
    pub(crate) version: &'a str,
    pub(crate) database: &'a str,
    pub(crate) time_limit: Duration,
    pub(crate) allow_write: bool,
    pub(crate) tables: Option<Vec<String>>,
    pub(crate) call_gap: Duration,
}

/// A server event as one audit record: `{"event":"server_event","time":...,"kind":...,
/// "user":...,"database":...,"protocol":...,"detail":...}`, with `"error_code":"refused"` for a
/// refused login or `"failed"` for a statement that failed.
pub fn server_event_record(
    kind: &str,
    user: &str,
    database: &str,
    protocol: &str,
    detail: &str,
    succeeded: bool,
) -> String {
    let mut out = JsonOut::new();
    out.begin_object()
        .key("event")
        .string("server_event")
        .key("time")
        .string(&now_rfc3339())
        .key("kind")
        .string(kind)
        .key("user")
        .string(user)
        .key("database")
        .string(database)
        .key("protocol")
        .string(protocol)
        .key("detail")
        .string(detail);
    if !succeeded {
        out.key("error_code").string(if kind == "login" { "refused" } else { "failed" });
    }
    out.end_object();
    out.finish()
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
        .boolean(!started.allow_write)
        .key("allow_write")
        .boolean(started.allow_write)
        .key("time_limit_ms")
        .unsigned(duration_ms(started.time_limit));
    if let Some(tables) = &started.tables {
        out.key("tables").begin_array();
        for name in tables {
            out.string(name);
        }
        out.end_array();
    }
    out.key("call_gap_ms")
        .unsigned(duration_ms(started.call_gap))
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

pub(crate) const MAX_ARGUMENT_BYTES: usize = 8 * 1024;

pub(crate) struct CallRequest<'a> {
    pub(crate) call_id: &'a str,
    pub(crate) started: &'a str,
    pub(crate) client_name: Option<&'a str>,
    pub(crate) client_version: Option<&'a str>,
    pub(crate) protocol: Option<&'a str>,
    pub(crate) role: Option<&'a str>,
    pub(crate) tool: &'a str,
    pub(crate) arguments: &'a str,
}

pub(crate) struct CallRecord<'a> {
    pub(crate) request: CallRequest<'a>,
    pub(crate) error_code: Option<&'a str>,
    pub(crate) sqlstate: Option<&'a str>,
    pub(crate) result_bytes: u64,
    pub(crate) rows: Option<u64>,
    pub(crate) truncated: bool,
    pub(crate) duration: Duration,
    pub(crate) write: Option<WriteFacts<'a>>,
}

pub(crate) struct WriteFacts<'a> {
    pub(crate) confirmation: Option<&'a str>,
    pub(crate) committed: bool,
    pub(crate) warning: Option<&'a str>,
}

fn request_fields(out: &mut JsonOut, event: &str, request: &CallRequest<'_>) {
    out.begin_object()
        .key("event")
        .string(event)
        .key("time")
        .string(request.started)
        .key("call_id")
        .string(request.call_id);
    if request.client_name.is_some() || request.client_version.is_some() {
        out.key("client")
            .begin_object()
            .optional_string("name", request.client_name)
            .optional_string("version", request.client_version)
            .end_object();
    }
    out.optional_string("protocol", request.protocol)
        .optional_string("role", request.role)
        .key("tool")
        .string(request.tool);
    let arguments = request.arguments;
    if arguments.len() <= MAX_ARGUMENT_BYTES {
        out.key("arguments").encoded(arguments);
    } else {
        let mut cut = MAX_ARGUMENT_BYTES;
        while !arguments.is_char_boundary(cut) {
            cut -= 1;
        }
        out.key("arguments_cut")
            .string(&arguments[..cut])
            .key("arguments_bytes")
            .unsigned(arguments.len() as u64);
    }
}

pub(crate) fn call_started_record(request: &CallRequest<'_>) -> String {
    let mut out = JsonOut::new();
    request_fields(&mut out, "call_started", request);
    out.end_object();
    out.finish()
}

pub(crate) fn call_record(call: &CallRecord<'_>) -> String {
    let mut out = JsonOut::new();
    request_fields(&mut out, "call", &call.request);
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
    out.key("truncated").boolean(call.truncated);
    if let Some(write) = &call.write {
        out.optional_string("confirmation", write.confirmation)
            .key("committed")
            .boolean(write.committed)
            .optional_string("warning", write.warning);
    }
    out.key("duration_ms")
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
    /// A write call about to change the database; the [`AuditEvent::Call`] with the same
    /// call id follows unless the server stopped during the write.
    CallStarted,
    /// A call ran past its time limit and the server exited.
    TimeLimitExceeded,
    /// A server event from `eggdb serve`: a login, a schema change or a privilege change,
    /// with its kind in `tool`, the user in `role` and the statement or client in `arguments`.
    ServerEvent,
    /// The server signed the chain's newest digest with its key; the key's start is in
    /// `arguments`.
    ChainSigned,
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
    /// The call's id, `<pid>-<server start in ms since 1970>-<number>`, which pairs a call
    /// with its call-started record.
    pub call_id: Option<String>,
    /// How a destructive statement was confirmed: `elicitation` (the user, through the
    /// client) or `confirm_argument` (the agent's `confirm: true`).
    pub confirmation: Option<String>,
    /// Whether a write call committed a change.
    pub committed: Option<bool>,
    /// A warning given with a call's result, such as a close that failed after a commit.
    pub warning: Option<String>,
    /// Whether the server let agents write (`--allow-write`), for a server start.
    pub allow_write: bool,
    /// The only tables and views the server showed its agent (`--tables`), for a server
    /// start; `None` when it showed every table.
    pub tables: Option<Vec<String>>,
    /// The pause after each call before the next opened the file (`--call-gap`), for a
    /// server start that recorded it.
    pub call_gap_ms: Option<u64>,
    /// For a call-started record: no call with its id follows, so the server stopped during
    /// the call and its outcome is unknown.
    pub outcome_unknown: bool,
    /// The role the agent acted as, for a call made over HTTP with a token tied to a role.
    pub role: Option<String>,
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

/// What [`read_audit`] counted in the whole audit file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditTotals {
    /// Every call in the file.
    pub calls: u64,
    /// Every call that failed.
    pub errors: u64,
    /// Every line that is not a record.
    pub unreadable: u64,
    /// Every call-started record with no call after it: the server stopped during the call.
    pub outcomes_unknown: u64,
    /// The client names, in the order they first appear.
    pub clients: Vec<String>,
}

const SHARING_VIOLATION: i32 = 32;
const LOCK_VIOLATION: i32 = 33;
const LONGEST_RECORD_BYTES: usize = 1 << 20;

fn is_locked_for_now(error: &io::Error) -> bool {
    cfg!(windows)
        && matches!(
            error.raw_os_error(),
            Some(SHARING_VIOLATION | LOCK_VIOLATION)
        )
}

fn patiently<T>(mut attempt: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let give_up = Instant::now() + LOCK_WAIT;
    loop {
        match attempt() {
            Err(error) if is_locked_for_now(&error) && Instant::now() < give_up => {
                std::thread::sleep(LOCK_POLL);
            }
            other => return other,
        }
    }
}

struct PatientFile(File);

impl Read for PatientFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        patiently(|| self.0.read(buffer))
    }
}

/// Reads the audit file at `path` line by line and hands `visit` every line, or only the last
/// `limit` lines when a limit is given; `visit` returns whether to go on. A call-started record
/// whose call follows in the file is handed over only when `every_record` is set, and one with
/// no call after it is marked [`AuditEntry::outcome_unknown`]. It returns the totals of the
/// whole file, or `None` when there is no audit file. It reads the file twice, the first time
/// to pair call-started records with their calls, keeping the ids of write calls only, and
/// otherwise holds one line at a time (the last `limit` with a limit); a line over 1 MiB,
/// which no record is, is skipped unread as unreadable. On Windows, where a server's lock
/// blocks reading for the moment it writes, each read waits up to five seconds. A write still
/// running when the file is read has no call yet, so it shows as an unknown outcome too.
pub fn read_audit(
    path: &Path,
    limit: Option<usize>,
    every_record: bool,
    visit: &mut dyn FnMut(AuditLine) -> bool,
) -> io::Result<Option<AuditTotals>> {
    let Some(mut reader) = open_audit(path)? else {
        return Ok(None);
    };
    let mut started = std::collections::HashSet::new();
    let mut answered = std::collections::HashSet::new();
    while let Some(bytes) = next_line(&mut reader)? {
        let Some((_, entry)) = bytes
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(record_of)
        else {
            continue;
        };
        match (entry.event, entry.call_id) {
            (AuditEvent::CallStarted, Some(call_id)) => {
                started.insert(call_id);
            }
            (AuditEvent::Call, Some(call_id)) if started.remove(&call_id) => {
                answered.insert(call_id);
            }
            _ => {}
        }
    }
    let Some(mut reader) = open_audit(path)? else {
        return Ok(None);
    };
    let mut totals = AuditTotals::default();
    let mut kept = VecDeque::new();
    let mut going = true;
    let mut number = 0;
    while let Some(bytes) = next_line(&mut reader)? {
        number += 1;
        let line = match bytes
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .and_then(record_of)
        {
            Some((raw, mut entry)) => {
                match entry.event {
                    AuditEvent::Call => {
                        totals.calls += 1;
                        if entry.error_code.is_some() {
                            totals.errors += 1;
                        }
                        if let Some(client) = &entry.client
                            && !totals.clients.contains(client)
                        {
                            totals.clients.push(client.clone());
                        }
                    }
                    AuditEvent::CallStarted => {
                        entry.outcome_unknown = entry
                            .call_id
                            .as_ref()
                            .is_none_or(|call_id| !answered.contains(call_id));
                        if entry.outcome_unknown {
                            totals.outcomes_unknown += 1;
                        } else if !every_record {
                            continue;
                        }
                    }
                    AuditEvent::ServerStarted
                    | AuditEvent::TimeLimitExceeded
                    | AuditEvent::ServerEvent
                    | AuditEvent::ChainSigned => {}
                }
                AuditLine::Record {
                    number,
                    raw,
                    entry: Box::new(entry),
                }
            }
            None => {
                totals.unreadable += 1;
                AuditLine::Unreadable { number }
            }
        };
        match limit {
            None if going => going = visit(line),
            None => {}
            Some(limit) => {
                kept.push_back(line);
                if kept.len() > limit {
                    kept.pop_front();
                }
            }
        }
    }
    for line in kept {
        if !visit(line) {
            break;
        }
    }
    Ok(Some(totals))
}

fn open_audit(path: &Path) -> io::Result<Option<BufReader<PatientFile>>> {
    match patiently(|| File::open(path)) {
        Ok(file) => Ok(Some(BufReader::new(PatientFile(file)))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn next_line(reader: &mut impl BufRead) -> io::Result<Option<Option<Vec<u8>>>> {
    let mut line = Vec::new();
    let mut too_long = false;
    let mut read_any = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        read_any = true;
        let (taken, ends) = match available.iter().position(|&byte| byte == b'\n') {
            Some(at) => (at + 1, true),
            None => (available.len(), false),
        };
        if !too_long {
            line.extend_from_slice(&available[..taken]);
            if line.len() > LONGEST_RECORD_BYTES {
                too_long = true;
                line = Vec::new();
            }
        }
        reader.consume(taken);
        if ends {
            break;
        }
    }
    if !read_any {
        return Ok(None);
    }
    if too_long {
        return Ok(Some(None));
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(Some(Some(line)))
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

fn texts_of(members: &[(String, Node)], name: &str) -> Option<Vec<String>> {
    match member(members, name) {
        Some(Json::Array(items)) => Some(
            items
                .iter()
                .filter_map(|item| match &item.value {
                    Json::String(text) => Some(text.clone()),
                    _ => None,
                })
                .collect(),
        ),
        _ => None,
    }
}

fn number_of(members: &[(String, Node)], name: &str) -> Option<u64> {
    match member(members, name) {
        Some(Json::Number(text)) => text.parse().ok(),
        _ => None,
    }
}

fn record_of(line: &str) -> Option<(String, AuditEntry)> {
    let document = parse_document(line).ok()?;
    let mut ascii = JsonOut::new();
    ascii.node(&document);
    let Json::Object(members) = document.value else {
        return None;
    };
    let event = match text_of(&members, "event")?.as_str() {
        "server_started" => AuditEvent::ServerStarted,
        "call" => AuditEvent::Call,
        "call_started" => AuditEvent::CallStarted,
        "time_limit_exceeded" => AuditEvent::TimeLimitExceeded,
        "server_event" => AuditEvent::ServerEvent,
        CHAIN_SIGNED_EVENT => AuditEvent::ChainSigned,
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
    let tool = text_of(&members, "tool").or_else(|| text_of(&members, "kind"));
    let entry = AuditEntry {
        event,
        time: text_of(&members, "time")?,
        client,
        arguments: text_of(&members, "detail").or_else(|| arguments_shown(&members, tool.as_deref())),
        tool,
        error_code: text_of(&members, "error_code"),
        sqlstate: text_of(&members, "sqlstate"),
        rows: number_of(&members, "rows"),
        truncated: matches!(member(&members, "truncated"), Some(Json::Boolean(true))),
        duration_ms: number_of(&members, "duration_ms"),
        version: text_of(&members, "version"),
        pid: number_of(&members, "pid"),
        time_limit_ms: number_of(&members, "time_limit_ms"),
        call_id: text_of(&members, "call_id"),
        confirmation: text_of(&members, "confirmation"),
        committed: match member(&members, "committed") {
            Some(Json::Boolean(committed)) => Some(*committed),
            _ => None,
        },
        warning: text_of(&members, "warning"),
        allow_write: matches!(member(&members, "allow_write"), Some(Json::Boolean(true))),
        tables: texts_of(&members, "tables"),
        call_gap_ms: number_of(&members, "call_gap_ms"),
        outcome_unknown: false,
        role: text_of(&members, "role").or_else(|| text_of(&members, "user")),
    };
    Some((ascii.finish(), entry))
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
            Some("run_sql" | "write_sql" | "explain_query") => Some("sql"),
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
        AuditEvent, AuditLine, AuditLog, AuditTotals, CallRecord, CallRequest, WriteFacts,
        audit_path_for, call_record, call_started_record, read_audit, rfc3339,
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
            request: request(tool, arguments, "1-1"),
            error_code,
            sqlstate: None,
            result_bytes: 10,
            rows: Some(2),
            truncated: false,
            duration: Duration::from_millis(12),
            write: None,
        })
    }

    fn request<'a>(tool: &'a str, arguments: &'a str, call_id: &'a str) -> CallRequest<'a> {
        CallRequest {
            call_id,
            started: "2026-10-28T09:14:07.123456Z",
            client_name: Some("claude-code"),
            client_version: Some("2.1.0"),
            protocol: Some("2025-11-25"),
            role: None,
            tool,
            arguments,
        }
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

    fn read_all(path: &Path, limit: Option<usize>) -> Option<(Vec<AuditLine>, AuditTotals)> {
        let mut lines = Vec::new();
        let totals = read_audit(path, limit, false, &mut |line| {
            lines.push(line);
            true
        })
        .unwrap()?;
        Some((lines, totals))
    }

    #[test]
    fn server_events_read_back_with_their_kind_user_and_detail() {
        let line = server_event_record("schema_change", "alice", "shop", "postgresql", "CREATE TABLE t (a int)", true);
        let (_, entry) = record_of(&line).expect("a record");
        assert_eq!(entry.event, AuditEvent::ServerEvent);
        assert_eq!(entry.tool.as_deref(), Some("schema_change"));
        assert_eq!(entry.role.as_deref(), Some("alice"));
        assert_eq!(entry.arguments.as_deref(), Some("CREATE TABLE t (a int)"));
        assert_eq!(entry.error_code, None);
        let refused = server_event_record("login", "mallory", "shop", "postgresql", "client=10.0.0.9 method=scram-sha-256", false);
        assert_eq!(record_of(&refused).expect("a record").1.error_code.as_deref(), Some("refused"));
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
            .and_then(|mut file| {
                std::io::Write::write_all(&mut file, b"not json\n")?;
                std::io::Write::write_all(&mut file, &vec![b'x'; (1 << 20) + 10])?;
                std::io::Write::write_all(&mut file, b"\n")
            })
            .unwrap();
        log.append(&call("kv_get", r#"{"key":"a"}"#, None)).unwrap();
        let (lines, totals) = read_all(&path, None).unwrap();
        assert_eq!((totals.calls, totals.errors, totals.unreadable), (3, 1, 2));
        assert_eq!(totals.clients, ["claude-code 2.1.0"]);
        let AuditLine::Record { entry, .. } = &lines[0] else {
            panic!("{:?}", lines[0]);
        };
        assert_eq!(entry.event, AuditEvent::Call);
        assert_eq!(entry.arguments.as_deref(), Some("SELECT 1\nFROM t"));
        assert_eq!(entry.rows, Some(2));
        assert_eq!(lines[2], AuditLine::Unreadable { number: 3 });
        assert_eq!(lines[3], AuditLine::Unreadable { number: 4 });
        assert_eq!(lines.len(), 5);
        let (last, totals) = read_all(&path, Some(1)).unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(totals.calls, 3);
        let mut seen = 0;
        read_audit(&path, None, false, &mut |_| {
            seen += 1;
            false
        })
        .unwrap();
        assert_eq!(seen, 1);
        assert!(read_all(&path.with_extension("missing"), None).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn appenders_sharing_the_file_never_interleave_their_lines() {
        let path = temporary("shared");
        let writers: Vec<_> = (0..4)
            .map(|writer| {
                let log = AuditLog::new(path.clone());
                std::thread::spawn(move || {
                    for record in 0..10 {
                        let arguments = format!(
                            r#"{{"sql":"{}"}}"#,
                            format!("{writer}-{record} ").repeat(400)
                        );
                        log.append(&call("run_sql", &arguments, None)).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let (lines, totals) = read_all(&path, None).unwrap();
        assert_eq!(lines.len(), 40);
        assert_eq!((totals.calls, totals.unreadable), (40, 0));
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
        assert_eq!(read_all(&path, None).unwrap().1.calls, 1);
        assert_eq!(read_all(&archived, None).unwrap().1.calls, 1);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&archived);
    }

    #[test]
    fn a_call_started_record_pairs_with_its_call_and_one_without_is_an_unknown_outcome() {
        let path = temporary("started");
        let log = AuditLog::new(path.clone());
        let insert = r#"{"sql":"INSERT INTO t VALUES (1)"}"#;
        log.append(&call_started_record(&request("write_sql", insert, "7-1")))
            .unwrap();
        log.append(&call_record(&CallRecord {
            request: request("write_sql", insert, "7-1"),
            error_code: None,
            sqlstate: None,
            result_bytes: 10,
            rows: Some(1),
            truncated: false,
            duration: Duration::from_millis(3),
            write: Some(WriteFacts {
                confirmation: Some("elicitation"),
                committed: true,
                warning: Some("committed; closing the database file failed"),
            }),
        }))
        .unwrap();
        let drop = r#"{"sql":"DROP TABLE t"}"#;
        log.append(&call_started_record(&request("write_sql", drop, "7-2")))
            .unwrap();
        let (shown, totals) = read_all(&path, None).unwrap();
        assert_eq!((totals.calls, totals.outcomes_unknown), (1, 1));
        assert_eq!(shown.len(), 2);
        let AuditLine::Record { entry, .. } = &shown[0] else {
            panic!("{:?}", shown[0]);
        };
        assert_eq!(entry.event, AuditEvent::Call);
        assert_eq!(entry.call_id.as_deref(), Some("7-1"));
        assert_eq!(entry.confirmation.as_deref(), Some("elicitation"));
        assert_eq!(entry.committed, Some(true));
        assert!(entry.warning.is_some());
        let AuditLine::Record { entry, .. } = &shown[1] else {
            panic!("{:?}", shown[1]);
        };
        assert_eq!(entry.event, AuditEvent::CallStarted);
        assert!(entry.outcome_unknown);
        assert_eq!(entry.arguments.as_deref(), Some("DROP TABLE t"));
        let mut every = 0;
        read_audit(&path, None, true, &mut |_| {
            every += 1;
            true
        })
        .unwrap();
        assert_eq!(every, 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_prompt_append_gives_up_on_a_held_lock_long_before_a_normal_one() {
        let path = temporary("prompt");
        let log = AuditLog::new(path.clone());
        log.append(&call("list_tables", "{}", None)).unwrap();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .unwrap();
        holder.lock().unwrap();
        let started = std::time::Instant::now();
        assert!(
            log.append_promptly(&call("list_tables", "{}", None))
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        holder.unlock().unwrap();
        drop(holder);
        log.append_promptly(&call("list_tables", "{}", None))
            .unwrap();
        assert_eq!(read_all(&path, None).unwrap().1.calls, 2);
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod chain_tests {
    use super::{AuditLog, verify_chain};

    #[test]
    fn records_link_to_the_line_before_and_an_edit_breaks_the_chain() {
        let directory =
            std::env::temp_dir().join(format!("eggdb-audit-chain-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("app.eggdb.mcp-audit.jsonl");
        std::fs::write(&path, "{\"event\":\"older\"}\n").unwrap();
        let log = AuditLog::new(path.clone());
        for index in 0..3 {
            log.append(&format!("{{\"event\":\"call\",\"n\":{index}}}"))
                .unwrap();
        }
        log.append("{}").unwrap();
        let report = verify_chain(&path).unwrap();
        assert_eq!(
            (report.chained, report.unchained, report.first_break),
            (4, 1, None)
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.lines().nth(1).unwrap().starts_with("{\"prev\":\""));
        std::fs::write(&path, text.replace("\"n\":1", "\"n\":9")).unwrap();
        assert_eq!(verify_chain(&path).unwrap().first_break, Some(4));
        assert!(log.append("not json").is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }
    #[test]
    fn rotating_continues_the_chain_in_a_new_file_and_old_files_are_removed_by_age() {
        let directory =
            std::env::temp_dir().join(format!("eggdb-audit-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("app.eggdb.mcp-audit.jsonl");
        let log = AuditLog::new(path.clone());
        let newest = log.append_with_digest("{\"event\":\"call\",\"n\":0}").unwrap();
        let archived = log.rotate().unwrap().expect("a moved file");
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.starts_with(&format!("{{\"prev\":\"{newest}\",\"event\":\"rotated\"")), "{first}");
        log.append("{\"event\":\"call\",\"n\":1}").unwrap();
        let report = verify_chain(&path).unwrap();
        assert_eq!((report.chained, report.first_break), (2, None));
        assert_eq!(log.remove_rotated_older_than(1).unwrap(), 0);
        assert!(archived.exists());
        std::fs::remove_file(&archived).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn an_anchor_kept_elsewhere_finds_its_record_until_the_end_is_cut_off() {
        let directory =
            std::env::temp_dir().join(format!("eggdb-audit-anchor-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("app.eggdb.mcp-audit.jsonl");
        let log = AuditLog::new(path.clone());
        log.append("{\"event\":\"call\",\"n\":0}").unwrap();
        let anchor = log.append_with_digest("{\"event\":\"call\",\"n\":1}").unwrap();
        log.append("{\"event\":\"call\",\"n\":2}").unwrap();
        let report = verify_chain_to_anchor(&path, Some(&anchor)).unwrap();
        assert_eq!(report.anchor_line, Some(2));
        assert_ne!(report.newest_digest, anchor);
        let text = std::fs::read_to_string(&path).unwrap();
        let kept: Vec<&str> = text.lines().take(1).collect();
        std::fs::write(&path, format!("{}\n", kept.join("\n"))).unwrap();
        let cut = verify_chain_to_anchor(&path, Some(&anchor)).unwrap();
        assert_eq!((cut.first_break, cut.anchor_line), (None, None));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }

    #[test]
    fn signed_chains_verify_against_their_key_and_forgeries_fail() {
        use super::verify_chain_signed;
        let directory =
            std::env::temp_dir().join(format!("eggdb-audit-signed-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("app.eggdb.mcp-audit.jsonl");
        let log = AuditLog::new(path.clone());
        let seed = [7u8; 32];
        let public = eggdb_crypto::ed25519_public_key(&seed);
        log.append("{\"event\":\"call\",\"n\":0}").unwrap();
        log.append("{\"event\":\"call\",\"n\":1}").unwrap();
        let signed = log.append_signed(&seed).unwrap();
        log.append("{\"event\":\"call\",\"n\":2}").unwrap();
        let report = verify_chain_signed(&path, None, Some(&public)).unwrap();
        assert_eq!((report.signatures, report.signed_through, report.bad_signature), (1, Some(2), None));
        assert_eq!(report.first_break, None);
        let other = eggdb_crypto::ed25519_public_key(&[8u8; 32]);
        assert_eq!(verify_chain_signed(&path, None, Some(&other)).unwrap().bad_signature, Some(3));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.lines().nth(2).unwrap().contains(&signed));
        let forged = text.replacen("\"n\":1", "\"n\":9", 1);
        std::fs::write(&path, forged).unwrap();
        let rewritten = verify_chain_signed(&path, None, Some(&public)).unwrap();
        assert!(rewritten.first_break.is_some() || rewritten.bad_signature.is_some());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }
}
