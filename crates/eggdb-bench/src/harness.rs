use crate::baseline::{Baseline, Gate, Metric, Verdict, judge, line};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

pub(crate) const OS: &str = std::env::consts::OS;

pub(crate) type Benchmark<'a> = (
    &'static str,
    Box<dyn FnMut() -> Result<Vec<Metric>, String> + 'a>,
);

struct Options {
    update: bool,
    baseline: PathBuf,
}

pub(crate) fn exit_code(program: &str, outcome: Result<bool, String>) -> ExitCode {
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("{program}: {message}");
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

pub(crate) fn run(
    title: &str,
    args: impl Iterator<Item = String>,
    benchmarks: Vec<Benchmark<'_>>,
) -> Result<bool, String> {
    let options = parse_options(args)?;
    if options.update && std::env::var_os("CI").is_some() {
        return Err("--update rewrites the committed baseline and is refused in CI".into());
    }
    let mut baseline = match std::fs::read_to_string(&options.baseline) {
        Ok(text) => Baseline::parse(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Baseline::default(),
        Err(error) => return Err(format!("{}: {error}", options.baseline.display())),
    };
    let mut results = Vec::new();
    for (name, mut benchmark) in benchmarks {
        let mut metrics = benchmark()?;
        if !options.update && ceiling_exceeded(&metrics, &baseline) {
            println!("{name}: a measurement is above its limit; measuring once more");
            metrics = benchmark()?;
        }
        results.extend(metrics);
    }
    if options.update {
        baseline.replace_os(OS, &results);
        std::fs::write(&options.baseline, baseline.render())
            .map_err(|error| format!("{}: {error}", options.baseline.display()))?;
        println!("updated {}", options.baseline.display());
    }
    Ok(report(title, &results, &baseline))
}

fn ceiling_exceeded(metrics: &[Metric], baseline: &Baseline) -> bool {
    metrics.iter().any(|metric| {
        matches!(metric.gate, Gate::Ceiling { .. })
            && matches!(verdict(metric, baseline), Verdict::Regressed { .. })
    })
}

fn verdict(metric: &Metric, baseline: &Baseline) -> Verdict {
    judge(
        metric,
        baseline.get(metric.os_key(OS), metric.benchmark, metric.name),
    )
}

fn report(title: &str, results: &[Metric], baseline: &Baseline) -> bool {
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
            "{:<26} {:<38} {:>12} {:>10}  {verdict}",
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
    write_summary(title, &table);
    passed
}

fn write_summary(title: &str, table: &str) {
    let Some(path) = std::env::var_os("GITHUB_STEP_SUMMARY") else {
        return;
    };
    let written = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .and_then(|mut file| writeln!(file, "### {title} on {OS}\n\n{table}"));
    if let Err(error) = written {
        eprintln!("could not write the job summary: {error}");
    }
}
