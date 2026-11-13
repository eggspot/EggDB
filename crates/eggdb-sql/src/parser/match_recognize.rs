use super::Parser;
use crate::ast::TableReference;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult};
use crate::lexer::{Token, TokenKind, tokenize_in};

const AGGREGATES: [&str; 5] = ["count", "sum", "avg", "min", "max"];

/// The parts of a `MATCH_RECOGNIZE` clause, each kept as the SQL text it was written in.
struct Recognition {
    partitions: Vec<String>,
    order: String,
    measures: Vec<(String, String)>,
    pattern: String,
    definitions: Vec<(String, String)>,
}

fn quoted_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn word<'t>(token: &'t Token<'t>) -> Option<&'t str> {
    match &token.kind {
        TokenKind::Word(word) => Some(word),
        _ => None,
    }
}

fn closing(tokens: &[Token<'_>], open: usize) -> SqlResult<usize> {
    let mut depth = 0usize;
    for (at, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            TokenKind::LeftParen => depth += 1,
            TokenKind::RightParen => {
                depth -= 1;
                if depth == 0 {
                    return Ok(at);
                }
            }
            TokenKind::End => break,
            _ => {}
        }
    }
    Err(SqlError::syntax("a parenthesis is not closed", tokens[open].start))
}

fn arguments(tokens: &[Token<'_>], open: usize, close: usize) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut depth = 0usize;
    let mut start = open + 1;
    for at in open + 1..close {
        match tokens[at].kind {
            TokenKind::LeftParen => depth += 1,
            TokenKind::RightParen => depth = depth.saturating_sub(1),
            TokenKind::Comma if depth == 0 => {
                found.push((start, at));
                start = at + 1;
            }
            _ => {}
        }
    }
    if start < close {
        found.push((start, close));
    }
    found
}

/// Rewrites the text of one expression inside the clause, token by token.
struct Rewriter<'r> {
    text: &'r str,
    tokens: Vec<Token<'r>>,
    variables: &'r [String],
}

impl<'r> Rewriter<'r> {
    fn new(text: &'r str, dialect: Dialect, variables: &'r [String]) -> SqlResult<Self> {
        Ok(Self { text, tokens: tokenize_in(text, dialect)?, variables })
    }

    fn piece(&self, from: usize, to: usize) -> &'r str {
        let start = self.tokens.get(from).map_or(self.text.len(), |token| token.start);
        let end = to.checked_sub(1).and_then(|last| self.tokens.get(last)).map_or(start, |token| token.end);
        self.text.get(start..end.max(start)).unwrap_or_default()
    }

    fn variable_at(&self, at: usize) -> Option<String> {
        let name = word(self.tokens.get(at)?)?;
        let upper = name.to_uppercase();
        (self.variables.contains(&upper) && self.tokens.get(at + 1).map(|token| &token.kind) == Some(&TokenKind::Dot)).then_some(upper)
    }

    /// The tokens `from..to` with each `VAR.column` turned into `column`, and the one variable
    /// they name, if any.
    fn unqualified(&self, from: usize, to: usize) -> SqlResult<(String, Option<String>)> {
        let mut out = String::new();
        let mut named: Option<String> = None;
        let mut last = from;
        let mut at = from;
        while at < to {
            if let Some(variable) = self.variable_at(at) {
                if named.as_ref().is_some_and(|known| *known != variable) {
                    return Err(SqlError::syntax("an expression in MATCH_RECOGNIZE may name one pattern variable", self.tokens[at].start));
                }
                out.push_str(self.piece(last, at));
                out.push(' ');
                named = Some(variable);
                at += 2;
                last = at;
                continue;
            }
            at += 1;
        }
        out.push_str(self.piece(last, to));
        Ok((out, named))
    }

    /// A `DEFINE` condition: `PREV(x[, n])` and `NEXT(x[, n])` become `lag` and `lead` over the
    /// partition in order, and the variable's own qualifier is dropped.
    fn condition(&self, own: &str, window: &str) -> SqlResult<String> {
        let mut out = String::new();
        let mut last = 0;
        let mut at = 0;
        let end = self.tokens.len().saturating_sub(1);
        while at < end {
            let navigation = word(&self.tokens[at]).map(str::to_ascii_lowercase);
            if let Some(direction @ ("prev" | "next")) = navigation.as_deref()
                && self.tokens.get(at + 1).map(|token| &token.kind) == Some(&TokenKind::LeftParen)
            {
                let close = closing(&self.tokens, at + 1)?;
                let parts = arguments(&self.tokens, at + 1, close);
                let Some(&(first_from, first_to)) = parts.first() else {
                    return Err(SqlError::syntax("PREV and NEXT take a column", self.tokens[at].start));
                };
                let (value, named) = self.unqualified(first_from, first_to)?;
                if named.as_deref().is_some_and(|variable| variable != own) {
                    return Err(SqlError::syntax("DEFINE may read only its own variable's rows through PREV and NEXT", self.tokens[at].start));
                }
                let offset = parts.get(1).map_or_else(|| "1".to_owned(), |&(from, to)| self.piece(from, to).to_owned());
                out.push_str(self.piece(last, at));
                out.push_str(&format!(" {}(({value}), {offset}) OVER ({window}) ", if direction == "prev" { "lag" } else { "lead" }));
                at = close + 1;
                last = at;
                continue;
            }
            if let Some(variable) = self.variable_at(at) {
                if variable != own {
                    return Err(SqlError::syntax("DEFINE may refer to the current row, and to earlier ones through PREV", self.tokens[at].start));
                }
                out.push_str(self.piece(last, at));
                out.push(' ');
                at += 2;
                last = at;
                continue;
            }
            at += 1;
        }
        out.push_str(self.piece(last, end));
        Ok(out)
    }

    /// A `MEASURES` expression over one match (`mr_m`): `FIRST`, `LAST`, the aggregates,
    /// `CLASSIFIER()` and `MATCH_NUMBER()` become subqueries over the match's rows, and a bare
    /// column is its last row's value.
    fn measure(&self, rows: &MatchRows) -> SqlResult<String> {
        let end = self.tokens.len().saturating_sub(1);
        let bare = match self.tokens.get(..end) {
            Some([only]) if word(only).is_some() => true,
            Some([_, dot, column]) if self.variable_at(0).is_some() && dot.kind == TokenKind::Dot && word(column).is_some() => true,
            _ => false,
        };
        if bare {
            let (value, named) = self.unqualified(0, end)?;
            return Ok(rows.value_at(&value, &rows.position(false, named.as_deref())));
        }
        let mut out = String::new();
        let mut last = 0;
        let mut at = 0;
        while at < end {
            let function = word(&self.tokens[at]).map(str::to_ascii_lowercase);
            let called = self.tokens.get(at + 1).map(|token| &token.kind) == Some(&TokenKind::LeftParen);
            let Some(function) = function.filter(|_| called) else {
                if self.variable_at(at).is_some() {
                    return Err(SqlError::syntax("a pattern variable's column in MEASURES needs FIRST, LAST or an aggregate around it", self.tokens[at].start));
                }
                at += 1;
                continue;
            };
            let close = closing(&self.tokens, at + 1)?;
            let replaced = match function.as_str() {
                "match_number" => Some("mr_m.mr_match".to_owned()),
                "classifier" => Some(rows.classifier_at(&rows.position(false, None))),
                "first" | "last" => {
                    let (value, named) = self.unqualified(at + 2, close)?;
                    Some(rows.value_at(&value, &rows.position(function == "first", named.as_deref())))
                }
                aggregate if AGGREGATES.contains(&aggregate) => {
                    let inside = self.piece(at + 2, close).trim().to_owned();
                    let (value, named) = match inside.as_str() {
                        "*" => ("*".to_owned(), None),
                        _ if inside.ends_with(".*") => {
                            let variable = inside.trim_end_matches(".*").trim().to_uppercase();
                            if !self.variables.contains(&variable) {
                                return Err(SqlError::syntax(format!("{variable} is not a pattern variable"), self.tokens[at].start));
                            }
                            ("*".to_owned(), Some(variable))
                        }
                        _ => self.unqualified(at + 2, close)?,
                    };
                    Some(rows.aggregate(aggregate, &value, named.as_deref()))
                }
                "prev" | "next" => {
                    return Err(SqlError::syntax("PREV and NEXT belong in DEFINE; MEASURES reads FIRST and LAST", self.tokens[at].start));
                }
                _ => None,
            };
            match replaced {
                Some(replaced) => {
                    out.push_str(self.piece(last, at));
                    out.push_str(&format!(" ({replaced}) "));
                    at = close + 1;
                    last = at;
                }
                None => at += 1,
            }
        }
        out.push_str(self.piece(last, end));
        Ok(out)
    }
}

/// How the measures reach the rows of one match: the match is the row `mr_m` of `mr_matches`,
/// its rows are the rows of `mr_numbered` in its partition from `start` to `end`.
struct MatchRows {
    partition_join: String,
}

const START: &str = "(mr_m.mr_found ->> 'start')::bigint";
const END: &str = "(mr_m.mr_found ->> 'end')::bigint";

impl MatchRows {
    fn position(&self, first: bool, variable: Option<&str>) -> String {
        match variable {
            None if first => START.to_owned(),
            None => END.to_owned(),
            Some(variable) => format!(
                "SELECT {}(mr_numbered.mr_row) FROM mr_numbered WHERE mr_numbered.mr_row BETWEEN {START} AND {END}{} AND mr_m.mr_found -> 'classifiers' ->> (mr_numbered.mr_row - {START})::integer = {}",
                if first { "min" } else { "max" },
                self.partition_join,
                quoted_literal(variable)
            ),
        }
    }

    fn value_at(&self, value: &str, position: &str) -> String {
        format!("SELECT {value} FROM mr_numbered WHERE mr_numbered.mr_row = ({position}){}", self.partition_join)
    }

    fn classifier_at(&self, position: &str) -> String {
        format!("mr_m.mr_found -> 'classifiers' ->> (({position}) - {START})::integer")
    }

    fn aggregate(&self, function: &str, value: &str, variable: Option<&str>) -> String {
        let only = variable.map_or_else(String::new, |variable| {
            format!(
                " AND mr_m.mr_found -> 'classifiers' ->> (mr_numbered.mr_row - {START})::integer = {}",
                quoted_literal(variable)
            )
        });
        format!(
            "SELECT {function}({value}) FROM mr_numbered WHERE mr_numbered.mr_row BETWEEN {START} AND {END}{}{only}",
            self.partition_join
        )
    }
}

impl Parser<'_> {
    pub(super) fn at_match_recognize(&self) -> bool {
        self.is_word("match_recognize") && self.peek_kind_at(1) == &TokenKind::LeftParen
    }

    fn clause_text(&mut self, stops: &[&str]) -> SqlResult<String> {
        let start = self.peek().start;
        let mut depth = 0usize;
        loop {
            match self.peek_kind() {
                TokenKind::End => return Err(self.unexpected()),
                TokenKind::LeftParen => depth += 1,
                TokenKind::RightParen if depth == 0 => break,
                TokenKind::RightParen => depth -= 1,
                TokenKind::Comma if depth == 0 && stops.contains(&",") => break,
                TokenKind::Word(word) if depth == 0 && stops.iter().any(|stop| word.eq_ignore_ascii_case(stop)) => break,
                _ => {}
            }
            self.at += 1;
        }
        let text = self.slice(start, self.previous_end());
        if text.trim().is_empty() {
            return Err(self.unexpected());
        }
        Ok(text)
    }

    fn recognition(&mut self) -> SqlResult<Recognition> {
        self.expect(&TokenKind::LeftParen)?;
        let mut partitions = Vec::new();
        if self.take_word("partition") {
            self.expect_word("by")?;
            loop {
                partitions.push(self.name()?.text);
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        self.expect_word("order")?;
        self.expect_word("by")?;
        let order = self.clause_text(&["measures", "one", "all", "after", "pattern"])?;
        let mut measures = Vec::new();
        if self.take_word("measures") {
            loop {
                if !self.take_word("final") && self.take_word("running") {
                    return Err(self.not_supported("RUNNING measures"));
                }
                let expression = self.clause_text(&["as"])?;
                self.expect_word("as")?;
                measures.push((expression, self.name()?.text));
                if !self.take(&TokenKind::Comma) {
                    break;
                }
            }
        }
        if self.take_word("one") {
            self.expect_word("row")?;
            self.expect_word("per")?;
            self.expect_word("match")?;
        } else if self.is_word("all") {
            return Err(self.not_supported("ALL ROWS PER MATCH"));
        }
        if self.take_word("after") {
            self.expect_word("match")?;
            self.expect_word("skip")?;
            if !(self.take_word("past") && self.take_word("last") && self.take_word("row")) {
                return Err(self.not_supported("AFTER MATCH SKIP other than PAST LAST ROW"));
            }
        }
        self.expect_word("pattern")?;
        self.expect(&TokenKind::LeftParen)?;
        let pattern = self.clause_text(&[])?;
        self.expect(&TokenKind::RightParen)?;
        if self.is_word("subset") {
            return Err(self.not_supported("SUBSET in MATCH_RECOGNIZE"));
        }
        self.expect_word("define")?;
        let mut definitions = Vec::new();
        loop {
            let variable = self.name()?.text.to_uppercase();
            self.expect_word("as")?;
            definitions.push((variable, self.clause_text(&[","])?));
            if !self.take(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RightParen)?;
        Ok(Recognition { partitions, order, measures, pattern, definitions })
    }

    /// SQL:2016's `MATCH_RECOGNIZE` after a table or subquery whose SQL is `source`, with
    /// `PARTITION BY` columns, `ORDER BY`, `MEASURES` (`FIRST`, `LAST`, `COUNT`, `SUM`, `AVG`,
    /// `MIN`, `MAX`, `CLASSIFIER()`, `MATCH_NUMBER()`, final semantics), `ONE ROW PER MATCH`,
    /// `AFTER MATCH SKIP PAST LAST ROW`, `PATTERN` and `DEFINE` (with `PREV` and `NEXT`). It is
    /// a derived table over the rows numbered per partition, each row's `DEFINE` flags matched by
    /// `row_pattern_matches` (D768, D1067).
    pub(super) fn match_recognize(&mut self, source: &str) -> SqlResult<TableReference> {
        self.at += 1;
        let recognition = self.recognition()?;
        let alias = if self.take_word("as") || self.at_bare_alias() { self.name()?.text } else { "match_recognize".to_owned() };
        let mut variables: Vec<String> = recognition.definitions.iter().map(|(variable, _)| variable.clone()).collect();
        for named in recognition.pattern.split(|character: char| !(character.is_alphanumeric() || character == '_')) {
            let named = named.to_uppercase();
            if !named.is_empty() && !named.starts_with(|character: char| character.is_ascii_digit()) && !variables.contains(&named) {
                variables.push(named);
            }
        }
        let partition_list = recognition.partitions.join(", ");
        let window = if partition_list.is_empty() {
            format!("ORDER BY {}", recognition.order)
        } else {
            format!("PARTITION BY {partition_list} ORDER BY {}", recognition.order)
        };
        let mut flags = Vec::new();
        for (variable, condition) in &recognition.definitions {
            let rewritten = Rewriter::new(condition, self.dialect, &variables)?.condition(variable, &window)?;
            flags.push(format!("{}, coalesce(({rewritten}), false)", quoted_literal(variable)));
        }
        let rows = MatchRows {
            partition_join: recognition
                .partitions
                .iter()
                .map(|column| format!(" AND mr_numbered.{column} IS NOT DISTINCT FROM mr_m.{column}"))
                .collect(),
        };
        let mut selected: Vec<String> = recognition.partitions.iter().map(|column| format!("mr_m.{column}")).collect();
        for (expression, name) in &recognition.measures {
            let rewritten = Rewriter::new(expression, self.dialect, &variables)?.measure(&rows)?;
            selected.push(format!("{rewritten} AS {name}"));
        }
        if selected.is_empty() {
            selected.push("mr_m.mr_match AS match_number".to_owned());
        }
        let grouped = if partition_list.is_empty() { String::new() } else { format!(" GROUP BY {partition_list}") };
        let group_columns = if partition_list.is_empty() { String::new() } else { format!("{partition_list}, ") };
        let matched_columns: String = recognition.partitions.iter().map(|column| format!("mr_g.{column}, ")).collect();
        let match_partition = if partition_list.is_empty() {
            String::new()
        } else {
            format!("PARTITION BY {} ", recognition.partitions.iter().map(|column| format!("mr_g.{column}")).collect::<Vec<_>>().join(", "))
        };
        let text = format!(
            "(WITH mr_numbered AS (SELECT mr_source.*, row_number() OVER ({window}) - 1 AS mr_row, jsonb_build_object({}) AS mr_flags FROM (SELECT * FROM {source}) AS mr_source), \
             mr_matches AS (SELECT {matched_columns}mr_e.value AS mr_found, row_number() OVER ({match_partition}ORDER BY (mr_e.value ->> 'start')::bigint) AS mr_match \
             FROM (SELECT {group_columns}jsonb_agg(mr_flags ORDER BY mr_row) AS mr_flag_list FROM mr_numbered{grouped}) AS mr_g \
             CROSS JOIN LATERAL jsonb_array_elements(row_pattern_matches(mr_g.mr_flag_list, {})) AS mr_e) \
             SELECT {} FROM mr_matches AS mr_m) AS {}",
            flags.join(", "),
            quoted_literal(&recognition.pattern),
            selected.join(", "),
            Self::quoted_alias(self.dialect, &alias)
        );
        let tokens = tokenize_in(&text, self.dialect)?;
        let mut parser = Parser::new(&text, &tokens).in_dialect(self.dialect);
        parser.derived_table()
    }

    fn quoted_alias(dialect: Dialect, alias: &str) -> String {
        match dialect {
            Dialect::MySql => format!("`{}`", alias.replace('`', "``")),
            Dialect::SqlServer => format!("[{}]", alias.replace(']', "]]")),
            _ => format!("\"{}\"", alias.replace('"', "\"\"")),
        }
    }
}
