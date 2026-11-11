//! `eggdb-sqllogictest DIR... [--minimum PERCENT] [--engine NAME]`: runs every `.test` file under
//! the given directories against a fresh in-memory EggDB database in the SQLite dialect, prints
//! each file's and the total pass rate, and exits with status 1 when the total is below the
//! minimum.

mod records;

use eggdb::sql::{Dialect, QueryResult, Value};
use records::{Expected, Record, SortMode};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const DEFAULT_ENGINE: &str = "sqlite";
const USAGE: &str = "usage: eggdb-sqllogictest DIR... [--minimum PERCENT] [--engine NAME]";

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    passed: u64,
    failed: u64,
}

impl Tally {
    fn add(&mut self, other: Tally) {
        self.passed += other.passed;
        self.failed += other.failed;
    }

    fn percent(self) -> f64 {
        let total = self.passed + self.failed;
        if total == 0 {
            return 100.0;
        }
        self.passed as f64 * 100.0 / total as f64
    }
}

fn test_files(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(Result::ok).map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            test_files(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "test") {
            found.push(path);
        }
    }
}

fn shown(value: &Value, kind: char) -> String {
    if matches!(value, Value::Null) {
        return "NULL".to_owned();
    }
    match kind {
        'I' => match value {
            Value::Real(number) => format!("{}", f64::from(*number).trunc() as i64),
            Value::Double(number) => format!("{}", number.trunc() as i64),
            other => {
                let text = other.to_text().unwrap_or_default();
                text.parse::<f64>()
                    .map_or_else(|_| "0".to_owned(), |number| format!("{}", number.trunc() as i64))
            }
        },
        'R' => {
            let text = value.to_text().unwrap_or_default();
            text.parse::<f64>()
                .map_or_else(|_| "0.000".to_owned(), |number| format!("{number:.3}"))
        }
        _ => {
            let text = value.to_text().unwrap_or_default();
            if text.is_empty() {
                return "(empty)".to_owned();
            }
            text.chars()
                .map(|character| if (' '..='~').contains(&character) { character } else { '@' })
                .collect()
        }
    }
}

fn md5_hex(values: &[String]) -> String {
    let mut joined = String::new();
    for value in values {
        joined.push_str(value);
        joined.push('\n');
    }
    eggdb_crypto::md5_checksum(joined.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn query_values(result: &QueryResult, types: &[char], sort: SortMode) -> Vec<String> {
    let mut rows: Vec<Vec<String>> = result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(at, value)| shown(value, types.get(at).copied().unwrap_or('T')))
                .collect()
        })
        .collect();
    if sort == SortMode::Rows {
        rows.sort();
    }
    let mut values: Vec<String> = rows.into_iter().flatten().collect();
    if sort == SortMode::Values {
        values.sort();
    }
    values
}

fn run_file(path: &Path, engine: &str) -> Tally {
    let mut tally = Tally::default();
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!("{}: cannot be read", path.display());
        return tally;
    };
    let Ok(db) = eggdb::Database::in_memory() else {
        eprintln!("{}: no in-memory database", path.display());
        return tally;
    };
    let store = db.sql_store();
    let mut session = store.session(Dialect::Sqlite);
    let mut hash_threshold = usize::MAX;
    for record in records::parse(&text, engine) {
        match record {
            Record::HashThreshold(count) => hash_threshold = if count == 0 { usize::MAX } else { count },
            Record::Halt => break,
            Record::Statement { line, sql, succeeds } => {
                let outcome = session.execute(&sql, &mut |_| {});
                if outcome.is_ok() == succeeds {
                    tally.passed += 1;
                } else {
                    tally.failed += 1;
                    eprintln!("{}:{line}: statement {}", path.display(), if succeeds { "failed" } else { "succeeded" });
                }
            }
            Record::Query { line, sql, types, sort, expected } => {
                let mut last = None;
                let outcome = session.execute(&sql, &mut |result| {
                    if result.columns.is_some() {
                        last = Some(result);
                    }
                });
                let Some(result) = last.filter(|_| outcome.is_ok()) else {
                    tally.failed += 1;
                    eprintln!("{}:{line}: query failed", path.display());
                    continue;
                };
                let values = query_values(&result, &types, sort);
                let matches = match expected {
                    Expected::Values(wanted) if wanted.len() > hash_threshold => {
                        md5_hex(&values) == md5_hex(&wanted)
                    }
                    Expected::Values(wanted) => values == wanted,
                    Expected::Hash { count, md5 } => values.len() == count && md5_hex(&values) == md5,
                };
                if matches {
                    tally.passed += 1;
                } else {
                    tally.failed += 1;
                    eprintln!("{}:{line}: query gave other values", path.display());
                }
            }
        }
    }
    tally
}

fn main() -> ExitCode {
    let mut directories = Vec::new();
    let mut minimum = 0.0_f64;
    let mut engine = DEFAULT_ENGINE.to_owned();
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--minimum" => match arguments.next().and_then(|value| value.parse().ok()) {
                Some(value) => minimum = value,
                None => {
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "--engine" => match arguments.next() {
                Some(value) => engine = value,
                None => {
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                }
            },
            _ => directories.push(PathBuf::from(argument)),
        }
    }
    if directories.is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let mut files = Vec::new();
    for directory in &directories {
        test_files(directory, &mut files);
    }
    let mut total = Tally::default();
    for file in &files {
        let tally = run_file(file, &engine);
        println!("{:6.2}% {} ({} of {})", tally.percent(), file.display(), tally.passed, tally.passed + tally.failed);
        total.add(tally);
    }
    println!(
        "{:6.2}% total ({} of {} records, {} files)",
        total.percent(),
        total.passed,
        total.passed + total.failed,
        files.len()
    );
    if total.percent() < minimum {
        eprintln!("below the minimum of {minimum:.2}%");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_shown_as_sqllogictest_prints_them() {
        assert_eq!(shown(&Value::Null, 'I'), "NULL");
        assert_eq!(shown(&Value::Double(2.75), 'I'), "2");
        assert_eq!(shown(&Value::BigInt(7), 'R'), "7.000");
        assert_eq!(shown(&Value::Text(String::new()), 'T'), "(empty)");
        assert_eq!(md5_hex(&[]), "d41d8cd98f00b204e9800998ecf8427e");
    }
}
