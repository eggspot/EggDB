use crate::baseline::Metric;
use crate::bench_file::BenchFile;
use crate::database::failed;
use eggdb::sql::Dialect;
use eggdb::{Database, Options};
use std::fmt::Write;
use std::path::Path;

pub(crate) const DISK: &str = "footprint.disk";

const ROWS: u64 = 100_000;
const ROWS_PER_INSERT: u64 = 500;
const NAME_BYTES: u64 = 16;
const PAYLOAD_BYTES_PER_ROW: u64 = 8 + NAME_BYTES + 4;
const SIZE_TOLERANCE_PERCENT: u32 = 5;

fn execute(db: &Database, sql: &str) -> Result<(), String> {
    db.sql_store()
        .session(Dialect::PostgreSql)
        .execute(sql, &mut |_| {})
        .map_err(failed(sql))
}

fn file_bytes(path: &Path) -> Result<u64, String> {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn settled_bytes(path: &Path, db: Database) -> Result<(Database, u64), String> {
    db.checkpoint().map_err(failed("checkpoint"))?;
    db.close().map_err(failed("close"))?;
    let bytes = file_bytes(path)?;
    let reopened = Database::open_with(path, Options::new()).map_err(failed("reopen"))?;
    Ok((reopened, bytes))
}

fn fill(db: &Database) -> Result<(), String> {
    execute(
        db,
        "CREATE TABLE ledger (id bigint PRIMARY KEY, name text NOT NULL, amount int NOT NULL)",
    )?;
    for first in (0..ROWS).step_by(ROWS_PER_INSERT as usize) {
        let mut insert = String::from("INSERT INTO ledger VALUES ");
        for row in first..ROWS.min(first + ROWS_PER_INSERT) {
            if row > first {
                insert.push_str(", ");
            }
            let _ = write!(insert, "({row}, 'name-{row:011}', {})", (row * 7919) % 1_000_000);
        }
        execute(db, &insert)?;
    }
    Ok(())
}

pub(crate) fn disk() -> Result<Vec<Metric>, String> {
    let file = BenchFile::new("footprint-disk")?;
    let created = Database::open_with(&file.path, Options::new()).map_err(failed("create"))?;
    let (created, empty_bytes) = settled_bytes(&file.path, created)?;
    fill(&created)?;
    let (filled, table_bytes) = settled_bytes(&file.path, created)?;
    execute(&filled, "CREATE INDEX ledger_amount ON ledger (amount)")?;
    let (indexed, indexed_bytes) = settled_bytes(&file.path, filled)?;
    indexed.close().map_err(failed("close"))?;
    let per_row = table_bytes.saturating_sub(empty_bytes) as f64 / ROWS as f64;
    let per_index_entry = indexed_bytes.saturating_sub(table_bytes) as f64 / ROWS as f64;
    Ok(vec![
        Metric::ceiling(DISK, "bytes_per_row", per_row, SIZE_TOLERANCE_PERCENT),
        Metric::ceiling(
            DISK,
            "bytes_per_index_entry",
            per_index_entry,
            SIZE_TOLERANCE_PERCENT,
        ),
        Metric::reported(
            DISK,
            "row_overhead_ratio",
            per_row / PAYLOAD_BYTES_PER_ROW as f64,
        ),
    ])
}
