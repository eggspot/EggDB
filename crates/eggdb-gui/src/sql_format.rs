use std::ops::Range;

use crate::dialect::QueryDialect;
use crate::highlight::{Carry, Lexeme, lex};

/// The marker `Toggle comment` puts before a line (`docs/ux/workspace.md` 10.9): `-- ` in
/// every dialect, since MySQL needs the space.
pub const COMMENT_MARKER: &str = "-- ";

const INDENT_STEP: usize = 2;

/// What `Toggle comment` does to one line, in characters: at column `at`, `removed`
/// characters go and `inserted` come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineEdit {
    /// The column the edit is at.
    pub at: usize,
    /// How many characters it removes there.
    pub removed: usize,
    /// How many characters it inserts there.
    pub inserted: usize,
}

impl LineEdit {
    /// Where a caret at `column` on the edited line lands after the edit.
    pub fn moved(&self, column: usize) -> usize {
        if self.inserted > 0 {
            return if column >= self.at {
                column + self.inserted
            } else {
                column
            };
        }
        if column <= self.at {
            column
        } else if column >= self.at + self.removed {
            column - self.removed
        } else {
            self.at
        }
    }

    /// The line after the edit.
    pub fn applied(&self, line: &str) -> String {
        let start = byte_at(line, self.at);
        let end = byte_at(line, self.at + self.removed);
        let mut out = String::with_capacity(line.len() + self.inserted);
        out.push_str(&line[..start]);
        if self.inserted > 0 {
            out.push_str(COMMENT_MARKER);
        }
        out.push_str(&line[end..]);
        out
    }
}

fn byte_at(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map_or(line.len(), |(at, _)| at)
}

fn indent_of(line: &str) -> usize {
    line.chars()
        .take_while(|character| *character == ' ' || *character == '\t')
        .count()
}

/// `Toggle comment` on `lines` (`docs/ux/workspace.md` 10.9): when every non-blank line
/// starts, after its indent, with `--`, that `--` and one following space go from each;
/// otherwise `-- ` goes in at the smallest indent of the non-blank lines. Blank lines are
/// left alone (`None`).
pub fn comment_edits(lines: &[&str]) -> Vec<Option<LineEdit>> {
    let filled = |line: &&&str| !line.trim().is_empty();
    let commented = lines.iter().filter(filled).all(|line| {
        let indent = byte_at(line, indent_of(line));
        line[indent..].starts_with("--")
    });
    let smallest = lines
        .iter()
        .filter(filled)
        .map(|line| indent_of(line))
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                return None;
            }
            if commented {
                let indent = indent_of(line);
                let after = &line[byte_at(line, indent)..];
                let removed = if after.starts_with("-- ") { 3 } else { 2 };
                Some(LineEdit {
                    at: indent,
                    removed,
                    inserted: 0,
                })
            } else {
                Some(LineEdit {
                    at: smallest,
                    removed: 0,
                    inserted: COMMENT_MARKER.chars().count(),
                })
            }
        })
        .collect()
}

/// Why `Format statement` left a range as it was: what is unclosed and the byte where it
/// opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError {
    /// `unclosed string`, `unclosed quoted name`, `unclosed comment` or `unclosed $$ body`.
    pub what: &'static str,
    /// The byte offset in the formatted text where it opened.
    pub at: usize,
}

/// A formatted range: the new text and how many statements it held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formatted {
    /// The range's new text.
    pub text: String,
    /// How many statements it holds.
    pub statements: usize,
}

impl Formatted {
    /// The status bar's line: `Formatted 1 statement`, `Formatted 3 statements`.
    pub fn notice(&self) -> String {
        match self.statements {
            1 => "Formatted 1 statement".to_owned(),
            count => format!("Formatted {count} statements"),
        }
    }
}

#[derive(Debug, Clone)]
struct Token<'a> {
    text: &'a str,
    lexeme: Lexeme,
    range: Range<usize>,
    newline_before: bool,
}

fn tokens_of(text: &str, dialect: QueryDialect) -> Result<Vec<Token<'_>>, FormatError> {
    let mut raw: Vec<(Range<usize>, Lexeme)> = Vec::new();
    let mut carry = Carry::Code;
    lex(
        text,
        dialect,
        &mut carry,
        &mut |range, lexeme| match raw.last_mut() {
            Some((last, previous))
                if *previous == lexeme
                    && last.end == range.start
                    && matches!(lexeme, Lexeme::String | Lexeme::QuotedName)
                    && text
                        .get(last.clone())
                        .is_some_and(|piece| !piece.is_empty()) =>
            {
                last.end = range.end;
            }
            _ => raw.push((range, lexeme)),
        },
    );
    if carry != Carry::Code {
        let what = match carry {
            Carry::Quoted(b'\'') | Carry::Escaped | Carry::OracleQuote(_) => "unclosed string",
            Carry::Quoted(_) | Carry::Bracketed => "unclosed quoted name",
            Carry::Comment { .. } => "unclosed comment",
            Carry::Dollar(_) | Carry::Code => "unclosed $$ body",
        };
        let at = raw
            .iter()
            .rev()
            .find(|(_, lexeme)| {
                matches!(
                    lexeme,
                    Lexeme::String | Lexeme::QuotedName | Lexeme::Comment
                )
            })
            .map_or(0, |(range, _)| range.start);
        return Err(FormatError { what, at });
    }
    let mut tokens = Vec::with_capacity(raw.len());
    let mut newline_before = false;
    for (range, lexeme) in joined_literals(raw, text) {
        let Some(piece) = text.get(range.clone()) else {
            continue;
        };
        if lexeme == Lexeme::Other && piece.trim().is_empty() {
            newline_before |= piece.contains('\n');
            continue;
        }
        tokens.push(Token {
            text: piece,
            lexeme,
            range,
            newline_before: std::mem::take(&mut newline_before),
        });
    }
    Ok(tokens)
}

const LITERAL_PREFIXES: [&str; 4] = ["N", "E", "X", "B"];

fn joined_literals(raw: Vec<(Range<usize>, Lexeme)>, text: &str) -> Vec<(Range<usize>, Lexeme)> {
    let mut out: Vec<(Range<usize>, Lexeme)> = Vec::with_capacity(raw.len());
    for item in raw {
        out.push(item);
        let parts = literal_tail(&out, text);
        if parts > 1 {
            let first = out.len() - parts;
            let Some((last_range, lexeme)) = out.pop() else {
                continue;
            };
            out.truncate(first + 1);
            if let Some((range, kind)) = out.last_mut() {
                range.end = last_range.end;
                *kind = lexeme;
            }
        }
    }
    out
}

fn literal_tail(out: &[(Range<usize>, Lexeme)], text: &str) -> usize {
    let piece = |(range, _): &(Range<usize>, Lexeme)| text.get(range.clone()).unwrap_or("");
    let touching = |pair: &[(Range<usize>, Lexeme)]| {
        pair.windows(2)
            .all(|window| window[0].0.end == window[1].0.start)
    };
    let word = |item: &(Range<usize>, Lexeme)| matches!(item.1, Lexeme::Word | Lexeme::Keyword);
    let quoted = |item: &(Range<usize>, Lexeme)| {
        (item.1 == Lexeme::String && piece(item).starts_with('\''))
            || (item.1 == Lexeme::QuotedName && piece(item).starts_with('"'))
    };
    match out {
        [.., mantissa, sign, exponent]
            if mantissa.1 == Lexeme::Number
                && is_exponent_lead(piece(mantissa))
                && matches!(piece(sign), "+" | "-")
                && exponent.1 == Lexeme::Number
                && piece(exponent).bytes().all(|byte| byte.is_ascii_digit())
                && touching(&out[out.len() - 3..]) =>
        {
            3
        }
        [.., prefix, ampersand, literal]
            if word(prefix)
                && piece(prefix).eq_ignore_ascii_case("U")
                && piece(ampersand) == "&"
                && quoted(literal)
                && touching(&out[out.len() - 3..]) =>
        {
            3
        }
        [.., prefix, literal]
            if word(prefix)
                && literal.1 == Lexeme::String
                && piece(literal).starts_with('\'')
                && LITERAL_PREFIXES
                    .iter()
                    .any(|known| piece(prefix).eq_ignore_ascii_case(known))
                && touching(&out[out.len() - 2..]) =>
        {
            2
        }
        [.., mark, number]
            if mark.1 == Lexeme::Other
                && piece(mark) == "?"
                && number.1 == Lexeme::Number
                && piece(number).bytes().all(|byte| byte.is_ascii_digit())
                && touching(&out[out.len() - 2..]) =>
        {
            2
        }
        _ => 0,
    }
}

fn is_exponent_lead(number: &str) -> bool {
    let Some(mantissa) = number
        .strip_suffix('e')
        .or_else(|| number.strip_suffix('E'))
    else {
        return false;
    };
    mantissa.starts_with(|first: char| first.is_ascii_digit())
        && mantissa
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

/// Whether `text` holds nothing to run: only whitespace and comments.
pub fn is_blank_sql(text: &str, dialect: QueryDialect) -> bool {
    let mut blank = true;
    let mut carry = Carry::Code;
    lex(text, dialect, &mut carry, &mut |range, lexeme| {
        let piece = text.get(range).unwrap_or("");
        if lexeme != Lexeme::Comment && !piece.trim().is_empty() {
            blank = false;
        }
    });
    blank
}

#[derive(Debug, Clone, Copy)]
struct Frame {
    indent: usize,
    depth: usize,
    list: bool,
    opened_at: Option<usize>,
}

struct Writer {
    out: String,
}

impl Writer {
    fn line_is_blank(&self) -> bool {
        let start = self.out.rfind('\n').map_or(0, |at| at + 1);
        self.out[start..].trim().is_empty()
    }

    fn line_indent(&self) -> usize {
        let start = self.out.rfind('\n').map_or(0, |at| at + 1);
        self.out[start..].chars().take_while(|c| *c == ' ').count()
    }

    fn newline(&mut self, indent: usize) {
        if self.line_is_blank() {
            let start = self.out.rfind('\n').map_or(0, |at| at + 1);
            self.out.truncate(start);
        } else {
            let kept = self.out.trim_end_matches(' ').len();
            self.out.truncate(kept);
            self.out.push('\n');
        }
        self.out.extend(std::iter::repeat_n(' ', indent));
    }

    fn push(&mut self, text: &str, spaced: bool) {
        if spaced && !self.line_is_blank() && !self.out.ends_with(' ') {
            self.out.push(' ');
        }
        self.out.push_str(text);
    }
}

fn upper(token: &Token<'_>) -> String {
    match token.lexeme {
        Lexeme::Keyword | Lexeme::Word => token.text.to_ascii_uppercase(),
        _ => String::new(),
    }
}

const JOIN_LEADS: [&str; 6] = ["LEFT", "RIGHT", "FULL", "INNER", "CROSS", "NATURAL"];
const LIST_CLAUSES: [&str; 3] = ["SELECT", "SET", "VALUES"];
const CLAUSE_MODIFIERS: [&str; 2] = ["DISTINCT", "ALL"];

fn next_words<'a>(tokens: &'a [Token<'_>], from: usize) -> impl Iterator<Item = String> + 'a {
    tokens
        .iter()
        .skip(from)
        .filter(|token| token.lexeme != Lexeme::Comment)
        .map(upper)
}

fn starts_clause(tokens: &[Token<'_>], index: usize, previous: &str) -> bool {
    let Some(token) = tokens.get(index) else {
        return false;
    };
    let word = upper(token);
    match word.as_str() {
        "SELECT" | "FROM" | "WHERE" | "HAVING" | "LIMIT" | "OFFSET" | "ON" | "SET" | "VALUES"
        | "RETURNING" | "INTERSECT" | "EXCEPT" | "MINUS" | "UNION" => true,
        "GROUP" | "ORDER" => next_words(tokens, index + 1).next().as_deref() == Some("BY"),
        "JOIN" => !JOIN_LEADS.contains(&previous) && previous != "OUTER",
        lead if JOIN_LEADS.contains(&lead) => {
            let mut after = next_words(tokens, index + 1);
            match after.next().as_deref() {
                Some("JOIN") => true,
                Some("OUTER") => after.next().as_deref() == Some("JOIN"),
                _ => false,
            }
        }
        _ => false,
    }
}

fn is_line_comment(token: &Token<'_>) -> bool {
    token.lexeme == Lexeme::Comment && !token.text.starts_with("/*")
}

fn spaced(previous: Option<&Token<'_>>, token: &Token<'_>) -> bool {
    let Some(previous) = previous else {
        return false;
    };
    let adjacent = previous.range.end == token.range.start;
    match (previous.text, token.text) {
        (_, "," | ")" | ";") | ("(", _) | (".", _) | (_, ".") => false,
        (_, "(") if matches!(previous.lexeme, Lexeme::Word | Lexeme::QuotedName) => !adjacent,
        (":" | "@" | "$", _) | (_, ":") if adjacent => false,
        _ if adjacent && previous.lexeme == Lexeme::Other && token.lexeme == Lexeme::Other => false,
        _ => true,
    }
}

/// `Format statement` on `text` in `dialect` (`docs/ux/workspace.md` 10.9), laid out from
/// `base_indent` spaces: keywords upper-case (strings, quoted names, comments and `$$` bodies
/// byte for byte), each top-level clause on its own line, list items indented 2 more, a
/// sub-query indented 2 from its opening line, one space between tokens. A range that does
/// not lex is an error naming what is unclosed.
pub fn format_sql(
    text: &str,
    dialect: QueryDialect,
    base_indent: usize,
) -> Result<Formatted, FormatError> {
    let tokens = tokens_of(text, dialect)?;
    let mut writer = Writer {
        out: " ".repeat(base_indent),
    };
    let base = Frame {
        indent: base_indent,
        depth: 0,
        list: false,
        opened_at: None,
    };
    let mut frames = vec![base];
    let mut depth = 0_usize;
    let mut item_due = false;
    let mut line_due = false;
    let mut statements = 0_usize;
    let mut in_statement = false;
    let mut previous_word = String::new();
    let mut previous: Option<&Token<'_>> = None;
    for (index, token) in tokens.iter().enumerate() {
        let frame = frames.last().copied().unwrap_or(base);
        let word = upper(token);
        if token.lexeme != Lexeme::Comment && !in_statement {
            in_statement = true;
            statements += 1;
        }
        let item_indent = frame.indent + if frame.list { INDENT_STEP } else { 0 };
        if token.lexeme == Lexeme::Comment {
            if token.newline_before || line_due {
                writer.newline(if item_due { item_indent } else { frame.indent });
            }
            writer.push(token.text, true);
            line_due = is_line_comment(token);
            previous = Some(token);
            continue;
        }
        let clause = depth == frame.depth && starts_clause(&tokens, index, &previous_word);
        if clause {
            writer.newline(frame.indent);
            item_due = false;
            line_due = false;
            if let Some(top) = frames.last_mut() {
                top.list = LIST_CLAUSES.contains(&word.as_str());
            }
            let shown = shown_text(token);
            writer.push(&shown, false);
            if LIST_CLAUSES.contains(&word.as_str()) {
                item_due = true;
            }
            previous_word = word;
            previous = Some(token);
            continue;
        }
        if item_due && !CLAUSE_MODIFIERS.contains(&word.as_str()) {
            writer.newline(item_indent);
            item_due = false;
            line_due = false;
        } else if line_due {
            writer.newline(item_indent);
            line_due = false;
        }
        match token.text {
            "(" => {
                writer.push("(", spaced(previous, token));
                depth += 1;
                let opens_query = matches!(
                    next_words(&tokens, index + 1).next().as_deref(),
                    Some("SELECT" | "WITH")
                );
                if opens_query {
                    frames.push(Frame {
                        indent: writer.line_indent() + INDENT_STEP,
                        depth,
                        list: false,
                        opened_at: Some(writer.line_indent()),
                    });
                }
            }
            ")" => {
                if let Some(top) = frames.last().copied()
                    && top.depth == depth
                    && let Some(opened_at) = top.opened_at
                {
                    frames.pop();
                    writer.newline(opened_at);
                    writer.push(")", false);
                } else {
                    writer.push(")", false);
                }
                depth = depth.saturating_sub(1);
            }
            "," => {
                writer.push(",", false);
                if frame.list && depth == frame.depth {
                    item_due = true;
                }
            }
            ";" => {
                writer.push(";", false);
                depth = 0;
                frames.truncate(1);
                if let Some(top) = frames.first_mut() {
                    top.list = false;
                }
                item_due = false;
                line_due = true;
                in_statement = false;
                if let Some(top) = frames.last_mut() {
                    *top = base;
                }
            }
            _ => {
                let shown = shown_text(token);
                writer.push(&shown, spaced(previous, token));
            }
        }
        if !word.is_empty() {
            previous_word = word;
        }
        previous = Some(token);
    }
    Ok(Formatted {
        text: writer.out.trim().to_owned(),
        statements,
    })
}

fn shown_text(token: &Token<'_>) -> String {
    if token.lexeme == Lexeme::Keyword {
        token.text.to_ascii_uppercase()
    } else {
        token.text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toggled(lines: &[&str]) -> Vec<String> {
        comment_edits(lines)
            .iter()
            .zip(lines)
            .map(|(edit, line)| edit.map_or_else(|| (*line).to_owned(), |e| e.applied(line)))
            .collect()
    }

    #[test]
    fn toggle_comment_inserts_at_the_smallest_indent_and_removes_one_space() {
        assert_eq!(
            toggled(&["SELECT 1", "  FROM t", "", "WHERE x"]),
            ["-- SELECT 1", "--   FROM t", "", "-- WHERE x"]
        );
        assert_eq!(
            toggled(&["-- SELECT 1", "  --FROM t", ""]),
            ["SELECT 1", "  FROM t", ""]
        );
        assert_eq!(toggled(&["-- a", "b"]), ["-- -- a", "-- b"]);
        let edit = comment_edits(&["  x"])[0].expect("a filled line is edited");
        assert_eq!(edit.at, 2);
        assert_eq!(edit.moved(1), 1);
        assert_eq!(edit.moved(3), 6);
        let removal = comment_edits(&["  -- x"])[0].expect("a comment is removed");
        assert_eq!(removal.moved(6), 3);
        assert_eq!(removal.moved(3), 2);
    }

    #[test]
    fn format_puts_each_clause_on_a_line_and_list_items_under_it() {
        let formatted = format_sql(
            "select id, name from users where id = 1 order by name;",
            QueryDialect::PostgreSql,
            0,
        )
        .expect("it lexes");
        assert_eq!(
            formatted.text,
            "SELECT\n  id,\n  name\nFROM users\nWHERE id = 1\nORDER BY name;"
        );
        assert_eq!(formatted.notice(), "Formatted 1 statement");
    }

    #[test]
    fn format_keeps_strings_comments_and_calls_and_indents_sub_queries() {
        let formatted = format_sql(
            "select count(*), 'select from' from t -- note\n where a in (select b from u) and t.c::int >= 2",
            QueryDialect::PostgreSql,
            0,
        )
        .expect("it lexes");
        assert_eq!(
            formatted.text,
            "SELECT\n  count(*),\n  'select from'\nFROM t -- note\nWHERE a IN (\n  SELECT\n    b\n  FROM u\n) AND t.c::int >= 2"
        );
    }

    #[test]
    fn format_lays_out_joins_set_and_values_and_counts_statements() {
        let formatted = format_sql(
            "update t set a = 1, b = 2 where c = 3; insert into t (a, b) values (1, 2), (3, 4); select * from a left outer join b on a.id = b.id",
            QueryDialect::MySql,
            0,
        )
        .expect("it lexes");
        assert_eq!(
            formatted.text,
            "UPDATE t\nSET\n  a = 1,\n  b = 2\nWHERE c = 3;\nINSERT INTO t (a, b)\nVALUES\n  (1, 2),\n  (3, 4);\nSELECT\n  *\nFROM a\nLEFT OUTER JOIN b\nON a.id = b.id"
        );
        assert_eq!(formatted.notice(), "Formatted 3 statements");
    }

    fn formatted(text: &str, dialect: QueryDialect) -> String {
        format_sql(text, dialect, 0).expect("it lexes").text
    }

    #[test]
    fn format_keeps_prefixed_literals_whole_in_each_dialect() {
        assert_eq!(
            formatted(
                "select * from users where name = N'Ada'",
                QueryDialect::SqlServer
            ),
            "SELECT\n  *\nFROM users\nWHERE name = N'Ada'"
        );
        assert_eq!(
            formatted("select n'Ada', x'ff'", QueryDialect::MySql),
            "SELECT\n  n'Ada',\n  x'ff'"
        );
        assert_eq!(
            formatted("select N'Ada' from dual", QueryDialect::Oracle),
            "SELECT\n  N'Ada'\nFROM dual"
        );
        assert_eq!(
            formatted(
                "select E'a\\tb', B'0101', X'ff', U&'d\\0061t', U&\"n\" from t",
                QueryDialect::PostgreSql
            ),
            "SELECT\n  E'a\\tb',\n  B'0101',\n  X'ff',\n  U&'d\\0061t',\n  U&\"n\"\nFROM t"
        );
        assert_eq!(
            formatted("select X'ff'", QueryDialect::Sqlite),
            "SELECT\n  X'ff'"
        );
        assert_eq!(
            formatted("select n 'a', u & 'b'", QueryDialect::PostgreSql),
            "SELECT\n  n 'a',\n  u & 'b'"
        );
    }

    fn relexed(text: &str, dialect: QueryDialect) -> Vec<(Lexeme, String)> {
        tokens_of(text, dialect)
            .expect("it lexes")
            .iter()
            .map(|token| (token.lexeme, shown_text(token)))
            .collect()
    }

    #[test]
    fn format_keeps_every_dialects_literals_and_parameters_whole() {
        let cases: [(QueryDialect, &str, &[&str]); 5] = [
            (
                QueryDialect::PostgreSql,
                "select $1, $12::int, E'a\\tb', e'it\\'s', B'0101', X'ff', U&'d\\0061t', U&\"n\", $$a;b$$, $tag$x $$ y$tag$, 1e-5, 1.5E+3, 2e10, a$b, café from t where c = :name",
                &[
                    "$1",
                    "$12::int",
                    "E'a\\tb'",
                    "e'it\\'s'",
                    "B'0101'",
                    "X'ff'",
                    "U&'d\\0061t'",
                    "U&\"n\"",
                    "$$a;b$$",
                    "$tag$x $$ y$tag$",
                    "1e-5",
                    "1.5E+3",
                    "2e10",
                    "a$b",
                    "café",
                    ":name",
                ],
            ),
            (
                QueryDialect::MySql,
                "select ?, @v, @@session.sql_mode, @'q v', n'Ada', x'ff', b'01', 0x1F, 1e-5, 'it\\'s', \"d\\\"q\", `order` from t",
                &[
                    "?",
                    "@v",
                    "@@session.sql_mode",
                    "@'q v'",
                    "n'Ada'",
                    "x'ff'",
                    "b'01'",
                    "0x1F",
                    "1e-5",
                    "'it\\'s'",
                    "\"d\\\"q\"",
                    "`order`",
                ],
            ),
            (
                QueryDialect::SqlServer,
                "select @p, @@ROWCOUNT, N'Ada', 0x1F, $1.5, 1.5E+3, [a b], x#y from #t join ##g on #t.a = ##g.a",
                &[
                    "@p",
                    "@@ROWCOUNT",
                    "N'Ada'",
                    "0x1F",
                    "$1.5",
                    "1.5E+3",
                    "[a b]",
                    "x#y",
                    "FROM #t",
                    "JOIN ##g",
                    "#t.a = ##g.a",
                ],
            ),
            (
                QueryDialect::Oracle,
                "select :1, :name, q'[it's]', Q'{a}', nq'<y>', NQ'!z!', N'Ada', n'a', 1.5E+3, 1e-5, emp# from dual",
                &[
                    ":1",
                    ":name",
                    "q'[it's]'",
                    "Q'{a}'",
                    "nq'<y>'",
                    "NQ'!z!'",
                    "N'Ada'",
                    "n'a'",
                    "1.5E+3",
                    "1e-5",
                    "emp#",
                ],
            ),
            (
                QueryDialect::Sqlite,
                "select ?, ?123, :a, @b, $c, X'ff', [x y], `z`, 1e-5 from t",
                &["?123", ":a", "@b", "$c", "X'ff'", "[x y]", "`z`", "1e-5"],
            ),
        ];
        for (dialect, statement, forms) in cases {
            let text = formatted(statement, dialect);
            for form in forms {
                assert!(
                    text.contains(form),
                    "{form:?} in {dialect:?} became {text:?}"
                );
            }
            assert_eq!(
                relexed(&text, dialect),
                relexed(statement, dialect),
                "{dialect:?}: {text:?}"
            );
            assert_eq!(formatted(&text, dialect), text, "{dialect:?}");
        }
        assert_eq!(
            formatted("select ?123, ? from t", QueryDialect::Sqlite),
            "SELECT\n  ?123,\n  ?\nFROM t"
        );
        assert_eq!(
            formatted("select * from ##g", QueryDialect::SqlServer),
            "SELECT\n  *\nFROM ##g"
        );
        assert_eq!(
            formatted("select q'[abc]' from dual", QueryDialect::Oracle),
            "SELECT\n  q'[abc]'\nFROM dual"
        );
    }

    #[test]
    fn format_keeps_exponent_numbers_whole_and_spaces_real_operators() {
        for dialect in [
            QueryDialect::PostgreSql,
            QueryDialect::MySql,
            QueryDialect::SqlServer,
            QueryDialect::Oracle,
            QueryDialect::Sqlite,
        ] {
            assert_eq!(
                formatted("select x from t where x < 1e-5 and y > 1.5E+3", dialect),
                "SELECT\n  x\nFROM t\nWHERE x < 1e-5 AND y > 1.5E+3"
            );
            assert_eq!(
                formatted("select a-1, 2e-b, 0x1e-5", dialect),
                "SELECT\n  a - 1,\n  2e - b,\n  0x1e - 5"
            );
        }
    }

    #[test]
    fn an_unclosed_string_is_reported_where_it_opened() {
        let error = format_sql("select 'abc", QueryDialect::PostgreSql, 0)
            .expect_err("an open string does not format");
        assert_eq!(error.what, "unclosed string");
        assert_eq!(error.at, 7);
    }

    #[test]
    fn whitespace_and_comments_are_blank() {
        assert!(is_blank_sql("  -- x\n /* y */ ", QueryDialect::PostgreSql));
        assert!(!is_blank_sql("-- x\nselect 1", QueryDialect::PostgreSql));
    }
}
