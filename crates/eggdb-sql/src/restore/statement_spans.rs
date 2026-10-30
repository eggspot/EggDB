use super::classify::{Action, Effect};
use super::classify_sqlserver::{
    classify_sqlserver, refused_as_not_supported, takes_rest_of_batch,
};
use super::column_rewrite::{AliasTypes, rewritten_table};
use super::script::{ClientCommand, ScriptItem, ScriptSource, ScriptStatement};
use super::script_reader::{Batch, BatchItem, ScriptReader, starts_data_statement};
use super::words::Words;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::tokenize_in;
use crate::parser::sqlserver_starts_statement;
use crate::session::sqlserver::parse_next_statement;
use crate::settings::Settings;
use std::io::BufRead;
use std::ops::Range;

const SPAN_BYTES: usize = 4 << 20;

pub(crate) struct SqlServerScript<'r> {
    reader: ScriptReader<'r>,
    batch: Vec<ScriptStatement>,
    next: usize,
    repeats_left: u64,
    settings: Settings,
    alias_types: AliasTypes,
}

impl<'r> SqlServerScript<'r> {
    pub(crate) fn new(input: &'r mut dyn BufRead) -> Self {
        Self {
            reader: ScriptReader::new(input),
            batch: Vec::new(),
            next: 0,
            repeats_left: 0,
            settings: Settings::default(),
            alias_types: AliasTypes::default(),
        }
    }

    fn next_of_batch(&mut self) -> Option<ScriptStatement> {
        loop {
            if self.next >= self.batch.len() {
                if self.repeats_left <= 1 || self.batch.is_empty() {
                    self.batch.clear();
                    self.repeats_left = 0;
                    return None;
                }
                self.repeats_left -= 1;
                self.next = 0;
            }
            let at = self.next;
            self.next += 1;
            if self.repeats_left > 1 {
                return self.batch.get(at).cloned();
            }
            if let Some(statement) = self.batch.get_mut(at) {
                return Some(std::mem::replace(
                    statement,
                    ScriptStatement::plain(String::new(), 0, 0),
                ));
            }
        }
    }
}

impl ScriptSource for SqlServerScript<'_> {
    fn next_item(&mut self) -> SqlResult<ScriptItem> {
        loop {
            if let Some(statement) = self.next_of_batch() {
                return Ok(ScriptItem::Statement(statement));
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
                    self.batch = statements_of(&batch, &self.settings, &mut self.alias_types);
                    self.next = 0;
                    self.repeats_left = batch.repeat;
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

pub(crate) fn statements_of(
    batch: &Batch,
    settings: &Settings,
    aliases: &mut AliasTypes,
) -> Vec<ScriptStatement> {
    if takes_rest_of_batch(&batch.text) {
        let text = batch.text.trim_end().to_owned();
        return vec![ScriptStatement::plain(text, batch.line, batch.end_line)];
    }
    let mut statements = Vec::new();
    let mut lines = LineCounter::new(&batch.text, batch.line);
    for piece in pieces_of(batch) {
        let Some(text) = batch.text.get(piece.clone()) else {
            continue;
        };
        let line = lines.line_at(piece.start);
        if text.len() > SPAN_BYTES {
            statements.extend(large_piece(text, line, settings, aliases));
        } else {
            statements.extend(spans_of(text, line, settings, aliases));
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

fn large_piece(
    text: &str,
    line: u64,
    settings: &Settings,
    aliases: &mut AliasTypes,
) -> Vec<ScriptStatement> {
    let mut statements = Vec::new();
    let mut lines = LineCounter::new(text, line);
    for group in statement_groups(text) {
        let Some(part) = text.get(group.clone()) else {
            continue;
        };
        let part_line = lines.line_at(group.start);
        if part.len() <= SPAN_BYTES {
            statements.extend(spans_of(part, part_line, settings, aliases));
        } else {
            statements.push(one_large_statement(part, part_line));
        }
    }
    statements
}

fn one_large_statement(text: &str, line: u64) -> ScriptStatement {
    let data = Words::sqlserver(text)
        .next()
        .is_some_and(|token| token.is_word("insert") || token.is_word("update") || token.is_word("delete"));
    let mut statement = ScriptStatement::plain(text.trim_end().to_owned(), line, line);
    if !data {
        statement.refused = Some(
            SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!(
                    "the statement at line {line} is longer than {} MiB and is not an INSERT, UPDATE or DELETE",
                    SPAN_BYTES >> 20
                ),
            )
            .with_hint("End the statements before it with ; or add GO between them, as SSMS does."),
        );
    }
    statement
}

fn statement_groups(text: &str) -> Vec<Range<usize>> {
    let mut groups = Vec::new();
    let mut start = 0;
    let mut last = 0;
    for boundary in statement_boundaries(text)
        .into_iter()
        .chain(std::iter::once(text.len()))
    {
        if boundary - start > SPAN_BYTES && last > start {
            groups.push(start..last);
            start = last;
        }
        last = boundary;
    }
    if start < text.len() {
        groups.push(start..text.len());
    }
    groups
        .into_iter()
        .filter_map(|group| {
            let blank = text
                .get(group.clone())?
                .bytes()
                .take_while(u8::is_ascii_whitespace)
                .count();
            let start = group.start + blank;
            (start < group.end).then_some(start..group.end)
        })
        .collect()
}

pub(crate) fn statement_boundaries(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut boundaries = Vec::new();
    let mut at = 0;
    let mut depth = 0u32;
    let mut blocks = 0u32;
    let mut line_start = false;
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        if line_start {
            line_start = false;
            if depth == 0
                && blocks == 0
                && at > 0
                && starts_data_statement(text.get(at..).unwrap_or(""))
                && boundaries
                    .last()
                    .is_none_or(|&last| !text.get(last..at).unwrap_or("").trim().is_empty())
            {
                boundaries.push(at);
            }
        }
        match byte {
            b'\n' => line_start = true,
            b';' if depth == 0 && blocks == 0 => boundaries.push(at + 1),
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
            _ if is_block_word_start(bytes, at) => {
                let end = word_end(bytes, at);
                let word = text.get(at..end).unwrap_or("");
                if word.eq_ignore_ascii_case("case")
                    || (word.eq_ignore_ascii_case("begin") && !begins_a_transaction(text, end))
                {
                    blocks = blocks.saturating_add(1);
                } else if word.eq_ignore_ascii_case("end") {
                    blocks = blocks.saturating_sub(1);
                }
                at = end;
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    boundaries
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'@' | b'#' | b'$') || byte >= 0x80
}

fn is_block_word_start(bytes: &[u8], at: usize) -> bool {
    bytes.get(at).is_some_and(|&byte| is_name_byte(byte))
        && at.checked_sub(1).is_none_or(|before| !is_name_byte(bytes[before]))
}

fn word_end(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).is_some_and(|&byte| is_name_byte(byte)) {
        at += 1;
    }
    at
}

fn begins_a_transaction(text: &str, after: usize) -> bool {
    let rest = text.get(after..).unwrap_or("").trim_start();
    let end = rest
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .unwrap_or(rest.len());
    let word = rest.get(..end).unwrap_or("");
    ["tran", "transaction", "distributed", "dialog", "conversation"]
        .iter()
        .any(|known| word.eq_ignore_ascii_case(known))
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

pub(crate) fn spans_of(
    text: &str,
    line: u64,
    settings: &Settings,
    aliases: &mut AliasTypes,
) -> Vec<ScriptStatement> {
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
                let statement_text = text.get(start..end).unwrap_or("");
                aliases.learn(statement_text);
                statements.push(ScriptStatement::plain(
                    statement_text.to_owned(),
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
                    aliases,
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
    aliases: &mut AliasTypes,
) -> SqlResult<ScriptStatement> {
    let mut statement = ScriptStatement::plain(text.trim_end().to_owned(), line, end_line);
    aliases.learn(text);
    let effect = match classify_sqlserver(text, &|_| false) {
        Action::Run(effect) => effect,
        Action::Skip(_) | Action::Ignore(_) => return Ok(statement),
    };
    let mut error = error;
    if let Effect::Table { .. } = effect
        && let Some(rewrite) = rewritten_table(text, aliases)
    {
        match parses_alone(&rewrite.text, settings) {
            Ok(()) => {
                statement.text = rewrite.text;
                statement.differences = rewrite.notes;
                statement.identity_not_kept = rewrite.identity_not_kept;
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
