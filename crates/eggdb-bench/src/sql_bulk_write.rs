use crate::allocations::CountingAllocator;
use crate::baseline::Metric;
use crate::bench_file::BenchFile;
use crate::database::failed;
use crate::measure;
use eggdb::sql::{Dialect, Session, Store};
use eggdb::{Database, Durability, Options};
use std::fmt::Write;
use std::hint::black_box;

const STATEMENTS_PER_TRANSACTION: u64 = 4;
const ROWS_PER_STATEMENT: u64 = 100;
const ROWS_PER_TRANSACTION: u64 = STATEMENTS_PER_TRANSACTION * ROWS_PER_STATEMENT;
const CACHE_BYTES: usize = 256 << 20;

pub(crate) const MYSQL: &str = "sql_mysql.bulk_write_in_transaction";
pub(crate) const SQLITE: &str = "sql_sqlite.bulk_write_in_transaction";
pub(crate) const ORACLE: &str = "sql_oracle.bulk_write_in_transaction";
pub(crate) const SQLSERVER: &str = "sql_sqlserver.bulk_write_in_transaction";

struct Writer<'store> {
    session: Session<'store>,
    dialect: Dialect,
    next_id: u64,
}

impl Writer<'_> {
    fn execute(&mut self, sql: &str) -> Result<(), String> {
        self.session
            .execute(sql, &mut |result| {
                black_box(result);
            })
            .map_err(failed(sql))
    }

    fn begin(&mut self) -> Result<(), String> {
        match self.dialect {
            Dialect::MySql => self.execute("START TRANSACTION"),
            Dialect::SqlServer => self.execute("BEGIN TRANSACTION"),
            Dialect::Oracle => Ok(()),
            _ => self.execute("BEGIN"),
        }
    }

    fn insert(&mut self) -> Result<(), String> {
        let mut insert = String::from("INSERT INTO accounts (id, balance) VALUES ");
        for row in 0..ROWS_PER_STATEMENT {
            if row > 0 {
                insert.push_str(", ");
            }
            let id = self.next_id;
            self.next_id += 1;
            let _ = write!(insert, "({id}, {})", id % 1000);
        }
        self.execute(&insert)
    }

    fn transactions(&mut self, count: u64) -> Result<(), String> {
        for _ in 0..count {
            self.begin()?;
            for _ in 0..STATEMENTS_PER_TRANSACTION {
                self.insert()?;
            }
            self.execute("COMMIT")?;
        }
        Ok(())
    }
}

fn writer<'store>(store: &'store Store<'_>, dialect: Dialect) -> Result<Writer<'store>, String> {
    let mut writer = Writer {
        session: store.session(dialect),
        dialect,
        next_id: 0,
    };
    writer.execute("CREATE TABLE accounts (id bigint PRIMARY KEY, balance int)")?;
    Ok(writer)
}

fn open(file: &BenchFile) -> Result<Database, String> {
    let options = Options::new()
        .cache_bytes(CACHE_BYTES)
        .durability(Durability::Off);
    Database::open_with(&file.path, options).map_err(failed("open"))
}

fn bulk_write(benchmark: &'static str, dialect: Dialect) -> Result<Vec<Metric>, String> {
    let subject_file = BenchFile::new(&format!("bulk-write-{}", dialect_name(dialect)))?;
    let reference_file =
        BenchFile::new(&format!("bulk-write-reference-{}", dialect_name(dialect)))?;
    let subject_db = open(&subject_file)?;
    let reference_db = open(&reference_file)?;
    let subject_store = subject_db.sql_store();
    let reference_store = reference_db.sql_store();
    let mut subject = writer(&subject_store, dialect)?;
    let mut reference = writer(&reference_store, Dialect::PostgreSql)?;
    let measured = measure::paired(
        &mut |iterations| subject.transactions(iterations),
        &mut |iterations| reference.transactions(iterations),
    )?;
    Ok(vec![
        Metric::ratio(benchmark, "ratio_to_postgresql", measured.ratio),
        Metric::reported(
            benchmark,
            "ns_per_row",
            measured.subject_ns / ROWS_PER_TRANSACTION as f64,
        ),
    ])
}

fn dialect_name(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::MySql => "mysql",
        Dialect::Sqlite => "sqlite",
        Dialect::Oracle => "oracle",
        Dialect::SqlServer => "sqlserver",
        _ => "postgresql",
    }
}

pub(crate) fn mysql(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    bulk_write(MYSQL, Dialect::MySql)
}

pub(crate) fn sqlite(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    bulk_write(SQLITE, Dialect::Sqlite)
}

pub(crate) fn oracle(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    bulk_write(ORACLE, Dialect::Oracle)
}

pub(crate) fn sqlserver(_allocator: &CountingAllocator) -> Result<Vec<Metric>, String> {
    bulk_write(SQLSERVER, Dialect::SqlServer)
}
