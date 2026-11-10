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
const ROWS_PER_INSERT: usize = 500;
const NULL_EVERY: usize = 10;
const CACHE_BYTES: usize = 64 << 20;

pub(crate) const MYSQL_NULLABLE_INDEX_LIMIT: &str = "sql_mysql.order_by_nullable_index_limit";
pub(crate) const SQLITE_NULLABLE_INDEX_LIMIT: &str = "sql_sqlite.order_by_nullable_index_limit";
pub(crate) const LIMIT_TOP_N: &str = "sql.order_by_limit_topn";

fn execute(db: &Database, dialect: Dialect, sql: &str) -> Result<(), String> {
    db.sql_store()
        .session(dialect)
        .execute(sql, &mut |result| {
            black_box(result);
        })
        .map_err(failed(sql))
}

fn created_at(row: usize) -> String {
    if row.is_multiple_of(NULL_EVERY) {
        "NULL".to_owned()
    } else {
        ((row * 7919) % ROWS).to_string()
    }
}

fn filled(path: &std::path::Path, dialect: Dialect, indexed: bool) -> Result<Database, String> {
    let options = Options::new()
        .cache_bytes(CACHE_BYTES)
        .durability(Durability::Off);
    let db = Database::open_with(path, options).map_err(failed("open"))?;
    execute(
        &db,
        dialect,
        "CREATE TABLE events (id int PRIMARY KEY, created_at int)",
    )?;
    if indexed {
        execute(
            &db,
            dialect,
            "CREATE INDEX events_created_at ON events (created_at)",
        )?;
    }
    for first in (0..ROWS).step_by(ROWS_PER_INSERT) {
        let mut insert = String::from("INSERT INTO events VALUES ");
        for row in first..ROWS.min(first + ROWS_PER_INSERT) {
            if row > first {
                insert.push_str(", ");
            }
            let _ = write!(insert, "({row}, {})", created_at(row));
        }
        execute(&db, dialect, &insert)?;
    }
    Ok(db)
}

fn repeated(db: &Database, dialect: Dialect, sql: &str, iterations: u64) -> Result<(), String> {
    for _ in 0..iterations {
        execute(db, dialect, sql)?;
    }
    Ok(())
}

fn nullable_index_limit(
    benchmark: &'static str,
    file_name: &str,
    dialect: Dialect,
) -> Result<Vec<Metric>, String> {
    let file = BenchFile::new(file_name)?;
    let db = filled(&file.path, dialect, true)?;
    let measured = measure::paired(
        &mut |iterations| {
            repeated(
                &db,
                dialect,
                "SELECT id FROM events ORDER BY created_at DESC LIMIT 10",
                iterations,
            )
        },
        &mut |iterations| {
            repeated(
                &db,
                dialect,
                "SELECT id FROM events ORDER BY id DESC LIMIT 10",
                iterations,
            )
        },
    )?;
    Ok(vec![
        Metric::ratio(benchmark, "ratio_to_primary_key_order", measured.ratio),
        Metric::reported(benchmark, "ns_per_query", measured.subject_ns),
    ])
}

pub(crate) fn mysql_nullable_index_limit(
    _allocator: &CountingAllocator,
) -> Result<Vec<Metric>, String> {
    nullable_index_limit(
        MYSQL_NULLABLE_INDEX_LIMIT,
        "mysql-order-nullable-index",
        Dialect::MySql,
    )
}

pub(crate) fn sqlite_nullable_index_limit(
    _allocator: &CountingAllocator,
) -> Result<Vec<Metric>, String> {
    nullable_index_limit(
        SQLITE_NULLABLE_INDEX_LIMIT,
        "sqlite-order-nullable-index",
        Dialect::Sqlite,
    )
}

pub(crate) fn limit_top_n(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let file = BenchFile::new("order-by-limit-top-n")?;
    let dialect = Dialect::PostgreSql;
    let db = filled(&file.path, dialect, false)?;
    let measured = measure::paired(
        &mut |iterations| {
            repeated(
                &db,
                dialect,
                "SELECT id FROM events ORDER BY created_at LIMIT 10",
                iterations,
            )
        },
        &mut |iterations| {
            repeated(
                &db,
                dialect,
                "SELECT id FROM events WHERE created_at < 0",
                iterations,
            )
        },
    )?;
    Ok(vec![
        Metric::ratio(LIMIT_TOP_N, "ratio_to_filtered_scan", measured.ratio),
        Metric::reported(LIMIT_TOP_N, "ns_per_query", measured.subject_ns),
    ])
}
