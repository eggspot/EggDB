/// How a query's expected values are ordered before they are compared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortMode {
    Unsorted,
    Rows,
    Values,
}

/// One record of a sqllogictest file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Record {
    Statement {
        line: usize,
        sql: String,
        succeeds: bool,
    },
    Query {
        line: usize,
        sql: String,
        types: Vec<char>,
        sort: SortMode,
        expected: Expected,
    },
    HashThreshold(usize),
    Halt,
}

/// A query's expected result: its values one per line, or their count and MD5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Expected {
    Values(Vec<String>),
    Hash { count: usize, md5: String },
}

/// Whether the record after a `skipif`/`onlyif` line runs on an engine named `engine`.
fn applies(condition: &str, engine: &str) -> bool {
    let mut words = condition.split_whitespace();
    match (words.next(), words.next()) {
        (Some("skipif"), Some(name)) => name != engine,
        (Some("onlyif"), Some(name)) => name == engine,
        _ => true,
    }
}

fn expected(lines: &[&str]) -> Expected {
    if let [only] = lines {
        let words: Vec<&str> = only.split_whitespace().collect();
        if let [count, "values", "hashing", "to", md5] = words.as_slice()
            && let Ok(count) = count.parse()
        {
            return Expected::Hash {
                count,
                md5: (*md5).to_owned(),
            };
        }
    }
    Expected::Values(
        lines
            .iter()
            .flat_map(|line| line.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
            .collect(),
    )
}

/// The records of a sqllogictest file that apply to `engine`; a record type the parser does not
/// know is skipped.
pub(crate) fn parse(text: &str, engine: &str) -> Vec<Record> {
    let lines: Vec<&str> = text.lines().collect();
    let mut records = Vec::new();
    let mut at = 0;
    while at < lines.len() {
        let line = lines[at].trim_end();
        if line.trim().is_empty() || line.starts_with('#') {
            at += 1;
            continue;
        }
        let start = at;
        let mut runs = true;
        while lines
            .get(at)
            .is_some_and(|line| line.starts_with("skipif") || line.starts_with("onlyif"))
        {
            runs &= applies(lines[at], engine);
            at += 1;
        }
        let Some(head) = lines.get(at).map(|line| line.trim_end()) else {
            break;
        };
        at += 1;
        let mut body = Vec::new();
        while lines.get(at).is_some_and(|line| !line.trim().is_empty()) {
            body.push(lines[at]);
            at += 1;
        }
        if !runs {
            continue;
        }
        let words: Vec<&str> = head.split_whitespace().collect();
        let record = match words.as_slice() {
            ["statement", outcome, ..] => Some(Record::Statement {
                line: start + 1,
                sql: body.join("\n"),
                succeeds: *outcome == "ok",
            }),
            ["query", types, rest @ ..] => {
                let split = body.iter().position(|line| line.trim() == "----");
                let (sql, results) = match split {
                    Some(split) => (&body[..split], &body[split + 1..]),
                    None => (body.as_slice(), &[][..]),
                };
                let sort = match rest.first() {
                    Some(&"rowsort") => SortMode::Rows,
                    Some(&"valuesort") => SortMode::Values,
                    _ => SortMode::Unsorted,
                };
                Some(Record::Query {
                    line: start + 1,
                    sql: sql.join("\n"),
                    types: types.chars().collect(),
                    sort,
                    expected: expected(results),
                })
            }
            ["hash-threshold", count] => count.parse().ok().map(Record::HashThreshold),
            ["halt"] => Some(Record::Halt),
            _ => None,
        };
        records.extend(record);
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_parse_with_conditions_sorts_and_hashes() {
        let text = "hash-threshold 8\n\nstatement ok\nCREATE TABLE t1(a INTEGER, b INTEGER)\n\nskipif sqlite\nstatement ok\nSELECT nothing\n\nquery II rowsort\nSELECT a, b FROM t1\n----\n1\n2\n3\n4\n\nquery I nosort\nSELECT a FROM t1\n----\n30 values hashing to 3c13dee48d9356ae19af2515e05e6b54\n";
        let records = parse(text, "sqlite");
        assert_eq!(records.len(), 4);
        assert_eq!(records[0], Record::HashThreshold(8));
        assert!(matches!(&records[2], Record::Query { sort: SortMode::Rows, types, .. } if types == &['I', 'I']));
        assert!(matches!(&records[3], Record::Query { expected: Expected::Hash { count: 30, .. }, .. }));
    }
}
