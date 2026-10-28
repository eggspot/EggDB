use crate::copy::{CopyChannel, CopyStart};
use crate::dialect::Dialect;
use crate::error::SqlResult;
use crate::memory_store::MemoryStore;
use crate::session::{QueryResult, Session};
use crate::tests::render;

const SUITES: [(&str, &str); 5] = [
    ("postgresql", include_str!("../correctness/postgresql.test")),
    ("mysql", include_str!("../correctness/mysql.test")),
    ("sqlite", include_str!("../correctness/sqlite.test")),
    ("oracle", include_str!("../correctness/oracle.test")),
    ("sqlserver", include_str!("../correctness/sqlserver.test")),
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Expected {
    Success,
    Output(String),
    Failure(String),
}

#[derive(Debug, Clone)]
struct Case {
    line: usize,
    chapter: String,
    sql: String,
    expected: Expected,
    differs: bool,
}

fn parse(suite: &str) -> Result<Vec<Case>, String> {
    let lines: Vec<&str> = suite.lines().collect();
    let mut cases = Vec::new();
    let mut chapter = String::new();
    let mut index = 0;
    while index < lines.len() {
        let header = lines[index].trim_end();
        let line = index + 1;
        index += 1;
        if header.is_empty() {
            continue;
        }
        if let Some(name) = header.strip_prefix("chapter ") {
            chapter = name.to_owned();
            continue;
        }
        let mut expected = match header.split_once(' ') {
            None if header == "ok" => Expected::Success,
            None if header == "rows" => Expected::Output(String::new()),
            Some(("error", state)) if state.len() == 5 => Expected::Failure(state.to_owned()),
            Some(("error", state_and_code))
                if state_and_code.len() > 6
                    && state_and_code.as_bytes()[5] == b' '
                    && state_and_code[6..]
                        .bytes()
                        .all(|byte| byte.is_ascii_digit()) =>
            {
                Expected::Failure(state_and_code.to_owned())
            }
            _ => return Err(format!("line {line}: unknown header '{header}'")),
        };
        let mut sql = Vec::new();
        while index < lines.len() && !lines[index].is_empty() && lines[index] != "----" {
            if lines[index] == "... differs" {
                break;
            }
            sql.push(lines[index]);
            index += 1;
        }
        if let Expected::Output(output) = &mut expected {
            if lines.get(index) != Some(&"----") {
                return Err(format!(
                    "line {line}: a rows case needs ---- and its output"
                ));
            }
            index += 1;
            let mut shown = Vec::new();
            while index < lines.len() && !lines[index].is_empty() && lines[index] != "... differs" {
                shown.push(lines[index]);
                index += 1;
            }
            *output = shown.join("\n");
        }
        let differs = lines.get(index) == Some(&"... differs");
        if differs {
            index += 1;
        }
        if sql.is_empty() {
            return Err(format!("line {line}: a case needs SQL"));
        }
        cases.push(Case {
            line,
            chapter: chapter.clone(),
            sql: sql.join("\n"),
            expected,
            differs,
        });
    }
    Ok(cases)
}

fn rendered(result: &QueryResult, dialect: Dialect) -> String {
    if dialect == Dialect::PostgreSql {
        return render(result);
    }
    let mut out = String::new();
    if let Some(columns) = &result.columns {
        let names: Vec<&str> = columns.iter().map(|column| column.name.as_str()).collect();
        out.push_str(&names.join("|"));
        out.push('\n');
        for row in &result.rows {
            let cells: Vec<String> = row
                .iter()
                .map(|value| dialect.value_text(value).unwrap_or_else(|| "NULL".into()))
                .collect();
            out.push_str(&cells.join("|"));
            out.push('\n');
        }
    }
    out.push_str(&result.tag);
    out
}

#[derive(Default)]
struct CopiedOut(Vec<u8>);

impl CopyChannel for CopiedOut {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Ok(())
    }

    fn read_copy_data(&mut self, _data: &mut Vec<u8>) -> SqlResult<bool> {
        Ok(false)
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        Ok(())
    }

    fn write_copy_data(&mut self, data: &[u8]) -> SqlResult<()> {
        self.0.extend_from_slice(data);
        Ok(())
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        Ok(())
    }
}

fn outcome(session: &mut Session<'_, MemoryStore>, sql: &str) -> Expected {
    let mut last = None;
    let dialect = session.dialect();
    let mut copied = CopiedOut::default();
    let ran = session.execute_script(
        sql,
        &mut |result| {
            last = Some(rendered(&result, dialect));
            true
        },
        &mut copied,
    );
    match ran {
        Ok(()) => Expected::Output(format!(
            "{}{}",
            String::from_utf8_lossy(&copied.0),
            last.unwrap_or_default()
        )),
        Err(error) => Expected::Failure(match error.vendor_code() {
            Some(code) => format!("{} {code}", error.state().code()),
            None => error.state().code().to_owned(),
        }),
    }
}

fn agrees(expected: &Expected, found: &Expected) -> bool {
    match (expected, found) {
        (Expected::Success, Expected::Output(_)) => true,
        (Expected::Failure(wanted), Expected::Failure(found)) if wanted.len() == 5 => {
            found.starts_with(wanted.as_str())
        }
        (other, found) => other == found,
    }
}

fn session_for<'s>(dialect: &str, store: &'s MemoryStore) -> Option<Session<'s, MemoryStore>> {
    Dialect::named(dialect)
        .filter(|found| found.name() == dialect)
        .map(|found| Session::with_dialect(store, found))
}

fn run_suite(dialect: &str) -> Result<(usize, usize), Vec<String>> {
    let Some((_, suite)) = SUITES.iter().find(|(name, _)| *name == dialect) else {
        return Err(vec![format!("no suite for the dialect {dialect}")]);
    };
    let cases = parse(suite).map_err(|problem| vec![problem])?;
    crate::memory_store::use_seeded_random_bytes();
    let store = MemoryStore::default();
    let Some(mut session) = session_for(dialect, &store) else {
        return Err(vec![format!("no session for the dialect {dialect}")]);
    };
    let mut problems = Vec::new();
    let mut differences = 0;
    for case in &cases {
        let found = outcome(&mut session, &case.sql);
        let agreed = agrees(&case.expected, &found);
        if case.differs {
            differences += 1;
            if agreed {
                problems.push(format!(
                    "line {} ({}): now matches {dialect}'s documented behaviour; remove '... differs' and its entry in docs/sql.md: {}",
                    case.line, case.chapter, case.sql
                ));
            }
        } else if !agreed {
            problems.push(format!(
                "line {} ({}): {}\n  expected {:?}\n  found    {:?}",
                case.line, case.chapter, case.sql, case.expected, found
            ));
        }
    }
    if problems.is_empty() {
        Ok((cases.len(), differences))
    } else {
        Err(problems)
    }
}

#[test]
fn the_postgresql_dialect_behaves_as_its_manual_documents() {
    match run_suite("postgresql") {
        Ok((cases, differences)) => {
            assert!(cases >= 150, "only {cases} cases");
            assert!(differences * 10 < cases, "{differences} known differences");
        }
        Err(problems) => panic!(
            "{} cases disagree:\n{}",
            problems.len(),
            problems.join("\n")
        ),
    }
}

#[test]
fn the_mysql_dialect_behaves_as_its_reference_manual_documents() {
    match run_suite("mysql") {
        Ok((cases, differences)) => {
            assert!(cases >= 60, "only {cases} cases");
            assert!(differences * 10 < cases, "{differences} known differences");
        }
        Err(problems) => panic!(
            "{} cases disagree:\n{}",
            problems.len(),
            problems.join("\n")
        ),
    }
}

#[test]
fn the_sqlite_dialect_behaves_as_its_documentation_describes() {
    match run_suite("sqlite") {
        Ok((cases, differences)) => {
            assert!(cases >= 80, "only {cases} cases");
            assert!(differences * 10 < cases, "{differences} known differences");
        }
        Err(problems) => panic!(
            "{} cases disagree:\n{}",
            problems.len(),
            problems.join("\n")
        ),
    }
}

#[test]
fn the_oracle_dialect_behaves_as_its_sql_language_reference_documents() {
    match run_suite("oracle") {
        Ok((cases, differences)) => {
            assert!(cases >= 80, "only {cases} cases");
            assert!(differences * 10 < cases, "{differences} known differences");
        }
        Err(problems) => panic!(
            "{} cases disagree:\n{}",
            problems.len(),
            problems.join("\n")
        ),
    }
}

#[test]
fn the_suite_runner_takes_only_dialects_it_knows() {
    assert!(run_suite("db2").is_err());
    assert_eq!(
        parse("rows\nSELECT 1").map(|cases| cases.len()),
        Err("line 1: a rows case needs ---- and its output".to_owned())
    );
    assert!(parse("maybe\nSELECT 1").is_err());
}

#[test]
fn the_sqlserver_dialect_behaves_as_the_transact_sql_reference_documents() {
    match run_suite("sqlserver") {
        Ok((cases, differences)) => {
            assert!(cases >= 60, "only {cases} cases");
            assert!(differences * 10 < cases, "{differences} known differences");
        }
        Err(problems) => panic!(
            "{} cases disagree:\n{}",
            problems.len(),
            problems.join("\n")
        ),
    }
}
