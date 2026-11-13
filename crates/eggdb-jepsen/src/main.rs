//! `eggdb-jepsen <eggdb binary> <work directory> [--seconds N] [--seed N] [--partitions 1]
//! [--cluster 1] [--delays 1]`: starts three `eggdb serve` processes as one replication group (a
//! primary and two replicas with automatic failover and one synchronous acknowledgement, D1029)
//! or, with `--cluster 1`, as three voters of a Raft cluster (`serve --cluster`, D1046), adds
//! unique elements to a RESP set from several clients while a nemesis kills (the leader more
//! often than the rest), restarts and pauses the processes, cuts them off and slows the network,
//! then heals the group and checks that every acknowledged element survived on the final
//! primary and that every other server holds exactly its set. Exits with status 1 on a
//! violation.

mod resp;

use resp::{Reply, RespClient};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const USAGE: &str = "usage: eggdb-jepsen <eggdb binary> <work directory> [--seconds N] [--seed N] [--partitions 1] [--cluster 1] [--delays 1]";
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
    cluster: u16,
}

fn ports(node: usize) -> Ports {
    let base = BASE_PORT + (node as u16) * 10;
    Ports { sql: base, resp: base + 1, replication: base + 2, cluster: base + 3 }
}

fn cluster_members() -> String {
    (0..NODES).map(|node| format!("{}=127.0.0.1:{}", node + 1, ports(node).cluster)).collect::<Vec<_>>().join(",")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Replication,
    Cluster,
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
    mode: Mode,
    processes: Mutex<Vec<Option<Child>>>,
}

impl Cluster {
    fn start(&self, node: usize, follow: Option<usize>) -> Result<(), String> {
        let ports = ports(node);
        let file = self.directory.join(format!("n{node}.eggdb"));
        let mut command = Command::new(&self.binary);
        command
            .arg("serve")
            .arg(&file)
            .args(["--listen", &format!("127.0.0.1:{}", ports.sql)])
            .args(["--resp-listen", &resp_address(node)])
            .env("EGGDB_PASSWORD", PASSWORD)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match self.mode {
            Mode::Cluster => {
                command
                    .args(["--cluster", &cluster_members()])
                    .args(["--cluster-node", &(node + 1).to_string()]);
                if !file.exists() {
                    command.arg("--create");
                } else if follow.is_some() {
                    command.arg("--cluster-seed");
                }
            }
            Mode::Replication => {
                let peers: Vec<String> = (0..NODES).filter(|other| *other != node).map(replication_address).collect();
                command
                    .args(["--replication-listen", &replication_address(node)])
                    .args(["--replication-advertise", &replication_address(node)])
                    .args(["--replication-peers", &peers.join(",")])
                    .args(["--failover-after", "3"])
                    .args(["--synchronous-replicas", "1"]);
                match follow {
                    Some(primary) => {
                        command.args(["--replica-of", &replication_address(primary)]);
                    }
                    None => {
                        command.arg("--create");
                    }
                }
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

    fn exited_by_itself(&self, node: usize) -> bool {
        let Ok(mut processes) = self.processes.lock() else {
            return false;
        };
        let exited = processes[node].as_mut().is_some_and(|child| child.try_wait().is_ok_and(|status| status.is_some()));
        if exited {
            processes[node] = None;
        }
        exited
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

fn iptables(action: &str, port: u16) -> bool {
    Command::new("sudo")
        .args(["-n", "iptables", action, "INPUT", "-p", "tcp", "-i", "lo", "--dport", &port.to_string(), "-j", "DROP"])
        .status()
        .is_ok_and(|status| status.success())
}

/// Slows every packet on the loopback interface by `delay` for `length`: needs `sudo tc`.
fn slow_network(delay: Duration, length: Duration) -> bool {
    let delay = format!("{}ms", delay.as_millis());
    let added = Command::new("sudo")
        .args(["-n", "tc", "qdisc", "add", "dev", "lo", "root", "netem", "delay", &delay])
        .status()
        .is_ok_and(|status| status.success());
    if !added {
        return false;
    }
    std::thread::sleep(length);
    let _ = Command::new("sudo").args(["-n", "tc", "qdisc", "del", "dev", "lo", "root"]).status();
    true
}

/// Cuts every connection to `node`'s replication listener (or cluster port) for `length`, so its
/// peers and, for a primary, its replicas cannot reach it while it keeps running: needs
/// `sudo iptables`.
fn partition(node: usize, mode: Mode, length: Duration) -> bool {
    let port = match mode {
        Mode::Replication => ports(node).replication,
        Mode::Cluster => ports(node).cluster,
    };
    if !iptables("-A", port) {
        return false;
    }
    std::thread::sleep(length);
    while iptables("-D", port) {}
    true
}

#[derive(Debug, Clone, Copy)]
struct Faults {
    partitions: bool,
    delays: bool,
}

fn nemesis(cluster: &Cluster, stop: &AtomicBool, seed: u64, faults: Faults, log: &Mutex<Vec<String>>) {
    let mut random = Random(seed.max(1));
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_secs(4));
        for node in 0..NODES {
            if cluster.exited_by_itself(node) {
                let primary = primary_of(cluster);
                let _ = cluster.start(node, primary.filter(|primary| *primary != node).or(Some(node)));
                if let Ok(mut log) = log.lock() {
                    log.push(format!("node {node} stopped by itself; restarted it to copy the leader"));
                }
            }
        }
        let mut node = random.below(NODES);
        let choice = random.below(6);
        if choice == 5
            && let Some(primary) = primary_of(cluster)
        {
            node = primary;
        }
        let entry = match choice {
            3 if faults.partitions => {
                if partition(node, cluster.mode, Duration::from_secs(8)) {
                    format!("cut node {node} off for 8 s")
                } else {
                    "could not partition: sudo iptables failed".to_owned()
                }
            }
            4 if faults.delays => {
                if slow_network(Duration::from_millis(150), Duration::from_secs(6)) {
                    "slowed the network by 150 ms for 6 s".to_owned()
                } else {
                    "could not slow the network: sudo tc failed".to_owned()
                }
            }
            0 | 5 => {
                cluster.kill(node);
                std::thread::sleep(Duration::from_secs(5));
                let primary = primary_of(cluster);
                let _ = cluster.start(node, primary.filter(|primary| *primary != node));
                format!("killed node {node}{} and restarted it following {primary:?}", if choice == 5 { " (the primary)" } else { "" })
            }
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

fn run(binary: &Path, directory: &Path, seconds: u64, seed: u64, faults: Faults, mode: Mode) -> Result<bool, String> {
    std::fs::create_dir_all(directory).map_err(|error| format!("cannot create {}: {error}", directory.display()))?;
    let cluster = Arc::new(Cluster {
        binary: binary.to_path_buf(),
        directory: directory.to_path_buf(),
        mode,
        processes: Mutex::new((0..NODES).map(|_| None).collect()),
    });
    cluster.start(0, None)?;
    std::thread::sleep(Duration::from_secs(2));
    for node in 1..NODES {
        cluster.start(node, if mode == Mode::Cluster { None } else { Some(0) })?;
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
        scope.spawn(|| nemesis(&cluster, &stop, seed, faults, &log));
        while started.elapsed() < Duration::from_secs(seconds) {
            std::thread::sleep(Duration::from_millis(200));
        }
        stop.store(true, Ordering::Release);
    });
    for node in 0..NODES {
        cluster.signal(node, "-CONT");
        if cluster.exited_by_itself(node) || !cluster.running(node) {
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
    let mut faults = Faults { partitions: false, delays: false };
    let mut mode = Mode::Replication;
    while let Some(argument) = arguments.next() {
        let value = arguments.next().and_then(|value| value.parse::<u64>().ok());
        match (argument.as_str(), value) {
            ("--seconds", Some(value)) => seconds = value,
            ("--seed", Some(value)) => seed = value,
            ("--partitions", Some(value)) => faults.partitions = value != 0,
            ("--delays", Some(value)) => faults.delays = value != 0,
            ("--cluster", Some(value)) => mode = if value != 0 { Mode::Cluster } else { Mode::Replication },
            _ => {
                eprintln!("{USAGE}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    match run(Path::new(&binary), Path::new(&directory), seconds, seed, faults, mode) {
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
