//! `eggdb-broker-compare`: EggDB's message broker protocols measured against the brokers they
//! are compatible with, on the same machine, through the same client of this crate's own (D7).
//!
//! Run with `cargo run --release --manifest-path crates/eggdb-broker-compare/Cargo.toml --
//! kafka [--results <file>]`, with `eggdb serve --kafka-listen` at `EGGDB_KAFKA_ADDRESS`
//! (`127.0.0.1:19092` by default) and Apache Kafka at `KAFKA_ADDRESS` (`127.0.0.1:9092`). Both
//! sides get the same records over the same wire, so the ratio compares the servers, not the
//! clients. The ratios are reported, not gated. A side that is not running is reported and the
//! program exits successfully.

mod kafka;
mod measure;

use std::process::ExitCode;

const BATCHES: usize = 2_000;
const BATCH_SIZE: usize = 100;

fn address(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.to_owned())
}

fn kafka(lines: &mut Vec<String>) -> Result<(), String> {
    let eggdb = address("EGGDB_KAFKA_ADDRESS", "127.0.0.1:19092");
    let other = address("KAFKA_ADDRESS", "127.0.0.1:9092");
    for side in [&eggdb, &other] {
        if std::net::TcpStream::connect(side).is_err() {
            println!("eggdb-broker-compare: nothing listens at {side}; Kafka not measured");
            return Ok(());
        }
    }
    let topic = format!("compare-{}", std::process::id());
    let produced_eggdb = kafka::produce(&eggdb, &topic, BATCHES, BATCH_SIZE)?;
    let produced_kafka = kafka::produce(&other, &topic, BATCHES, BATCH_SIZE)?;
    measure::report("kafka.produce (batches of 100 records of 100 bytes, acks=1)", &produced_eggdb, &produced_kafka, "Kafka", lines);
    let wanted = (BATCHES * BATCH_SIZE) as u64;
    let consumed_eggdb = kafka::consume(&eggdb, &topic, wanted)?;
    let consumed_kafka = kafka::consume(&other, &topic, wanted)?;
    measure::report("kafka.fetch (catching up from offset 0)", &consumed_eggdb, &consumed_kafka, "Kafka", lines);
    Ok(())
}

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let broker = arguments.next().unwrap_or_default();
    let mut results = None;
    while let Some(argument) = arguments.next() {
        match (argument.as_str(), arguments.next()) {
            ("--results", Some(path)) => results = Some(path),
            _ => {
                eprintln!("usage: eggdb-broker-compare kafka [--results <file>]");
                return ExitCode::from(2);
            }
        }
    }
    let mut lines = Vec::new();
    let outcome = match broker.as_str() {
        "kafka" => kafka(&mut lines),
        _ => {
            eprintln!("usage: eggdb-broker-compare kafka [--results <file>]");
            return ExitCode::from(2);
        }
    };
    if let Err(message) = outcome {
        eprintln!("eggdb-broker-compare: {message}");
        return ExitCode::from(1);
    }
    if let Some(path) = results
        && let Err(error) = std::fs::write(&path, lines.join("\n") + "\n")
    {
        eprintln!("eggdb-broker-compare: cannot write {path}: {error}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
