use crate::baseline::{
    BLOCKING_FACTOR, Baseline, Gate, Metric, Verdict, delta_text, judge, line, result_line,
};
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
    results: Option<PathBuf>,
    only: Option<String>,
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
        results: None,
        only: None,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--update" => options.update = true,
            "--baseline" => {
                options.baseline = args.next().ok_or("--baseline needs a path")?.into();
            }
            "--results" => {
                options.results = Some(args.next().ok_or("--results needs a path")?.into());
            }
            "--only" => {
                options.only = Some(args.next().ok_or("--only needs a benchmark family")?);
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
    let chosen = |name: &str| {
        options
            .only
            .as_deref()
            .is_none_or(|family| name.split('.').next() == Some(family))
    };
    for (name, mut benchmark) in benchmarks.into_iter().filter(|(name, _)| chosen(name)) {
        let mut metrics = benchmark()?;
        if !options.update && blocking_ceiling(&metrics, &baseline) {
            println!(
                "{name}: a measurement is more than {BLOCKING_FACTOR}x its line; measuring once more"
            );
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
    let report = report(&results, &baseline);
    write_summary(title, &report.table);
    if let Some(path) = &options.results {
        write_results(path, &report.result_lines)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        println!("wrote {}", path.display());
    }
    Ok(report.passed)
}

fn write_results(path: &std::path::Path, lines: &str) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, lines)
}

fn blocking_ceiling(metrics: &[Metric], baseline: &Baseline) -> bool {
    metrics.iter().any(|metric| {
        matches!(metric.gate, Gate::Ceiling { .. })
            && matches!(verdict(metric, baseline), Verdict::Blocked { .. })
    })
}

fn verdict(metric: &Metric, baseline: &Baseline) -> Verdict {
    judge(
        metric,
        baseline.get(metric.os_key(OS), metric.benchmark, metric.name),
    )
}

struct Report {
    passed: bool,
    result_lines: String,
    table: String,
}

fn report(results: &[Metric], baseline: &Baseline) -> Report {
    let mut passed = true;
    let mut slower = 0;
    let mut blocked = 0;
    let mut missing = Vec::new();
    let mut result_lines = String::new();
    let mut table = String::from(
        "| benchmark | metric | value | baseline | change | verdict |\n|---|---|---|---|---|---|\n",
    );
    println!("system: {OS}");
    for metric in results {
        let expected = baseline.get(metric.os_key(OS), metric.benchmark, metric.name);
        let verdict = judge(metric, expected);
        passed &= !verdict.fails(metric.gate);
        match verdict {
            Verdict::Slower { .. } => slower += 1,
            Verdict::Blocked { .. } => blocked += 1,
            Verdict::Missing => missing.push(line(metric, OS)),
            Verdict::Within | Verdict::Improved | Verdict::Reported => {}
        }
        result_lines.push_str(&result_line(metric, OS, expected, verdict));
        result_lines.push('\n');
        let change = delta_text(metric.value, expected);
        let expected = expected.map_or_else(|| "-".to_string(), |value| format!("{value}"));
        println!(
            "{:<26} {:<38} {:>12} {:>10} {:>9}  {verdict}",
            metric.benchmark,
            metric.name,
            metric.formatted_value(),
            expected,
            change
        );
        table.push_str(&format!(
            "| {} | {} | {} | {expected} | {change} | {verdict} |\n",
            metric.benchmark,
            metric.name,
            metric.formatted_value()
        ));
    }
    let totals = format!(
        "{slower} slower than the baseline (reported, not blocking), {blocked} more than {BLOCKING_FACTOR}x slower (blocking)"
    );
    println!("\n{totals}");
    table.push_str(&format!("\n{totals}\n"));
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
    Report {
        passed,
        result_lines,
        table,
    }
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

#[cfg(test)]
mod tests {
    use super::{OS, Report, parse_options, report, write_results};
    use crate::baseline::{Baseline, Metric};

    fn baseline() -> Baseline {
        Baseline::parse(&format!(
            "{OS} cache.hit ratio_to_reference 1.0\nany cache.hit allocations_per_op 0\n"
        ))
        .unwrap()
    }

    fn measured(ratio: f64) -> Report {
        report(
            &[
                Metric::ratio("cache.hit", "ratio_to_reference", ratio),
                Metric::exact("cache.hit", "allocations_per_op", 0.0),
                Metric::reported("cache.hit", "ns_per_op", 5.0),
            ],
            &baseline(),
        )
    }

    #[test]
    fn a_slowdown_within_twice_the_line_is_reported_and_passes() {
        let report = measured(1.9);
        assert!(report.passed);
        assert!(
            report
                .table
                .contains("| cache.hit | ratio_to_reference | 1.900 | 1 | +90.0% | SLOWER")
        );
        assert!(report.table.contains(
            "1 slower than the baseline (reported, not blocking), 0 more than 2x slower (blocking)"
        ));
        assert_eq!(
            report.result_lines,
            format!(
                "{OS} cache.hit ratio_to_reference 1.900 1 +90.0% slower\n{OS} cache.hit allocations_per_op 0 0 0% ok\n{OS} cache.hit ns_per_op 5.000 - - reported\n"
            )
        );
    }

    #[test]
    fn a_slowdown_of_more_than_twice_the_line_fails() {
        let report = measured(2.1);
        assert!(!report.passed);
        assert!(report.table.contains("BLOCKED"));
        assert!(report.table.contains("1 more than 2x slower (blocking)"));
    }

    #[test]
    fn the_results_path_is_an_option() {
        let args = ["--results", "out/results.txt"].map(String::from);
        let options = parse_options(args.into_iter()).unwrap();
        assert_eq!(options.results.unwrap().to_str(), Some("out/results.txt"));
        assert!(parse_options(["--results".to_string()].into_iter()).is_err());
        assert!(parse_options(std::iter::empty()).unwrap().results.is_none());
    }

    #[test]
    fn the_results_file_is_written_into_a_folder_that_does_not_exist_yet() {
        let folder =
            std::env::temp_dir().join(format!("eggdb-bench-results-{}", std::process::id()));
        let path = folder.join("nested").join("results.txt");
        write_results(&path, "linux a b 1 1 0% ok\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "linux a b 1 1 0% ok\n"
        );
        std::fs::remove_dir_all(&folder).unwrap();
    }
}
