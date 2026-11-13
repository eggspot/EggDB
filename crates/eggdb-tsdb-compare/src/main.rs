//! `eggdb-tsdb-compare`: EggDB's metrics store (`eggdb serve --observability-listen`) measured
//! against InfluxDB 2 and Prometheus on the same machine, through the same client of this
//! crate's own (D7, D220).
//!
//! Run with `cargo run --release --manifest-path crates/eggdb-tsdb-compare/Cargo.toml --
//! [--results <file>]`, with EggDB at `EGGDB_OBSERVABILITY_ADDRESS` (`127.0.0.1:18086`, user
//! `eggdb` with `EGGDB_PASSWORD`), InfluxDB at `INFLUXDB_ADDRESS` (`127.0.0.1:8086`, token
//! `INFLUXDB_TOKEN`, organization `INFLUXDB_ORG`, bucket `INFLUXDB_BUCKET`) and Prometheus, started
//! with `--web.enable-remote-write-receiver`, at `PROMETHEUS_ADDRESS` (`127.0.0.1:9090`). A server
//! that is not running is reported and left out. The ratios are reported, not gated.

mod http;
mod influx;
mod measure;
mod prometheus;

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

const HOSTS: usize = 100;
const POINTS_PER_HOST: usize = 600;
const LINES_PER_BATCH: usize = 5_000;
const FLUX_RUNS: usize = 50;
const SERIES: usize = 100;
const SAMPLES_PER_SERIES: usize = 600;
const SAMPLES_PER_REQUEST: usize = 2_000;
const PROMQL_RUNS: usize = 50;

fn setting(variable: &str, default: &str) -> String {
    std::env::var(variable).unwrap_or_else(|_| default.to_owned())
}

fn listening(address: &str, name: &str) -> bool {
    let up = std::net::TcpStream::connect(address).is_ok();
    if !up {
        println!("eggdb-tsdb-compare: nothing listens at {address}; {name} left out");
    }
    up
}

fn now_seconds() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs() as i64)
}

fn compare_influxdb(eggdb: &str, password: &str, lines: &mut Vec<String>) -> Result<(), String> {
    let other = setting("INFLUXDB_ADDRESS", "127.0.0.1:8086");
    if !listening(&other, "InfluxDB") {
        return Ok(());
    }
    let (token, org, bucket) = (setting("INFLUXDB_TOKEN", "compare-token"), setting("INFLUXDB_ORG", "eggdb"), setting("INFLUXDB_BUCKET", "compare"));
    let shape = influx::Shape { hosts: HOSTS, points_per_host: POINTS_PER_HOST, lines_per_batch: LINES_PER_BATCH, last_second: now_seconds() - 5 };
    let eggdb_target = influx::Target { address: eggdb, token: password, org: &org, bucket: &bucket };
    let other_target = influx::Target { address: &other, token: &token, org: &org, bucket: &bucket };
    let written = (influx::write(&eggdb_target, shape)?, influx::write(&other_target, shape)?);
    measure::report("influx.write (line protocol, 5,000 lines a request)", "lines", &written.0, &written.1, "InfluxDB", lines);
    let queried = (influx::queries(&eggdb_target, shape, FLUX_RUNS)?, influx::queries(&other_target, shape, FLUX_RUNS)?);
    measure::report("influx.query (Flux, one host's 1m means over 10 minutes)", "queries", &queried.0, &queried.1, "InfluxDB", lines);
    Ok(())
}

fn compare_prometheus(eggdb: &str, password: &str, lines: &mut Vec<String>) -> Result<(), String> {
    let other = setting("PROMETHEUS_ADDRESS", "127.0.0.1:9090");
    if !listening(&other, "Prometheus") {
        return Ok(());
    }
    let shape = prometheus::Shape {
        series: SERIES,
        samples_per_series: SAMPLES_PER_SERIES,
        samples_per_request: SAMPLES_PER_REQUEST,
        last_millis: (now_seconds() - 5) * 1000,
    };
    let eggdb_authorization = Some(http::basic("eggdb", password));
    let written = (prometheus::write(eggdb, eggdb_authorization.clone(), shape)?, prometheus::write(&other, None, shape)?);
    measure::report("prometheus.remote_write (100 series, 20 samples each a request)", "samples", &written.0, &written.1, "Prometheus", lines);
    let queried = (
        prometheus::queries(eggdb, eggdb_authorization, shape, PROMQL_RUNS)?,
        prometheus::queries(&other, None, shape, PROMQL_RUNS)?,
    );
    measure::report("prometheus.query_range (sum by job of rate over 1m, 15s steps)", "queries", &queried.0, &queried.1, "Prometheus", lines);
    Ok(())
}

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let mut results = None;
    while let Some(argument) = arguments.next() {
        match (argument.as_str(), arguments.next()) {
            ("--results", Some(path)) => results = Some(path),
            _ => {
                eprintln!("usage: eggdb-tsdb-compare [--results <file>]");
                return ExitCode::from(2);
            }
        }
    }
    let eggdb = setting("EGGDB_OBSERVABILITY_ADDRESS", "127.0.0.1:18086");
    let password = setting("EGGDB_PASSWORD", "compare");
    if !listening(&eggdb, "EggDB") {
        return ExitCode::SUCCESS;
    }
    let mut lines = Vec::new();
    for (name, outcome) in [
        ("InfluxDB", compare_influxdb(&eggdb, &password, &mut lines)),
        ("Prometheus", compare_prometheus(&eggdb, &password, &mut lines)),
    ] {
        if let Err(message) = outcome {
            eprintln!("eggdb-tsdb-compare: {name}: {message}");
            return ExitCode::from(1);
        }
    }
    if let Some(path) = results
        && let Err(error) = std::fs::write(&path, lines.join("\n") + "\n")
    {
        eprintln!("eggdb-tsdb-compare: cannot write {path}: {error}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
