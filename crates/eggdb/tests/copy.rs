//! `COPY` on a real database file: a million rows loaded and unloaded byte for byte through
//! the embedded API, and a process killed while a script of `COPY`s runs, after which every
//! `COPY` is whole or absent.

mod common;

use common::TempPath;
use eggdb::sql::{CopyChannel, CopyStart, Dialect, SqlResult};
use eggdb::{Database, Options};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

const MILLION: usize = 1_000_000;
const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Default)]
struct Feed {
    data: Vec<u8>,
    at: usize,
    out: Vec<u8>,
}

impl CopyChannel for Feed {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Ok(())
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        if self.at >= self.data.len() {
            return Ok(false);
        }
        let end = (self.at + CHUNK_BYTES).min(self.data.len());
        data.extend_from_slice(&self.data[self.at..end]);
        self.at = end;
        Ok(true)
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Ok(())
    }

    fn write_copy_data(&mut self, data: &[u8]) -> SqlResult<()> {
        self.out.extend_from_slice(data);
        Ok(())
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Ok(())
    }
}

fn million_rows() -> Vec<u8> {
    let mut data = Vec::with_capacity(MILLION * 40);
    for id in 0..MILLION {
        let name = match id % 97 {
            0 => format!("tab\\there {id}"),
            1 => format!("back\\\\slash\\nline {id}"),
            _ => format!("name {id}"),
        };
        let score = if id % 10 == 0 {
            "\\N".to_owned()
        } else {
            format!("{}.{:02}", id % 1000, id % 100)
        };
        let day = format!("2024-{:02}-{:02}", id % 12 + 1, id % 28 + 1);
        let _ = writeln!(data, "{id}\t{name}\t{score}\t{day}");
    }
    data
}

#[test]
fn a_million_rows_copy_in_and_out_byte_for_byte() {
    let file = TempPath::new("copy-million");
    let db = Database::open_with(file.path(), Options::new().cache_bytes(1 << 30)).unwrap();
    let store = db.sql_store();
    let mut session = store.session(Dialect::PostgreSql);
    session
        .execute(
            "CREATE TABLE big (id integer PRIMARY KEY, name text, score numeric, day date)",
            &mut |_| {},
        )
        .unwrap();
    let mut feed = Feed {
        data: million_rows(),
        ..Feed::default()
    };
    let mut tags = Vec::new();
    session
        .execute_with_copy(
            "COPY big FROM STDIN; COPY big TO STDOUT",
            &mut |result| {
                tags.push(result.tag);
                true
            },
            &mut feed,
        )
        .unwrap();
    assert_eq!(tags, ["COPY 1000000", "COPY 1000000"]);
    assert!(
        feed.out == feed.data,
        "the unloaded rows differ from the loaded ones"
    );
    session.close().unwrap();
    assert_eq!(db.check().unwrap().leaked_pages, 0);
    db.close().unwrap();
}

#[test]
fn a_killed_process_leaves_each_copy_whole_or_absent() {
    const ROUNDS: usize = 300;
    const ROWS: usize = 40;
    let mut script = String::from(
        "CREATE TABLE c (round integer, n integer, note text, PRIMARY KEY (round, n));\n\
         CREATE INDEX c_note ON c (note);\n",
    );
    for round in 0..ROUNDS {
        script.push_str("COPY c FROM STDIN;\n");
        for n in 0..ROWS {
            script.push_str(&format!("{round}\t{n}\tnote {}\n", (round * 7 + n) % 13));
        }
        script.push_str("\\.\n");
    }
    let mut complete_seen = 0;
    for delay_ms in [0u64, 50, 150, 400, 900, 2_000] {
        let file = TempPath::new("copy-killed");
        Database::open(file.path()).unwrap().close().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_eggdb"))
            .arg("sql")
            .arg(file.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let bytes = script.clone().into_bytes();
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(&bytes);
        });
        std::thread::sleep(Duration::from_millis(delay_ms));
        let _ = child.kill();
        let _ = child.wait();
        let _ = writer.join();
        let db = Database::open(file.path()).unwrap();
        let store = db.sql_store();
        let mut session = store.session(Dialect::PostgreSql);
        let mut counts = Vec::new();
        let grouped = session.execute(
            "SELECT round, count(*) FROM c GROUP BY round ORDER BY round",
            &mut |result| counts.extend(result.rows),
        );
        if grouped.is_ok() {
            for (expected_round, row) in counts.iter().enumerate() {
                assert_eq!(
                    row[0].to_text(),
                    Some(expected_round.to_string()),
                    "after {delay_ms} ms: a COPY is missing before a later one"
                );
                assert_eq!(
                    row[1].to_text(),
                    Some(ROWS.to_string()),
                    "after {delay_ms} ms: COPY {expected_round} is partial"
                );
            }
            complete_seen += counts.len();
            session.check_indexes().unwrap();
        }
        session.close().unwrap();
        assert_eq!(db.check().unwrap().leaked_pages, 0);
        db.close().unwrap();
    }
    assert!(
        complete_seen > 0,
        "no run committed a COPY before it was killed"
    );
}
