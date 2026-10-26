//! `eggdb-bench`: EggDB's benchmarks, gated against `benchmarks/baseline.txt` (D7, D80).
//!
//! Run with `cargo run --release -p eggdb-bench`. Each benchmark reports metrics of three
//! kinds:
//! - counts (file operations, evictions and heap allocations per operation), the same on every
//!   machine, which fail when they differ from their baseline line for `any` system;
//! - ratios of a hot path's time to a reference workload run in the same process, which fail
//!   when the median of interleaved samples is more than 30% above the line for this system,
//!   after one retry;
//! - times in nanoseconds per operation, reported and never gated.
//!
//! `--update` rewrites this system's lines and the shared count lines, `--baseline <path>`
//! reads another file, and the results are added to `$GITHUB_STEP_SUMMARY` when it is set.

mod allocations;
mod baseline;
mod btree;
mod log;
mod measure;
mod page_cache;

use crate::allocations::CountingAllocator;
use crate::baseline::{Baseline, Gate, Metric, Verdict, judge, line};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator::new();

type Benchmark = fn(&CountingAllocator) -> Result<Vec<Metric>, String>;

const BENCHMARKS: [(&str, Benchmark); 8] = [
    (page_cache::HIT, page_cache::hit),
    (page_cache::MISS, page_cache::miss),
    (page_cache::COMMIT, page_cache::commit),
    (log::COMMIT_FULL, log::commit_full),
    (log::COMMIT_NORMAL, log::commit_normal),
    (log::COMMIT_OFF, log::commit_off),
    (btree::GET, btree::get),
    (btree::PUT, btree::put),
];

const OS: &str = std::env::consts::OS;

struct Options {
    update: bool,
    baseline: PathBuf,
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("eggdb-bench: {message}");
            ExitCode::from(2)
        }
    }
}

fn parse_options(mut args: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        update: false,
        baseline: PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../benchmarks/baseline.txt"
        )),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--update" => options.update = true,
            "--baseline" => {
                options.baseline = args.next().ok_or("--baseline needs a path")?.into();
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(options)
}

fn run() -> Result<bool, String> {
    let options = parse_options(std::env::args().skip(1))?;
    if options.update && std::env::var_os("CI").is_some() {
        return Err("--update rewrites the committed baseline and is refused in CI".into());
    }
    let mut baseline = match std::fs::read_to_string(&options.baseline) {
        Ok(text) => Baseline::parse(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Baseline::default(),
        Err(error) => return Err(format!("{}: {error}", options.baseline.display())),
    };
    let mut results = Vec::new();
    for (name, benchmark) in BENCHMARKS {
        let mut metrics = benchmark(&ALLOCATOR)?;
        if !options.update && ratio_regressed(&metrics, &baseline) {
            println!("{name}: a ratio is above its limit; measuring once more");
            metrics = benchmark(&ALLOCATOR)?;
        }
        results.extend(metrics);
    }
    if options.update {
        baseline.replace_os(OS, &results);
        std::fs::write(&options.baseline, baseline.render(&results))
            .map_err(|error| format!("{}: {error}", options.baseline.display()))?;
        println!("updated {}", options.baseline.display());
    }
    Ok(report(&results, &baseline))
}

fn ratio_regressed(metrics: &[Metric], baseline: &Baseline) -> bool {
    metrics.iter().any(|metric| {
        metric.gate == Gate::Ratio && matches!(verdict(metric, baseline), Verdict::Regressed { .. })
    })
}

fn verdict(metric: &Metric, baseline: &Baseline) -> Verdict {
    judge(
        metric,
        baseline.get(metric.os_key(OS), metric.benchmark, metric.name),
    )
}

fn report(results: &[Metric], baseline: &Baseline) -> bool {
    let mut passed = true;
    let mut missing = Vec::new();
    let mut table = String::from(
        "| benchmark | metric | value | baseline | verdict |\n|---|---|---|---|---|\n",
    );
    println!("system: {OS}");
    for metric in results {
        let expected = baseline.get(metric.os_key(OS), metric.benchmark, metric.name);
        let verdict = judge(metric, expected);
        passed &= !verdict.fails(metric.gate);
        if verdict == Verdict::Missing {
            missing.push(line(metric, OS));
        }
        let expected = expected.map_or_else(|| "-".to_string(), |value| format!("{value}"));
        println!(
            "{:<18} {:<36} {:>12} {:>10}  {verdict}",
            metric.benchmark,
            metric.name,
            metric.formatted_value(),
            expected
        );
        table.push_str(&format!(
            "| {} | {} | {} | {expected} | {verdict} |\n",
            metric.benchmark,
            metric.name,
            metric.formatted_value()
        ));
    }
    if !missing.is_empty() {
        println!("\nlines missing from the baseline (add them, or run with --update):");
        for line in &missing {
            println!("{line}");
        }
        table.push_str(&format!(
            "\nLines missing from the baseline:\n```\n{}\n```\n",
            missing.join("\n")
        ));
    }
    write_summary(&table);
    passed
}

fn write_summary(table: &str) {
    let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") else {
        return;
    };
    let written = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "### Benchmarks on {OS}\n\n{table}"));
    if let Err(error) = written {
        eprintln!("eggdb-bench: could not write the job summary: {error}");
    }
}
