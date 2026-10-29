//! psql's `\d` family over EggDB's PostgreSQL catalog: the fixture's `pg_dump` from
//! PostgreSQL 16 is restored, and every query psql 16 and psql 18 sent to PostgreSQL 16 for
//! `scripts/pg-catalog/commands.txt` (recorded by `scripts/pg-catalog/record.sh`, relation OIDs
//! written as `{schema.name}`) runs on EggDB with EggDB's OIDs in their place. Each query's rows
//! and column types must be PostgreSQL's. The printed bytes are compared by CI's `pg-catalog`
//! job (`scripts/pg-catalog/check.sh`).

use eggdb::Database;
use eggdb::sql::{Dialect, QueryResult, Session, Store, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_DIRECTORY: AtomicU32 = AtomicU32::new(0);
const DATABASE: &str = "eggdb_catalog";
const USER: &str = "eggdb";

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new(name: &str) -> Self {
        let number = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "eggdb-pg-catalog-test-{name}-{}-{number}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/pg_catalog")
        .join(name)
}

struct Recorded {
    query: String,
    types: String,
    rows: Vec<String>,
}

fn recorded(label: &str) -> Vec<Recorded> {
    let text = std::fs::read_to_string(data(&format!("{label}.txt"))).unwrap();
    let mut blocks: Vec<Recorded> = Vec::new();
    let mut section = "";
    for line in text.lines() {
        match line {
            "=== query" => {
                blocks.push(Recorded {
                    query: String::new(),
                    types: String::new(),
                    rows: Vec::new(),
                });
                section = "query";
            }
            "=== types" => section = "types",
            "=== rows" => section = "rows",
            _ => {
                let Some(block) = blocks.last_mut() else {
                    continue;
                };
                match section {
                    "query" => {
                        block.query.push_str(line);
                        block.query.push('\n');
                    }
                    "types" => block.types = line.to_owned(),
                    _ => block.rows.push(line.to_owned()),
                }
            }
        }
    }
    blocks
}

fn restored(directory: &TempDirectory) -> PathBuf {
    let file = directory.path.join("catalog.db");
    let output = Command::new(env!("CARGO_BIN_EXE_eggdb"))
        .arg("restore")
        .arg("--create")
        .arg(&file)
        .arg(data("fixture.sql"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    file
}

fn results(session: &mut Session<'_, Store<'_>>, sql: &str) -> Result<QueryResult, String> {
    let mut found = None;
    session
        .execute(sql, &mut |result| found = Some(result))
        .map_err(|error| error.to_string())?;
    found.ok_or_else(|| "no result".to_owned())
}

fn shown(value: &Value) -> String {
    value.to_text().unwrap_or_default()
}

fn relation_oids(session: &mut Session<'_, Store<'_>>) -> HashMap<String, String> {
    let result = results(
        session,
        "SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')",
    )
    .unwrap();
    result
        .rows
        .iter()
        .map(|row| {
            (
                format!("{}.{}", shown(&row[1]), shown(&row[2])),
                shown(&row[0]),
            )
        })
        .collect()
}

fn compare(label: &str) {
    let directory = TempDirectory::new(label);
    let file = restored(&directory);
    let db = Database::open(&file).unwrap();
    let store = db.sql_store();
    let mut session = store.session(Dialect::PostgreSql);
    session.identify(USER, DATABASE);
    let oids = relation_oids(&mut session);
    let names: HashMap<&str, &str> = oids
        .iter()
        .map(|(name, oid)| (oid.as_str(), name.as_str()))
        .collect();
    let blocks = recorded(label);
    assert!(blocks.len() > 200, "{} recorded queries", blocks.len());
    let mut wrong = Vec::new();
    for block in &blocks {
        let mut query = block.query.clone();
        for (name, oid) in &oids {
            query = query.replace(&format!("'{{{name}}}'"), &format!("'{oid}'"));
        }
        let result = match results(&mut session, &query) {
            Ok(result) => result,
            Err(error) => {
                wrong.push(format!("{query}\nfailed: {error}"));
                continue;
            }
        };
        let types = result
            .columns
            .as_ref()
            .map(|columns| {
                columns
                    .iter()
                    .map(|column| {
                        column
                            .sql_type
                            .map_or_else(|| "text".to_owned(), |sql_type| sql_type.name())
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .unwrap_or_default();
        let rows: Vec<String> = result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| {
                        let text = shown(value);
                        match names.get(text.as_str()) {
                            Some(name) => format!("{{{name}}}"),
                            None => text,
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect();
        if types != block.types {
            wrong.push(format!(
                "{query}\ncolumn types {types}\n   PostgreSQL {}",
                block.types
            ));
        }
        if rows != block.rows {
            wrong.push(format!(
                "{query}\nrows {rows:#?}\nPostgreSQL {:#?}",
                block.rows
            ));
        }
    }
    session.close().unwrap();
    db.close().unwrap();
    assert!(
        wrong.is_empty(),
        "{} of {} queries differ from PostgreSQL 16:\n{}",
        wrong.len(),
        blocks.len(),
        wrong
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

#[test]
fn psql_16_describe_queries_give_postgresql_16_rows_and_types() {
    compare("psql16");
}

#[test]
fn psql_18_describe_queries_give_postgresql_16_rows_and_types() {
    compare("psql18");
}
