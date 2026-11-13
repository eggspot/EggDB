//! `eggdb-sparql-suite DIR... [--minimum PERCENT]`: runs the syntax and query evaluation tests of
//! every W3C SPARQL test manifest (`manifest.ttl`) under the given directories against a fresh
//! in-memory EggDB RDF store per test, prints each manifest's and the total pass rate, and exits
//! with status 1 when the total is below the minimum. Update evaluation tests and tests whose
//! files EggDB cannot read (RDF/XML, CSV and TSV results) are counted as skipped.

mod json;
mod results;
mod xml;

use eggdb_keyspace::MemoryKeyspace;
use eggdb_rdf::{Format, Quad, RDF_TYPE, RdfErrorKind, Term};
use results::{Answer, read_expected, same_answer};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage: eggdb-sparql-suite DIR... [--minimum PERCENT]";
const MANIFEST: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-manifest#";
const QUERY: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-query#";
const APPROVAL: &str = "http://www.w3.org/2001/sw/DataAccess/tests/test-dawg#";
const MAX_DIRECTORY_DEPTH: usize = 6;

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    passed: u64,
    failed: u64,
    skipped: u64,
}

impl Tally {
    fn add(&mut self, other: Tally) {
        self.passed += other.passed;
        self.failed += other.failed;
        self.skipped += other.skipped;
    }

    fn percent(self) -> f64 {
        let total = self.passed + self.failed;
        if total == 0 {
            return 100.0;
        }
        self.passed as f64 * 100.0 / total as f64
    }
}

enum Outcome {
    Passed,
    Failed(String),
    Skipped(String),
}

fn manifests(directory: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > MAX_DIRECTORY_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(Result::ok).map(|entry| entry.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            manifests(&path, depth + 1, found);
        } else if path.file_name().is_some_and(|name| name == "manifest.ttl") {
            found.push(path);
        }
    }
}

fn file_iri(path: &Path) -> String {
    let absolute = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = absolute.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') { format!("file://{text}") } else { format!("file:///{text}") }
}

fn path_of(term: &Term) -> Option<PathBuf> {
    let Term::Iri(iri) = term else {
        return None;
    };
    let path = iri.strip_prefix("file://")?;
    let path = if path.len() > 2 && path.as_bytes()[2] == b':' { &path[1..] } else { path };
    Some(PathBuf::from(path))
}

struct Manifest {
    quads: Vec<Quad>,
}

impl Manifest {
    fn objects<'m>(&'m self, subject: &'m Term, predicate: &'m str) -> impl Iterator<Item = &'m Term> {
        self.quads
            .iter()
            .filter(move |quad| quad.subject == *subject && matches!(&quad.predicate, Term::Iri(iri) if iri == predicate))
            .map(|quad| &quad.object)
    }

    fn object(&self, subject: &Term, predicate: &str) -> Option<&Term> {
        self.objects(subject, predicate).next()
    }

    fn tests(&self) -> Vec<(Term, String)> {
        let mut found = Vec::new();
        for quad in &self.quads {
            let (Term::Iri(predicate), Term::Iri(kind)) = (&quad.predicate, &quad.object) else {
                continue;
            };
            if predicate == RDF_TYPE
                && let Some(kind) = kind.strip_prefix(MANIFEST)
                && kind.contains("Test")
                && !found.iter().any(|(subject, _)| *subject == quad.subject)
            {
                found.push((quad.subject.clone(), kind.to_owned()));
            }
        }
        found
    }
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))
}

fn data_format(path: &Path) -> Option<Format> {
    Format::named(path.extension()?.to_str()?)
}

fn load(keyspace: &MemoryKeyspace, file: &Term, graph: Option<&Term>) -> Result<(), Outcome> {
    let path = path_of(file).ok_or_else(|| Outcome::Skipped("a data file outside the suite".into()))?;
    let format = data_format(&path).ok_or_else(|| Outcome::Skipped(format!("{} is in a format EggDB does not read", path.display())))?;
    let text = read(&path).map_err(Outcome::Failed)?;
    let base = file_iri(&path);
    eggdb_rdf::load(keyspace, 0, &text, format, &base, graph).map_err(|error| Outcome::Failed(format!("loading {}: {}", path.display(), error.message)))?;
    Ok(())
}

fn syntax_test(manifest: &Manifest, test: &Term, kind: &str) -> Outcome {
    let Some(action) = manifest.object(test, &format!("{MANIFEST}action")) else {
        return Outcome::Failed("no mf:action".into());
    };
    let Some(path) = path_of(action) else {
        return Outcome::Failed("mf:action is not a file".into());
    };
    let text = match read(&path) {
        Ok(text) => text,
        Err(message) => return Outcome::Failed(message),
    };
    let base = file_iri(&path);
    let keyspace = MemoryKeyspace::default();
    let update = kind.contains("Update");
    let outcome = if update {
        eggdb_rdf::update(&keyspace, 0, &text, &base).map(drop)
    } else {
        eggdb_rdf::query(&keyspace, 0, &text, &base).map(drop)
    };
    let positive = kind.starts_with("Positive");
    match (positive, outcome) {
        (true, Ok(())) | (false, Err(_)) => Outcome::Passed,
        (true, Err(error)) if error.kind != RdfErrorKind::Syntax && error.kind != RdfErrorKind::NotSupported => Outcome::Passed,
        (true, Err(error)) => Outcome::Failed(format!("rejected: {}", error.message)),
        (false, Ok(())) => Outcome::Failed("accepted a query the suite says is invalid".into()),
    }
}

fn evaluation_test(manifest: &Manifest, test: &Term) -> Outcome {
    let Some(action) = manifest.object(test, &format!("{MANIFEST}action")) else {
        return Outcome::Failed("no mf:action".into());
    };
    let Some(query_path) = manifest.object(action, &format!("{QUERY}query")).and_then(path_of) else {
        return Outcome::Failed("no qt:query".into());
    };
    let keyspace = MemoryKeyspace::default();
    for data in manifest.objects(action, &format!("{QUERY}data")) {
        if let Err(outcome) = load(&keyspace, data, None) {
            return outcome;
        }
    }
    for graph in manifest.objects(action, &format!("{QUERY}graphData")) {
        if let Err(outcome) = load(&keyspace, graph, Some(graph)) {
            return outcome;
        }
    }
    let Some(result) = manifest.object(test, &format!("{MANIFEST}result")).and_then(path_of) else {
        return Outcome::Failed("no mf:result".into());
    };
    let expected = match read_expected(&result, &file_iri(&result)) {
        Ok(expected) => expected,
        Err(message) if message.contains("not read yet") => return Outcome::Skipped(message),
        Err(message) => return Outcome::Failed(message),
    };
    let text = match read(&query_path) {
        Ok(text) => text,
        Err(message) => return Outcome::Failed(message),
    };
    let actual = match eggdb_rdf::query(&keyspace, 0, &text, &file_iri(&query_path)) {
        Ok(outcome) => Answer::of(outcome),
        Err(error) => return Outcome::Failed(error.message),
    };
    let ordered = text.to_ascii_uppercase().contains("ORDER BY");
    if same_answer(&expected, &actual, ordered) {
        Outcome::Passed
    } else {
        Outcome::Failed("a different answer".into())
    }
}

fn run_test(manifest: &Manifest, test: &Term, kind: &str) -> Outcome {
    let withdrawn = manifest
        .object(test, &format!("{APPROVAL}approval"))
        .is_some_and(|approval| matches!(approval, Term::Iri(iri) if iri.ends_with("Withdrawn")));
    if withdrawn {
        return Outcome::Skipped("withdrawn".into());
    }
    match kind {
        "PositiveSyntaxTest" | "PositiveSyntaxTest11" | "NegativeSyntaxTest" | "NegativeSyntaxTest11" | "PositiveUpdateSyntaxTest11"
        | "NegativeUpdateSyntaxTest11" => syntax_test(manifest, test, kind),
        "QueryEvaluationTest" => evaluation_test(manifest, test),
        other => Outcome::Skipped(format!("{other} is not run")),
    }
}

fn run_manifest(path: &Path) -> Tally {
    let mut tally = Tally::default();
    let quads = match read(path).and_then(|text| eggdb_rdf::parse(&text, Format::Turtle, &file_iri(path)).map_err(|error| error.message)) {
        Ok(quads) => quads,
        Err(message) => {
            eprintln!("FAIL {}: the manifest: {message}", path.display());
            tally.failed += 1;
            return tally;
        }
    };
    let manifest = Manifest { quads };
    for (test, kind) in manifest.tests() {
        let name = match manifest.object(&test, &format!("{MANIFEST}name")) {
            Some(Term::Literal(name)) => name.lexical.clone(),
            _ => test.to_ntriples(),
        };
        match run_test(&manifest, &test, &kind) {
            Outcome::Passed => tally.passed += 1,
            Outcome::Failed(reason) => {
                tally.failed += 1;
                eprintln!("FAIL {} {name}: {reason}", path.display());
            }
            Outcome::Skipped(_) => tally.skipped += 1,
        }
    }
    tally
}

fn main() -> ExitCode {
    let mut directories = Vec::new();
    let mut minimum = 0.0f64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--minimum" {
            match args.next().and_then(|value| value.parse::<f64>().ok()) {
                Some(value) => minimum = value,
                None => {
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                }
            }
        } else {
            directories.push(PathBuf::from(arg));
        }
    }
    if directories.is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let mut found = Vec::new();
    for directory in &directories {
        manifests(directory, 0, &mut found);
    }
    let mut total = Tally::default();
    for manifest in &found {
        let tally = run_manifest(manifest);
        println!(
            "{}: {}/{} passed, {} skipped ({:.1}%)",
            manifest.display(),
            tally.passed,
            tally.passed + tally.failed,
            tally.skipped,
            tally.percent()
        );
        total.add(tally);
    }
    println!(
        "total: {}/{} passed, {} skipped, {} manifests ({:.2}%)",
        total.passed,
        total.passed + total.failed,
        total.skipped,
        found.len(),
        total.percent()
    );
    if total.percent() < minimum { ExitCode::from(1) } else { ExitCode::SUCCESS }
}
