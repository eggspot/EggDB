//! `eggdb-jepsen <eggdb binary> <work directory> [--seconds N] [--seed N]`: starts three `eggdb
//! serve` processes as one replication group (a primary and two replicas with automatic
//! failover and one synchronous acknowledgement), adds unique elements to a RESP set from
//! several clients while a nemesis kills, restarts and pauses the processes, then heals the
//! group and checks that every acknowledged element survived on the final primary and that
//! every replica holds exactly the primary's set (D1029). Exits with status 1 on a violation.

mod resp;

use resp::{Reply, RespClient};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const USAGE: &str = "usage: eggdb-jepsen <eggdb binary> <work directory> [--seconds N] [--seed N]";
const NODES: usize = 3;
const CLIENTS: usize = 4;
const USER: &str = "eggdb";
const PASSWORD: &str = "jepsen-password";
const SET_KEY: &str = "jepsen";
const BASE_PORT: u16 = 47_000;
const HEAL_WAIT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy)]
struct Ports {
    sql: u16,
    resp: u16,
    replication: u16,
}

fn ports(node: usize) -> Ports {
    let base = BASE_PORT + (node as u16) * 10;
    Ports { sql: base, resp: base + 1, replication: base + 2 }
}

fn replication_address(node: usize) -> String {
    format!("127.0.0.1:{}", ports(node).replication)
}

fn resp_address(node: usize) -> String {
    format!("127.0.0.1:{}", ports(node).resp)
}

struct Cluster {
    binary: PathBuf,
    directory: PathBuf,
    processes: Mutex<Vec<Option<Child>>>,
}

impl Cluster {
    fn start(&self, node: usize, follow: Option<usize>) -> Result<(), String> {
        let ports = ports(node);
        let peers: Vec<String> = (0..NODES).filter(|other| *other != node).map(replication_address).collect();
        let mut command = Command::new(&self.binary);
        command
            .arg("serve")
            .arg(self.directory.join(format!("n{node}.eggdb")))
            .args(["--listen", &format!("127.0.0.1:{}", ports.sql)])
            .args(["--resp-listen", &resp_address(node)])
            .args(["--replication-listen", &replication_address(node)])
            .args(["--replication-advertise", &replication_address(node)])
            .args(["--replication-peers", &peers.join(",")])
            .args(["--failover-after", "3"])
            .args(["--synchronous-replicas", "1"])
            .env("EGGDB_PASSWORD", PASSWORD)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match follow {
            Some(primary) => {
                command.args(["--replica-of", &replication_address(primary)]);
            }
            None => {
                command.arg("--create");
            }
        }
        let child = command.spawn().map_err(|error| format!("cannot start node {node}: {error}"))?;
        if let Ok(mut processes) = self.processes.lock() {
            processes[node] = Some(child);
        }
        Ok(())
    }

    fn kill(&self, node: usize) {
        if let Ok(mut processes) = self.processes.lock()
            && let Some(mut child) = processes[node].take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn signal(&self, node: usize, signal: &str) {
        let pid = self.processes.lock().ok().and_then(|processes| processes[node].as_ref().map(Child::id));
        if let Some(pid) = pid {
            let _ = Command::new("kill").arg(signal).arg(pid.to_string()).status();
        }
    }

    fn running(&self, node: usize) -> bool {
        self.processes.lock().is_ok_and(|processes| processes[node].is_some())
    }

    fn stop_all(&self) {
        for node in 0..NODES {
            self.signal(node, "-CONT");
            self.kill(node);
        }
    }
}

fn primary_of(cluster: &Cluster) -> Option<usize> {
    (0..NODES).filter(|node| cluster.running(*node)).find(|node| {
        RespClient::connect(&resp_address(*node), USER, PASSWORD)
            .and_then(|mut client| client.call(&["SADD", "jepsen-probe", "x"]))
            .is_ok_and(|reply| matches!(reply, Reply::Integer(_)))
    })
}

fn members(node: usize) -> Result<BTreeSet<String>, String> {
    let mut client = RespClient::connect(&resp_address(node), USER, PASSWORD)?;
    match client.call(&["SMEMBERS", SET_KEY])? {
        Reply::Array(items) => Ok(items
            .into_iter()
            .filter_map(|item| match item {
                Reply::Bulk(Some(bytes)) => String::from_utf8(bytes).ok(),
                _ => None,
            })
            .collect()),
        other => Err(format!("SMEMBERS answered {other:?}")),
    }
}

#[derive(Default)]
struct History {
    acknowledged: BTreeSet<String>,
    unknown: BTreeSet<String>,
    refused: u64,
}

fn client_loop(id: usize, stop: &AtomicBool, counter: &AtomicU64, history: &Mutex<History>) {
    let mut connection: Option<(usize, RespClient)> = None;
    let mut node = id % NODES;
    while !stop.load(Ordering::Acquire) {
        let value = format!("{id}-{}", counter.fetch_add(1, Ordering::AcqRel));
        if connection.as_ref().is_none_or(|(at, _)| *at != node) {
            connection = RespClient::connect(&resp_address(node), USER, PASSWORD).ok().map(|client| (node, client));
        }
        let Some((_, client)) = connection.as_mut() else {
            node = (node + 1) % NODES;
            std::thread::sleep(Duration::from_millis(50));
            continue;
        };
        let outcome = client.call(&["SADD", SET_KEY, &value]);
        let Ok(mut history) = history.lock() else {
            return;
        };
        match outcome {
            Ok(Reply::Integer(_)) => {
                history.acknowledged.insert(value);
            }
            Ok(_) => {
                history.refused += 1;
                node = (node + 1) % NODES;
            }
            Err(_) => {
                history.unknown.insert(value);
                connection = None;
                node = (node + 1) % NODES;
            }
        }
    }
}

struct Random(u64);

impl Random {
    fn below(&mut self, bound: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % bound as u64) as usize
    }
}

fn nemesis(cluster: &Cluster, stop: &AtomicBool, seed: u64, log: &Mutex<Vec<String>>) {
    let mut random = Random(seed.max(1));
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_secs(4));
        let node = random.below(NODES);
        let entry = match random.below(3) {
            0 => {
                cluster.kill(node);
                std::thread::sleep(Duration::from_secs(5));
                let primary = primary_of(cluster);
                let _ = cluster.start(node, primary.filter(|primary| *primary != node));
                format!("killed node {node} and restarted it following {primary:?}")
            }
            1 => {
                cluster.signal(node, "-STOP");
                std::thread::sleep(Duration::from_secs(5));
                cluster.signal(node, "-CONT");
                format!("paused node {node} for 5 s")
            }
            _ => format!("left node {node} alone"),
        };
        if let Ok(mut log) = log.lock() {
            log.push(entry);
        }
    }
}

fn run(binary: &Path, directory: &Path, seconds: u64, seed: u64) -> Result<bool, String> {
    std::fs::create_dir_all(directory).map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let cluster = Arc::new(Cluster {
        binary: binary.to_path_buf(),
        directory: directory.to_path_buf(),
        processes: Mutex::new((0..NODES).map(|_| None).collect()),
    });
    cluster.start(0, None)?;
    std::thread::sleep(Duration::from_secs(2));
    for node in 1..NODES {
        cluster.start(node, Some(0))?;
    }
    std::thread::sleep(Duration::from_secs(3));
    let stop = AtomicBool::new(false);
    let counter = AtomicU64::new(0);
    let history = Mutex::new(History::default());
    let log = Mutex::new(Vec::new());
    let started = Instant::now();
    std::thread::scope(|scope| {
        for id in 0..CLIENTS {
            let (stop, counter, history) = (&stop, &counter, &history);
            scope.spawn(move || client_loop(id, stop, counter, history));
        }
        scope.spawn(|| nemesis(&cluster, &stop, seed, &log));
        while started.elapsed() < Duration::from_secs(seconds) {
            std::thread::sleep(Duration::from_millis(200));
        }
        stop.store(true, Ordering::Release);
    });
    for node in 0..NODES {
        cluster.signal(node, "-CONT");
        if !cluster.running(node) {
            let primary = primary_of(&cluster);
            let _ = cluster.start(node, primary.filter(|primary| *primary != node));
        }
    }
    std::thread::sleep(HEAL_WAIT);
    let outcome = check(&cluster, &history, &log);
    cluster.stop_all();
    outcome
}

fn check(cluster: &Cluster, history: &Mutex<History>, log: &Mutex<Vec<String>>) -> Result<bool, String> {
    let history = history.lock().map_err(|_| "the history was lost")?;
    let primary = primary_of(cluster).ok_or("no primary takes writes after the group healed")?;
    let final_set = members(primary)?;
    let lost: Vec<&String> = history.acknowledged.iter().filter(|value| !final_set.contains(*value)).collect();
    let unexpected: Vec<&String> = final_set
        .iter()
        .filter(|value| !history.acknowledged.contains(*value) && !history.unknown.contains(*value))
        .collect();
    let mut divergent = Vec::new();
    for node in (0..NODES).filter(|node| *node != primary) {
        match members(node) {
            Ok(set) if set == final_set => {}
            Ok(set) => divergent.push(format!("node {node} holds {} elements, the primary {}", set.len(), final_set.len())),
            Err(message) => divergent.push(format!("node {node}: {message}")),
        }
    }
    println!(
        "acknowledged {}, unknown {}, refused {}, final {} on node {primary}",
        history.acknowledged.len(),
        history.unknown.len(),
        history.refused,
        final_set.len()
    );
    if let Ok(log) = log.lock() {
        for entry in log.iter() {
            println!("nemesis: {entry}");
        }
    }
    for value in &lost {
        println!("LOST acknowledged element {value}");
    }
    for value in &unexpected {
        println!("UNEXPECTED element {value}, never written");
    }
    for entry in &divergent {
        println!("DIVERGENT {entry}");
    }
    Ok(lost.is_empty() && unexpected.is_empty() && divergent.is_empty())
}

fn main() -> std::process::ExitCode {
    let mut arguments = std::env::args().skip(1);
    let (Some(binary), Some(directory)) = (arguments.next(), arguments.next()) else {
        eprintln!("{USAGE}");
        return std::process::ExitCode::from(2);
    };
    let mut seconds = 60u64;
    let mut seed = 1u64;
    while let Some(argument) = arguments.next() {
        let value = arguments.next().and_then(|value| value.parse::<u64>().ok());
        match (argument.as_str(), value) {
            ("--seconds", Some(value)) => seconds = value,
            ("--seed", Some(value)) => seed = value,
            _ => {
                eprintln!("{USAGE}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    match run(Path::new(&binary), Path::new(&directory), seconds, seed) {
        Ok(true) => {
            println!("no lost writes and no divergence");
            std::process::ExitCode::SUCCESS
        }
        Ok(false) => std::process::ExitCode::from(1),
        Err(message) => {
            eprintln!("eggdb-jepsen: {message}");
            std::process::ExitCode::from(2)
        }
    }
}
