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

pub(crate) const COMPARE: &str = "sql_citext.compare";
pub(crate) const INDEX_LOOKUP: &str = "sql_citext.index_lookup";

fn word(row: usize) -> String {
    if row.is_multiple_of(2) {
        format!("Customer.{row}@Example.COM")
    } else {
        format!("customer.{row}@example.com")
    }
}

fn execute(db: &Database, sql: &str) -> Result<(), String> {
    db.sql_store()
        .session(Dialect::PostgreSql)
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
        "CREATE TABLE folded (id integer PRIMARY KEY, email citext)",
        "CREATE TABLE plain (id integer PRIMARY KEY, email text)",
        "CREATE UNIQUE INDEX folded_email ON folded (email)",
        "CREATE UNIQUE INDEX plain_email ON plain (email)",
    ] {
        execute(&db, statement)?;
    }
    for table in ["folded", "plain"] {
        let mut insert = format!("INSERT INTO {table} VALUES ");
        for row in 0..ROWS {
            if row > 0 {
                insert.push_str(", ");
            }
            let _ = write!(insert, "({row}, '{}')", word(row));
        }
        execute(&db, &insert)?;
    }
    Ok(db)
}

fn repeated(db: &Database, sql: &str, iterations: u64) -> Result<(), String> {
    for _ in 0..iterations {
        execute(db, sql)?;
    }
    Ok(())
}

pub(crate) fn compare(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let file = BenchFile::new("citext-compare")?;
    let db = filled(&file.path)?;
    let measured = measure::paired_calibrated_from(
        1,
        &mut measure::timed(&mut |iterations| {
            repeated(
                &db,
                "SELECT count(*) FROM folded WHERE email > 'CUSTOMER.5'",
                iterations,
            )
        }),
        &mut measure::timed(&mut |iterations| {
            repeated(
                &db,
                "SELECT count(*) FROM plain WHERE lower(email) > lower('CUSTOMER.5')",
                iterations,
            )
        }),
    )?;
    Ok(vec![
        Metric::ratio(COMPARE, "ratio_to_lower_text", measured.ratio),
        Metric::reported(COMPARE, "ns_per_row", measured.subject_ns / ROWS as f64),
        Metric::reported(
            COMPARE,
            "reference_ns_per_row",
            measured.reference_ns / ROWS as f64,
        ),
    ])
}

pub(crate) fn index_lookup(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    let file = BenchFile::new("citext-index-lookup")?;
    let db = filled(&file.path)?;
    let measured = measure::paired(
        &mut |iterations| {
            repeated(
                &db,
                "SELECT id FROM folded WHERE email = 'CUSTOMER.777@EXAMPLE.COM'",
                iterations,
            )
        },
        &mut |iterations| {
            repeated(
                &db,
                "SELECT id FROM plain WHERE email = 'customer.777@example.com'",
                iterations,
            )
        },
    )?;
    Ok(vec![
        Metric::ratio(INDEX_LOOKUP, "ratio_to_text_index", measured.ratio),
        Metric::reported(INDEX_LOOKUP, "ns_per_lookup", measured.subject_ns),
    ])
}
