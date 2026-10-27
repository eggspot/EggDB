use crate::baseline::Metric;
use crate::measure;
use crate::sql_store::DatabaseStore;
use crate::stores::{Stores, failed};
use crate::workloads::{RatioGate, paired_metrics};
use eggdb::Durability;
use eggdb_sql::Session;
use rusqlite::Connection;
use std::hint::black_box;

pub(crate) const POINT_SELECT: &str = "sqlite_compare.sql_point_select";
pub(crate) const BULK_INSERT: &str = "sqlite_compare.sql_bulk_insert";

const ROWS: u64 = 100_000;
const KNOWN_GAP_TOLERANCE_PERCENT: u32 = 15;
const ROWS_PER_STATEMENT: u64 = 100;
const ROWS_PER_TRANSACTION: u64 = 1000;
const EGGDB_TABLE: &str =
    "CREATE TABLE people (id bigint PRIMARY KEY, name text NOT NULL, score double precision)";
const SQLITE_TABLE: &str =
    "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL)";

type SqlSession<'s> = Session<'s, DatabaseStore<'s>>;

fn insert_script(first: u64, rows: u64) -> String {
    let mut script = String::from("BEGIN;\n");
    let mut id = first;
    let end = first + rows;
    while id < end {
        script.push_str("INSERT INTO people VALUES ");
        let statement_end = end.min(id + ROWS_PER_STATEMENT);
        for row in id..statement_end {
            if row > id {
                script.push_str(", ");
            }
            script.push_str(&format!("({row}, 'person {row}', {}.5)", row % 1000));
        }
        script.push_str(";\n");
        id = statement_end;
    }
    script.push_str("COMMIT;\n");
    script
}

fn eggdb_run(session: &mut SqlSession<'_>, sql: &str) -> Result<usize, String> {
    let mut rows = 0;
    session
        .execute(sql, &mut |result| rows += result.rows.len())
        .map_err(failed("EggDB SQL"))?;
    Ok(rows)
}

fn sqlite_run(sqlite: &Connection, sql: &str) -> Result<(), String> {
    sqlite.execute_batch(sql).map_err(failed("SQLite SQL"))
}

fn eggdb_insert(session: &mut SqlSession<'_>, first: u64, rows: u64) -> Result<(), String> {
    let mut next = first;
    while next < first + rows {
        let batch = ROWS_PER_TRANSACTION.min(first + rows - next);
        eggdb_run(session, &insert_script(next, batch))?;
        next += batch;
    }
    Ok(())
}

fn sqlite_insert(sqlite: &Connection, first: u64, rows: u64) -> Result<(), String> {
    let mut next = first;
    while next < first + rows {
        let batch = ROWS_PER_TRANSACTION.min(first + rows - next);
        sqlite_run(sqlite, &insert_script(next, batch))?;
        next += batch;
    }
    Ok(())
}

fn point_query(index: u64) -> String {
    format!(
        "SELECT name, score FROM people WHERE id = {}",
        index.wrapping_mul(7919) % ROWS
    )
}

fn eggdb_select(session: &mut SqlSession<'_>, first: u64, count: u64) -> Result<usize, String> {
    let mut found = 0;
    for index in first..first + count {
        found += eggdb_run(session, &point_query(index))?;
    }
    if found as u64 != count {
        return Err("EggDB missed a row".into());
    }
    Ok(found)
}

fn sqlite_select(sqlite: &Connection, first: u64, count: u64) -> Result<usize, String> {
    let mut folded = 0;
    for index in first..first + count {
        let name: String = sqlite
            .query_row(&point_query(index), [], |row| row.get(0))
            .map_err(failed("SQLite select"))?;
        folded ^= name.len();
    }
    Ok(folded)
}

pub(crate) fn point_select() -> Result<Vec<Metric>, String> {
    let stores = Stores::open("sql-point-select", Durability::Normal, 0)?;
    let store = DatabaseStore(&stores.eggdb);
    let mut session = Session::new(&store);
    eggdb_run(&mut session, EGGDB_TABLE)?;
    sqlite_run(&stores.sqlite, SQLITE_TABLE)?;
    eggdb_insert(&mut session, 0, ROWS)?;
    sqlite_insert(&stores.sqlite, 0, ROWS)?;
    let (mut eggdb_next, mut sqlite_next) = (0, 0);
    let measured = measure::paired(
        &mut |count| {
            black_box(eggdb_select(&mut session, eggdb_next, count)?);
            eggdb_next += count;
            Ok(())
        },
        &mut |count| {
            black_box(sqlite_select(&stores.sqlite, sqlite_next, count)?);
            sqlite_next += count;
            Ok(())
        },
    )?;
    Ok(paired_metrics(
        POINT_SELECT,
        measured,
        RatioGate::GatedWithin(KNOWN_GAP_TOLERANCE_PERCENT),
        ["ns_per_select", "sqlite_ns_per_select"],
    ))
}

pub(crate) fn bulk_insert() -> Result<Vec<Metric>, String> {
    let stores = Stores::open("sql-bulk-insert", Durability::Off, 0)?;
    let store = DatabaseStore(&stores.eggdb);
    let mut session = Session::new(&store);
    eggdb_run(&mut session, EGGDB_TABLE)?;
    sqlite_run(&stores.sqlite, SQLITE_TABLE)?;
    let (mut eggdb_next, mut sqlite_next) = (0, 0);
    let per_transaction = measure::paired_calibrated_from(
        1,
        &mut measure::timed(&mut |transactions| {
            let rows = transactions * ROWS_PER_TRANSACTION;
            eggdb_insert(&mut session, eggdb_next, rows)?;
            eggdb_next += rows;
            Ok(())
        }),
        &mut measure::timed(&mut |transactions| {
            let rows = transactions * ROWS_PER_TRANSACTION;
            sqlite_insert(&stores.sqlite, sqlite_next, rows)?;
            sqlite_next += rows;
            Ok(())
        }),
    )?;
    let per_row = measure::Paired {
        subject_ns: per_transaction.subject_ns / ROWS_PER_TRANSACTION as f64,
        reference_ns: per_transaction.reference_ns / ROWS_PER_TRANSACTION as f64,
        ratio: per_transaction.ratio,
    };
    Ok(paired_metrics(
        BULK_INSERT,
        per_row,
        RatioGate::GatedWithin(KNOWN_GAP_TOLERANCE_PERCENT),
        ["ns_per_row", "sqlite_ns_per_row"],
    ))
}

#[cfg(test)]
mod tests {
    use super::{EGGDB_TABLE, SQLITE_TABLE, eggdb_insert, eggdb_run, sqlite_insert};
    use crate::sql_store::DatabaseStore;
    use crate::stores::Stores;
    use eggdb::Durability;
    use eggdb_sql::Session;

    #[test]
    fn both_databases_hold_the_same_rows_and_find_each_one() {
        let stores = Stores::open("test-sql", Durability::Off, 0).unwrap();
        let store = DatabaseStore(&stores.eggdb);
        let mut session = Session::new(&store);
        eggdb_run(&mut session, EGGDB_TABLE).unwrap();
        stores.sqlite.execute_batch(SQLITE_TABLE).unwrap();
        eggdb_insert(&mut session, 0, 2500).unwrap();
        sqlite_insert(&stores.sqlite, 0, 2500).unwrap();
        let mut eggdb_rows = Vec::new();
        session
            .execute("SELECT id, name, score FROM people", &mut |result| {
                for row in result.rows {
                    let cells: Vec<String> =
                        row.iter().map(|value| value.to_text().unwrap()).collect();
                    eggdb_rows.push(cells.join("|"));
                }
            })
            .unwrap();
        let mut select = stores
            .sqlite
            .prepare("SELECT id, name, score FROM people ORDER BY id")
            .unwrap();
        let sqlite_rows: Vec<String> = select
            .query_map([], |row| {
                Ok(format!(
                    "{}|{}|{}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, f64>(2)?
                ))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(eggdb_rows.len(), 2500);
        assert_eq!(eggdb_rows, sqlite_rows);
    }
}
