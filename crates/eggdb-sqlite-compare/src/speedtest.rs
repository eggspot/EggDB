use crate::baseline::Metric;
use crate::measure;
use crate::stores::{Stores, failed};
use crate::workloads::{RatioGate, paired_metrics};
use eggdb::Durability;
use eggdb::sql::{Dialect, Store as DatabaseStore};
use eggdb_sql::Session;
use rusqlite::Connection;
use std::fmt::Write;

pub(crate) const SPEEDTEST: &str = "sqlite_compare.speedtest";

const ROWS: u64 = 2000;
const RANGE_SELECTS: u64 = 25;
const LIKE_SELECTS: u64 = 10;
const INDEXED_SELECTS: u64 = 100;
const UPDATES: u64 = 100;
const REPLACES: u64 = 500;

const ONES: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: [&str; 10] = [
    "", "ten", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

fn words(number: u64) -> String {
    let mut out = String::new();
    let mut left = number;
    for (scale, name) in [(1_000_000_000, "billion"), (1_000_000, "million"), (1000, "thousand")] {
        if left >= scale {
            out.push_str(&words(left / scale));
            out.push(' ');
            out.push_str(name);
            left %= scale;
            if left > 0 {
                out.push(' ');
            }
        }
    }
    if left >= 100 {
        out.push_str(ONES[(left / 100) as usize]);
        out.push_str(" hundred");
        left %= 100;
        if left > 0 {
            out.push(' ');
        }
    }
    if left >= 20 {
        out.push_str(TENS[(left / 10) as usize]);
        left %= 10;
        if left > 0 {
            out.push(' ');
        }
    }
    if left > 0 || out.is_empty() {
        out.push_str(ONES[left as usize]);
    }
    out
}

fn scrambled(index: u64) -> u64 {
    index.wrapping_mul(2_654_435_761) % (ROWS * 1000)
}

fn phases() -> Vec<String> {
    let mut phases = Vec::new();
    phases.push(
        "CREATE TABLE t1(a INTEGER, b INTEGER, c TEXT);\
         CREATE TABLE t2(a INTEGER PRIMARY KEY, b INTEGER, c TEXT);\
         CREATE TABLE t3(a INTEGER, b INTEGER, c TEXT);\
         CREATE INDEX t3a ON t3(a);"
            .to_owned(),
    );
    let mut unindexed = String::from("BEGIN;");
    let mut ordered = String::from("BEGIN;");
    let mut unordered = String::from("BEGIN;");
    for row in 1..=ROWS {
        let value = scrambled(row);
        let _ = write!(unindexed, "INSERT INTO t1 VALUES({value},{row},'{}');", words(value));
        let _ = write!(ordered, "INSERT INTO t2 VALUES({row},{value},'{}');", words(value));
        let _ = write!(unordered, "INSERT INTO t3 VALUES({row},{value},'{}');", words(row));
    }
    for script in [unindexed, ordered, unordered] {
        phases.push(script + "COMMIT;");
    }
    let mut ranges = String::from("BEGIN;");
    for select in 0..RANGE_SELECTS {
        let low = select * ROWS / RANGE_SELECTS;
        let _ = write!(
            ranges,
            "SELECT count(*), avg(b), sum(length(c)) FROM t1 WHERE b BETWEEN {low} AND {};",
            low + ROWS / 10
        );
    }
    phases.push(ranges + "COMMIT;");
    let mut likes = String::from("BEGIN;");
    for select in 0..LIKE_SELECTS {
        let _ = write!(
            likes,
            "SELECT count(*), avg(b), sum(length(c)) FROM t1 WHERE c LIKE '%{}%';",
            words(select + 1)
        );
        let _ = write!(
            likes,
            "SELECT a, b, c FROM t1 WHERE c LIKE '%{}%' ORDER BY a LIMIT 10;",
            words(select + 1)
        );
    }
    phases.push(likes + "COMMIT;");
    phases.push(
        "CREATE INDEX t1b ON t1(b);CREATE INDEX t1c ON t1(c);CREATE INDEX t2b ON t2(b);".to_owned(),
    );
    let mut indexed = String::from("BEGIN;");
    for select in 0..INDEXED_SELECTS {
        let low = scrambled(select) % ROWS;
        let _ = write!(
            indexed,
            "SELECT count(*), avg(b), sum(length(c)) FROM t1 WHERE b BETWEEN {low} AND {};",
            low + 10
        );
        let _ = write!(indexed, "SELECT c FROM t2 WHERE a = {};", low + 1);
    }
    phases.push(indexed + "COMMIT;");
    let mut updates = String::from("BEGIN;");
    for update in 0..UPDATES {
        let low = update * ROWS / UPDATES;
        let _ = write!(
            updates,
            "UPDATE t2 SET b = b * 2 WHERE a BETWEEN {low} AND {};",
            low + ROWS / UPDATES
        );
    }
    updates.push_str("UPDATE t1 SET b = b + 1;");
    phases.push(updates + "COMMIT;");
    phases.push(
        "SELECT count(*) FROM t1 JOIN t2 ON t1.b = t2.a WHERE t2.b > 100;\
         SELECT t1.c, (SELECT count(*) FROM t3 WHERE t3.a = t1.b) FROM t1 WHERE t1.b < 200;"
            .to_owned(),
    );
    let mut replaces = String::from("BEGIN;");
    for replace in 0..REPLACES {
        let id = scrambled(replace) % ROWS + 1;
        let _ = write!(replaces, "INSERT OR REPLACE INTO t2 VALUES({id},{replace},'{}');", words(replace));
    }
    phases.push(replaces + "COMMIT;");
    phases.push(
        "BEGIN;DELETE FROM t2 WHERE a % 3 = 0;DELETE FROM t3 WHERE a BETWEEN 100 AND 600;COMMIT;"
            .to_owned(),
    );
    phases.push("DROP TABLE t1;DROP TABLE t2;DROP TABLE t3;".to_owned());
    phases
}

fn eggdb_run(session: &mut Session<'_>, phases: &[String], runs: u64) -> Result<(), String> {
    for _ in 0..runs {
        for phase in phases {
            session
                .execute(phase, &mut |result| {
                    std::hint::black_box(result);
                })
                .map_err(failed("EggDB speedtest"))?;
        }
    }
    Ok(())
}

fn sqlite_run(sqlite: &Connection, phases: &[String], runs: u64) -> Result<(), String> {
    for _ in 0..runs {
        for phase in phases {
            run_reading_rows(sqlite, phase)?;
        }
    }
    Ok(())
}

fn run_reading_rows(sqlite: &Connection, script: &str) -> Result<(), String> {
    let mut rest = script.trim();
    while !rest.is_empty() {
        let Some(statement) = sqlite_first_statement(sqlite, rest)? else {
            break;
        };
        rest = statement;
    }
    Ok(())
}

fn sqlite_first_statement<'t>(
    sqlite: &Connection,
    script: &'t str,
) -> Result<Option<&'t str>, String> {
    let Some(end) = script.find(';') else {
        return Ok(None);
    };
    let (first, rest) = script.split_at(end);
    let mut statement = sqlite.prepare(first).map_err(failed("SQLite prepare"))?;
    let mut rows = statement.raw_query();
    while let Some(row) = rows.next().map_err(failed("SQLite step"))? {
        std::hint::black_box(row.get_ref(0).ok());
    }
    Ok(Some(rest[1..].trim_start()))
}

pub(crate) fn speedtest() -> Result<Vec<Metric>, String> {
    let stores = Stores::open("speedtest", Durability::Normal, 0)?;
    let store = DatabaseStore(&stores.eggdb);
    let mut session = Session::with_dialect(&store, Dialect::Sqlite);
    let phases = phases();
    let measured = measure::paired_calibrated_from(
        1,
        &mut measure::timed(&mut |runs| eggdb_run(&mut session, &phases, runs)),
        &mut measure::timed(&mut |runs| sqlite_run(&stores.sqlite, &phases, runs)),
    )?;
    Ok(paired_metrics(
        SPEEDTEST,
        measured,
        RatioGate::Gated,
        ["ns_per_run", "sqlite_ns_per_run"],
    ))
}

#[cfg(test)]
mod tests {
    use super::words;

    #[test]
    fn numbers_are_spelled_in_english_words() {
        assert_eq!(words(0), "zero");
        assert_eq!(words(15), "fifteen");
        assert_eq!(words(42), "forty two");
        assert_eq!(words(100), "one hundred");
        assert_eq!(
            words(1_234_567),
            "one million two hundred thirty four thousand five hundred sixty seven"
        );
    }
}
