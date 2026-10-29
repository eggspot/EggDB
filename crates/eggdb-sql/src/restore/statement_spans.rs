use super::classify::{Action, Effect};
use super::classify_sqlserver::{
    classify_sqlserver, refused_as_not_supported, takes_rest_of_batch,
};
use super::column_rewrite::without_unsupported_columns;
use super::script::{ClientCommand, ScriptItem, ScriptSource, ScriptStatement};
use super::script_reader::{Batch, BatchItem, ScriptReader};
use super::words::Words;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::tokenize_in;
use crate::parser::sqlserver_starts_statement;
use crate::session::sqlserver::parse_next_statement;
use crate::settings::Settings;
use std::collections::VecDeque;
use std::io::BufRead;
use std::ops::Range;

const SPAN_BYTES: usize = 4 << 20;

pub(crate) struct SqlServerScript<'r> {
    reader: ScriptReader<'r>,
    queue: VecDeque<ScriptItem>,
    settings: Settings,
}

impl<'r> SqlServerScript<'r> {
    pub(crate) fn new(input: &'r mut dyn BufRead) -> Self {
        Self {
            reader: ScriptReader::new(input),
            queue: VecDeque::new(),
            settings: Settings::default(),
        }
    }
}

impl ScriptSource for SqlServerScript<'_> {
    fn next_item(&mut self) -> SqlResult<ScriptItem> {
        loop {
            if let Some(item) = self.queue.pop_front() {
                return Ok(item);
            }
            match self.reader.next_item()? {
                BatchItem::End => return Ok(ScriptItem::End),
                BatchItem::Command(command) => {
                    return Ok(ScriptItem::ClientCommand(ClientCommand {
                        name: command.name,
                        shown: command.shown,
                        argument: command.argument,
                        line: command.line,
                        script: 0,
                    }));
                }
                BatchItem::Batch(batch) => {
                    let statements = statements_of(&batch, &self.settings);
                    for _ in 0..batch.repeat {
                        self.queue
                            .extend(statements.iter().cloned().map(ScriptItem::Statement));
                    }
                }
            }
        }
    }

    fn line_number(&self) -> u64 {
        self.reader.line_number()
    }

    fn skip_data(&mut self) -> SqlResult<u64> {
        Ok(0)
    }

    fn copy_data(&mut self, _out: &mut Vec<u8>) -> SqlResult<bool> {
        Ok(false)
    }

    fn copy_lines(&self) -> u64 {
        0
    }
}

pub(crate) fn statements_of(batch: &Batch, settings: &Settings) -> Vec<ScriptStatement> {
    if takes_rest_of_batch(&batch.text) {
        let text = batch.text.trim_end().to_owned();
        return vec![ScriptStatement::plain(text, batch.line, batch.end_line)];
    }
    let mut statements = Vec::new();
    let mut lines = LineCounter::new(&batch.text, batch.line);
    for piece in pieces_of(batch) {
        let text = &batch.text[piece.clone()];
        let line = lines.line_at(piece.start);
        if text.len() > SPAN_BYTES {
            statements.push(large_piece(text, line));
        } else {
            statements.extend(spans_of(text, line, settings));
        }
    }
    statements
}

fn pieces_of(batch: &Batch) -> Vec<Range<usize>> {
    if batch.text.len() <= SPAN_BYTES {
        return std::iter::once(0..batch.text.len()).collect();
    }
    let mut pieces = Vec::new();
    let mut start = 0;
    for &cut in &batch.cut_points {
        if cut > start && cut <= batch.text.len() {
            pieces.push(start..cut);
            start = cut;
        }
    }
    pieces.push(start..batch.text.len());
    pieces
}

fn large_piece(text: &str, line: u64) -> ScriptStatement {
    let starts_with_insert = Words::sqlserver(text)
        .next()
        .is_some_and(|token| token.is_word("insert"));
    let mut statement = ScriptStatement::plain(text.trim_end().to_owned(), line, line);
    if !starts_with_insert {
        statement.refused = Some(
            SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!(
                    "the batch at line {line} holds more than {} MiB of statements other than one INSERT",
                    SPAN_BYTES >> 20
                ),
            )
            .with_hint("Add GO between its statements, as SSMS does."),
        );
    }
    statement
}

struct LineCounter<'t> {
    text: &'t str,
    offset: usize,
    line: u64,
}

impl<'t> LineCounter<'t> {
    fn new(text: &'t str, line: u64) -> Self {
        Self {
            text,
            offset: 0,
            line,
        }
    }

    fn line_at(&mut self, offset: usize) -> u64 {
        if offset > self.offset {
            let end = offset.min(self.text.len());
            self.line += self.text.as_bytes()[self.offset..end]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count() as u64;
            self.offset = end;
        }
        self.line
    }
}

pub(crate) fn spans_of(text: &str, line: u64, settings: &Settings) -> Vec<ScriptStatement> {
    let tokens = match tokenize_in(text, Dialect::SqlServer) {
        Ok(tokens) => tokens,
        Err(error) => return vec![by_leading_words(text, line, error)],
    };
    let mut lines = LineCounter::new(text, line);
    let mut statements = Vec::new();
    let mut resume = (0, 0);
    while let Some(parsed) =
        parse_next_statement(text, &tokens, settings, Dialect::SqlServer, resume)
    {
        let Some(first) = tokens.get(parsed.first_token) else {
            break;
        };
        let start = first.start;
        let statement_line = lines.line_at(start);
        match parsed.outcome {
            Ok(next) => {
                let end = tokens
                    .get(next.0.saturating_sub(1))
                    .map_or(text.len(), |token| token.end)
                    .max(start);
                resume = next;
                if next.0 <= parsed.first_token {
                    resume = (parsed.first_token + 1, next.1);
                }
                let end_line = lines.line_at(end);
                statements.push(ScriptStatement::plain(
                    text[start..end].to_owned(),
                    statement_line,
                    end_line,
                ));
            }
            Err(error) => {
                let end = unparsed_statement_end(text, start);
                let next_token = tokens
                    .partition_point(|token| token.start < end)
                    .max(parsed.first_token + 1)
                    .min(tokens.len().saturating_sub(1));
                resume = (next_token, 0);
                let end_line = lines.line_at(end);
                let statement_text = &text[start..end];
                match refused_statement(
                    statement_text,
                    rebased(error, start),
                    statement_line,
                    end_line,
                    settings,
                ) {
                    Ok(statement) => statements.push(statement),
                    Err(error) => {
                        let mut whole =
                            ScriptStatement::plain(text.trim_end().to_owned(), line, end_line);
                        whole.refused = Some(match error.position() {
                            Some(position) => error.at(position.saturating_add(start)),
                            None => error.at(start),
                        });
                        return vec![whole];
                    }
                }
            }
        }
        if resume.0 >= tokens.len() {
            break;
        }
    }
    statements
}

fn rebased(error: SqlError, start: usize) -> SqlError {
    match error.position() {
        Some(position) if position >= start => error.at(position - start),
        _ => error,
    }
}

fn by_leading_words(text: &str, line: u64, error: SqlError) -> ScriptStatement {
    let mut statement = ScriptStatement::plain(text.trim_end().to_owned(), line, line);
    if let Action::Run(_) = classify_sqlserver(text, &|_| false) {
        statement.refused = Some(error);
    }
    statement
}

fn refused_statement(
    text: &str,
    error: SqlError,
    line: u64,
    end_line: u64,
    settings: &Settings,
) -> SqlResult<ScriptStatement> {
    let mut statement = ScriptStatement::plain(text.trim_end().to_owned(), line, end_line);
    let effect = match classify_sqlserver(text, &|_| false) {
        Action::Run(effect) => effect,
        Action::Skip(_) | Action::Ignore(_) => return Ok(statement),
    };
    let mut error = error;
    if let Effect::Table { .. } = effect
        && let Some((rewritten, notes)) = without_unsupported_columns(text)
    {
        match parses_alone(&rewritten, settings) {
            Ok(()) => {
                statement.text = rewritten;
                statement.differences = notes;
                return Ok(statement);
            }
            Err(again) => error = again,
        }
    }
    if refused_as_not_supported(&error) {
        statement.refused = Some(error);
        return Ok(statement);
    }
    Err(error)
}

fn parses_alone(text: &str, settings: &Settings) -> SqlResult<()> {
    let tokens = tokenize_in(text, Dialect::SqlServer)?;
    match parse_next_statement(text, &tokens, settings, Dialect::SqlServer, (0, 0)) {
        Some(parsed) => {
            let next = parsed.outcome?;
            let rest = tokens.get(next.0..).unwrap_or_default();
            if rest
                .iter()
                .all(|token| token.kind == crate::lexer::TokenKind::End)
            {
                Ok(())
            } else {
                Err(SqlError::syntax(
                    "the rewritten CREATE TABLE is followed by more statements",
                    0,
                ))
            }
        }
        None => Ok(()),
    }
}

pub(crate) fn unparsed_statement_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut at = start;
    let mut depth = 0u32;
    let mut line_start = false;
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        if line_start {
            line_start = false;
            if depth == 0 && starts_statement_word(&text[at..]) {
                return at;
            }
        }
        match byte {
            b'\n' => line_start = true,
            b';' if depth == 0 => return at + 1,
            b'(' => depth = depth.saturating_add(1),
            b')' => depth = depth.saturating_sub(1),
            b'\'' | b'[' | b'"' => {
                let close = if byte == b'[' { b']' } else { byte };
                at += 1;
                while at < bytes.len() {
                    if bytes[at] == close {
                        if bytes.get(at + 1) == Some(&close) {
                            at += 2;
                            continue;
                        }
                        break;
                    }
                    at += 1;
                }
            }
            b'-' if next == Some(b'-') => {
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
                continue;
            }
            b'/' if next == Some(b'*') => {
                let mut nested = 1u32;
                at += 2;
                while at < bytes.len() && nested > 0 {
                    if bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'*') {
                        nested += 1;
                        at += 1;
                    } else if bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/') {
                        nested -= 1;
                        at += 1;
                    }
                    at += 1;
                }
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    bytes.len()
}

fn starts_statement_word(line: &str) -> bool {
    let trimmed = line.trim_start_matches([' ', '\t', '\r']);
    let length = trimmed
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(trimmed.len());
    length > 0 && sqlserver_starts_statement(&trimmed[..length].to_ascii_lowercase())
}
