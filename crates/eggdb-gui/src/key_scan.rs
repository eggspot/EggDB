use std::fmt::Write;
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

/// The key space a keys tab scans (`docs/ux/workspace.md` 10.4) and how many keys the
/// catalog counted in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySpaceScan {
    /// The collection; `None` for the default key space.
    pub collection: Option<String>,
    /// The catalog's count of its keys, which the banner and the summary give.
    pub total: KeyCount,
}

impl KeySpaceScan {
    /// The tab's title: `Keys`, or `Keys in users` for a collection.
    pub fn title(&self) -> String {
        match &self.collection {
            Some(name) => format!("Keys in {name}"),
            None => "Keys".to_owned(),
        }
    }

    /// The read-only editor's line: `SCAN LIMIT 100`, or `SCAN users LIMIT 100`.
    pub fn statement(&self) -> String {
        match &self.collection {
            Some(name) => format!("SCAN {name} LIMIT {KEY_SCAN_LIMIT}"),
            None => format!("SCAN LIMIT {KEY_SCAN_LIMIT}"),
        }
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

    /// Whether the space holds more keys than the `shown` ones.
    pub fn has_more_than(&self, shown: usize) -> bool {
        match self.total {
            KeyCount::Exactly(count) => count > shown as u64,
            KeyCount::MoreThanLimit => true,
        }
    }

    /// How many rows the grid says the result has: the catalog's count, or one past the
    /// shown keys when it only knows there are more than its limit.
    pub fn row_count(&self, shown: usize) -> usize {
        match self.total {
            KeyCount::Exactly(count) => usize::try_from(count).unwrap_or(usize::MAX).max(shown),
            KeyCount::MoreThanLimit => shown.saturating_add(1),
        }
    }

    /// The status line: `12 keys · 0.3 ms`, or `100 keys shown of 1,204 · 0.6 ms`.
    pub fn summary(&self, shown: usize, elapsed: Duration) -> String {
        if self.has_more_than(shown) {
            format!(
                "{} keys shown of {} · {}",
                thousands(shown),
                self.total_text(),
                elapsed_text(elapsed)
            )
        } else {
            format!("{} · {}", keys_text(shown), elapsed_text(elapsed))
        }
    }

    /// The banner over a cut scan: `Showing the first 100 keys of 1,204 in key order.`
    pub fn banner(&self, shown: usize) -> String {
        format!(
            "Showing the first {} keys of {} in key order.",
            thousands(shown),
            self.total_text()
        )
    }

    /// The scan's answer as the keys grid shows it, from the space's first keys in key order.
    pub fn table(&self, entries: &[(Vec<u8>, Vec<u8>)]) -> ResultTable {
        let rows: Vec<Vec<Option<String>>> = entries
            .iter()
            .take(KEY_SCAN_LIMIT)
            .map(|(key, value)| {
                vec![
                    Some(spelled(key)),
                    Some(spelled(value)),
                    Some(thousands(value.len())),
                ]
            })
            .collect();
        ResultTable {
            columns: KEY_COLUMNS.map(str::to_owned).to_vec(),
            kinds: vec![
                ValueKind::SpelledBytes,
                ValueKind::SpelledBytes,
                ValueKind::Number,
            ],
            tag: format!("SCAN {}", rows.len()),
            row_count: self.row_count(rows.len()),
            rows,
        }
    }

    /// Reads the space's first [`KEY_SCAN_LIMIT`] keys in key order from an open file, in one
    /// short read transaction.
    pub fn scan_file(&self, database: &Database) -> Result<ResultTable, StatementFailure> {
        let entries = self
            .read_entries(database)
            .map_err(|error| StatementFailure::from_engine(&error))?;
        Ok(self.table(&entries))
    }

    fn read_entries(&self, database: &Database) -> eggdb::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let transaction = database.begin_read()?;
        match &self.collection {
            None => transaction
                .scan(..[0xFF_u8].as_slice())
                .take(KEY_SCAN_LIMIT)
                .collect(),
            Some(name) => {
                let collection = transaction.collection(name)?;
                collection.scan(..).take(KEY_SCAN_LIMIT).collect()
            }
        }
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
/// `\\` escaped, and every control or invalid byte as `\xNN`.
pub fn spelled(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        for character in chunk.valid().chars() {
            match character {
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\\' => out.push_str("\\\\"),
                control if control.is_control() => {
                    let mut buffer = [0_u8; 4];
                    for byte in control.encode_utf8(&mut buffer).bytes() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn users(total: KeyCount) -> KeySpaceScan {
        KeySpaceScan {
            collection: Some("users".to_owned()),
            total,
        }
    }

    #[test]
    fn a_key_space_is_titled_and_scanned_as_the_design_words_it() {
        let default = KeySpaceScan {
            collection: None,
            total: KeyCount::Exactly(3),
        };
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
        let many = users(KeyCount::Exactly(1204));
        assert_eq!(
            many.summary(100, Duration::from_micros(600)),
            "100 keys shown of 1,204 · 0.6 ms"
        );
        assert_eq!(
            many.banner(100),
            "Showing the first 100 keys of 1,204 in key order."
        );
        assert_eq!(many.row_count(100), 1204);
        let past_limit = users(KeyCount::MoreThanLimit);
        assert!(past_limit.has_more_than(100));
        assert_eq!(past_limit.row_count(100), 101);
        assert_eq!(past_limit.total_text(), "100,000+");
    }

    #[test]
    fn the_table_has_a_key_value_and_bytes_column_and_a_scan_tag() {
        let entries = vec![
            (b"a".to_vec(), b"{\"n\":1}".to_vec()),
            (b"b\x01".to_vec(), Vec::new()),
        ];
        let table = users(KeyCount::Exactly(2)).table(&entries);
        assert_eq!(table.columns, ["key", "value", "bytes"]);
        assert_eq!(table.kinds[2], ValueKind::Number);
        assert_eq!(table.tag, "SCAN 2");
        assert_eq!(table.row_count, 2);
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
    }

    #[test]
    fn a_file_scan_reads_the_first_hundred_keys_of_a_space_in_key_order() {
        let path = std::env::temp_dir().join(format!(
            "eggdb-gui-key-scan-{}.eggdb",
            std::process::id()
        ));
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
        let table = users(KeyCount::Exactly(150))
            .scan_file(&database)
            .expect("the scan reads");
        assert_eq!(table.rows.len(), KEY_SCAN_LIMIT);
        assert_eq!(table.row_count, 150);
        assert_eq!(table.rows[0][0].as_deref(), Some("user:000"));
        assert_eq!(table.rows[99][0].as_deref(), Some("user:099"));
        let default = KeySpaceScan {
            collection: None,
            total: KeyCount::Exactly(1),
        }
        .scan_file(&database)
        .expect("the default space reads");
        assert_eq!(default.rows.len(), 1);
        assert_eq!(default.rows[0][1].as_deref(), Some("hello"));
        drop(database);
        let _ = std::fs::remove_file(&path);
    }
}
