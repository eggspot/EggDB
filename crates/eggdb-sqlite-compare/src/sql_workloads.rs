use crate::baseline::Metric;
use crate::measure;
use crate::stores::{Stores, failed};
use crate::workloads::{RatioGate, paired_metrics};
use eggdb::Durability;
use eggdb::sql::Store as DatabaseStore;
use eggdb_sql::Session;
use rusqlite::Connection;
use std::hint::black_box;

pub(crate) const POINT_SELECT: &str = "sqlite_compare.sql_point_select";
pub(crate) const INDEXED_LOOKUP: &str = "sqlite_compare.sql_indexed_lookup";
pub(crate) const BULK_INSERT: &str = "sqlite_compare.sql_bulk_insert";
pub(crate) const JOIN: &str = "sqlite_compare.sql_join";
pub(crate) const GROUP_BY: &str = "sqlite_compare.sql_group_by";

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

const NAME_INDEX: &str = "CREATE INDEX people_name ON people (name)";

fn indexed_query(index: u64) -> String {
    format!(
        "SELECT id, score FROM people WHERE name = 'person {}'",
        index.wrapping_mul(7919) % ROWS
    )
}

fn eggdb_indexed(session: &mut SqlSession<'_>, first: u64, count: u64) -> Result<usize, String> {
    let mut found = 0;
    for index in first..first + count {
        found += eggdb_run(session, &indexed_query(index))?;
    }
    if found as u64 != count {
        return Err("EggDB missed a row through the index".into());
    }
    Ok(found)
}

fn sqlite_indexed(sqlite: &Connection, first: u64, count: u64) -> Result<i64, String> {
    let mut folded = 0;
    for index in first..first + count {
        let id: i64 = sqlite
            .query_row(&indexed_query(index), [], |row| row.get(0))
            .map_err(failed("SQLite indexed select"))?;
        folded ^= id;
    }
    Ok(folded)
}

pub(crate) fn indexed_lookup() -> Result<Vec<Metric>, String> {
    let stores = Stores::open("sql-indexed-lookup", Durability::Normal, 0)?;
    let store = DatabaseStore(&stores.eggdb);
    let mut session = Session::new(&store);
    eggdb_run(&mut session, EGGDB_TABLE)?;
    sqlite_run(&stores.sqlite, SQLITE_TABLE)?;
    eggdb_insert(&mut session, 0, ROWS)?;
    sqlite_insert(&stores.sqlite, 0, ROWS)?;
    eggdb_run(&mut session, NAME_INDEX)?;
    sqlite_run(&stores.sqlite, NAME_INDEX)?;
    let (mut eggdb_next, mut sqlite_next) = (0, 0);
    let measured = measure::paired(
        &mut |count| {
            black_box(eggdb_indexed(&mut session, eggdb_next, count)?);
            eggdb_next += count;
            Ok(())
        },
        &mut |count| {
            black_box(sqlite_indexed(&stores.sqlite, sqlite_next, count)?);
            sqlite_next += count;
            Ok(())
        },
    )?;
    Ok(paired_metrics(
        INDEXED_LOOKUP,
        measured,
        RatioGate::Gated,
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

const JOINED_ROWS: u64 = 10_000;
const BUCKETS: u64 = 100;
const EGGDB_ORDERS: &str = "CREATE TABLE orders (id bigint PRIMARY KEY, person_id bigint NOT NULL, bucket integer NOT NULL, amount integer NOT NULL)";
const SQLITE_ORDERS: &str = "CREATE TABLE orders (id INTEGER PRIMARY KEY, person_id INTEGER NOT NULL, bucket INTEGER NOT NULL, amount INTEGER NOT NULL)";
const JOIN_QUERY: &str = "SELECT count(*), sum(o.amount) FROM orders o JOIN people p ON p.id = o.person_id WHERE p.score > 100";
const GROUP_BY_QUERY: &str =
    "SELECT bucket, count(*), sum(amount), min(amount), max(amount) FROM orders GROUP BY bucket";

fn orders_script() -> String {
    let mut script = String::from("BEGIN;\n");
    for id in 0..JOINED_ROWS {
        script.push_str(&format!(
            "INSERT INTO orders VALUES ({id}, {}, {}, {});\n",
            id.wrapping_mul(7919) % JOINED_ROWS,
            id % BUCKETS,
            id % 997
        ));
    }
    script.push_str("COMMIT;\n");
    script
}

fn sqlite_rows(sqlite: &Connection, sql: &str) -> Result<usize, String> {
    let mut statement = sqlite.prepare(sql).map_err(failed("SQLite prepare"))?;
    let mut rows = statement.query([]).map_err(failed("SQLite query"))?;
    let mut count = 0;
    while let Some(row) = rows.next().map_err(failed("SQLite row"))? {
        black_box(row.get::<_, i64>(0).map_err(failed("SQLite value"))?);
        count += 1;
    }
    Ok(count)
}

fn repeated(
    query: &'static str,
    benchmark: &'static str,
    expected_rows: usize,
) -> Result<Vec<Metric>, String> {
    let stores = Stores::open(
        benchmark.trim_start_matches("sqlite_compare."),
        Durability::Normal,
        0,
    )?;
    let store = DatabaseStore(&stores.eggdb);
    let mut session = Session::new(&store);
    eggdb_run(&mut session, EGGDB_TABLE)?;
    sqlite_run(&stores.sqlite, SQLITE_TABLE)?;
    eggdb_insert(&mut session, 0, JOINED_ROWS)?;
    sqlite_insert(&stores.sqlite, 0, JOINED_ROWS)?;
    eggdb_run(&mut session, EGGDB_ORDERS)?;
    sqlite_run(&stores.sqlite, SQLITE_ORDERS)?;
    let script = orders_script();
    eggdb_run(&mut session, &script)?;
    sqlite_run(&stores.sqlite, &script)?;
    if eggdb_run(&mut session, query)? != expected_rows
        || sqlite_rows(&stores.sqlite, query)? != expected_rows
    {
        return Err(format!("{benchmark}: unexpected row count"));
    }
    let measured = measure::paired(
        &mut |count| {
            for _ in 0..count {
                black_box(eggdb_run(&mut session, query)?);
            }
            Ok(())
        },
        &mut |count| {
            for _ in 0..count {
                black_box(sqlite_rows(&stores.sqlite, query)?);
            }
            Ok(())
        },
    )?;
    Ok(paired_metrics(
        benchmark,
        measured,
        RatioGate::GatedWithin(KNOWN_GAP_TOLERANCE_PERCENT),
        ["ns_per_query", "sqlite_ns_per_query"],
    ))
}

pub(crate) fn join() -> Result<Vec<Metric>, String> {
    repeated(JOIN_QUERY, JOIN, 1)
}

pub(crate) fn group_by() -> Result<Vec<Metric>, String> {
    repeated(GROUP_BY_QUERY, GROUP_BY, BUCKETS as usize)
}

#[cfg(test)]
mod tests {
    use super::{
        EGGDB_ORDERS, EGGDB_TABLE, JOIN_QUERY, SQLITE_ORDERS, SQLITE_TABLE, eggdb_insert,
        eggdb_run, orders_script, sqlite_insert,
    };
    use crate::stores::Stores;
    use eggdb::Durability;
    use eggdb::sql::Store as DatabaseStore;
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

    fn eggdb_text(session: &mut Session<'_, DatabaseStore<'_>>, sql: &str) -> Vec<String> {
        let mut rows = Vec::new();
        session
            .execute(sql, &mut |result| {
                for row in result.rows {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|value| value.to_text().unwrap_or_else(|| "NULL".into()))
                        .collect();
                    rows.push(cells.join("|"));
                }
            })
            .unwrap();
        rows
    }

    fn sqlite_text(sqlite: &rusqlite::Connection, sql: &str) -> Vec<String> {
        let mut statement = sqlite.prepare(sql).unwrap();
        let width = statement.column_count();
        statement
            .query_map([], |row| {
                let mut cells = Vec::with_capacity(width);
                for index in 0..width {
                    cells.push(match row.get::<_, Option<i64>>(index) {
                        Ok(Some(number)) => number.to_string(),
                        Ok(None) => "NULL".to_owned(),
                        Err(_) => row.get::<_, String>(index)?,
                    });
                }
                Ok(cells.join("|"))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn joins_and_groups_match_sqlite() {
        let stores = Stores::open("test-sql-join", Durability::Off, 0).unwrap();
        let store = DatabaseStore(&stores.eggdb);
        let mut session = Session::new(&store);
        eggdb_run(&mut session, EGGDB_TABLE).unwrap();
        stores.sqlite.execute_batch(SQLITE_TABLE).unwrap();
        eggdb_insert(&mut session, 0, 10_000).unwrap();
        sqlite_insert(&stores.sqlite, 0, 10_000).unwrap();
        eggdb_run(&mut session, EGGDB_ORDERS).unwrap();
        stores.sqlite.execute_batch(SQLITE_ORDERS).unwrap();
        let script = orders_script();
        eggdb_run(&mut session, &script).unwrap();
        stores.sqlite.execute_batch(&script).unwrap();
        for sql in [
            JOIN_QUERY,
            "SELECT bucket, count(*), sum(amount), min(amount), max(amount) FROM orders GROUP BY bucket ORDER BY bucket",
            "SELECT p.id, count(o.id) FROM people p LEFT JOIN orders o ON o.person_id = p.id WHERE p.id < 50 GROUP BY p.id ORDER BY p.id",
            "SELECT o.bucket, count(DISTINCT o.person_id) FROM orders o JOIN people p ON p.id = o.person_id GROUP BY o.bucket HAVING count(*) > 99 ORDER BY 2 DESC, 1 LIMIT 5",
            "SELECT DISTINCT amount % 7 FROM orders ORDER BY 1",
            "SELECT p.name, o.amount FROM people p JOIN orders o ON o.person_id = p.id WHERE p.id = 77 ORDER BY o.id",
        ] {
            let ordered = sql.contains("ORDER BY") || !sql.contains("GROUP BY");
            let mut eggdb = eggdb_text(&mut session, sql);
            let mut sqlite = sqlite_text(&stores.sqlite, sql);
            if !ordered {
                eggdb.sort();
                sqlite.sort();
            }
            assert_eq!(eggdb, sqlite, "{sql}");
        }
    }

    #[test]
    fn a_join_and_a_group_by_allocate_per_statement_not_per_row() {
        let stores = Stores::open("test-sql-allocations", Durability::Off, 0).unwrap();
        let store = DatabaseStore(&stores.eggdb);
        let mut session = Session::new(&store);
        eggdb_run(&mut session, EGGDB_TABLE).unwrap();
        eggdb_insert(&mut session, 0, 10_000).unwrap();
        eggdb_run(&mut session, EGGDB_ORDERS).unwrap();
        eggdb_run(&mut session, &orders_script()).unwrap();
        for (sql, most) in [(JOIN_QUERY, 300), (super::GROUP_BY_QUERY, 1000)] {
            eggdb_run(&mut session, sql).unwrap();
            let before = crate::thread_allocations::made_on_this_thread();
            eggdb_run(&mut session, sql).unwrap();
            let made = crate::thread_allocations::made_on_this_thread() - before;
            println!("ALLOCATIONS {made} for {sql}");
            assert!(made <= most, "{made} allocations for {sql}");
        }
    }
}
