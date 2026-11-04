use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::bench_file::BenchFile;
use crate::database::failed;
use crate::measure;
use eggdb::sql::Dialect;
use eggdb::{Database, Durability, Options};
use std::fmt::Write;
use std::hint::black_box;

const ROWS: usize = 20_000;
const CACHE_BYTES: usize = 64 << 20;

pub(crate) const NULLABLE_INDEX_LIMIT: &str = "sql_mysql.order_by_nullable_index_limit";

fn execute(db: &Database, sql: &str) -> Result<(), String> {
    db.sql_store()
        .session(Dialect::MySql)
        .execute(sql, &mut |result| {
            black_box(result);
        })
        .map_err(failed(sql))
}

fn filled(path: &std::path::Path) -> Result<Database, String> {
    let options = Options::new()
        .cache_bytes(CACHE_BYTES)
        .durability(Durability::Off);
    let db = Database::open_with(path, options).map_err(failed("open"))?;
    for statement in [
        "CREATE TABLE events (id int PRIMARY KEY, created_at int)",
        "CREATE INDEX events_created_at ON events (created_at)",
    ] {
        execute(&db, statement)?;
    }
    let mut insert = String::from("INSERT INTO events VALUES ");
    for row in 0..ROWS {
        if row > 0 {
            insert.push_str(", ");
        }
        let _ = write!(insert, "({row}, {})", (row * 7919) % ROWS);
    }
    execute(&db, &insert)?;
    Ok(db)
}

fn repeated(db: &Database, sql: &str, iterations: u64) -> Result<(), String> {
    for _ in 0..iterations {
        execute(db, sql)?;
    }
    Ok(())
}

pub(crate) fn nullable_index_limit(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let file = BenchFile::new("mysql-order-nullable-index")?;
    let db = filled(&file.path)?;
    let measured = measure::paired(
        &mut |iterations| {
            repeated(
                &db,
                "SELECT id FROM events ORDER BY created_at DESC LIMIT 10",
                iterations,
            )
        },
        &mut |iterations| {
            repeated(
                &db,
                "SELECT id FROM events ORDER BY id DESC LIMIT 10",
                iterations,
            )
        },
    )?;
    Ok(vec![
        Metric::ratio(
            NULLABLE_INDEX_LIMIT,
            "ratio_to_primary_key_order",
            measured.ratio,
        ),
        Metric::reported(NULLABLE_INDEX_LIMIT, "ns_per_query", measured.subject_ns),
    ])
}
