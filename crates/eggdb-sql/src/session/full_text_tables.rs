use super::Session;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind};

const MOST_ROWS_BY_RANK: i64 = 1 << 31;

struct FullTextCall {
    start: usize,
    end: usize,
    freetext: bool,
    table: String,
    columns: Option<Vec<String>>,
    condition: String,
    top: Option<i64>,
}

fn unbracketed(token: &Token<'_>) -> Option<String> {
    match &token.kind {
        TokenKind::Word(word) => Some(word.to_ascii_lowercase()),
        TokenKind::QuotedIdentifier(name) => Some(name.clone()),
        _ => None,
    }
}

fn bracketed(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn refused(message: &str) -> SqlError {
    SqlError::new(SqlState::SYNTAX_ERROR, message.to_owned()).with_vendor_code(7601)
}

fn depth_zero_end(tokens: &[Token<'_>], mut at: usize) -> usize {
    let mut depth = 0usize;
    while let Some(token) = tokens.get(at) {
        match token.kind {
            TokenKind::LeftParen => depth += 1,
            TokenKind::RightParen if depth == 0 => return at,
            TokenKind::RightParen => depth -= 1,
            TokenKind::Comma if depth == 0 => return at,
            TokenKind::End | TokenKind::Semicolon => return at,
            _ => {}
        }
        at += 1;
    }
    at
}

fn calls(text: &str, tokens: &[Token<'_>]) -> SqlResult<Vec<FullTextCall>> {
    let mut found = Vec::new();
    let mut at = 0;
    while at + 1 < tokens.len() {
        let freetext = match &tokens[at].kind {
            TokenKind::Word(word) if word.eq_ignore_ascii_case("containstable") => false,
            TokenKind::Word(word) if word.eq_ignore_ascii_case("freetexttable") => true,
            _ => {
                at += 1;
                continue;
            }
        };
        if tokens[at + 1].kind != TokenKind::LeftParen {
            at += 1;
            continue;
        }
        let start = tokens[at].start;
        let mut cursor = at + 2;
        let mut parts = Vec::new();
        while let Some(part) = tokens.get(cursor).and_then(unbracketed) {
            parts.push(part);
            cursor += 1;
            if tokens.get(cursor).map(|token| &token.kind) == Some(&TokenKind::Dot) {
                cursor += 1;
            } else {
                break;
            }
        }
        let table = match parts.as_slice() {
            [name] | [_, name] if parts.len() == 1 || parts[0] == "dbo" => name.clone(),
            [schema, name] => crate::schemas::qualified(schema, name),
            [_, schema, name] => crate::schemas::qualified(schema, name),
            _ => return Err(refused("CONTAINSTABLE and FREETEXTTABLE name a table first")),
        };
        if tokens.get(cursor).map(|token| &token.kind) != Some(&TokenKind::Comma) {
            return Err(refused("CONTAINSTABLE and FREETEXTTABLE take a table, its columns and a search condition"));
        }
        cursor += 1;
        let columns = match tokens.get(cursor).map(|token| &token.kind) {
            Some(TokenKind::Operator(symbol)) if *symbol == "*" => {
                cursor += 1;
                None
            }
            Some(TokenKind::LeftParen) => {
                cursor += 1;
                let mut names = Vec::new();
                while let Some(name) = tokens.get(cursor).and_then(unbracketed) {
                    names.push(name);
                    cursor += 1;
                    if tokens.get(cursor).map(|token| &token.kind) == Some(&TokenKind::Comma) {
                        cursor += 1;
                    } else {
                        break;
                    }
                }
                if tokens.get(cursor).map(|token| &token.kind) != Some(&TokenKind::RightParen) || names.is_empty() {
                    return Err(refused("a column list of CONTAINSTABLE or FREETEXTTABLE is not closed"));
                }
                cursor += 1;
                Some(names)
            }
            _ => {
                let name = tokens
                    .get(cursor)
                    .and_then(unbracketed)
                    .ok_or_else(|| refused("CONTAINSTABLE and FREETEXTTABLE take a column, a column list or *"))?;
                cursor += 1;
                Some(vec![name])
            }
        };
        if tokens.get(cursor).map(|token| &token.kind) != Some(&TokenKind::Comma) {
            return Err(refused("CONTAINSTABLE and FREETEXTTABLE need a search condition"));
        }
        cursor += 1;
        let condition_end = depth_zero_end(tokens, cursor);
        let condition = text
            .get(tokens.get(cursor).map_or(0, |token| token.start)..tokens.get(condition_end).map_or(text.len(), |token| token.start))
            .unwrap_or_default()
            .trim()
            .to_owned();
        cursor = condition_end;
        let mut top = None;
        while tokens.get(cursor).map(|token| &token.kind) == Some(&TokenKind::Comma) {
            cursor += 1;
            match tokens.get(cursor).map(|token| &token.kind) {
                Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("language") => {
                    cursor = depth_zero_end(tokens, cursor + 1);
                }
                Some(TokenKind::Integer(digits)) => {
                    top = Some(digits.parse::<i64>().unwrap_or(MOST_ROWS_BY_RANK).clamp(0, MOST_ROWS_BY_RANK));
                    cursor += 1;
                }
                _ => return Err(refused("CONTAINSTABLE and FREETEXTTABLE take LANGUAGE and a top count after the condition")),
            }
        }
        let Some(close) = tokens.get(cursor).filter(|token| token.kind == TokenKind::RightParen) else {
            return Err(refused("CONTAINSTABLE or FREETEXTTABLE is not closed"));
        };
        found.push(FullTextCall {
            start,
            end: close.end,
            freetext,
            table,
            columns,
            condition,
            top,
        });
        at = cursor + 1;
    }
    Ok(found)
}

impl Session<'_> {
    /// Rewrites Transact-SQL's `CONTAINSTABLE (table, columns, condition [, LANGUAGE x] [, n])`
    /// and `FREETEXTTABLE (...)` in `FROM` into derived tables of `KEY`, the table's one-column
    /// primary key standing for its full-text key, and `RANK`, ordered by rank and cut to `n`
    /// rows when a count is given (D1327); `*` names the table's text columns.
    pub(super) fn prepare_full_text_tables(&mut self, text: &str) -> SqlResult<Option<String>> {
        if self.dialect != Dialect::SqlServer {
            return Ok(None);
        }
        let lowered = text.to_ascii_lowercase();
        if !lowered.contains("containstable") && !lowered.contains("freetexttable") {
            return Ok(None);
        }
        let tokens = crate::lexer::tokenize_in(text, Dialect::SqlServer)?;
        let found = calls(text, &tokens)?;
        if found.is_empty() {
            return Ok(None);
        }
        let mut rewritten = String::with_capacity(text.len() + 256);
        let mut copied = 0;
        for call in found {
            let table = self
                .read(|reader, tables| tables.table(reader, &call.table))?
                .ok_or_else(|| {
                    SqlError::new(SqlState::UNDEFINED_TABLE, format!("Invalid object name '{}'.", call.table)).with_vendor_code(208)
                })?;
            let [key] = table.primary_key.as_slice() else {
                return Err(SqlError::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!(
                        "Cannot use a CONTAINS or FREETEXT predicate on table or indexed view '{}' because it is not full-text indexed: EggDB takes the table's one-column primary key as its full-text key.",
                        call.table
                    ),
                )
                .with_vendor_code(7601));
            };
            let columns: Vec<String> = match &call.columns {
                Some(columns) => columns.clone(),
                None => table
                    .columns
                    .iter()
                    .filter(|column| column.sql_type.is_character())
                    .map(|column| column.name.clone())
                    .collect(),
            };
            if columns.is_empty() {
                return Err(refused("CONTAINSTABLE and FREETEXTTABLE need a text column"));
            }
            let listed = columns.iter().map(|column| bracketed(column)).collect::<Vec<_>>().join(", ");
            let (predicate, rank) = if call.freetext {
                ("FREETEXT", crate::sqlserver_fulltext::FREETEXT_RANK_FUNCTION)
            } else {
                ("CONTAINS", crate::sqlserver_fulltext::CONTAINS_RANK_FUNCTION)
            };
            let top = call.top.map(|count| format!("TOP ({count}) ")).unwrap_or_default();
            let order = if call.top.is_some() { " ORDER BY [RANK] DESC" } else { "" };
            rewritten.push_str(&text[copied..call.start]);
            rewritten.push_str(&format!(
                "(SELECT {top}{} AS [KEY], {rank}({}, {listed}) AS [RANK] FROM {} WHERE {predicate}(({listed}), {}){order})",
                bracketed(&table.columns[*key].name),
                call.condition,
                call.table
                    .split(crate::schemas::QUALIFIER)
                    .map(bracketed)
                    .collect::<Vec<_>>()
                    .join("."),
                call.condition,
            ));
            copied = call.end;
        }
        rewritten.push_str(&text[copied..]);
        Ok(Some(rewritten))
    }
}
