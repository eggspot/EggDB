use super::words::is_word_byte;
use crate::error::{SqlError, SqlResult, SqlState};
use std::io::BufRead;

const MAX_LINE_BYTES: usize = (1 << 30) + 2;
const MAX_STATEMENT_BYTES: usize = 1 << 30;
const COPY_PIECE_BYTES: usize = 64 * 1024;
const REUSED_LINE_BYTES: usize = 1 << 20;
const BYTE_ORDER_MARK: &[u8] = b"\xEF\xBB\xBF";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DumpStatement {
    pub(crate) text: String,
    pub(crate) line: u64,
    pub(crate) end_line: u64,
    pub(crate) copy_from_stdin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MetaCommand {
    pub(crate) name: String,
    pub(crate) argument: String,
    pub(crate) line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Item {
    Statement(DumpStatement),
    Meta(MetaCommand),
    End,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Lex {
    Normal,
    Quote { backslashes: bool },
    Identifier,
    Dollar(Vec<u8>),
    Comment(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Copy {
    None,
    Data,
    Ended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Routine {
    #[default]
    Start,
    AfterCreate,
    AfterOr,
    AfterReplace,
    Body,
    Other,
}

#[derive(Debug, Default)]
struct Atomic {
    routine: Routine,
    begin_seen: bool,
    depth: u32,
}

impl Atomic {
    fn word(&mut self, word: &[u8]) {
        let is = |expected: &str| word.eq_ignore_ascii_case(expected.as_bytes());
        self.routine = match self.routine {
            Routine::Start if is("create") => Routine::AfterCreate,
            Routine::AfterCreate if is("or") => Routine::AfterOr,
            Routine::AfterOr if is("replace") => Routine::AfterReplace,
            Routine::AfterCreate | Routine::AfterReplace if is("function") || is("procedure") => {
                Routine::Body
            }
            Routine::Body => Routine::Body,
            _ => Routine::Other,
        };
        if self.routine != Routine::Body {
            return;
        }
        if self.depth == 0 {
            if self.begin_seen && is("atomic") {
                self.depth = 1;
            }
            self.begin_seen = is("begin");
        } else if is("case") {
            self.depth += 1;
        } else if is("end") {
            self.depth -= 1;
        }
    }
}

pub(crate) struct DumpReader<'r> {
    input: &'r mut dyn BufRead,
    line: Vec<u8>,
    line_number: u64,
    pending: Option<(String, u64, usize)>,
    statement: String,
    statement_line: u64,
    started: bool,
    lex: Lex,
    lex_line: u64,
    depth: u32,
    word: Vec<u8>,
    atomic: Atomic,
    copy: Copy,
    copy_lines: u64,
    first_line: bool,
    max_line: usize,
    max_statement: usize,
}

impl<'r> DumpReader<'r> {
    #[cfg(test)]
    pub(crate) fn with_limits(
        input: &'r mut dyn BufRead,
        max_line: usize,
        max_statement: usize,
    ) -> Self {
        Self {
            max_line,
            max_statement,
            ..Self::new(input)
        }
    }

    pub(crate) fn new(input: &'r mut dyn BufRead) -> Self {
        Self {
            input,
            line: Vec::new(),
            line_number: 0,
            pending: None,
            statement: String::new(),
            statement_line: 0,
            started: false,
            lex: Lex::Normal,
            lex_line: 0,
            depth: 0,
            word: Vec::new(),
            atomic: Atomic::default(),
            copy: Copy::None,
            copy_lines: 0,
            first_line: true,
            max_line: MAX_LINE_BYTES,
            max_statement: MAX_STATEMENT_BYTES,
        }
    }

    pub(crate) fn line_number(&self) -> u64 {
        self.line_number
    }

    pub(crate) fn copy_lines(&self) -> u64 {
        self.copy_lines
    }

    pub(crate) fn next_item(&mut self) -> SqlResult<Item> {
        if self.copy == Copy::Data {
            self.skip_copy_data()?;
        }
        self.copy = Copy::None;
        loop {
            let (line, number, start) = match self.pending.take() {
                Some(pending) => pending,
                None => match self.read_text_line()? {
                    Some(line) => (line, self.line_number, 0),
                    None => return self.at_end(),
                },
            };
            let outcome = self.line_item(&line[start..], number);
            match outcome {
                Ok((item, Some(rest))) => {
                    self.pending = Some((line, number, start + rest));
                    if let Some(item) = item {
                        return Ok(item);
                    }
                }
                Ok((item, None)) => {
                    self.recycle(line);
                    if let Some(item) = item {
                        return Ok(item);
                    }
                }
                Err(error) => {
                    self.recycle(line);
                    return Err(error);
                }
            }
        }
    }

    fn recycle(&mut self, line: String) {
        let mut bytes = line.into_bytes();
        bytes.clear();
        if bytes.capacity() <= REUSED_LINE_BYTES && self.line.capacity() < bytes.capacity() {
            self.line = bytes;
        }
    }

    fn read_text_line(&mut self) -> SqlResult<Option<String>> {
        let mut line = std::mem::take(&mut self.line);
        line.clear();
        let read = match self.read_line_into(&mut line) {
            Ok(read) => read,
            Err(error) => {
                self.line = line;
                return Err(error);
            }
        };
        if !read {
            self.line = line;
            return Ok(None);
        }
        self.line_number += 1;
        if self.first_line {
            self.first_line = false;
            if line.starts_with(BYTE_ORDER_MARK) {
                line.drain(..BYTE_ORDER_MARK.len());
            }
        }
        let number = self.line_number;
        String::from_utf8(line).map(Some).map_err(|error| {
            SqlError::new(
                SqlState::CHARACTER_NOT_IN_REPERTOIRE,
                format!(
                    "line {number} of the dump is not UTF-8 text (invalid byte at column {})",
                    error.utf8_error().valid_up_to() + 1
                ),
            )
        })
    }

    fn line_item(&mut self, line: &str, number: u64) -> SqlResult<(Option<Item>, Option<usize>)> {
        let bytes = line.as_bytes();
        if self.lex == Lex::Normal
            && bytes
                .iter()
                .find(|byte| !matches!(byte, b' ' | b'\t'))
                .is_some_and(|&byte| byte == b'\\')
        {
            return Ok((Some(Item::Meta(meta_command(line, number))), None));
        }
        let was_started = self.started;
        let (end, started_at) = self.scan(bytes, number);
        let from = if was_started {
            0
        } else {
            started_at.unwrap_or(bytes.len())
        };
        let to = end.map_or(bytes.len(), |end| end + 1);
        if from < to {
            if self.statement.len() + (to - from) > self.max_statement {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "the statement at line {} of the dump is longer than 1 GiB",
                        self.statement_line
                    ),
                ));
            }
            self.statement.push_str(&line[from..to]);
        }
        let Some(end) = end else {
            return Ok((None, None));
        };
        let rest = (end + 1 < bytes.len()).then_some(end + 1);
        Ok((self.finish_statement(number), rest))
    }

    fn finish_statement(&mut self, end_line: u64) -> Option<Item> {
        let started = std::mem::replace(&mut self.started, false);
        self.depth = 0;
        self.word.clear();
        self.atomic = Atomic::default();
        let text = std::mem::take(&mut self.statement);
        if !started {
            return None;
        }
        let copy_from_stdin = is_copy_from_stdin(&text);
        if copy_from_stdin {
            self.copy = Copy::Data;
            self.copy_lines = 0;
        }
        Some(Item::Statement(DumpStatement {
            text,
            line: self.statement_line,
            end_line,
            copy_from_stdin,
        }))
    }

    fn at_end(&mut self) -> SqlResult<Item> {
        let what = match self.lex {
            Lex::Normal => None,
            Lex::Quote { .. } => Some("a quoted string"),
            Lex::Identifier => Some("a quoted identifier"),
            Lex::Dollar(_) => Some("a dollar-quoted string"),
            Lex::Comment(_) => Some("a comment"),
        };
        if let Some(what) = what {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "the dump ends inside {what} that starts at line {}",
                    self.lex_line
                ),
            )
            .with_hint("The dump is cut short or damaged; take it again."));
        }
        let end_line = self.line_number;
        match self.finish_statement(end_line) {
            Some(Item::Statement(mut statement)) => {
                self.copy = Copy::None;
                statement.copy_from_stdin = false;
                Ok(Item::Statement(statement))
            }
            _ => Ok(Item::End),
        }
    }

    fn end_word(&mut self) -> bool {
        if self.word.is_empty() {
            return false;
        }
        let word = std::mem::take(&mut self.word);
        self.atomic.word(&word);
        let escape_prefix = word.eq_ignore_ascii_case(b"e");
        self.word = word;
        self.word.clear();
        escape_prefix
    }

    fn start_here(&mut self, at: usize, number: u64, started_at: &mut Option<usize>) {
        if !self.started {
            self.started = true;
            self.statement_line = number;
            *started_at = Some(at);
        }
    }

    fn scan(&mut self, line: &[u8], number: u64) -> (Option<usize>, Option<usize>) {
        let mut started_at = None;
        let mut at = 0;
        while at < line.len() {
            let byte = line[at];
            match self.lex {
                Lex::Normal => {
                    if is_word_byte(byte) && !(byte == b'$' && self.word.is_empty()) {
                        self.start_here(at, number, &mut started_at);
                        if self.word.len() < 16 {
                            self.word.push(byte);
                        }
                        at += 1;
                        continue;
                    }
                    let escape_prefix = self.end_word();
                    match byte {
                        b'-' if line.get(at + 1) == Some(&b'-') => return (None, started_at),
                        b'/' if line.get(at + 1) == Some(&b'*') => {
                            self.lex = Lex::Comment(1);
                            self.lex_line = number;
                            at += 2;
                            continue;
                        }
                        b'\'' => {
                            self.start_here(at, number, &mut started_at);
                            self.lex = Lex::Quote {
                                backslashes: escape_prefix,
                            };
                            self.lex_line = number;
                        }
                        b'"' => {
                            self.start_here(at, number, &mut started_at);
                            self.lex = Lex::Identifier;
                            self.lex_line = number;
                        }
                        b'$' => {
                            self.start_here(at, number, &mut started_at);
                            if let Some(length) = dollar_tag_length(&line[at..]) {
                                self.lex = Lex::Dollar(line[at..at + length].to_vec());
                                self.lex_line = number;
                                at += length;
                                continue;
                            }
                        }
                        b'(' => {
                            self.start_here(at, number, &mut started_at);
                            self.depth = self.depth.saturating_add(1);
                        }
                        b')' => {
                            self.start_here(at, number, &mut started_at);
                            self.depth = self.depth.saturating_sub(1);
                        }
                        b';' => {
                            if self.depth == 0 && self.atomic.depth == 0 {
                                return (Some(at), started_at);
                            }
                        }
                        byte if byte.is_ascii_whitespace() => {}
                        _ => self.start_here(at, number, &mut started_at),
                    }
                    at += 1;
                }
                Lex::Quote { backslashes } => {
                    if backslashes && byte == b'\\' {
                        at += 2;
                        continue;
                    }
                    if byte == b'\'' {
                        if line.get(at + 1) == Some(&b'\'') {
                            at += 2;
                            continue;
                        }
                        self.lex = Lex::Normal;
                    }
                    at += 1;
                }
                Lex::Identifier => {
                    if byte == b'"' {
                        if line.get(at + 1) == Some(&b'"') {
                            at += 2;
                            continue;
                        }
                        self.lex = Lex::Normal;
                    }
                    at += 1;
                }
                Lex::Dollar(ref tag) => {
                    if line[at..].starts_with(tag) {
                        at += tag.len();
                        self.lex = Lex::Normal;
                        continue;
                    }
                    at += 1;
                }
                Lex::Comment(depth) => {
                    if byte == b'/' && line.get(at + 1) == Some(&b'*') {
                        self.lex = Lex::Comment(depth.saturating_add(1));
                        at += 2;
                        continue;
                    }
                    if byte == b'*' && line.get(at + 1) == Some(&b'/') {
                        self.lex = if depth <= 1 {
                            Lex::Normal
                        } else {
                            Lex::Comment(depth - 1)
                        };
                        at += 2;
                        continue;
                    }
                    at += 1;
                }
            }
        }
        if self.lex == Lex::Normal {
            self.end_word();
        }
        (None, started_at)
    }

    pub(crate) fn copy_data(&mut self, out: &mut Vec<u8>) -> SqlResult<bool> {
        if self.copy != Copy::Data {
            return Ok(false);
        }
        let start = out.len();
        while out.len() - start < COPY_PIECE_BYTES {
            let line_start = out.len();
            if !self.read_line_into(out)? {
                self.copy = Copy::Ended;
                break;
            }
            self.line_number += 1;
            if is_end_of_copy(&out[line_start..]) {
                out.truncate(line_start);
                self.copy = Copy::Ended;
                break;
            }
            self.copy_lines += 1;
        }
        Ok(out.len() > start)
    }

    pub(crate) fn skip_copy_data(&mut self) -> SqlResult<u64> {
        let mut skipped = 0;
        while self.copy == Copy::Data {
            match self.skip_line()? {
                None => self.copy = Copy::Ended,
                Some(true) => {
                    self.line_number += 1;
                    self.copy = Copy::Ended;
                }
                Some(false) => {
                    self.line_number += 1;
                    self.copy_lines += 1;
                    skipped += 1;
                }
            }
        }
        Ok(skipped)
    }

    fn read_line_into(&mut self, out: &mut Vec<u8>) -> SqlResult<bool> {
        let start = out.len();
        loop {
            let available = match self.input.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(read_failure(&error)),
            };
            if available.is_empty() {
                return Ok(out.len() > start);
            }
            let (take, done) = match available.iter().position(|&byte| byte == b'\n') {
                Some(index) => (index + 1, true),
                None => (available.len(), false),
            };
            if out.len() - start + take > self.max_line {
                return Err(line_too_long(self.line_number + 1));
            }
            out.extend_from_slice(&available[..take]);
            self.input.consume(take);
            if done {
                return Ok(true);
            }
        }
    }

    fn skip_line(&mut self) -> SqlResult<Option<bool>> {
        let mut prefix = [0u8; 4];
        let mut length = 0usize;
        loop {
            let available = match self.input.fill_buf() {
                Ok(available) => available,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(read_failure(&error)),
            };
            if available.is_empty() {
                if length == 0 {
                    return Ok(None);
                }
                return Ok(Some(is_end_of_copy(&prefix[..length.min(4)])));
            }
            let (take, done) = match available.iter().position(|&byte| byte == b'\n') {
                Some(index) => (index + 1, true),
                None => (available.len(), false),
            };
            for &byte in &available[..take] {
                if length < prefix.len() {
                    prefix[length] = byte;
                }
                length += 1;
            }
            self.input.consume(take);
            if length > self.max_line {
                return Err(line_too_long(self.line_number + 1));
            }
            if done {
                return Ok(Some(length <= 4 && is_end_of_copy(&prefix[..length])));
            }
        }
    }
}

fn read_failure(error: &std::io::Error) -> SqlError {
    SqlError::new(SqlState::IO_ERROR, format!("cannot read the dump: {error}"))
}

fn line_too_long(number: u64) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("line {number} of the dump is longer than 1 GiB"),
    )
}

fn is_end_of_copy(line: &[u8]) -> bool {
    matches!(line, b"\\." | b"\\.\n" | b"\\.\r\n" | b"\\.\r")
}

fn dollar_tag_length(text: &[u8]) -> Option<usize> {
    let mut length = 1;
    loop {
        match *text.get(length)? {
            b'$' => return Some(length + 1),
            byte if is_word_byte(byte) && !(length == 1 && byte.is_ascii_digit()) => length += 1,
            _ => return None,
        }
    }
}

fn meta_command(line: &str, number: u64) -> MetaCommand {
    let body = line
        .trim_start_matches([' ', '\t'])
        .trim_start_matches('\\')
        .trim_end_matches(['\n', '\r']);
    let name_length = match body.chars().next() {
        Some(first) if first.is_ascii_alphabetic() => body
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(body.len()),
        Some(first) => first.len_utf8(),
        None => 0,
    };
    MetaCommand {
        name: body[..name_length].to_owned(),
        argument: body[name_length..].trim().to_owned(),
        line: number,
    }
}

fn is_copy_from_stdin(text: &str) -> bool {
    let mut words = super::words::Words::new(text);
    if !words.next().is_some_and(|token| token.is_word("copy")) {
        return false;
    }
    let mut depth = 0u32;
    let mut after_from = false;
    for token in words {
        if token.is_symbol(b'(') {
            depth += 1;
        } else if token.is_symbol(b')') {
            depth = depth.saturating_sub(1);
        } else if depth == 0 {
            if after_from && token.is_word("stdin") {
                return true;
            }
            after_from = token.is_word("from");
        }
    }
    false
}
