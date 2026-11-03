use std::ops::Range;

use crate::dialect::QueryDialect;
use crate::highlight::{Carry, Lexeme, lex};

const BLOCK_STARTS: [&str; 2] = ["BEGIN", "DECLARE"];
const BLOCK_OBJECTS: [&str; 4] = ["PROCEDURE", "FUNCTION", "TRIGGER", "PACKAGE"];
const JOINS: [&str; 7] = ["JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "NATURAL"];
const SET_OPERATORS: [&str; 4] = ["UNION", "INTERSECT", "EXCEPT", "MINUS"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Segment {
    range: Range<usize>,
    has_code: bool,
}

fn line_around(text: &str, at: usize) -> &str {
    let start = text[..at].rfind('\n').map_or(0, |index| index + 1);
    let end = text[at..].find('\n').map_or(text.len(), |index| at + index);
    text[start..end].trim()
}

fn line_end_after(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |index| at + index)
}

fn batch_line(dialect: QueryDialect, text: &str, range: &Range<usize>, lexeme: Lexeme) -> bool {
    let word = &text[range.clone()];
    match dialect {
        QueryDialect::Oracle => {
            lexeme == Lexeme::Other && word == "/" && line_around(text, range.start) == "/"
        }
        QueryDialect::SqlServer => {
            matches!(lexeme, Lexeme::Keyword | Lexeme::Word)
                && word.eq_ignore_ascii_case("GO")
                && line_around(text, range.start).eq_ignore_ascii_case("GO")
        }
        _ => false,
    }
}

fn opens_block(dialect: QueryDialect, words: &[String]) -> bool {
    if !matches!(dialect, QueryDialect::Oracle | QueryDialect::SqlServer) {
        return false;
    }
    match words.first().map(String::as_str) {
        Some(first) if BLOCK_STARTS.contains(&first) => true,
        Some("CREATE" | "ALTER") => words
            .iter()
            .skip(1)
            .any(|word| BLOCK_OBJECTS.contains(&word.as_str())),
        _ => false,
    }
}

fn segments(text: &str, dialect: QueryDialect) -> Vec<Segment> {
    let mut found = Vec::new();
    let mut start = 0;
    let mut has_code = false;
    let mut words: Vec<String> = Vec::new();
    let mut skip_to = 0;
    let mut carry = Carry::Code;
    lex(text, dialect, &mut carry, &mut |range, lexeme| {
        if range.start < skip_to {
            return;
        }
        let ends_batch = batch_line(dialect, text, &range, lexeme);
        let separates =
            ends_batch || (lexeme == Lexeme::Semicolon && !opens_block(dialect, &words));
        if separates {
            found.push(Segment {
                range: start..range.start,
                has_code,
            });
            start = if ends_batch {
                line_end_after(text, range.end)
            } else {
                range.end
            };
            skip_to = start;
            has_code = false;
            words.clear();
            return;
        }
        match lexeme {
            Lexeme::Comment => {}
            Lexeme::Other if text[range.clone()].trim().is_empty() => {}
            Lexeme::Keyword | Lexeme::Word => {
                has_code = true;
                if words.len() < 4 {
                    words.push(text[range].to_ascii_uppercase());
                }
            }
            _ => has_code = true,
        }
    });
    found.push(Segment {
        range: start.min(text.len())..text.len(),
        has_code,
    });
    found
}

fn trimmed(text: &str, range: Range<usize>) -> Range<usize> {
    let slice = &text[range.clone()];
    let leading = slice.len() - slice.trim_start().len();
    let trailing = slice.len() - slice.trim_end().len();
    let start = range.start + leading;
    start..(range.end - trailing).max(start)
}

/// The statements of `text` in `dialect`, as byte ranges without their `;` and the space
/// around them, skipping empty ones and ones that hold only comments. A `;` inside a string,
/// a quoted name, a comment or a PostgreSQL `$$` body does not split; in Oracle and SQL Server
/// a `BEGIN` or `DECLARE` block, or a `CREATE PROCEDURE` (`FUNCTION`, `TRIGGER`, `PACKAGE`),
/// runs to a line holding only `/` (Oracle) or `GO` (SQL Server), which also end a statement.
pub fn statements(text: &str, dialect: QueryDialect) -> Vec<Range<usize>> {
    segments(text, dialect)
        .into_iter()
        .filter(|segment| segment.has_code)
        .map(|segment| trimmed(text, segment.range))
        .collect()
}

/// The statement around byte `offset` of `text`: from the previous `;` (or the start) to the
/// next `;` (or the end), with [`statements`]'s rules. A caret after the last `;` of a line,
/// with only space or comments after it, means the statement before it.
pub fn statement_at(text: &str, offset: usize, dialect: QueryDialect) -> Option<Range<usize>> {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let all = segments(text, dialect);
    let mut index = all
        .iter()
        .position(|segment| offset <= segment.range.end)
        .unwrap_or(all.len().saturating_sub(1));
    let segment = all.get(index)?;
    let before = &text[segment.range.start.min(offset)..offset];
    let rest_of_line = &text[offset..line_end_after(text, offset)];
    if index > 0
        && before.trim().is_empty()
        && !before.contains('\n')
        && rest_of_line.trim().is_empty()
    {
        index -= 1;
    }
    all[..=index]
        .iter()
        .rev()
        .find(|segment| segment.has_code)
        .or_else(|| all[index..].iter().find(|segment| segment.has_code))
        .map(|segment| trimmed(text, segment.range.clone()))
}

/// The table a `SELECT` reads when it reads exactly one, as written in it (`customers`,
/// `shop.orders`, `"Order Lines"`): what `Copy as INSERT` names. `None` for a join, a list
/// of tables, a set operation or no `FROM`.
pub fn source_table(sql: &str, dialect: QueryDialect) -> Option<String> {
    let mut runs: Vec<(Range<usize>, Lexeme)> = Vec::new();
    let mut carry = Carry::Code;
    lex(sql, dialect, &mut carry, &mut |range, lexeme| {
        let blank = lexeme == Lexeme::Comment
            || (lexeme == Lexeme::Other && sql[range.clone()].trim().is_empty());
        if !blank {
            runs.push((range, lexeme));
        }
    });
    let mut depth = 0_i32;
    let mut from = None;
    for (index, (range, lexeme)) in runs.iter().enumerate() {
        let text = &sql[range.clone()];
        match (lexeme, text) {
            (Lexeme::Other, "(") => depth += 1,
            (Lexeme::Other, ")") => depth -= 1,
            (Lexeme::Keyword | Lexeme::Word, word) if depth == 0 => {
                let upper = word.to_ascii_uppercase();
                if SET_OPERATORS.contains(&upper.as_str()) {
                    return None;
                }
                if upper == "FROM" && from.is_none() {
                    from = Some(index + 1);
                }
            }
            _ => {}
        }
    }
    let mut at = from?;
    let mut name = String::new();
    loop {
        let (range, lexeme) = runs.get(at)?;
        if !matches!(lexeme, Lexeme::Word | Lexeme::QuotedName) {
            return None;
        }
        name.push_str(&sql[range.clone()]);
        at += 1;
        match runs.get(at) {
            Some((range, Lexeme::Other)) if &sql[range.clone()] == "." => {
                name.push('.');
                at += 1;
            }
            _ => break,
        }
    }
    let mut rest = runs[at..]
        .iter()
        .map(|(range, lexeme)| (&sql[range.clone()], *lexeme));
    let mut next = rest.next();
    match next {
        Some((word, Lexeme::Keyword)) if word.eq_ignore_ascii_case("AS") => {
            rest.next();
            next = rest.next();
        }
        Some((_, Lexeme::Word | Lexeme::QuotedName)) => next = rest.next(),
        _ => {}
    }
    match next {
        Some((",", _)) => None,
        Some((word, Lexeme::Keyword)) if JOINS.contains(&word.to_ascii_uppercase().as_str()) => {
            None
        }
        Some((word, Lexeme::Word)) if word.eq_ignore_ascii_case("NATURAL") => None,
        _ => Some(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(text: &str, dialect: QueryDialect) -> Vec<&str> {
        statements(text, dialect)
            .into_iter()
            .map(|range| &text[range])
            .collect()
    }

    fn at(text: &str, caret: usize, dialect: QueryDialect) -> Option<&str> {
        statement_at(text, caret, dialect).map(|range| &text[range])
    }

    #[test]
    fn statements_split_at_semicolons_outside_strings_comments_and_bodies() {
        let pg = QueryDialect::PostgreSql;
        assert_eq!(texts("SELECT 1;\nSELECT 2;", pg), ["SELECT 1", "SELECT 2"]);
        assert_eq!(
            texts("SELECT 'a;b'; SELECT 2", pg),
            ["SELECT 'a;b'", "SELECT 2"]
        );
        assert_eq!(
            texts("SELECT 1 -- x;y\n; SELECT 2", pg),
            ["SELECT 1 -- x;y", "SELECT 2"]
        );
        assert_eq!(texts("SELECT /* ; */ 1;", pg), ["SELECT /* ; */ 1"]);
        assert_eq!(
            texts(
                "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql; SELECT f()",
                pg
            ),
            [
                "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql",
                "SELECT f()"
            ]
        );
        assert_eq!(
            texts("SELECT \"a;b\" FROM t;", pg),
            ["SELECT \"a;b\" FROM t"]
        );
        assert_eq!(texts(" ;; -- only a comment\n;", pg), Vec::<&str>::new());
        assert_eq!(
            texts("SELECT `a;b`; SELECT 'x\\';y'", QueryDialect::MySql),
            ["SELECT `a;b`", "SELECT 'x\\';y'"]
        );
        assert_eq!(
            texts("SELECT 'x\\'; SELECT 2", pg),
            ["SELECT 'x\\'", "SELECT 2"]
        );
        assert_eq!(
            texts("SELECT [a;b] FROM t; SELECT 2", QueryDialect::SqlServer),
            ["SELECT [a;b] FROM t", "SELECT 2"]
        );
        assert_eq!(
            texts("SELECT 1 # a;b\n;", QueryDialect::MySql),
            ["SELECT 1 # a;b"]
        );
    }

    #[test]
    fn oracle_and_sql_server_blocks_run_to_their_batch_line() {
        let oracle = "BEGIN\n  INSERT INTO t VALUES (1);\n  COMMIT;\nEND;\n/\nSELECT 1 FROM dual;";
        assert_eq!(
            texts(oracle, QueryDialect::Oracle),
            [
                "BEGIN\n  INSERT INTO t VALUES (1);\n  COMMIT;\nEND;",
                "SELECT 1 FROM dual"
            ]
        );
        let tsql = "CREATE PROCEDURE p AS BEGIN SELECT 1; SELECT 2; END\nGO\nEXEC p";
        assert_eq!(
            texts(tsql, QueryDialect::SqlServer),
            [
                "CREATE PROCEDURE p AS BEGIN SELECT 1; SELECT 2; END",
                "EXEC p"
            ]
        );
        assert_eq!(
            texts("SELECT 4 / 2; SELECT 1", QueryDialect::Oracle),
            ["SELECT 4 / 2", "SELECT 1"]
        );
    }

    #[test]
    fn the_statement_under_the_caret_is_the_one_between_its_semicolons() {
        let pg = QueryDialect::PostgreSql;
        let text = "SELECT 1;\nSELECT 2;";
        assert_eq!(at(text, 0, pg), Some("SELECT 1"));
        assert_eq!(at(text, 8, pg), Some("SELECT 1"));
        assert_eq!(at(text, 9, pg), Some("SELECT 1"));
        assert_eq!(at(text, 12, pg), Some("SELECT 2"));
        assert_eq!(at(text, text.len(), pg), Some("SELECT 2"));
        assert_eq!(at("SELECT 1;\n\n", 11, pg), Some("SELECT 1"));
        assert_eq!(at("", 0, pg), None);
        assert_eq!(at("  ;  ", 1, pg), None);
        assert_eq!(at("SELECT 'a;b'; SELECT 2", 9, pg), Some("SELECT 'a;b'"));
        assert_eq!(at(";\nSELECT 3", 0, pg), Some("SELECT 3"));
    }

    #[test]
    fn the_source_table_is_named_only_when_one_table_is_read() {
        let pg = QueryDialect::PostgreSql;
        assert_eq!(
            source_table("SELECT id, name FROM customers ORDER BY id", pg).as_deref(),
            Some("customers")
        );
        assert_eq!(
            source_table("SELECT * FROM shop.orders o WHERE o.id = 1", pg).as_deref(),
            Some("shop.orders")
        );
        assert_eq!(
            source_table("SELECT * FROM \"Order Lines\" AS l", pg).as_deref(),
            Some("\"Order Lines\"")
        );
        assert_eq!(
            source_table("SELECT * FROM a JOIN b ON a.id = b.id", pg),
            None
        );
        assert_eq!(source_table("SELECT * FROM a, b", pg), None);
        assert_eq!(
            source_table("SELECT * FROM a UNION SELECT * FROM b", pg),
            None
        );
        assert_eq!(source_table("SELECT 1", pg), None);
        assert_eq!(source_table("SELECT * FROM (SELECT 1) AS s", pg), None);
        assert_eq!(
            source_table("SELECT (SELECT max(id) FROM b) FROM a", pg).as_deref(),
            Some("a")
        );
        assert_eq!(
            source_table("SELECT * FROM [order lines]", QueryDialect::SqlServer).as_deref(),
            Some("[order lines]")
        );
    }
}
