use super::script::{ScriptItem, ScriptSource, ScriptStatement};
use crate::error::{SqlError, SqlResult, SqlState};
use std::collections::VecDeque;
use std::io::BufRead;

const MAX_LINE_BYTES: usize = 1 << 30;
const MAX_STATEMENT_BYTES: usize = 1 << 30;
const BYTE_ORDER_MARK: &[u8] = b"\xEF\xBB\xBF";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lex {
    Normal,
    Quote(u8),
    BlockComment,
}

/// Splits a script the `mysql` client would run, as `mysqldump` writes it, into statements:
/// each ends at the current delimiter (`;` until a `DELIMITER` line changes it) outside
/// strings, backquoted names and comments. `/*!` comments stay in the statement, as the server
/// runs what they hold; other comments are left out.
pub(crate) struct MysqlScript<'r> {
    input: &'r mut dyn BufRead,
    line_number: u64,
    delimiter: Vec<u8>,
    statement: Vec<u8>,
    statement_line: u64,
    lex: Lex,
    ready: VecDeque<ScriptStatement>,
    ended: bool,
}

impl<'r> MysqlScript<'r> {
    pub(crate) fn new(input: &'r mut dyn BufRead) -> Self {
        Self {
            input,
            line_number: 0,
            delimiter: b";".to_vec(),
            statement: Vec::new(),
            statement_line: 0,
            lex: Lex::Normal,
            ready: VecDeque::new(),
            ended: false,
        }
    }

    fn read_line(&mut self) -> SqlResult<Option<Vec<u8>>> {
        let mut line = Vec::new();
        let read = self
            .input
            .by_ref()
            .take(MAX_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .map_err(|error| SqlError::new(SqlState::IO_ERROR, format!("cannot read the dump: {error}")))?;
        if read == 0 {
            return Ok(None);
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, format!("line {} of the dump is longer than 1 GiB", self.line_number + 1)));
        }
        self.line_number += 1;
        if self.line_number == 1 && line.starts_with(BYTE_ORDER_MARK) {
            line.drain(..BYTE_ORDER_MARK.len());
        }
        Ok(Some(line))
    }

    fn finish_statement(&mut self) -> SqlResult<()> {
        let bytes = std::mem::take(&mut self.statement);
        let text = String::from_utf8(bytes).map_err(|error| {
            SqlError::new(
                SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                format!("the statement starting at line {} is not UTF-8 text (invalid byte {})", self.statement_line, error.utf8_error().valid_up_to() + 1),
            )
        })?;
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            self.ready.push_back(ScriptStatement::plain(trimmed.to_owned(), self.statement_line, self.line_number));
        }
        Ok(())
    }

    fn delimiter_command(line: &[u8]) -> Option<Vec<u8>> {
        let trimmed = line.trim_ascii();
        let word = trimmed.get(..9)?;
        if !word.eq_ignore_ascii_case(b"delimiter") || !trimmed.get(9).is_some_and(u8::is_ascii_whitespace) {
            return None;
        }
        let rest = trimmed[9..].trim_ascii();
        let end = rest.iter().position(u8::is_ascii_whitespace).unwrap_or(rest.len());
        (end > 0).then(|| rest[..end].to_vec())
    }

    fn push(&mut self, byte: u8) -> SqlResult<()> {
        if self.statement.len() >= MAX_STATEMENT_BYTES {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("the statement starting at line {} is longer than 1 GiB", self.statement_line),
            ));
        }
        if self.statement.is_empty() {
            if byte.is_ascii_whitespace() {
                return Ok(());
            }
            self.statement_line = self.line_number;
        }
        self.statement.push(byte);
        Ok(())
    }

    fn line(&mut self, line: &[u8]) -> SqlResult<()> {
        if self.lex == Lex::Normal && self.statement.iter().all(u8::is_ascii_whitespace) {
            if let Some(delimiter) = Self::delimiter_command(line) {
                self.statement.clear();
                self.delimiter = delimiter;
                return Ok(());
            }
        }
        let mut at = 0;
        let mut comment_ended_line = false;
        while at < line.len() {
            let byte = line[at];
            match self.lex {
                Lex::BlockComment => {
                    if line[at..].starts_with(b"*/") {
                        self.lex = Lex::Normal;
                        at += 2;
                        self.push(b' ')?;
                        continue;
                    }
                }
                Lex::Quote(quote) => {
                    self.push(byte)?;
                    if byte == b'\\' && quote != b'`' {
                        if let Some(next) = line.get(at + 1) {
                            self.push(*next)?;
                            at += 1;
                        }
                    } else if byte == quote {
                        if line.get(at + 1) == Some(&quote) {
                            self.push(quote)?;
                            at += 1;
                        } else {
                            self.lex = Lex::Normal;
                        }
                    }
                }
                Lex::Normal => {
                    if line[at..].starts_with(&self.delimiter) {
                        self.finish_statement()?;
                        at += self.delimiter.len();
                        continue;
                    }
                    let dash_comment = line[at..].starts_with(b"--") && line.get(at + 2).is_none_or(|next| next.is_ascii_whitespace() || next.is_ascii_control());
                    if dash_comment || byte == b'#' {
                        comment_ended_line = true;
                        break;
                    }
                    if line[at..].starts_with(b"/*") && line.get(at + 2) != Some(&b'!') {
                        self.lex = Lex::BlockComment;
                        at += 2;
                        continue;
                    }
                    if matches!(byte, b'\'' | b'"' | b'`') {
                        self.lex = Lex::Quote(byte);
                    }
                    self.push(byte)?;
                }
            }
            at += 1;
        }
        if comment_ended_line && !self.statement.is_empty() {
            self.push(b'\n')?;
        }
        Ok(())
    }
}

impl ScriptSource for MysqlScript<'_> {
    fn next_item(&mut self) -> SqlResult<ScriptItem> {
        loop {
            if let Some(statement) = self.ready.pop_front() {
                return Ok(ScriptItem::Statement(statement));
            }
            if self.ended {
                return Ok(ScriptItem::End);
            }
            match self.read_line()? {
                Some(line) => self.line(&line)?,
                None => {
                    self.ended = true;
                    if matches!(self.lex, Lex::Quote(_)) {
                        return Err(SqlError::new(
                            SqlState::SYNTAX_ERROR,
                            format!("the dump ends inside a quoted string or name that starts in the statement at line {}", self.statement_line),
                        ));
                    }
                    self.finish_statement()?;
                }
            }
        }
    }

    fn line_number(&self) -> u64 {
        self.line_number
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
