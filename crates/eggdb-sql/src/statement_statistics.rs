//! Statement statistics and the slow-statement log (D286): every statement a session runs is
//! timed and counted under its normalized text, the statement with each literal replaced by a
//! numbered placeholder, so `pg_stat_statements` lists calls, times and rows per statement shape
//! and the slow-statement log names a statement without the values it carried. Both are off
//! until a server turns them on with [`collect`] and [`log_slow_statements`], and cost a load of
//! two flags per statement while off.

use crate::lexer::{Token, TokenKind};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock, RwLock};
use std::time::Duration;

/// How many statements `eggdb serve` keeps statistics for unless told otherwise, as
/// PostgreSQL's `pg_stat_statements.max` defaults to.
pub const DEFAULT_CAPACITY: usize = 5000;

/// The longest normalized text kept for a statement, in bytes; longer texts are cut at a
/// character boundary, and statements that differ only after the cut still count apart.
pub const MAX_QUERY_BYTES: usize = 4096;

const SHARDS: usize = 16;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

static CAPACITY: AtomicUsize = AtomicUsize::new(0);
static DEALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static RESET_AT: AtomicU64 = AtomicU64::new(0);
static SLOW_LOG_ON: AtomicBool = AtomicBool::new(false);
static SLOW_LOG: RwLock<Option<SlowLog>> = RwLock::new(None);
static TABLE: OnceLock<[Mutex<Shard>; SHARDS]> = OnceLock::new();

/// Receives each line of the slow-statement log.
pub type SlowStatementSink = Box<dyn Fn(&SlowStatement<'_>) + Send + Sync>;

struct SlowLog {
    threshold: Duration,
    sink: SlowStatementSink,
}

/// A statement that took at least the slow-statement threshold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SlowStatement<'a> {
    /// How long the statement ran.
    pub duration: Duration,
    /// The database it ran in.
    pub database: &'a str,
    /// The user it ran as.
    pub user: &'a str,
    /// The protocol of the connection it came on, such as `postgresql`, `mysql` or
    /// `sqlserver`.
    pub protocol: &'a str,
    /// The rows it returned or changed.
    pub rows: u64,
    /// Its normalized text: literals replaced by `$1`, `$2` and so on.
    pub query: &'a str,
}

impl fmt::Display for SlowStatement<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "slow statement duration_ms={:.3} database={} user={} protocol={} rows={} query={}",
            self.duration.as_secs_f64() * 1000.0,
            Quoted(self.database),
            Quoted(self.user),
            Quoted(self.protocol),
            self.rows,
            Quoted(self.query),
        )
    }
}

struct Quoted<'a>(&'a str);

impl fmt::Display for Quoted<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let plain = !self.0.is_empty()
            && self
                .0
                .chars()
                .all(|character| character.is_alphanumeric() || "_-.:/@$".contains(character));
        if plain {
            return formatter.write_str(self.0);
        }
        formatter.write_str("\"")?;
        for character in self.0.chars() {
            match character {
                '"' => formatter.write_str("\\\"")?,
                '\\' => formatter.write_str("\\\\")?,
                '\n' | '\r' | '\t' => formatter.write_str(" ")?,
                control if control.is_control() || matches!(control, '\u{2028}' | '\u{2029}') => {
                    write!(formatter, "\\u{{{:04x}}}", u32::from(control))?
                }
                other => write!(formatter, "{other}")?,
            }
        }
        formatter.write_str("\"")
    }
}

/// What one statement shape has cost since the statistics were last reset.
#[derive(Debug, Clone, PartialEq)]
pub struct StatementStatistics {
    /// The user the statements ran as.
    pub user: String,
    /// The database the statements ran in.
    pub database: String,
    /// A stable 64-bit hash of the full normalized text.
    pub query_id: i64,
    /// The normalized text, at most [`MAX_QUERY_BYTES`] long.
    pub query: String,
    /// How many times it ran to completion.
    pub calls: u64,
    /// Its total running time, in milliseconds.
    pub total_ms: f64,
    /// Its shortest run, in milliseconds.
    pub min_ms: f64,
    /// Its longest run, in milliseconds.
    pub max_ms: f64,
    /// Its mean running time, in milliseconds.
    pub mean_ms: f64,
    /// The population standard deviation of its running time, in milliseconds.
    pub stddev_ms: f64,
    /// The rows it returned or changed, in all.
    pub rows: u64,
}

/// What the statistics table has done as a whole: PostgreSQL's `pg_stat_statements_info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatisticsInfo {
    /// How many times the least-called entries were dropped to make room.
    pub deallocations: u64,
    /// When the statistics were last reset or turned on, in microseconds since 2000-01-01
    /// UTC, PostgreSQL's epoch.
    pub reset_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    user: Box<str>,
    database: Box<str>,
    query_id: i64,
    query: Box<str>,
    calls: u64,
    total_ms: f64,
    min_ms: f64,
    max_ms: f64,
    mean_ms: f64,
    squared_deviations: f64,
    rows: u64,
}

impl Entry {
    fn add(&mut self, milliseconds: f64, rows: u64) {
        self.calls = self.calls.saturating_add(1);
        self.total_ms += milliseconds;
        self.min_ms = self.min_ms.min(milliseconds);
        self.max_ms = self.max_ms.max(milliseconds);
        let before = self.mean_ms;
        self.mean_ms += (milliseconds - before) / self.calls as f64;
        self.squared_deviations += (milliseconds - before) * (milliseconds - self.mean_ms);
        self.rows = self.rows.saturating_add(rows);
    }

    fn statistics(&self) -> StatementStatistics {
        let stddev_ms = if self.calls > 1 {
            (self.squared_deviations / self.calls as f64)
                .max(0.0)
                .sqrt()
        } else {
            0.0
        };
        StatementStatistics {
            user: self.user.to_string(),
            database: self.database.to_string(),
            query_id: self.query_id,
            query: self.query.to_string(),
            calls: self.calls,
            total_ms: self.total_ms,
            min_ms: self.min_ms,
            max_ms: self.max_ms,
            mean_ms: self.mean_ms,
            stddev_ms,
            rows: self.rows,
        }
    }
}

#[derive(Debug, Default)]
struct Shard {
    entries: HashMap<u64, Entry>,
}

impl Shard {
    fn record(
        &mut self,
        capacity: usize,
        key: u64,
        statement: &RecordedStatement<'_>,
        normalized: &Normalized,
    ) -> bool {
        let milliseconds = statement.duration.as_secs_f64() * 1000.0;
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.add(milliseconds, statement.rows);
            return false;
        }
        let deallocated = self.entries.len() >= capacity.max(1);
        if deallocated {
            self.drop_least_called(capacity.max(1));
        }
        let mut entry = Entry {
            user: statement.user.into(),
            database: statement.database.into(),
            query_id: normalized.query_id,
            query: normalized.text.as_str().into(),
            calls: 0,
            total_ms: 0.0,
            min_ms: f64::INFINITY,
            max_ms: 0.0,
            mean_ms: 0.0,
            squared_deviations: 0.0,
            rows: 0,
        };
        entry.add(milliseconds, statement.rows);
        self.entries.insert(key, entry);
        deallocated
    }

    fn drop_least_called(&mut self, capacity: usize) {
        let needed = (self.entries.len() + 1).saturating_sub(capacity);
        let dropped = needed.max(capacity / 20).min(self.entries.len());
        let mut by_calls: Vec<(u64, u64)> = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.calls, *key))
            .collect();
        if dropped < by_calls.len() {
            by_calls.select_nth_unstable(dropped);
        }
        for (_, key) in by_calls.iter().take(dropped) {
            self.entries.remove(key);
        }
    }
}

fn table() -> &'static [Mutex<Shard>; SHARDS] {
    TABLE.get_or_init(|| std::array::from_fn(|_| Mutex::new(Shard::default())))
}

fn locked(shard: &Mutex<Shard>) -> MutexGuard<'_, Shard> {
    shard
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn shard_capacity(capacity: usize) -> usize {
    capacity.div_ceil(SHARDS).max(1)
}

/// Keeps statistics for at most about `capacity` statement shapes across the process, emptying
/// what was kept before; `0` turns them off. Entries are spread over sixteen shards, each
/// holding a sixteenth of `capacity` (at least one), and a shard that is full drops its
/// least-called twentieth (at least one entry) before it takes a new statement.
pub fn collect(capacity: usize) {
    CAPACITY.store(capacity, Ordering::Relaxed);
    reset();
}

/// Whether statistics are being kept.
pub fn collecting() -> bool {
    CAPACITY.load(Ordering::Relaxed) > 0
}

/// Empties the statistics and starts their reset time again, as `pg_stat_statements_reset()`.
pub fn reset() {
    for shard in table() {
        locked(shard).entries.clear();
    }
    DEALLOCATIONS.store(0, Ordering::Relaxed);
    RESET_AT.store(crate::datetime::statement_now() as u64, Ordering::Relaxed);
}

/// Drops the statistics of the statements whose query ID is `query_id`, in every database, as
/// `pg_stat_statements_reset(0, 0, query_id)`.
pub fn reset_query(query_id: i64) {
    for shard in table() {
        locked(shard)
            .entries
            .retain(|_, entry| entry.query_id != query_id);
    }
}

/// The statistics kept now, most total time first.
pub fn statistics() -> Vec<StatementStatistics> {
    let mut all: Vec<StatementStatistics> = table()
        .iter()
        .flat_map(|shard| {
            locked(shard)
                .entries
                .values()
                .map(Entry::statistics)
                .collect::<Vec<_>>()
        })
        .collect();
    all.sort_by(|left, right| right.total_ms.total_cmp(&left.total_ms));
    all
}

/// How many times entries were dropped and when the statistics were last reset.
pub fn info() -> StatisticsInfo {
    StatisticsInfo {
        deallocations: DEALLOCATIONS.load(Ordering::Relaxed),
        reset_at: RESET_AT.load(Ordering::Relaxed) as i64,
    }
}

/// Sends every statement that runs for at least `threshold` to `sink`, in place of any sink
/// given before.
pub fn log_slow_statements(threshold: Duration, sink: SlowStatementSink) {
    let mut log = SLOW_LOG
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *log = Some(SlowLog { threshold, sink });
    SLOW_LOG_ON.store(true, Ordering::Relaxed);
}

/// Stops the slow-statement log and drops its sink.
pub fn stop_logging_slow_statements() {
    SLOW_LOG_ON.store(false, Ordering::Relaxed);
    let taken = SLOW_LOG
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    drop(taken);
}

pub(crate) fn wanted() -> bool {
    collecting() || SLOW_LOG_ON.load(Ordering::Relaxed)
}

pub(crate) struct RecordedStatement<'a> {
    pub(crate) database: &'a str,
    pub(crate) user: &'a str,
    pub(crate) protocol: &'a str,
    pub(crate) duration: Duration,
    pub(crate) rows: u64,
}

pub(crate) fn record(tokens: &[Token<'_>], text: &str, statement: &RecordedStatement<'_>) {
    let capacity = CAPACITY.load(Ordering::Relaxed);
    let slow = SLOW_LOG_ON.load(Ordering::Relaxed);
    if capacity == 0 && !slow {
        return;
    }
    let normalized = normalize(tokens, text);
    if normalized.text.is_empty() {
        return;
    }
    if capacity > 0 {
        let key = keyed(statement.user, statement.database, normalized.query_id);
        let shard = &table()[(key % SHARDS as u64) as usize];
        if locked(shard).record(shard_capacity(capacity), key, statement, &normalized) {
            DEALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
    }
    if slow {
        let log = SLOW_LOG
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(log) = log.as_ref()
            && statement.duration >= log.threshold
        {
            (log.sink)(&SlowStatement {
                duration: statement.duration,
                database: statement.database,
                user: statement.user,
                protocol: statement.protocol,
                rows: statement.rows,
                query: &normalized.text,
            });
        }
    }
}

fn keyed(user: &str, database: &str, query_id: i64) -> u64 {
    let mut hash = Fnv::new();
    hash.write(user.as_bytes());
    hash.write(&[0]);
    hash.write(database.as_bytes());
    hash.write(&[0]);
    hash.write(&query_id.to_le_bytes());
    hash.0
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(FNV_OFFSET)
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Normalized {
    pub(crate) text: String,
    pub(crate) query_id: i64,
}

struct NormalizedText {
    text: String,
    hash: Fnv,
}

impl NormalizedText {
    fn push(&mut self, piece: &str) {
        self.hash.write(piece.as_bytes());
        let room = MAX_QUERY_BYTES.saturating_sub(self.text.len());
        if piece.len() <= room {
            self.text.push_str(piece);
            return;
        }
        let mut cut = room;
        while cut > 0 && !piece.is_char_boundary(cut) {
            cut -= 1;
        }
        self.text.push_str(piece.get(..cut).unwrap_or_default());
    }
}

fn is_literal(kind: &TokenKind<'_>) -> bool {
    matches!(
        kind,
        TokenKind::Integer(_) | TokenKind::Decimal(_) | TokenKind::Binary(_) | TokenKind::String(_)
    )
}

/// The statement in `tokens` (spans of `text`) with every literal replaced by `$n`, numbered
/// after the highest `$n` parameter the statement already has; runs of spaces and comments
/// between tokens become one space, and a trailing `;` is left out.
pub(crate) fn normalize(tokens: &[Token<'_>], text: &str) -> Normalized {
    let mut next = crate::session::highest_parameter(tokens);
    let meaningful = tokens
        .iter()
        .rposition(|token| !matches!(token.kind, TokenKind::End | TokenKind::Semicolon))
        .map_or(0, |last| last + 1);
    let mut built = NormalizedText {
        text: String::new(),
        hash: Fnv::new(),
    };
    let mut previous_end: Option<usize> = None;
    for token in tokens.iter().take(meaningful) {
        if previous_end.is_some_and(|end| end < token.start) {
            built.push(" ");
        }
        if is_literal(&token.kind) {
            next = next.saturating_add(1);
            built.push(&format!("${next}"));
        } else {
            built.push(text.get(token.start..token.end).unwrap_or_default());
        }
        previous_end = Some(token.end);
    }
    Normalized {
        query_id: built.hash.0 as i64,
        text: built.text,
    }
}

/// The rows a statement's result returned, or changed as its command tag counts them.
pub(crate) fn rows_of(result: &crate::session::QueryResult) -> u64 {
    if result.columns.is_some() {
        return result.rows.len() as u64;
    }
    result
        .tag
        .rsplit(' ')
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized(text: &str) -> Normalized {
        let tokens = crate::lexer::tokenize(text).unwrap_or_default();
        normalize(&tokens, text)
    }

    #[test]
    fn the_same_statement_counts_apart_per_user_and_database() {
        let id = normalized("SELECT 1").query_id;
        let first = keyed("alice", "main", id);
        assert_ne!(first, keyed("bob", "main", id));
        assert_ne!(first, keyed("alice", "other", id));
        assert_ne!(keyed("ab", "c", id), keyed("a", "bc", id));
        assert_eq!(first, keyed("alice", "main", id));
    }

    #[test]
    fn slow_lines_escape_every_control_character() {
        let line = SlowStatement {
            duration: Duration::from_millis(1),
            database: "main\u{1b}[2J",
            user: "eggdb",
            protocol: "postgresql",
            rows: 0,
            query: "SELECT 1\u{0}\u{7f}\u{85}\u{2028}",
        }
        .to_string();
        assert_eq!(
            line,
            "slow statement duration_ms=1.000 database=\"main\\u{001b}[2J\" user=eggdb protocol=postgresql rows=0 query=\"SELECT 1\\u{0000}\\u{007f}\\u{0085}\\u{2028}\""
        );
        assert!(!line.chars().any(char::is_control));
    }

    fn statement(database: &str, milliseconds: u64, rows: u64) -> RecordedStatement<'_> {
        RecordedStatement {
            database,
            user: "eggdb",
            protocol: "postgresql",
            duration: Duration::from_millis(milliseconds),
            rows,
        }
    }

    #[test]
    fn literals_become_numbered_placeholders_and_spacing_folds() {
        let shape = normalized("SELECT *  FROM t /* note */ WHERE id = 42 AND name = 'secret';");
        assert_eq!(shape.text, "SELECT * FROM t WHERE id = $1 AND name = $2");
        assert_eq!(
            normalized("select * from t where id=7").text,
            "select * from t where id=$1"
        );
        assert_eq!(
            normalized("UPDATE t SET a = 1.5, b = $$body$$ WHERE id = $1").text,
            "UPDATE t SET a = $2, b = $3 WHERE id = $1"
        );
    }

    #[test]
    fn the_same_shape_has_the_same_id_whatever_its_values() {
        let first = normalized("SELECT name FROM users WHERE password = 'hunter2'");
        let second = normalized("SELECT name FROM users WHERE password = 'swordfish'");
        assert_eq!(first, second);
        assert!(!first.text.contains("hunter2"));
        assert_ne!(
            first.query_id,
            normalized("SELECT name FROM users").query_id
        );
    }

    #[test]
    fn long_texts_are_cut_at_a_character_boundary_but_hashed_whole() {
        let shape = normalized(&format!(
            "SELECT \"{}\" FROM t",
            "é".repeat(MAX_QUERY_BYTES)
        ));
        assert!(shape.text.len() <= MAX_QUERY_BYTES);
        let other = normalized(&format!(
            "SELECT \"{}\" FROM u",
            "é".repeat(MAX_QUERY_BYTES)
        ));
        assert_eq!(shape.text, other.text);
        assert_ne!(shape.query_id, other.query_id);
    }

    #[test]
    fn a_shard_keeps_calls_times_and_rows_per_statement() {
        let mut shard = Shard::default();
        let shape = normalized("SELECT 1");
        for milliseconds in [2, 4, 6] {
            shard.record(10, 1, &statement("main", milliseconds, 1), &shape);
        }
        let kept = shard
            .entries
            .get(&1)
            .map(Entry::statistics)
            .expect("the statement is kept");
        assert_eq!((kept.calls, kept.rows), (3, 3));
        assert_eq!((kept.min_ms, kept.max_ms, kept.mean_ms), (2.0, 6.0, 4.0));
        assert!((kept.total_ms - 12.0).abs() < 1e-9);
        assert!((kept.stddev_ms - (8.0f64 / 3.0).sqrt()).abs() < 1e-9);
        assert_eq!(kept.query, "SELECT $1");
    }

    #[test]
    fn a_full_shard_drops_its_least_called_statements() {
        let mut shard = Shard::default();
        let shape = normalized("SELECT 1");
        for key in 0..4u64 {
            for _ in 0..=key {
                shard.record(4, key, &statement("main", 1, 0), &shape);
            }
        }
        assert_eq!(shard.entries.len(), 4);
        assert!(shard.record(4, 99, &statement("main", 1, 0), &shape));
        assert_eq!(shard.entries.len(), 4);
        assert!(!shard.entries.contains_key(&0));
        assert!(shard.entries.contains_key(&3) && shard.entries.contains_key(&99));
    }

    #[test]
    fn the_shard_capacity_covers_the_whole_capacity() {
        assert_eq!(shard_capacity(1), 1);
        assert!(shard_capacity(DEFAULT_CAPACITY) * SHARDS >= DEFAULT_CAPACITY);
        assert_eq!(shard_capacity(32), 2);
    }

    #[test]
    fn slow_lines_are_one_line_of_key_value_pairs() {
        let line = SlowStatement {
            duration: Duration::from_micros(12_345),
            database: "main",
            user: "my user",
            protocol: "postgresql",
            rows: 3,
            query: "SELECT \"a\"\nFROM t WHERE id = $1",
        }
        .to_string();
        assert_eq!(
            line,
            "slow statement duration_ms=12.345 database=main user=\"my user\" protocol=postgresql rows=3 query=\"SELECT \\\"a\\\" FROM t WHERE id = $1\""
        );
    }

    fn texts(session: &mut crate::Session<'_>, sql: &str) -> Vec<String> {
        let mut rendered = Vec::new();
        session
            .execute(sql, &mut |result| {
                rendered.push(crate::tests::render(&result));
            })
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        rendered
    }

    #[test]
    fn sessions_record_their_statements_and_every_dialect_reads_them() {
        collect(100_000);
        let store = crate::memory_store::MemoryStore::default();
        let mut session = crate::Session::new(&store);
        session.identify("eggdb", "statistics_probe");
        for id in [1, 2, 3] {
            texts(&mut session, &format!("SELECT {id} AS probe_value"));
        }
        assert_eq!(
            texts(
                &mut session,
                "SELECT query, calls, rows FROM pg_stat_statements WHERE query LIKE '%AS probe_value'"
            ),
            vec!["query|calls|rows\nSELECT $1 AS probe_value|3|3\nSELECT 1".to_owned()]
        );
        for dialect in [
            crate::Dialect::MySql,
            crate::Dialect::SqlServer,
            crate::Dialect::Oracle,
        ] {
            let mut other = crate::Session::with_dialect(&store, dialect);
            other.identify("eggdb", "statistics_probe");
            let mut found = 0;
            other
                .execute(
                    "SELECT calls FROM pg_catalog.pg_stat_statements",
                    &mut |result| found += result.rows.len(),
                )
                .unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
            assert!(found >= 1, "{dialect:?}");
        }
        let mut mysql = crate::Session::with_dialect(&store, crate::Dialect::MySql);
        mysql.identify("eggdb", "statistics_probe");
        let digests = texts(
            &mut mysql,
            "SELECT digest_text, count_star, sum_rows_sent FROM performance_schema.events_statements_summary_by_digest WHERE digest_text LIKE '%AS probe_value'",
        );
        assert_eq!(digests, vec!["digest_text|count_star|sum_rows_sent\nSELECT $1 AS probe_value|3|3\nSELECT 1".to_owned()]);
        texts(&mut session, "SELECT pg_stat_statements_reset()");
        assert_eq!(
            texts(
                &mut session,
                "SELECT count(*) FROM pg_stat_statements WHERE query LIKE '%probe_value'"
            ),
            vec!["count\n0\nSELECT 1".to_owned()]
        );
        collect(0);
    }

    #[test]
    fn rows_come_from_the_result_or_its_tag() {
        let changed = crate::session::QueryResult::command("UPDATE 7");
        assert_eq!(rows_of(&changed), 7);
    }
}
