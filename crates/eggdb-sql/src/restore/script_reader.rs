use super::script_text::DecodedLines;
use crate::error::{SqlError, SqlResult, SqlState};
use std::collections::HashMap;
use std::io::BufRead;

const MAX_BATCH_BYTES: u64 = 1 << 30;
const SEGMENT_BYTES: usize = 1 << 20;
const MAX_VARIABLES: usize = 1024;
const MAX_VARIABLE_NAME_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Batch {
    pub(crate) text: String,
    pub(crate) line: u64,
    pub(crate) end_line: u64,
    pub(crate) repeat: u64,
    pub(crate) cut_points: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SqlcmdCommand {
    pub(crate) name: String,
    pub(crate) shown: String,
    pub(crate) argument: String,
    pub(crate) line: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BatchItem {
    Batch(Batch),
    Command(SqlcmdCommand),
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lex {
    Normal,
    String,
    Bracket,
    Quoted,
    Comment(u32),
}

pub(crate) struct ScriptReader<'r> {
    lines: DecodedLines<'r>,
    line: String,
    lex: Lex,
    lex_line: u64,
    depth: u32,
    batch: String,
    batch_line: Option<u64>,
    cut_points: Vec<usize>,
    takes_rest: Option<bool>,
    segmented: bool,
    cuts_large_batches: bool,
    variables: HashMap<String, String>,
    replacing_variables: bool,
    ended: bool,
}

impl<'r> ScriptReader<'r> {
    pub(crate) fn new(input: &'r mut dyn BufRead) -> Self {
        Self::over(DecodedLines::new(input))
    }

    pub(crate) fn whole_batches(input: &'r mut dyn BufRead) -> Self {
        Self {
            cuts_large_batches: false,
            ..Self::over(DecodedLines::new(input))
        }
    }

    pub(crate) fn over(lines: DecodedLines<'r>) -> Self {
        Self {
            lines,
            line: String::new(),
            lex: Lex::Normal,
            lex_line: 0,
            depth: 0,
            batch: String::new(),
            batch_line: None,
            cut_points: Vec::new(),
            takes_rest: None,
            segmented: false,
            cuts_large_batches: true,
            variables: HashMap::new(),
            replacing_variables: false,
            ended: false,
        }
    }

    pub(crate) fn line_number(&self) -> u64 {
        self.lines.line_number()
    }

    pub(crate) fn next_item(&mut self) -> SqlResult<BatchItem> {
        if self.ended {
            return Ok(BatchItem::End);
        }
        loop {
            let mut line = std::mem::take(&mut self.line);
            if !self.lines.next_line(&mut line)? {
                self.line = line;
                self.ended = true;
                return self.at_end();
            }
            let number = self.lines.line_number();
            let item = self.line_item(&line, number);
            self.line = line;
            if let Some(item) = item? {
                return Ok(item);
            }
        }
    }

    fn at_end(&mut self) -> SqlResult<BatchItem> {
        let what = match self.lex {
            Lex::Normal => None,
            Lex::String => Some("a string"),
            Lex::Bracket => Some("a [bracketed] name"),
            Lex::Quoted => Some("a quoted name"),
            Lex::Comment(_) => Some("a comment"),
        };
        if let Some(what) = what {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "the script ends inside {what} that starts at line {}",
                    self.lex_line
                ),
            )
            .with_hint("The script is cut short or damaged; script it again."));
        }
        Ok(self.finish_batch(1).unwrap_or(BatchItem::End))
    }

    fn line_item(&mut self, line: &str, number: u64) -> SqlResult<Option<BatchItem>> {
        let content = line.trim_end_matches(['\n', '\r']);
        if self.lex == Lex::Normal {
            let start = content.trim_start_matches([' ', '\t']);
            if start.starts_with(':') || start.starts_with("!!") {
                return self.command(start, number);
            }
            if let Some(count) = go_line(start, number)? {
                return self.end_batch(count, number);
            }
        }
        let replaced;
        let line = if self.replacing_variables && line.contains("$(") {
            replaced = self.replace_variables(line, number)?;
            replaced.as_str()
        } else {
            line
        };
        let segment = self.note_cut_point(line);
        let grown = self.batch.len() as u64 + line.len() as u64;
        if grown > MAX_BATCH_BYTES {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!(
                    "the batch at line {} of the script is longer than 1 GiB",
                    self.batch_line.unwrap_or(number)
                ),
            )
            .with_hint("Add GO between its statements, as SSMS does."));
        }
        if self.batch_line.is_none() {
            if line.trim().is_empty() {
                return Ok(segment);
            }
            self.batch_line = Some(number);
        }
        self.batch.push_str(line);
        self.scan(line, number);
        Ok(segment)
    }

    fn note_cut_point(&mut self, line: &str) -> Option<BatchItem> {
        if !self.cuts_large_batches
            || self.lex != Lex::Normal
            || self.depth != 0
            || !starts_data_statement(line)
        {
            return None;
        }
        if self.batch.trim().is_empty() {
            return None;
        }
        let takes_rest = *self
            .takes_rest
            .get_or_insert_with(|| super::classify_sqlserver::takes_rest_of_batch(&self.batch));
        if takes_rest {
            return None;
        }
        if self.batch.len() < SEGMENT_BYTES {
            self.cut_points.push(self.batch.len());
            return None;
        }
        self.segmented = true;
        let segment = self.finish_batch(1);
        self.takes_rest = Some(false);
        segment
    }

    fn end_batch(&mut self, count: u64, number: u64) -> SqlResult<Option<BatchItem>> {
        if count > 1 {
            if self.segmented {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "GO {count} at line {number} repeats a batch longer than 1 MiB, which eggdb restore does not do"
                    ),
                )
                .with_hint("Repeat the batch's statements in the script instead."));
            }
            let total = (self.batch.len() as u64).saturating_mul(count);
            if total > MAX_BATCH_BYTES {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "GO {count} at line {number} repeats its batch to more than 1 GiB of statements"
                    ),
                ));
            }
        }
        let item = self.finish_batch(count);
        self.takes_rest = None;
        self.segmented = false;
        Ok(item)
    }

    fn finish_batch(&mut self, repeat: u64) -> Option<BatchItem> {
        let text = std::mem::take(&mut self.batch);
        let cut_points = std::mem::take(&mut self.cut_points);
        let line = self.batch_line.take();
        self.depth = 0;
        let line = line?;
        Some(BatchItem::Batch(Batch {
            text,
            line,
            end_line: self.lines.line_number(),
            repeat,
            cut_points,
        }))
    }

    fn command(&mut self, start: &str, number: u64) -> SqlResult<Option<BatchItem>> {
        let (name, shown, argument) = if let Some(rest) = start.strip_prefix("!!") {
            ("!!".to_owned(), "!!".to_owned(), rest.trim().to_owned())
        } else {
            let body = &start[1..];
            let length = body
                .find(|character: char| !character.is_ascii_alphabetic())
                .unwrap_or(body.len());
            let word = body[..length].to_ascii_lowercase();
            let rest = body[length..].trim();
            if word == "on" && rest.to_ascii_lowercase().starts_with("error") {
                (
                    "on error".to_owned(),
                    ":on error".to_owned(),
                    rest["error".len()..].trim().to_owned(),
                )
            } else {
                (word.clone(), format!(":{word}"), rest.to_owned())
            }
        };
        match name.as_str() {
            "setvar" => {
                self.set_variable(&argument, number)?;
                self.replacing_variables = true;
            }
            "on error" => self.replacing_variables = true,
            "quit" | "exit" => {
                self.ended = true;
                self.batch.clear();
                self.cut_points.clear();
                self.batch_line = None;
            }
            _ => {}
        }
        Ok(Some(BatchItem::Command(SqlcmdCommand {
            name,
            shown,
            argument,
            line: number,
        })))
    }

    fn set_variable(&mut self, argument: &str, number: u64) -> SqlResult<()> {
        let length = argument
            .find(|character: char| character.is_whitespace())
            .unwrap_or(argument.len());
        let name = &argument[..length];
        if name.is_empty()
            || name.len() > MAX_VARIABLE_NAME_BYTES
            || !name.chars().all(is_variable_character)
        {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!("line {number}: :setvar needs a variable name of letters, digits, _ and -"),
            ));
        }
        let key = name.to_ascii_uppercase();
        let value = argument[length..].trim();
        if value.is_empty() {
            self.variables.remove(&key);
            return Ok(());
        }
        let value = match value.strip_prefix('"') {
            Some(quoted) => {
                let Some(inner) = quoted.strip_suffix('"') else {
                    return Err(SqlError::new(
                        SqlState::SYNTAX_ERROR,
                        format!("line {number}: the value of :setvar {name} has no closing quote"),
                    ));
                };
                inner.replace("\"\"", "\"")
            }
            None => value.to_owned(),
        };
        if !self.variables.contains_key(&key) && self.variables.len() >= MAX_VARIABLES {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!(
                    "line {number}: the script defines more than {MAX_VARIABLES} sqlcmd variables"
                ),
            ));
        }
        self.variables.insert(key, value);
        Ok(())
    }

    fn replace_variables(&self, line: &str, number: u64) -> SqlResult<String> {
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        while let Some(at) = rest.find("$(") {
            out.push_str(&rest[..at]);
            let after = &rest[at + 2..];
            let name_length = after
                .find(|character: char| !is_variable_character(character))
                .unwrap_or(after.len());
            if name_length == 0
                || name_length > MAX_VARIABLE_NAME_BYTES
                || !after[name_length..].starts_with(')')
            {
                out.push_str("$(");
                rest = after;
                continue;
            }
            let name = &after[..name_length];
            let Some(value) = self.variables.get(&name.to_ascii_uppercase()) else {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_PARAMETER,
                    format!("line {number}: '{name}' scripting variable not defined."),
                )
                .with_hint("Define it with :setvar before this line, as sqlcmd needs."));
            };
            if (out.len() + value.len()) as u64 > MAX_BATCH_BYTES {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "line {number} grows past 1 GiB when its sqlcmd variables are replaced"
                    ),
                ));
            }
            out.push_str(value);
            rest = &after[name_length + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    fn scan(&mut self, line: &str, number: u64) {
        let bytes = line.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            let byte = bytes[at];
            let next = bytes.get(at + 1).copied();
            match self.lex {
                Lex::Normal => match byte {
                    b'-' if next == Some(b'-') => return,
                    b'/' if next == Some(b'*') => {
                        self.lex = Lex::Comment(1);
                        self.lex_line = number;
                        at += 1;
                    }
                    b'\'' => self.open(Lex::String, number),
                    b'[' => self.open(Lex::Bracket, number),
                    b'"' => self.open(Lex::Quoted, number),
                    b'(' => self.depth = self.depth.saturating_add(1),
                    b')' => self.depth = self.depth.saturating_sub(1),
                    _ => {}
                },
                Lex::String | Lex::Bracket | Lex::Quoted => {
                    let close = match self.lex {
                        Lex::String => b'\'',
                        Lex::Bracket => b']',
                        _ => b'"',
                    };
                    if byte == close {
                        if next == Some(close) {
                            at += 1;
                        } else {
                            self.lex = Lex::Normal;
                        }
                    }
                }
                Lex::Comment(depth) => {
                    if byte == b'/' && next == Some(b'*') {
                        self.lex = Lex::Comment(depth.saturating_add(1));
                        at += 1;
                    } else if byte == b'*' && next == Some(b'/') {
                        self.lex = if depth <= 1 {
                            Lex::Normal
                        } else {
                            Lex::Comment(depth - 1)
                        };
                        at += 1;
                    }
                }
            }
            at += 1;
        }
    }

    fn open(&mut self, lex: Lex, number: u64) {
        self.lex = lex;
        self.lex_line = number;
    }
}

fn is_variable_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '-'
}

pub(crate) fn starts_data_statement(line: &str) -> bool {
    let mut words = line
        .trim_start()
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'));
    match words.next() {
        Some(word)
            if word.eq_ignore_ascii_case("insert")
                || word.eq_ignore_ascii_case("update")
                || word.eq_ignore_ascii_case("delete") =>
        {
            true
        }
        Some(word) if word.eq_ignore_ascii_case("set") => words
            .find(|word| !word.is_empty())
            .is_some_and(|word| word.eq_ignore_ascii_case("identity_insert")),
        _ => false,
    }
}

fn go_line(start: &str, number: u64) -> SqlResult<Option<u64>> {
    let bytes = start.as_bytes();
    if bytes.len() < 2 || !bytes[..2].eq_ignore_ascii_case(b"go") {
        return Ok(None);
    }
    if bytes
        .get(2)
        .is_some_and(|&byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80)
    {
        return Ok(None);
    }
    let mut rest = start[2..].trim_start_matches([' ', '\t']);
    let digits = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    let count = if digits == 0 {
        1
    } else {
        let count = rest[..digits].parse::<u64>().unwrap_or(0);
        if count == 0 {
            return Err(SqlError::new(
                SqlState::SYNTAX_ERROR,
                format!("line {number}: GO takes a count of 1 or more"),
            ));
        }
        rest = rest[digits..].trim_start_matches([' ', '\t']);
        count
    };
    loop {
        if rest.is_empty() || rest.starts_with("--") {
            return Ok(Some(count));
        }
        if let Some(comment) = rest.strip_prefix("/*") {
            let Some(close) = comment.find("*/") else {
                return Err(SqlError::new(
                    SqlState::SYNTAX_ERROR,
                    format!("line {number}: a comment opened on a GO line must close on it"),
                ));
            };
            rest = comment[close + 2..].trim_start_matches([' ', '\t']);
            continue;
        }
        return Ok(None);
    }
}
