//! `eggdb-fuzz TARGET [--seconds N] [--seed S] [--out DIR]`: mutates valid inputs of one of
//! EggDB's decoders of untrusted bytes for as long as asked, and checks that each mutated input
//! is decoded or refused without a panic, and that each valid input round-trips. An input that
//! panics or fails its round trip is written to `DIR` and the run exits with status 1.

mod mutate;
mod targets;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

const DEFAULT_SECONDS: u64 = 60;
const USAGE: &str = "usage: eggdb-fuzz zstd|lz4|sqlite|arrow [--seconds N] [--seed S] [--out DIR]";

struct Options {
    target: targets::Target,
    seconds: u64,
    seed: u64,
    out: PathBuf,
}

fn options(arguments: &[String]) -> Result<Options, String> {
    let (first, rest) = arguments.split_first().ok_or(USAGE)?;
    let target = targets::Target::named(first).ok_or_else(|| format!("unknown target {first}; {USAGE}"))?;
    let mut options = Options {
        target,
        seconds: DEFAULT_SECONDS,
        seed: 0x5eed_e66d_b000_0001,
        out: PathBuf::from("fuzz-findings"),
    };
    let mut items = rest.iter();
    while let Some(flag) = items.next() {
        let value = items.next().ok_or_else(|| format!("{flag} needs a value; {USAGE}"))?;
        match flag.as_str() {
            "--seconds" => options.seconds = value.parse().map_err(|_| format!("bad --seconds {value}"))?,
            "--seed" => options.seed = value.parse().map_err(|_| format!("bad --seed {value}"))?,
            "--out" => options.out = PathBuf::from(value),
            other => return Err(format!("unknown option {other}; {USAGE}")),
        }
    }
    Ok(options)
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let options = match options(&arguments) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    std::panic::set_hook(Box::new(|_| {}));
    let deadline = Instant::now() + Duration::from_secs(options.seconds);
    let mut random = mutate::Random::new(options.seed);
    let corpus = options.target.corpus(&mut random);
    let mut runs: u64 = 0;
    while Instant::now() < deadline {
        let seed = &corpus[random.below(corpus.len())];
        let input = mutate::mutated(&mut random, seed, &corpus);
        if let Err(problem) = options.target.check(&input) {
            let _ = std::fs::create_dir_all(&options.out);
            let path = options.out.join(format!("{}-{runs}.bin", options.target.name()));
            let _ = std::fs::write(&path, &input);
            eprintln!("{} after {runs} runs: {problem}; input in {}", options.target.name(), path.display());
            return ExitCode::from(1);
        }
        runs += 1;
    }
    println!("{}: {runs} runs, no finding", options.target.name());
    ExitCode::SUCCESS
}
