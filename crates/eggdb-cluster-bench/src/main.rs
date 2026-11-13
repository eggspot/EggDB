//! `eggdb-cluster-bench <eggdb binary> <work directory> [--workload ycsb-a|ycsb-b|ycsb-c|tpcc]
//! [--nodes N] [--seconds N] [--clients N] [--records N] [--warehouses N] [--seed N]`: starts a
//! Raft cluster of `eggdb serve --cluster` processes on this machine, loads a YCSB table or a
//! TPC-C schema through the leader, then runs the workload from several clients for the given
//! time over the PostgreSQL protocol, writes going to the leader and YCSB reads spread over
//! every node, and prints one `name value unit` line per result (D1053). The TPC-C mix follows
//! the specification's transactions and shares but is not an audited TPC-C result.

mod tpcc;
mod workload;

use eggdb_pgwire::client::{ConnectOptions, PgClient, SslMode};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use workload::{Latencies, Random, Zipfian, read_percent};

const USAGE: &str = "usage: eggdb-cluster-bench <eggdb binary> <work directory> [--workload ycsb-a|ycsb-b|ycsb-c|tpcc] [--nodes N] [--seconds N] [--clients N] [--records N] [--warehouses N] [--seed N]";
const USER: &str = "eggdb";
const PASSWORD: &str = "cluster-bench-password";
const BASE_PORT: u16 = 48_000;
const LEADER_WAIT: Duration = Duration::from_secs(60);
const LOAD_ROWS_PER_STATEMENT: u64 = 500;

#[derive(Debug, Clone)]
struct Settings {
    binary: PathBuf,
    directory: PathBuf,
    workload: String,
    nodes: usize,
    seconds: u64,
    clients: usize,
    records: u64,
    warehouses: u64,
    seed: u64,
}

fn sql_port(node: usize) -> u16 {
    BASE_PORT + (node as u16) * 10
}

fn cluster_port(node: usize) -> u16 {
    sql_port(node) + 1
}

struct Cluster {
    processes: Mutex<Vec<Child>>,
}

impl Cluster {
    fn start(settings: &Settings) -> Result<Self, String> {
        std::fs::create_dir_all(&settings.directory).map_err(|error| format!("cannot create {}: {error}", settings.directory.display()))?;
        let members: Vec<String> = (0..settings.nodes).map(|node| format!("{}=127.0.0.1:{}", node + 1, cluster_port(node))).collect();
        let mut processes = Vec::new();
        for node in 0..settings.nodes {
            let child = Command::new(&settings.binary)
                .arg("serve")
                .arg(settings.directory.join(format!("n{node}.eggdb")))
                .arg("--create")
                .args(["--listen", &format!("127.0.0.1:{}", sql_port(node))])
                .args(["--cluster", &members.join(",")])
                .args(["--cluster-node", &(node + 1).to_string()])
                .env("EGGDB_PASSWORD", PASSWORD)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| format!("cannot start node {node}: {error}"))?;
            processes.push(child);
        }
        Ok(Self { processes: Mutex::new(processes) })
    }

    fn stop(&self) {
        if let Ok(mut processes) = self.processes.lock() {
            for child in processes.iter_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

fn connect(node: usize) -> Result<PgClient, String> {
    PgClient::connect(&ConnectOptions {
        host: "127.0.0.1".to_owned(),
        port: sql_port(node),
        user: USER.to_owned(),
        database: "eggdb".to_owned(),
        password: Some(PASSWORD.to_owned()),
        ssl_mode: SslMode::Disable,
        ssl_root_cert: None,
        replication: false,
    })
    .map_err(|error| format!("cannot connect to node {node}: {error}"))
}

fn leader(nodes: usize) -> Result<usize, String> {
    let started = Instant::now();
    while started.elapsed() < LEADER_WAIT {
        for node in 0..nodes {
            if let Ok(mut client) = connect(node)
                && client.simple_query("CREATE TABLE IF NOT EXISTS cluster_bench_probe (id integer PRIMARY KEY)").is_ok()
            {
                return Ok(node);
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!("no node took writes within {} s", LEADER_WAIT.as_secs()))
}

fn key(record: u64) -> String {
    format!("user{record:012}")
}

fn field(random: &mut Random) -> String {
    (0..100).map(|_| char::from(b'a' + random.below(26) as u8)).collect()
}

fn load_ycsb(client: &mut PgClient, records: u64, random: &mut Random) -> Result<(), String> {
    client
        .simple_query("CREATE TABLE usertable (ycsb_key text PRIMARY KEY, field0 text NOT NULL)")
        .map_err(|error| format!("cannot create usertable: {error}"))?;
    let mut record = 0;
    while record < records {
        let end = (record + LOAD_ROWS_PER_STATEMENT).min(records);
        let values: Vec<String> = (record..end).map(|index| format!("('{}', '{}')", key(index), field(random))).collect();
        client
            .simple_query(&format!("INSERT INTO usertable VALUES {}", values.join(", ")))
            .map_err(|error| format!("cannot load usertable: {error}"))?;
        record = end;
    }
    Ok(())
}

fn ycsb_client(settings: &Settings, id: usize, leader: usize, stop: &AtomicBool) -> Result<(Latencies, Latencies), String> {
    let reads_percent = read_percent(&settings.workload).ok_or("not a YCSB workload")?;
    let zipfian = Zipfian::new(settings.records);
    let mut random = Random::new(settings.seed.wrapping_add(id as u64 + 1));
    let mut writer = connect(leader)?;
    let mut reader = connect(id % settings.nodes)?;
    let (mut reads, mut updates) = (Latencies::default(), Latencies::default());
    while !stop.load(Ordering::Acquire) {
        let record = zipfian.next(&mut random);
        let started = Instant::now();
        if random.below(100) < reads_percent {
            reader
                .simple_query(&format!("SELECT field0 FROM usertable WHERE ycsb_key = '{}'", key(record)))
                .map_err(|error| format!("a read failed: {error}"))?;
            reads.record(started.elapsed().as_micros() as u64);
        } else {
            writer
                .simple_query(&format!("UPDATE usertable SET field0 = '{}' WHERE ycsb_key = '{}'", field(&mut random), key(record)))
                .map_err(|error| format!("an update failed: {error}"))?;
            updates.record(started.elapsed().as_micros() as u64);
        }
    }
    Ok((reads, updates))
}

fn report(name: &str, latencies: &Latencies, seconds: f64) {
    if latencies.count() == 0 {
        return;
    }
    println!("{name}.throughput {:.1} ops/s", latencies.count() as f64 / seconds);
    println!("{name}.p50 {} us", latencies.percentile(50.0));
    println!("{name}.p99 {} us", latencies.percentile(99.0));
}

fn run_clients<T: Send>(settings: &Settings, body: &(dyn Fn(usize, &AtomicBool) -> Result<T, String> + Sync)) -> Result<(Vec<T>, f64), String> {
    let stop = AtomicBool::new(false);
    let started = Instant::now();
    let outcomes: Vec<Result<T, String>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..settings.clients).map(|id| {
            let stop = &stop;
            scope.spawn(move || body(id, stop))
        }).collect();
        std::thread::sleep(Duration::from_secs(settings.seconds));
        stop.store(true, Ordering::Release);
        workers.into_iter().map(|worker| worker.join().unwrap_or_else(|_| Err("a client stopped unexpectedly".to_owned()))).collect()
    });
    let elapsed = started.elapsed().as_secs_f64();
    let results = outcomes.into_iter().collect::<Result<Vec<T>, String>>()?;
    Ok((results, elapsed))
}

fn run(settings: &Settings) -> Result<(), String> {
    let cluster = Cluster::start(settings)?;
    let outcome = (|| -> Result<(), String> {
        let leader = leader(settings.nodes)?;
        let mut random = Random::new(settings.seed);
        let mut client = connect(leader)?;
        println!("cluster.nodes {} nodes", settings.nodes);
        if settings.workload == "tpcc" {
            let loaded = Instant::now();
            tpcc::load(&mut client, settings.warehouses, &mut random)?;
            println!("tpcc.load {:.1} s", loaded.elapsed().as_secs_f64());
            let (results, elapsed) = run_clients(settings, &|id, stop| tpcc::client(leader, connect, settings.warehouses, settings.seed.wrapping_add(id as u64 + 1), stop))?;
            let mut new_orders = Latencies::default();
            let mut all = Latencies::default();
            let mut aborted = 0;
            for result in results {
                new_orders.merge(result.new_orders);
                all.merge(result.all);
                aborted += result.aborted;
            }
            println!("tpcc.tpmC {:.1} new-orders/min", new_orders.count() as f64 * 60.0 / elapsed);
            report("tpcc.transactions", &all, elapsed);
            println!("tpcc.aborted {aborted} transactions");
            return Ok(());
        }
        let loaded = Instant::now();
        load_ycsb(&mut client, settings.records, &mut random)?;
        println!("{}.load {:.1} s", settings.workload, loaded.elapsed().as_secs_f64());
        let (results, elapsed) = run_clients(settings, &|id, stop| ycsb_client(settings, id, leader, stop))?;
        let (mut reads, mut updates) = (Latencies::default(), Latencies::default());
        for (client_reads, client_updates) in results {
            reads.merge(client_reads);
            updates.merge(client_updates);
        }
        report(&format!("{}.read", settings.workload), &reads, elapsed);
        report(&format!("{}.update", settings.workload), &updates, elapsed);
        Ok(())
    })();
    cluster.stop();
    outcome
}

fn parse(arguments: Vec<String>) -> Result<Settings, String> {
    let mut arguments = arguments.into_iter();
    let (Some(binary), Some(directory)) = (arguments.next(), arguments.next()) else {
        return Err(USAGE.to_owned());
    };
    let mut settings = Settings {
        binary: PathBuf::from(binary),
        directory: PathBuf::from(directory),
        workload: "ycsb-a".to_owned(),
        nodes: 3,
        seconds: 60,
        clients: 8,
        records: 100_000,
        warehouses: 4,
        seed: 1,
    };
    while let Some(option) = arguments.next() {
        let value = arguments.next().ok_or_else(|| USAGE.to_owned())?;
        let number = || value.parse::<u64>().map_err(|_| USAGE.to_owned());
        match option.as_str() {
            "--workload" if value == "tpcc" || read_percent(&value).is_some() => settings.workload = value.clone(),
            "--nodes" => settings.nodes = number()?.clamp(1, 9) as usize,
            "--seconds" => settings.seconds = number()?.max(1),
            "--clients" => settings.clients = number()?.clamp(1, 256) as usize,
            "--records" => settings.records = number()?.max(2),
            "--warehouses" => settings.warehouses = number()?.clamp(1, 1_000),
            "--seed" => settings.seed = number()?,
            _ => return Err(USAGE.to_owned()),
        }
    }
    Ok(settings)
}

fn main() -> std::process::ExitCode {
    let settings = match parse(std::env::args().skip(1).collect()) {
        Ok(settings) => settings,
        Err(message) => {
            eprintln!("{message}");
            return std::process::ExitCode::from(2);
        }
    };
    match run(&settings) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("eggdb-cluster-bench: {message}");
            std::process::ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_read_with_their_limits() {
        let parsed = parse(vec!["eggdb".into(), "/tmp/b".into(), "--workload".into(), "tpcc".into(), "--nodes".into(), "50".into()]).unwrap();
        assert_eq!(parsed.workload, "tpcc");
        assert_eq!(parsed.nodes, 9);
        assert!(parse(vec!["eggdb".into(), "/tmp/b".into(), "--workload".into(), "ycsb-z".into()]).is_err());
        assert!(parse(vec!["eggdb".into()]).is_err());
        assert_eq!(key(42), "user000000000042");
    }
}
