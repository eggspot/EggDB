use std::fmt::Write;
use std::ops::Bound;
use std::sync::Arc;
use std::time::Duration;

use eggdb::Database;

use crate::catalog::{KEY_COUNT_LIMIT, KeyCount};
use crate::connection::{ResultTable, StatementFailure};
use crate::grid::ValueKind;
use crate::status_line::{elapsed_text, thousands};

/// How many keys a keys tab's scan reads.
pub const KEY_SCAN_LIMIT: usize = 100;
/// The keys grid's columns.
pub const KEY_COLUMNS: [&str; 3] = ["key", "value", "bytes"];
/// How many bytes of a value its cell spells, before `…`.
pub const SHOWN_VALUE_BYTES: usize = 64;

/// One scanned key with the start of its value: the rest is dropped as the scan reads it, so
/// huge values never pile up in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEntry {
    /// The whole key.
    pub key: Vec<u8>,
    /// The value's first [`SHOWN_VALUE_BYTES`] bytes.
    pub value_start: Vec<u8>,
    /// The whole value's size in bytes.
    pub value_bytes: usize,
}

impl KeyEntry {
    /// Keeps `key` and the start of `value`.
    pub fn new(key: Vec<u8>, mut value: Vec<u8>) -> Self {
        let value_bytes = value.len();
        value.truncate(SHOWN_VALUE_BYTES);
        value.shrink_to_fit();
        Self {
            key,
            value_start: value,
            value_bytes,
        }
    }

    fn value_cell(&self) -> String {
        let mut cell = spelled(&self.value_start);
        if self.value_bytes > self.value_start.len() {
            cell.push('…');
        }
        cell
    }
}

/// Where a keys page starts (`docs/ux/workspace.md` 10.10).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PageStart {
    /// The space's first key.
    #[default]
    First,
    /// After this key, in key order.
    After(Vec<u8>),
    /// A server's `SCAN` cursor, with the keys an earlier reply gave past the page's end,
    /// which come first.
    Cursor {
        /// The cursor the next `SCAN` sends.
        cursor: u64,
        /// The keys carried over from the earlier reply.
        carried: CarriedKeys,
        /// The keys the earlier pages showed, which a `SCAN` may return again and this page
        /// leaves out.
        shown: Option<Arc<ShownKeys>>,
    },
}

/// The keys a server's reply gave past a page's end, shared by every later page's start, so
/// a reply's keys are held once however many pages they fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarriedKeys {
    keys: Arc<[Vec<u8>]>,
    from: usize,
}

impl Default for CarriedKeys {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl CarriedKeys {
    /// Carries `keys`, in the order a reply gave them.
    pub fn new(keys: Vec<Vec<u8>>) -> Self {
        Self {
            keys: keys.into(),
            from: 0,
        }
    }

    /// The keys not yet shown.
    pub fn remaining(&self) -> &[Vec<u8>] {
        self.keys.get(self.from..).unwrap_or_default()
    }

    /// How many keys are not yet shown.
    pub fn len(&self) -> usize {
        self.remaining().len()
    }

    /// Whether every carried key was shown.
    pub fn is_empty(&self) -> bool {
        self.remaining().is_empty()
    }

    /// The same keys, once a page showed `shown` more of them.
    pub fn after(&self, shown: usize) -> Self {
        Self {
            keys: Arc::clone(&self.keys),
            from: self.from.saturating_add(shown).min(self.keys.len()),
        }
    }
}

/// The keys one page showed, linked to the pages before it, so every later page's start shares
/// them instead of copying them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownKeys {
    /// The page's keys.
    pub page: Vec<Vec<u8>>,
    /// The pages before it.
    pub earlier: Option<Arc<ShownKeys>>,
    pages: usize,
    bytes: usize,
}

/// How many earlier pages a server page checks its keys against.
pub const REMEMBERED_PAGES: usize = 1_000;
/// How many bytes of keys a server page checks its keys against, past the newest page.
pub const REMEMBERED_BYTES: usize = 32 * 1024 * 1024;

fn page_bytes(page: &[Vec<u8>]) -> usize {
    page.iter()
        .fold(0_usize, |total, key| total.saturating_add(key.len()))
}

impl ShownKeys {
    /// `page`, shown after the `earlier` pages. Once the chain holds twice the pages or bytes
    /// [`ShownKeys::remembered`] reads, it is rebuilt from the ones it still reads, so paging
    /// on never grows it.
    pub fn linked(page: Vec<Vec<u8>>, earlier: Option<Arc<ShownKeys>>) -> Arc<Self> {
        let earlier = match earlier {
            Some(chain)
                if chain.pages >= 2 * REMEMBERED_PAGES || chain.bytes > 2 * REMEMBERED_BYTES =>
            {
                chain.trimmed()
            }
            other => other,
        };
        Arc::new(Self::on(page, earlier))
    }

    fn on(page: Vec<Vec<u8>>, earlier: Option<Arc<ShownKeys>>) -> Self {
        let (pages, bytes) = earlier
            .as_deref()
            .map_or((0, 0), |chain| (chain.pages, chain.bytes));
        Self {
            bytes: bytes.saturating_add(page_bytes(&page)),
            pages: pages.saturating_add(1),
            page,
            earlier,
        }
    }

    fn chain(&self) -> impl Iterator<Item = &ShownKeys> {
        std::iter::successors(Some(self), |shown| shown.earlier.as_deref())
    }

    fn trimmed(&self) -> Option<Arc<ShownKeys>> {
        let mut kept: Vec<&Vec<Vec<u8>>> = Vec::new();
        let mut bytes = 0_usize;
        for shown in self.chain() {
            bytes = bytes.saturating_add(page_bytes(&shown.page));
            if kept.len() + 1 >= REMEMBERED_PAGES || bytes > REMEMBERED_BYTES {
                break;
            }
            kept.push(&shown.page);
        }
        kept.into_iter().rev().fold(None, |earlier, page| {
            Some(Arc::new(Self::on(page.clone(), earlier)))
        })
    }

    /// The keys of this page and of the pages before it, newest first, up to
    /// [`REMEMBERED_PAGES`] pages and, past this page, [`REMEMBERED_BYTES`] bytes.
    pub fn remembered(&self) -> impl Iterator<Item = &Vec<u8>> {
        let mut bytes = 0_usize;
        self.chain()
            .take(REMEMBERED_PAGES)
            .enumerate()
            .take_while(move |(index, shown)| {
                bytes = bytes.saturating_add(page_bytes(&shown.page));
                *index == 0 || bytes <= REMEMBERED_BYTES
            })
            .flat_map(|(_, shown)| shown.page.iter())
    }
}

impl Drop for ShownKeys {
    fn drop(&mut self) {
        let mut next = self.earlier.take();
        while let Some(shown) = next {
            next = Arc::into_inner(shown).and_then(|mut only| only.earlier.take());
        }
    }
}

/// Leaves out the shown keys of every start in `starts` more than [`REMEMBERED_PAGES`] back,
/// so a long `Back` history holds only cursors: a page read again from there may repeat a key
/// a page before it showed.
pub fn forget_old_shown_keys(starts: &mut [PageStart]) {
    let Some(old) = starts.len().checked_sub(REMEMBERED_PAGES + 1) else {
        return;
    };
    if let Some(PageStart::Cursor { shown, .. }) = starts.get_mut(old) {
        *shown = None;
    }
}

/// What reading a page learnt beyond its rows: where the next page starts (`None` on the
/// last page) and, from a server, the space's count read with it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageEnd {
    /// Where the next page starts, when more keys exist.
    pub next: Option<PageStart>,
    /// The space's key count read with the page (a server's `DBSIZE`).
    pub total: Option<KeyCount>,
}

/// One page a keys tab read: the grid's table and what follows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPage {
    /// The page as the keys grid shows it.
    pub table: ResultTable,
    /// Where the next page starts.
    pub end: PageEnd,
}

/// The key space a keys tab scans (`docs/ux/workspace.md` 10.4, 10.10), the prefix that
/// filters it, where the page starts and how many keys the catalog counted in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySpaceScan {
    /// The collection; `None` for the default key space.
    pub collection: Option<String>,
    /// The catalog's count of its keys, which the banner and the summary give.
    pub total: KeyCount,
    /// The bytes every listed key starts with; empty for every key.
    pub prefix: Vec<u8>,
    /// Where the page starts.
    pub start: PageStart,
    /// The 1-based position of the page's first key among the pages as read.
    pub first_position: usize,
    /// Whether more keys follow the page, once it was read.
    pub more: bool,
    /// Whether the space is a server's RESP database, scanned in the server's order.
    pub over_resp: bool,
}

/// A prefix field's text that does not spell bytes, with the line shown under the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrefixError {
    /// The line under the field, `Error: \x needs two hex digits`.
    pub message: String,
}

/// The bytes a prefix field's text spells in `docs/ux/ui.md`'s spelling: `\xNN` is a byte,
/// `\\` a backslash, `\t` `\n` `\r` those bytes, everything else its UTF-8.
pub fn unspelled(text: &str) -> Result<Vec<u8>, PrefixError> {
    let mut out = Vec::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            let mut buffer = [0_u8; 4];
            out.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            continue;
        }
        match characters.next() {
            Some('\\') => out.push(b'\\'),
            Some('t') => out.push(b'\t'),
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('x') => {
                let digits: String = characters.by_ref().take(2).collect();
                let byte = (digits.len() == 2)
                    .then(|| u8::from_str_radix(&digits, 16).ok())
                    .flatten()
                    .ok_or_else(|| PrefixError {
                        message: "Error: \\x needs two hex digits".to_owned(),
                    })?;
                out.push(byte);
            }
            Some(other) => {
                return Err(PrefixError {
                    message: format!("Error: \\{other} is not an escape; \\\\ is one backslash"),
                });
            }
            None => {
                return Err(PrefixError {
                    message: "Error: a lone \\ needs a second \\".to_owned(),
                });
            }
        }
    }
    Ok(out)
}

/// `bytes` quoted for the editor's line: ui.md's spelling between `'`, each `'` doubled.
pub fn quoted(bytes: &[u8]) -> String {
    format!("'{}'", spelled(bytes).replace('\'', "''"))
}

fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xFF {
            end.push(last + 1);
            return Some(end);
        }
    }
    None
}

impl KeySpaceScan {
    /// The first page of `collection`'s keys (the default space for `None`), which the
    /// catalog counted as `total`.
    pub fn new(collection: Option<String>, total: KeyCount) -> Self {
        Self {
            collection,
            total,
            prefix: Vec::new(),
            start: PageStart::First,
            first_position: 1,
            more: false,
            over_resp: false,
        }
    }

    /// The tab's title: `Keys`, or `Keys in users` for a collection.
    pub fn title(&self) -> String {
        match &self.collection {
            Some(name) => format!("Keys in {name}"),
            None => "Keys".to_owned(),
        }
    }

    /// The read-only editor's line, stating what ran: `SCAN users LIMIT 100`, with
    /// `PREFIX 'user:'` for a filter and `AFTER 'user:0100'` or `CURSOR 1532` past page 1.
    pub fn statement(&self) -> String {
        let mut line = "SCAN".to_owned();
        if let Some(name) = &self.collection {
            line.push(' ');
            line.push_str(name);
        }
        if !self.prefix.is_empty() {
            line.push_str(" PREFIX ");
            line.push_str(&quoted(&self.prefix));
        }
        match &self.start {
            PageStart::First => {}
            PageStart::After(key) => {
                line.push_str(" AFTER ");
                line.push_str(&quoted(key));
            }
            PageStart::Cursor { cursor, .. } => {
                let _ = write!(line, " CURSOR {cursor}");
            }
        }
        let _ = write!(line, " LIMIT {KEY_SCAN_LIMIT}");
        line
    }

    /// The catalog's count as text: `1,204`, or `100,000+` past the catalog's limit.
    pub fn total_text(&self) -> String {
        match self.total {
            KeyCount::Exactly(count) => thousands(usize::try_from(count).unwrap_or(usize::MAX)),
            KeyCount::MoreThanLimit => format!(
                "{}+",
                thousands(usize::try_from(KEY_COUNT_LIMIT).unwrap_or(usize::MAX))
            ),
        }
    }

    /// Whether the space holds more keys than the `shown` ones, by the catalog's count.
    pub fn has_more_than(&self, shown: usize) -> bool {
        match self.total {
            KeyCount::Exactly(count) => count > shown as u64,
            KeyCount::MoreThanLimit => true,
        }
    }

    /// How many rows the grid says the result has: on an unfiltered page 1 the catalog's
    /// count; otherwise the shown keys, one more when more follow.
    pub fn row_count(&self, shown: usize) -> usize {
        if self.first_position == 1 && self.prefix.is_empty() && self.more {
            return match self.total {
                KeyCount::Exactly(count) => usize::try_from(count)
                    .unwrap_or(usize::MAX)
                    .max(shown.saturating_add(1)),
                KeyCount::MoreThanLimit => shown.saturating_add(1),
            };
        }
        shown.saturating_add(usize::from(self.more))
    }

    fn page_range(&self, shown: usize) -> String {
        let last = self.first_position.saturating_add(shown).saturating_sub(1);
        format!("{}–{}", thousands(self.first_position), thousands(last))
    }

    fn order_words(&self) -> &'static str {
        if self.over_resp {
            "in the server's order"
        } else {
            "in key order"
        }
    }

    /// The status line (`docs/ux/workspace.md` 10.10): `12 keys · 0.3 ms`, `100 keys shown
    /// of 1,204 · 0.6 ms`, `Keys 101–200 of 1,204 · 0.5 ms`, `0 keys start with 'user:' ·
    /// 0.2 ms`.
    pub fn summary(&self, shown: usize, elapsed: Duration) -> String {
        let elapsed = elapsed_text(elapsed);
        let filtered = !self.prefix.is_empty();
        if self.first_position > 1 && shown > 0 {
            return if filtered {
                format!("Keys {} · {elapsed}", self.page_range(shown))
            } else {
                format!(
                    "Keys {} of {} · {elapsed}",
                    self.page_range(shown),
                    self.total_text()
                )
            };
        }
        if shown == 0 && filtered {
            return format!("0 keys start with {} · {elapsed}", quoted(&self.prefix));
        }
        match (self.more, filtered) {
            (true, false) => format!(
                "{} keys shown of {} · {elapsed}",
                thousands(shown),
                self.total_text()
            ),
            (true, true) => format!("{} keys shown · {elapsed}", thousands(shown)),
            (false, _) => format!("{} · {elapsed}", keys_text(shown)),
        }
    }

    /// The banner over a page more keys follow: `Showing the first 100 keys of 1,204 in key
    /// order.`, `Showing keys 101–200 starting with 'user:' in key order.`
    pub fn banner(&self, shown: usize) -> String {
        let order = self.order_words();
        let filter = if self.prefix.is_empty() {
            format!("of {}", self.total_text())
        } else {
            format!("starting with {}", quoted(&self.prefix))
        };
        if self.first_position > 1 {
            format!("Showing keys {} {filter} {order}.", self.page_range(shown))
        } else {
            format!(
                "Showing the first {} keys {filter} {order}.",
                thousands(shown)
            )
        }
    }

    /// The grid's first body row on a filtered page with no keys: `No keys start with
    /// 'user:'`.
    pub fn empty_text(&self) -> Option<String> {
        (!self.prefix.is_empty()).then(|| format!("No keys start with {}", quoted(&self.prefix)))
    }

    /// The scan's page as the keys grid shows it, from the first [`KEY_SCAN_LIMIT`] of
    /// `entries` (the scan reads one more to learn whether more exist).
    pub fn table(&self, entries: &[KeyEntry]) -> ResultTable {
        let rows: Vec<Vec<Option<String>>> = entries
            .iter()
            .take(KEY_SCAN_LIMIT)
            .map(|entry| {
                vec![
                    Some(spelled(&entry.key)),
                    Some(entry.value_cell()),
                    Some(thousands(entry.value_bytes)),
                ]
            })
            .collect();
        let shown = rows.len();
        ResultTable {
            columns: KEY_COLUMNS.map(str::to_owned).to_vec(),
            kinds: vec![
                ValueKind::SpelledBytes,
                ValueKind::SpelledBytes,
                ValueKind::Number,
            ],
            tag: format!("SCAN {shown}"),
            row_count: Self {
                more: self.more || entries.len() > KEY_SCAN_LIMIT,
                ..self.clone()
            }
            .row_count(shown),
            rows,
        }
    }

    /// The page with its end, from the entries read past its start (at most one more than a
    /// page).
    pub fn page(&self, entries: &[KeyEntry]) -> KeyPage {
        let more = entries.len() > KEY_SCAN_LIMIT;
        let next = more
            .then(|| entries.get(KEY_SCAN_LIMIT - 1))
            .flatten()
            .map(|last| PageStart::After(last.key.clone()));
        KeyPage {
            table: self.table(entries),
            end: PageEnd { next, total: None },
        }
    }

    /// Reads the page's keys in key order from an open file, in one short read transaction:
    /// those starting with the prefix, after the page's start, one more than a page.
    pub fn scan_file(&self, database: &Database) -> Result<KeyPage, StatementFailure> {
        let entries = self
            .read_entries(database)
            .map_err(|error| StatementFailure::from_engine(&error))?;
        Ok(self.page(&entries))
    }

    fn bounds(&self) -> (Bound<Vec<u8>>, Bound<Vec<u8>>) {
        let lower = match &self.start {
            PageStart::After(key) if key.as_slice() >= self.prefix.as_slice() => {
                Bound::Excluded(key.clone())
            }
            _ if self.prefix.is_empty() => Bound::Unbounded,
            _ => Bound::Included(self.prefix.clone()),
        };
        let mut upper = prefix_end(&self.prefix);
        if self.collection.is_none() {
            upper = Some(match upper {
                Some(end) if end.as_slice() < [0xFF_u8].as_slice() => end,
                _ => vec![0xFF],
            });
        }
        let upper = if self.prefix.is_empty() && self.collection.is_some() {
            Bound::Unbounded
        } else {
            upper.map_or(Bound::Unbounded, Bound::Excluded)
        };
        (lower, upper)
    }

    fn read_entries(&self, database: &Database) -> eggdb::Result<Vec<KeyEntry>> {
        let transaction = database.begin_read()?;
        let kept = |entry: eggdb::Result<(Vec<u8>, Vec<u8>)>| {
            entry.map(|(key, value)| KeyEntry::new(key, value))
        };
        let bounds = self.bounds();
        match &self.collection {
            None => transaction
                .scan(bounds)
                .take(KEY_SCAN_LIMIT + 1)
                .map(kept)
                .collect(),
            Some(name) => {
                let collection = transaction.collection(name)?;
                collection
                    .scan(bounds)
                    .take(KEY_SCAN_LIMIT + 1)
                    .map(kept)
                    .collect()
            }
        }
    }
}

/// A keys tab's prefix field and the pages it has shown (`docs/ux/workspace.md` 10.10).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyPages {
    /// The prefix field's text, in ui.md's spelling.
    pub prefix_text: String,
    /// Whether the field has the focus.
    pub prefix_focused: bool,
    /// Whether the field's text is selected (it is when the field takes the focus).
    pub prefix_selected: bool,
    /// The prefix that was active when the field took the focus, which `Esc` restores.
    pub prefix_on_focus: String,
    /// The start of every page shown before the current one, page 1's first.
    pub earlier: Vec<PageStart>,
    /// Where the next page starts, when more keys exist.
    pub next: Option<PageStart>,
    /// The pages before the one being read, adopted once it arrives.
    pub pending_earlier: Option<Vec<PageStart>>,
    /// When the typed prefix runs, 150 ms after the last key.
    pub due: Option<std::time::Instant>,
    /// Which row the current cell goes to when the page arrives: the first after `Next 100`,
    /// the last after `Back`.
    pub land_on_last: bool,
}

impl KeyPages {
    /// The field's error line, when its text does not spell bytes.
    pub fn prefix_error(&self) -> Option<String> {
        unspelled(&self.prefix_text)
            .err()
            .map(|error| error.message)
    }

    /// Whether `Back` acts: a page before this one exists.
    pub fn can_go_back(&self) -> bool {
        !self.earlier.is_empty()
    }

    /// Whether `Next 100` acts: the page read 101 entries.
    pub fn can_go_next(&self) -> bool {
        self.next.is_some()
    }
}

/// `1 key`, `0 keys`, `12 keys`.
pub fn keys_text(count: usize) -> String {
    match count {
        1 => "1 key".to_owned(),
        _ => format!("{} keys", thousands(count)),
    }
}

/// Bytes as `docs/ux/ui.md` spells them: valid printable UTF-8 as it is, `\t` `\n` `\r` and
/// `\\` escaped, and every control, invisible format (bidirectional overrides and isolates,
/// zero-width characters, the byte order mark, tags) or invalid byte as `\xNN`, so no key can
/// draw as another.
pub fn spelled(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            match character {
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\\' => out.push_str("\\\\"),
                hidden if hidden.is_control() || is_invisible_format(hidden) => {
                    let mut buffer = [0_u8; 4];
                    for byte in hidden.encode_utf8(&mut buffer).bytes() {
                        let _ = write!(out, "\\x{byte:02x}");
                    }
                }
                printable => out.push(printable),
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02x}");
        }
    }
    out
}

fn is_invisible_format(character: char) -> bool {
    matches!(
        character,
        '\u{00AD}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users(total: KeyCount) -> KeySpaceScan {
        KeySpaceScan::new(Some("users".to_owned()), total)
    }

    #[test]
    fn a_key_space_is_titled_and_scanned_as_the_design_words_it() {
        let default = KeySpaceScan::new(None, KeyCount::Exactly(3));
        assert_eq!(default.title(), "Keys");
        assert_eq!(default.statement(), "SCAN LIMIT 100");
        assert_eq!(users(KeyCount::Exactly(3)).title(), "Keys in users");
        assert_eq!(
            users(KeyCount::Exactly(3)).statement(),
            "SCAN users LIMIT 100"
        );
    }

    #[test]
    fn bytes_are_spelled_as_text_with_escapes_for_everything_unprintable() {
        assert_eq!(spelled(b"user:42"), "user:42");
        assert_eq!(spelled("café".as_bytes()), "café");
        assert_eq!(spelled(b"a\tb\nc\rd\\e"), "a\\tb\\nc\\rd\\\\e");
        assert_eq!(spelled(&[0x00, b'k', 0xFF, 0x7F]), "\\x00k\\xff\\x7f");
        assert_eq!(
            spelled("user\u{202E}gpj.exe".as_bytes()),
            "user\\xe2\\x80\\xaegpj.exe"
        );
        assert_eq!(spelled("a\u{200B}b".as_bytes()), "a\\xe2\\x80\\x8bb");
        assert_eq!(
            spelled("\u{2066}x\u{2069}".as_bytes()),
            "\\xe2\\x81\\xa6x\\xe2\\x81\\xa9"
        );
        assert_eq!(spelled("\u{FEFF}k".as_bytes()), "\\xef\\xbb\\xbfk");
    }

    #[test]
    fn the_summary_and_banner_count_the_shown_keys_against_the_catalog() {
        let elapsed = Duration::from_micros(300);
        assert_eq!(
            users(KeyCount::Exactly(12)).summary(12, elapsed),
            "12 keys · 0.3 ms"
        );
        assert_eq!(
            users(KeyCount::Exactly(1)).summary(1, elapsed),
            "1 key · 0.3 ms"
        );
        assert_eq!(
            users(KeyCount::Exactly(0)).summary(0, Duration::from_micros(200)),
            "0 keys · 0.2 ms"
        );
        let many = KeySpaceScan {
            more: true,
            ..users(KeyCount::Exactly(1204))
        };
        assert_eq!(
            many.summary(100, Duration::from_micros(600)),
            "100 keys shown of 1,204 · 0.6 ms"
        );
        assert_eq!(
            many.banner(100),
            "Showing the first 100 keys of 1,204 in key order."
        );
        assert_eq!(many.row_count(100), 1204);
        let past_limit = KeySpaceScan {
            more: true,
            ..users(KeyCount::MoreThanLimit)
        };
        assert!(past_limit.has_more_than(100));
        assert_eq!(past_limit.row_count(100), 101);
        assert_eq!(past_limit.total_text(), "100,000+");
    }

    #[test]
    fn the_table_has_a_key_value_and_bytes_column_and_a_scan_tag() {
        let entries = vec![
            KeyEntry::new(b"a".to_vec(), b"{\"n\":1}".to_vec()),
            KeyEntry::new(b"b\x01".to_vec(), Vec::new()),
            KeyEntry::new(b"c".to_vec(), vec![b'z'; 16 * 1024 * 1024]),
        ];
        let table = users(KeyCount::Exactly(3)).table(&entries);
        assert_eq!(table.columns, ["key", "value", "bytes"]);
        assert_eq!(table.kinds[2], ValueKind::Number);
        assert_eq!(entries[2].value_start.len(), SHOWN_VALUE_BYTES);
        assert_eq!(table.tag, "SCAN 3");
        assert_eq!(table.row_count, 3);
        assert_eq!(
            table.rows[0],
            [
                Some("a".to_owned()),
                Some("{\"n\":1}".to_owned()),
                Some("7".to_owned())
            ]
        );
        assert_eq!(
            table.rows[1],
            [
                Some("b\\x01".to_owned()),
                Some(String::new()),
                Some("0".to_owned())
            ]
        );
        assert_eq!(
            table.rows[2],
            [
                Some("c".to_owned()),
                Some(format!("{}…", "z".repeat(SHOWN_VALUE_BYTES))),
                Some("16,777,216".to_owned())
            ]
        );
    }

    #[test]
    fn a_file_scan_reads_the_first_hundred_keys_of_a_space_in_key_order() {
        let path =
            std::env::temp_dir().join(format!("eggdb-gui-key-scan-{}.eggdb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let database = Database::open(&path).expect("the file opens");
        database
            .write(|transaction| {
                transaction.put("greeting", "hello")?;
                transaction.create_collection("users")?;
                let mut users = transaction.collection("users")?;
                for number in 0..150_u32 {
                    users.put(format!("user:{number:03}"), "x")?;
                }
                Ok(())
            })
            .expect("the keys are written");
        let page = users(KeyCount::Exactly(150))
            .scan_file(&database)
            .expect("the scan reads");
        assert_eq!(page.end.next, Some(PageStart::After(b"user:099".to_vec())));
        let table = page.table;
        assert_eq!(table.rows.len(), KEY_SCAN_LIMIT);
        assert_eq!(table.row_count, 150);
        assert_eq!(table.rows[0][0].as_deref(), Some("user:000"));
        assert_eq!(table.rows[99][0].as_deref(), Some("user:099"));
        let default = KeySpaceScan::new(None, KeyCount::Exactly(1))
            .scan_file(&database)
            .expect("the default space reads")
            .table;
        assert_eq!(default.rows.len(), 1);
        assert_eq!(default.rows[0][1].as_deref(), Some("hello"));
        drop(database);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_prefix_field_spells_bytes_and_rejects_a_bad_escape() {
        assert_eq!(unspelled("user:").as_deref(), Ok(&b"user:"[..]));
        assert_eq!(unspelled("\\x00a").as_deref(), Ok(&b"\x00a"[..]));
        assert_eq!(unspelled("\\\\").as_deref(), Ok(&b"\\"[..]));
        assert_eq!(
            unspelled("\\x4").map_err(|error| error.message),
            Err("Error: \\x needs two hex digits".to_owned())
        );
        assert!(unspelled("a\\").is_err());
        assert_eq!(quoted(b"it's"), "'it''s'");
    }

    #[test]
    fn the_line_status_and_banner_follow_the_prefix_and_the_page() {
        let elapsed = Duration::from_micros(500);
        let mut scan = users(KeyCount::Exactly(1204));
        scan.prefix = b"user:".to_vec();
        assert_eq!(scan.statement(), "SCAN users PREFIX 'user:' LIMIT 100");
        scan.more = true;
        assert_eq!(scan.summary(100, elapsed), "100 keys shown · 0.5 ms");
        assert_eq!(
            scan.banner(100),
            "Showing the first 100 keys starting with 'user:' in key order."
        );
        scan.start = PageStart::After(b"user:0100".to_vec());
        scan.first_position = 101;
        assert_eq!(
            scan.statement(),
            "SCAN users PREFIX 'user:' AFTER 'user:0100' LIMIT 100"
        );
        assert_eq!(scan.summary(100, elapsed), "Keys 101–200 · 0.5 ms");
        assert_eq!(
            scan.banner(100),
            "Showing keys 101–200 starting with 'user:' in key order."
        );
        scan.prefix.clear();
        assert_eq!(scan.summary(100, elapsed), "Keys 101–200 of 1,204 · 0.5 ms");
        assert_eq!(
            scan.banner(100),
            "Showing keys 101–200 of 1,204 in key order."
        );
        scan.first_position = 1201;
        scan.more = false;
        assert_eq!(
            scan.summary(4, Duration::from_micros(200)),
            "Keys 1,201–1,204 of 1,204 · 0.2 ms"
        );
        assert_eq!(scan.row_count(4), 4);
        let mut none = users(KeyCount::Exactly(3));
        none.prefix = b"user:".to_vec();
        assert_eq!(
            none.summary(0, Duration::from_micros(200)),
            "0 keys start with 'user:' · 0.2 ms"
        );
        assert_eq!(
            none.empty_text().as_deref(),
            Some("No keys start with 'user:'")
        );
        let mut server = KeySpaceScan::new(Some("resp3".to_owned()), KeyCount::Exactly(1204));
        server.over_resp = true;
        server.prefix = b"user:".to_vec();
        server.start = PageStart::Cursor {
            cursor: 1532,
            carried: CarriedKeys::default(),
            shown: None,
        };
        assert_eq!(
            server.statement(),
            "SCAN resp3 PREFIX 'user:' CURSOR 1532 LIMIT 100"
        );
        assert!(server.banner(100).ends_with("in the server's order."));
    }

    #[test]
    fn a_prefix_and_pages_read_a_file_with_more_than_a_hundred_keys() {
        let path =
            std::env::temp_dir().join(format!("eggdb-gui-key-pages-{}.eggdb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let database = Database::open(&path).expect("the file opens");
        database
            .write(|transaction| {
                for number in 0..250_u32 {
                    transaction.put(format!("user:{number:04}"), "u")?;
                }
                for number in 0..20_u32 {
                    transaction.put(format!("zone:{number:02}"), "z")?;
                }
                transaction.put([0_u8, 1], "zero")?;
                Ok(())
            })
            .expect("the keys are written");
        let mut scan = KeySpaceScan::new(None, KeyCount::Exactly(271));
        scan.prefix = b"user:".to_vec();
        let first = scan.scan_file(&database).expect("page 1 reads");
        assert_eq!(first.table.rows.len(), 100);
        assert_eq!(first.table.rows[0][0].as_deref(), Some("user:0000"));
        let Some(next) = first.end.next else {
            panic!("more keys start with user:");
        };
        scan.start = next;
        scan.first_position = 101;
        let second = scan.scan_file(&database).expect("page 2 reads");
        assert_eq!(second.table.rows[0][0].as_deref(), Some("user:0100"));
        scan.start = second.end.next.expect("a third page exists");
        scan.first_position = 201;
        let third = scan.scan_file(&database).expect("page 3 reads");
        assert_eq!(third.table.rows.len(), 50);
        assert_eq!(third.table.rows[49][0].as_deref(), Some("user:0249"));
        assert_eq!(third.end.next, None);
        let mut zero = KeySpaceScan::new(None, KeyCount::Exactly(271));
        zero.prefix = unspelled("\\x00").expect("an escape spells a byte");
        let zero = zero.scan_file(&database).expect("the zero prefix reads");
        assert_eq!(zero.table.rows.len(), 1);
        assert_eq!(zero.table.rows[0][0].as_deref(), Some("\\x00\\x01"));
        drop(database);
        let _ = std::fs::remove_file(&path);
    }

    fn chain_depth(shown: &ShownKeys) -> usize {
        shown.chain().count()
    }

    #[test]
    fn the_shown_keys_chain_stays_bounded_however_long_one_pages() {
        let mut shown: Option<Arc<ShownKeys>> = None;
        for page in 0..(5 * REMEMBERED_PAGES) {
            shown = Some(ShownKeys::linked(
                vec![page.to_string().into_bytes()],
                shown,
            ));
            let depth = shown.as_deref().map_or(0, chain_depth);
            assert!(depth <= 2 * REMEMBERED_PAGES, "{depth} pages linked");
        }
        let newest = shown.expect("pages were shown");
        let remembered: Vec<&Vec<u8>> = newest.remembered().collect();
        assert_eq!(remembered.len(), REMEMBERED_PAGES);
        assert_eq!(
            remembered[0],
            &(5 * REMEMBERED_PAGES - 1).to_string().into_bytes()
        );
        let half = || vec![vec![b'k'; REMEMBERED_BYTES / 2 + 1]];
        let big = ShownKeys::linked(half(), Some(ShownKeys::linked(half(), None)));
        assert_eq!(big.remembered().count(), 1);
        let after_big = ShownKeys::linked(vec![b"a".to_vec()], Some(big));
        assert_eq!(after_big.remembered().count(), 2);
    }

    #[test]
    fn a_very_long_shown_keys_chain_drops_without_recursing() {
        let dropped = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(|| {
                let mut shown: Option<Arc<ShownKeys>> = None;
                for _ in 0..300_000 {
                    shown = Some(Arc::new(ShownKeys::on(vec![b"k".to_vec()], shown)));
                }
                drop(shown);
            })
            .expect("the thread starts")
            .join();
        assert!(dropped.is_ok());
    }

    #[test]
    fn carried_keys_are_shared_by_every_later_start() {
        let carried =
            CarriedKeys::new((0..250_u32).map(|key| key.to_be_bytes().to_vec()).collect());
        let next = carried.after(KEY_SCAN_LIMIT);
        assert_eq!(next.len(), 150);
        assert_eq!(next.remaining()[0], 100_u32.to_be_bytes().to_vec());
        assert!(Arc::ptr_eq(&carried.keys, &next.keys));
        assert!(next.after(1_000).is_empty());
        assert!(CarriedKeys::default().is_empty());
    }

    #[test]
    fn starts_further_back_than_the_remembered_pages_forget_their_shown_keys() {
        let shown = Some(ShownKeys::linked(vec![b"a".to_vec()], None));
        let start = PageStart::Cursor {
            cursor: 1,
            carried: CarriedKeys::default(),
            shown,
        };
        let mut starts = vec![start; REMEMBERED_PAGES + 1];
        forget_old_shown_keys(&mut starts);
        assert!(matches!(&starts[0], PageStart::Cursor { shown: None, .. }));
        assert!(matches!(
            &starts[1],
            PageStart::Cursor { shown: Some(_), .. }
        ));
        let mut few = starts[1..].to_vec();
        forget_old_shown_keys(&mut few);
        assert!(matches!(&few[0], PageStart::Cursor { shown: Some(_), .. }));
    }
}
