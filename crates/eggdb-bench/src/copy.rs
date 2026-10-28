use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::bench_file::BenchFile;
use crate::database::failed;
use crate::measure;
use eggdb::sql::{CopyChannel, CopyStart, Dialect, SqlResult};
use eggdb::{Database, Durability, Options};
use std::fmt::Write;
use std::hint::black_box;
use std::time::{Duration, Instant};

const ROWS: usize = 100_000;
const CACHE_BYTES: usize = 256 << 20;
const CHUNK_BYTES: usize = 64 * 1024;

pub(crate) const FROM: &str = "sql_copy.from_stdin";
pub(crate) const TO: &str = "sql_copy.to_stdout";

#[derive(Default)]
struct Rows {
    data: Vec<u8>,
    at: usize,
    written: usize,
}

impl CopyChannel for Rows {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        self.at = 0;
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
        self.written += data.len();
        Ok(())
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Ok(())
    }
}

fn name(row: usize) -> String {
    format!("customer {row}")
}

fn copy_text() -> Vec<u8> {
    let mut text = String::with_capacity(ROWS * 32);
    for row in 0..ROWS {
        let _ = writeln!(text, "{row}\t{}\t{}", name(row), row % 1000);
    }
    text.into_bytes()
}

fn insert_text() -> String {
    let mut text = String::with_capacity(ROWS * 40);
    text.push_str("INSERT INTO rows VALUES ");
    for row in 0..ROWS {
        if row > 0 {
            text.push_str(", ");
        }
        let _ = write!(text, "({row}, '{}', {})", name(row), row % 1000);
    }
    text
}

fn database(path: &std::path::Path) -> Result<Database, String> {
    let options = Options::new()
        .cache_bytes(CACHE_BYTES)
        .durability(Durability::Off);
    let db = Database::open_with(path, options).map_err(failed("open"))?;
    db.sql_store()
        .session(Dialect::PostgreSql)
        .execute(
            "CREATE TABLE rows (id integer PRIMARY KEY, name text, amount integer)",
            &mut |_| {},
        )
        .map_err(failed("create"))?;
    Ok(db)
}

fn run(db: &Database, sql: &str, rows: &mut Rows) -> Result<(), String> {
    let store = db.sql_store();
    let mut session = store.session(Dialect::PostgreSql);
    session
        .execute_with_copy(
            sql,
            &mut |result| {
                black_box(result);
                true
            },
            rows,
        )
        .map_err(failed(sql))
}

fn reload(db: &Database, sql: &str, rows: &mut Rows, iterations: u64) -> Result<Duration, String> {
    let mut spent = Duration::ZERO;
    for _ in 0..iterations {
        run(db, "DELETE FROM rows", rows)?;
        let start = Instant::now();
        run(db, sql, rows)?;
        spent += start.elapsed();
    }
    Ok(spent)
}

pub(crate) fn from_stdin(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let subject_file = BenchFile::new("copy-from")?;
    let reference_file = BenchFile::new("copy-from-reference")?;
    let subject = database(&subject_file.path)?;
    let reference = database(&reference_file.path)?;
    let insert = insert_text();
    let mut rows = Rows {
        data: copy_text(),
        ..Rows::default()
    };
    let mut nothing = Rows::default();
    let measured = measure::paired_calibrated_from(
        1,
        &mut |iterations| reload(&subject, "COPY rows FROM STDIN", &mut rows, iterations),
        &mut |iterations| reload(&reference, &insert, &mut nothing, iterations),
    )?;
    Ok(vec![
        Metric::ratio(FROM, "ratio_to_insert_values", measured.ratio),
        Metric::reported(FROM, "ns_per_row", measured.subject_ns / ROWS as f64),
        Metric::reported(
            FROM,
            "reference_ns_per_row",
            measured.reference_ns / ROWS as f64,
        ),
    ])
}

fn unload(db: &Database, sql: &str, rows: &mut Rows, iterations: u64) -> Result<(), String> {
    for _ in 0..iterations {
        run(db, sql, rows)?;
    }
    Ok(())
}

pub(crate) fn to_stdout(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let file = BenchFile::new("copy-to")?;
    let db = database(&file.path)?;
    let mut rows = Rows {
        data: copy_text(),
        ..Rows::default()
    };
    run(&db, "COPY rows FROM STDIN", &mut rows)?;
    let mut sink = Rows::default();
    let mut nothing = Rows::default();
    let measured = measure::paired_calibrated_from(
        1,
        &mut measure::timed(&mut |iterations| {
            unload(&db, "COPY rows TO STDOUT", &mut sink, iterations)
        }),
        &mut measure::timed(&mut |iterations| {
            unload(&db, "SELECT * FROM rows", &mut nothing, iterations)
        }),
    )?;
    Ok(vec![
        Metric::ratio(TO, "ratio_to_select", measured.ratio),
        Metric::reported(TO, "ns_per_row", measured.subject_ns / ROWS as f64),
        Metric::reported(
            TO,
            "reference_ns_per_row",
            measured.reference_ns / ROWS as f64,
        ),
    ])
}
