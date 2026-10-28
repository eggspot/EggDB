use super::copy_io::{output_closed, read_chunk};
use crate::cli::output::Output;
use eggdb_sql::{CopyChannel, CopyStart, SqlError, SqlResult, SqlState};
use eggdb_ui::{Completion, InlineTerminal, LineEnd, LineReader};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

const END_OF_DATA: &str = "\\.";
const DATA_PROMPT: &str = ">> ";
const DATA_INSTRUCTIONS: &str = "Enter data to be copied followed by a newline.\nEnd with a backslash and a period on a line by itself, or an EOF signal.\n";

pub(super) enum CopyLines<'s> {
    Batch(&'s mut VecDeque<String>),
    Terminal(&'s mut LineReader),
}

pub(super) struct ShellCopy<'s, 'o> {
    pub(super) lines: CopyLines<'s>,
    pub(super) file_in: Option<File>,
    pub(super) file_out: Option<BufWriter<File>>,
    pub(super) stdout: &'s RefCell<Output<'o>>,
    pub(super) copied_out: &'s Cell<bool>,
}

fn is_end_of_data(line: &str) -> bool {
    line.strip_suffix('\r').unwrap_or(line) == END_OF_DATA
}

fn terminal_failed(error: &std::io::Error) -> SqlError {
    SqlError::new(
        SqlState::IO_ERROR,
        format!("cannot read the COPY data from the terminal: {error}"),
    )
}

impl ShellCopy<'_, '_> {
    fn next_line(&mut self) -> SqlResult<Option<String>> {
        match &mut self.lines {
            CopyLines::Batch(lines) => Ok(lines.pop_front()),
            CopyLines::Terminal(reader) => {
                let mut terminal =
                    InlineTerminal::open().map_err(|error| terminal_failed(&error))?;
                let read =
                    reader.read_line(&mut terminal, DATA_PROMPT, &mut |_, cursor| Completion {
                        start: cursor,
                        candidates: Vec::new(),
                        add_space: false,
                    });
                drop(terminal);
                match read.map_err(|error| terminal_failed(&error))? {
                    LineEnd::Line(line) => Ok(Some(line)),
                    LineEnd::EndOfInput => Ok(None),
                    LineEnd::Cleared => Err(SqlError::new(
                        SqlState::QUERY_CANCELED,
                        "COPY from stdin failed: canceled with Ctrl-C",
                    )),
                }
            }
        }
    }
}

impl CopyChannel for ShellCopy<'_, '_> {
    fn start_copy_in(&mut self, _start: &CopyStart) -> SqlResult<()> {
        if self.file_in.is_none() && matches!(self.lines, CopyLines::Terminal(_)) {
            let mut stdout = self.stdout.borrow_mut();
            stdout.text(DATA_INSTRUCTIONS);
            stdout.finish().map_err(|_| output_closed())?;
        }
        Ok(())
    }

    fn read_copy_data(&mut self, data: &mut Vec<u8>) -> SqlResult<bool> {
        if let Some(file) = &mut self.file_in {
            return read_chunk(file, data);
        }
        match self.next_line()? {
            Some(line) if !is_end_of_data(&line) => {
                data.extend_from_slice(line.as_bytes());
                data.push(b'\n');
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn abandon_copy_in(&mut self) {
        if self.file_in.is_some() {
            return;
        }
        if let CopyLines::Batch(lines) = &mut self.lines {
            while let Some(line) = lines.pop_front() {
                if is_end_of_data(&line) {
                    return;
                }
            }
        }
    }

    fn start_copy_out(&mut self, _start: &CopyStart) -> SqlResult<()> {
        self.copied_out.set(true);
        Ok(())
    }

    fn write_copy_data(&mut self, data: &[u8]) -> SqlResult<()> {
        if let Some(file) = &mut self.file_out {
            return file.write_all(data).map_err(|error| {
                SqlError::new(
                    SqlState::IO_ERROR,
                    format!("cannot write the COPY data: {error}"),
                )
            });
        }
        let mut stdout = self.stdout.borrow_mut();
        stdout.bytes(data);
        stdout.line_done().map_err(|_| output_closed())
    }

    fn finish_copy_out(&mut self) -> SqlResult<()> {
        match &mut self.file_out {
            Some(file) => file.flush().map_err(|error| {
                SqlError::new(
                    SqlState::IO_ERROR,
                    format!("cannot write the COPY data: {error}"),
                )
            }),
            None => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CopyEnd {
    Shell,
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BackslashCopy {
    pub(super) sql: String,
    pub(super) from: bool,
    pub(super) end: CopyEnd,
}

fn skip_space(bytes: &[u8], mut at: usize) -> usize {
    while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
        at += 1;
    }
    at
}

fn quoted_end(bytes: &[u8], at: usize, quote: u8) -> Result<usize, String> {
    let mut index = at + 1;
    while index < bytes.len() {
        if bytes[index] == quote {
            if bytes.get(index + 1) == Some(&quote) {
                index += 2;
                continue;
            }
            return Ok(index + 1);
        }
        index += 1;
    }
    Err("\\copy: unterminated quoted string".to_owned())
}

fn parenthesized_end(bytes: &[u8], at: usize) -> Result<usize, String> {
    let mut depth = 0usize;
    let mut index = at;
    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'\'' | b'"') => {
                index = quoted_end(bytes, index, quote)?;
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index + 1);
                }
            }
            _ => {}
        }
        index += 1;
    }
    Err("\\copy: parse error at end of line".to_owned())
}

fn word_end(bytes: &[u8], at: usize) -> Result<usize, String> {
    let mut index = at;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index = quoted_end(bytes, index, b'"')?;
                continue;
            }
            byte if byte.is_ascii_whitespace() || byte == b'(' => break,
            _ => index += 1,
        }
    }
    Ok(index)
}

pub(super) fn backslash_copy_arguments(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("\\copy")?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(rest)
}

pub(super) fn parse_backslash_copy(arguments: &str) -> Result<BackslashCopy, String> {
    let bytes = arguments.as_bytes();
    let start = skip_space(bytes, 0);
    if start >= bytes.len() {
        return Err("\\copy: arguments required".to_owned());
    }
    let mut end = if bytes[start] == b'(' {
        parenthesized_end(bytes, start)?
    } else {
        word_end(bytes, start)?
    };
    let mut at = skip_space(bytes, end);
    if bytes.get(at) == Some(&b'(') {
        end = parenthesized_end(bytes, at)?;
        at = skip_space(bytes, end);
    }
    let source = arguments[start..end].trim();
    let direction_end = word_end(bytes, at)?;
    let from = match arguments[at..direction_end].to_ascii_lowercase().as_str() {
        "from" => true,
        "to" => false,
        "" => return Err("\\copy: parse error at end of line".to_owned()),
        other => return Err(format!("\\copy: parse error at \"{other}\"")),
    };
    at = skip_space(bytes, direction_end);
    let (end_of_copy, after) = if bytes.get(at) == Some(&b'\'') {
        let closing = quoted_end(bytes, at, b'\'')?;
        let path = arguments[at + 1..closing - 1].replace("''", "'");
        (CopyEnd::File(PathBuf::from(path)), closing)
    } else {
        let target_end = word_end(bytes, at)?;
        let target = &arguments[at..target_end];
        let shell = match target.to_ascii_lowercase().as_str() {
            "" => return Err("\\copy: parse error at end of line".to_owned()),
            "stdin" | "pstdin" if from => true,
            "stdout" | "pstdout" if !from => true,
            "program" => {
                return Err(
                    "\\copy with program is not supported: eggdb shell never runs other programs"
                        .to_owned(),
                );
            }
            _ => false,
        };
        let end_of_copy = if shell {
            CopyEnd::Shell
        } else {
            CopyEnd::File(PathBuf::from(target))
        };
        (end_of_copy, target_end)
    };
    let options = arguments[after..].trim().trim_end_matches(';').trim_end();
    let client = if from { "FROM STDIN" } else { "TO STDOUT" };
    let sql = if options.is_empty() {
        format!("COPY {source} {client}")
    } else {
        format!("COPY {source} {client} {options}")
    };
    Ok(BackslashCopy {
        sql,
        from,
        end: end_of_copy,
    })
}

#[cfg(test)]
mod tests {
    use super::{BackslashCopy, CopyEnd, backslash_copy_arguments, parse_backslash_copy};
    use std::path::PathBuf;

    fn parsed(line: &str) -> Result<BackslashCopy, String> {
        parse_backslash_copy(backslash_copy_arguments(line).unwrap_or("not \\copy"))
    }

    #[test]
    fn backslash_copy_becomes_a_copy_with_the_client() {
        assert_eq!(
            parsed("\\copy t from 'data file.csv' csv header"),
            Ok(BackslashCopy {
                sql: "COPY t FROM STDIN csv header".into(),
                from: true,
                end: CopyEnd::File(PathBuf::from("data file.csv")),
            })
        );
        assert_eq!(
            parsed("\\copy \"My T\" (a, \"b c\") to out.tsv with (format csv);"),
            Ok(BackslashCopy {
                sql: "COPY \"My T\" (a, \"b c\") TO STDOUT with (format csv)".into(),
                from: false,
                end: CopyEnd::File(PathBuf::from("out.tsv")),
            })
        );
        assert_eq!(
            parsed("\\copy (select ')' as x) TO stdout"),
            Ok(BackslashCopy {
                sql: "COPY (select ')' as x) TO STDOUT".into(),
                from: false,
                end: CopyEnd::Shell,
            })
        );
        assert_eq!(
            parsed("\\copy t FROM pstdin").map(|copy| copy.end),
            Ok(CopyEnd::Shell)
        );
        assert_eq!(
            parsed("\\copy t from 'it''s.csv'").map(|copy| copy.end),
            Ok(CopyEnd::File(PathBuf::from("it's.csv")))
        );
    }

    #[test]
    fn malformed_backslash_copies_are_refused() {
        assert!(backslash_copy_arguments("\\copyx t").is_none());
        for line in [
            "\\copy",
            "\\copy t",
            "\\copy t into x",
            "\\copy (select 1 to stdout",
            "\\copy t from 'open",
            "\\copy t from program 'cat x'",
            "\\copy t from",
        ] {
            assert!(parsed(line).is_err(), "{line}");
        }
    }
}
