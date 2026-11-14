//! `eggdb-tpcc-compare`: EggDB measured against PostgreSQL and MySQL on the same machine, each
//! over its own protocol, with a TPC-C-shaped schema and its five transactions: New-Order,
//! Payment, Order-Status, Delivery and Stock-Level (D7, D1140).
//!
//! Run with `cargo run --release --manifest-path crates/eggdb-tpcc-compare/Cargo.toml`, with
//! `eggdb serve` reachable at `EGGDB_POSTGRESQL_URL` and `EGGDB_MYSQL_URL`, PostgreSQL at
//! `POSTGRESQL_URL` and MySQL at `MYSQL_URL` (MySQL passwords in `EGGDB_PASSWORD` and
//! `MYSQL_PASSWORD`). Each pair whose two servers answer loads the same generated population
//! (`TPCC_WAREHOUSES`, `TPCC_CUSTOMERS` per district and `TPCC_ITEMS`) and runs the same
//! transactions; the time per transaction is reported as EggDB's ratio to the other database. A
//! missing server skips its pair. This is not an audited TPC-C run: no terminals, keying or
//! think times, a smaller population, and no response-time constraints.

#[path = "../../eggdb-bench/src/baseline.rs"]
#[allow(dead_code)]
mod baseline;
#[path = "../../eggdb-bench/src/harness.rs"]
mod harness;
#[path = "../../eggdb-bench/src/measure.rs"]
#[allow(dead_code)]
mod measure;
mod sql;
mod tpcc;

use crate::baseline::Metric;
use crate::harness::Benchmark;
use crate::sql::Connection;
use crate::tpcc::Scale;
use std::cell::RefCell;
use std::process::ExitCode;

fn setting(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn count(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|text| text.parse().ok()).unwrap_or(default).max(1)
}

#[derive(Debug)]
struct Pair {
    reference: &'static str,
    eggdb: Connection,
    other: Connection,
    loaded: bool,
    steps: (u64, u64),
}

impl Pair {
    fn measure(&mut self, name: &str, scale: Scale) -> Result<measure::Paired, String> {
        if !self.loaded {
            tpcc::load(&mut self.eggdb, scale)?;
            tpcc::load(&mut self.other, scale)?;
            self.loaded = true;
        }
        let (eggdb, other) = (&mut self.eggdb, &mut self.other);
        let (ours, theirs) = (&mut self.steps.0, &mut self.steps.1);
        measure::paired(
            &mut |iterations| {
                for _ in 0..iterations {
                    tpcc::run(eggdb, name, scale, *ours)?;
                    *ours += 1;
                }
                Ok(())
            },
            &mut |iterations| {
                for _ in 0..iterations {
                    tpcc::run(other, name, scale, *theirs)?;
                    *theirs += 1;
                }
                Ok(())
            },
        )
    }
}

fn pairs() -> Vec<Pair> {
    let mut pairs = Vec::new();
    let eggdb_password = std::env::var("EGGDB_PASSWORD").ok();
    let postgres = (
        Connection::postgres(&setting("EGGDB_POSTGRESQL_URL", "postgresql://eggdb@127.0.0.1:5433/eggdb")),
        Connection::postgres(&setting("POSTGRESQL_URL", "postgresql://postgres:postgres@127.0.0.1:5432/postgres")),
    );
    match postgres {
        (Ok(eggdb), Ok(other)) => pairs.push(Pair { reference: "eggdb_to_postgresql", eggdb, other, loaded: false, steps: (0, 0) }),
        (eggdb, other) => println!(
            "eggdb-tpcc-compare: skipping PostgreSQL ({})",
            eggdb.err().or(other.err()).unwrap_or_default()
        ),
    }
    let mysql = (
        Connection::mysql(&setting("EGGDB_MYSQL_URL", "mysql://eggdb@127.0.0.1:3307/eggdb"), eggdb_password),
        Connection::mysql(&setting("MYSQL_URL", "mysql://root@127.0.0.1:3306/tpcc"), std::env::var("MYSQL_PASSWORD").ok()),
    );
    match mysql {
        (Ok(eggdb), Ok(other)) => pairs.push(Pair { reference: "eggdb_to_mysql", eggdb, other, loaded: false, steps: (0, 0) }),
        (eggdb, other) => println!(
            "eggdb-tpcc-compare: skipping MySQL ({})",
            eggdb.err().or(other.err()).unwrap_or_default()
        ),
    }
    pairs
}

fn main() -> ExitCode {
    let scale = Scale {
        warehouses: count("TPCC_WAREHOUSES", 1),
        customers: count("TPCC_CUSTOMERS", 300).max(3),
        items: count("TPCC_ITEMS", 10_000),
    };
    let pairs = RefCell::new(pairs());
    if pairs.borrow().is_empty() {
        println!("eggdb-tpcc-compare: no pair of servers answered; nothing measured");
        return ExitCode::SUCCESS;
    }
    let title = format!(
        "EggDB against {} with {} warehouse(s), {} customers per district and {} items",
        pairs.borrow().iter().map(|pair| pair.reference).collect::<Vec<_>>().join(" and "),
        scale.warehouses,
        scale.customers,
        scale.items
    );
    println!("{title}");
    let pairs = &pairs;
    let benchmarks: Vec<Benchmark<'_>> = tpcc::TRANSACTIONS
        .iter()
        .map(|&name| -> Benchmark<'_> {
            (
                name,
                Box::new(move || {
                    let mut metrics = Vec::new();
                    for pair in pairs.borrow_mut().iter_mut() {
                        let measured = pair.measure(name, scale)?;
                        metrics.push(Metric::ratio(name, pair.reference, measured.ratio));
                    }
                    Ok(metrics)
                }),
            )
        })
        .collect();
    let outcome = harness::run(&title, std::env::args().skip(1), benchmarks);
    harness::exit_code("eggdb-tpcc-compare", outcome)
}
