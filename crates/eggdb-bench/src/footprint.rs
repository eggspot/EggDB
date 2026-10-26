use crate::baseline::Metric;
use crate::bench_file::BenchFile;
use crate::database::failed;
use crate::measure::median;
use eggdb::{Database, Options};
use eggdb_platform::process::run_with_peak_memory;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Instant;

pub(crate) const EXIT_AT_ONCE: &str = "--exit-at-once";

pub(crate) const BINARY: &str = "footprint.binary";
pub(crate) const START: &str = "footprint.start";
pub(crate) const OPEN_AND_GET: &str = "footprint.open_and_get";

const SIZE_TOLERANCE_PERCENT: u32 = 1;
const MEMORY_TOLERANCE_PERCENT: u32 = 5;
const START_TOLERANCE_PERCENT: u32 = 50;
const ROUNDS: usize = 20;
const FIXTURE_ENTRIES: u64 = 10_000;
const FIXTURE_KEY: &str = "key-005000";
const REFERENCE_RATIO: &str = "ratio_to_a_program_that_exits_at_once";

pub(crate) fn binary() -> Result<Vec<Metric>, String> {
    let eggdb = built_eggdb()?;
    let bytes = std::fs::metadata(&eggdb)
        .map_err(|error| format!("{}: {error}", eggdb.display()))?
        .len();
    Ok(vec![Metric::ceiling(
        BINARY,
        "stripped_release_bytes",
        bytes as f64,
        SIZE_TOLERANCE_PERCENT,
    )])
}

pub(crate) fn start() -> Result<Vec<Metric>, String> {
    let eggdb = built_eggdb()?;
    let runs = run_against_reference(&mut || {
        let mut command = Command::new(&eggdb);
        command.arg("--version");
        command
    })?;
    Ok(runs.metrics(START, "ns_per_start"))
}

pub(crate) fn open_and_get() -> Result<Vec<Metric>, String> {
    let eggdb = built_eggdb()?;
    let fixture = BenchFile::new("footprint-fixture")?;
    write_fixture(&fixture.path)?;
    let runs = run_against_reference(&mut || {
        let mut command = Command::new(&eggdb);
        command.arg("get").arg(&fixture.path).arg(FIXTURE_KEY);
        command
    })?;
    Ok(runs.metrics(OPEN_AND_GET, "ns_per_run"))
}

fn built_eggdb() -> Result<PathBuf, String> {
    static BUILT: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    BUILT.get_or_init(build_eggdb).clone()
}

fn build_eggdb() -> Result<PathBuf, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| env!("CARGO").into());
    let mut build = Command::new(cargo);
    build.args([
        "build",
        "--quiet",
        "--manifest-path",
        concat!(env!("CARGO_MANIFEST_DIR"), "/../eggdb/Cargo.toml"),
        "--bin",
        "eggdb",
    ]);
    if !cfg!(debug_assertions) {
        build.arg("--release");
    }
    let status = build.status().map_err(failed("cargo build"))?;
    if !status.success() {
        return Err(format!("building the eggdb binary failed: {status}"));
    }
    let directory = std::env::current_exe()
        .map_err(failed("this benchmark's path"))?
        .parent()
        .ok_or("this benchmark's path has no directory")?
        .to_path_buf();
    Ok(directory.join(format!("eggdb{}", std::env::consts::EXE_SUFFIX)))
}

fn write_fixture(path: &Path) -> Result<(), String> {
    let db = Database::open_with(path, Options::new()).map_err(failed("create the fixture"))?;
    db.write(|tx| {
        for index in 0..FIXTURE_ENTRIES {
            tx.put(format!("key-{index:06}"), format!("value-{index:06}"))?;
        }
        Ok(())
    })
    .map_err(failed("fill the fixture"))?;
    db.close().map_err(failed("close the fixture"))
}

struct Runs {
    ns: f64,
    ratio: f64,
    peak_resident_bytes: f64,
}

impl Runs {
    fn metrics(&self, benchmark: &'static str, ns_name: &'static str) -> Vec<Metric> {
        vec![
            Metric::ceiling(
                benchmark,
                REFERENCE_RATIO,
                self.ratio,
                START_TOLERANCE_PERCENT,
            ),
            Metric::ceiling(
                benchmark,
                "peak_resident_bytes",
                self.peak_resident_bytes,
                MEMORY_TOLERANCE_PERCENT,
            ),
            Metric::reported(benchmark, ns_name, self.ns),
        ]
    }
}

fn run_against_reference(subject: &mut dyn FnMut() -> Command) -> Result<Runs, String> {
    let reference = std::env::current_exe().map_err(failed("this benchmark's path"))?;
    let reference = || {
        let mut command = Command::new(&reference);
        command.arg(EXIT_AT_ONCE);
        command
    };
    timed_run(&mut reference())?;
    timed_run(&mut subject())?;
    let mut ns = Vec::with_capacity(ROUNDS);
    let mut ratios = Vec::with_capacity(ROUNDS);
    let mut peaks = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let reference_ns = timed_run(&mut reference())?;
        let subject_ns = timed_run(&mut subject())?;
        ns.push(subject_ns);
        ratios.push(subject_ns / reference_ns);
        peaks.push(peak_resident_bytes(subject())? as f64);
    }
    Ok(Runs {
        ns: median(&mut ns),
        ratio: median(&mut ratios),
        peak_resident_bytes: median(&mut peaks),
    })
}

fn quiet(command: &mut Command) -> (&mut Command, String) {
    let program = command.get_program().to_string_lossy().into_owned();
    let command = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    (command, program)
}

fn timed_run(command: &mut Command) -> Result<f64, String> {
    let (command, program) = quiet(command);
    let start = Instant::now();
    let status = command.status().map_err(failed(&program))?;
    let ns = start.elapsed().as_nanos() as f64;
    if !status.success() {
        return Err(format!("{program} failed: {status}"));
    }
    Ok(ns)
}

fn peak_resident_bytes(mut command: Command) -> Result<u64, String> {
    let program = quiet(&mut command).1;
    let exit = run_with_peak_memory(command).map_err(failed(&program))?;
    if exit.code != Some(0) {
        return Err(format!("{program} exited with {:?}", exit.code));
    }
    Ok(exit.peak_resident_bytes)
}
