use super::IncludeOpener;
use super::script::{ClientCommand, ScriptItem, ScriptSource};
use super::sqlplus_reader::{SqlPlusReader, StatementMode, mode_of};
use crate::error::{SqlError, SqlResult, SqlState};
use std::io::BufRead;

/// A SQL*Plus command a [`SqlPlusScript`] hands to its caller as it is, to act on or report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlPlusCommand {
    /// The command's full name in lower case (`set`, `spool`, `host`, `whenever`, `column`,
    /// ...), or what the reader saw: `/` after a statement ended by `;`, `.` for a statement
    /// ended unrun, `unterminated` for text at the end with no `;` or `/`, and `unknown` for a
    /// line that is neither SQL nor a command.
    pub name: String,
    /// The command as a report shows it: `SET PAGESIZE`, `COLUMN`, `!`, or the start of an
    /// unrun statement.
    pub shown: String,
    /// What follows the command's name, with substitution variables replaced; kept for `SPOOL`
    /// and `WHENEVER`, empty for the others.
    pub argument: String,
    /// Where the command is: `line 4`, or `hr_populate.sql line 61` in an included script.
    pub place: String,
}

/// One step of a SQL*Plus script, read as `eggdb restore --dialect oracle` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlPlusStep {
    /// A SQL statement or PL/SQL block to run, with substitution variables replaced. `EXECUTE
    /// p` comes as the block `BEGIN p; END;`.
    Statement {
        /// The statement's text.
        text: String,
        /// Where it starts: `line 4`, or `hr_populate.sql line 61`.
        place: String,
    },
    /// `PROMPT`: the text to print, with substitution variables replaced.
    Prompt(String),
    /// `CONNECT user`: the statement that makes the user's schema the current one. No login
    /// is made.
    Connect {
        /// `ALTER SESSION SET CURRENT_SCHEMA = ...` for the user's schema.
        statement: String,
    },
    /// `EXIT` or `QUIT`: the script ends here.
    Exit {
        /// `EXIT ROLLBACK`: the open transaction is rolled back instead of committed.
        rollback: bool,
        /// The status the script asks the program to exit with (`FAILURE`, `WARNING`, a
        /// number, a variable), in upper case; `None` for none, `SUCCESS` or `0`.
        status: Option<String>,
        /// Where the command is.
        place: String,
    },
    /// `SET SERVEROUTPUT`: whether the caller prints the `DBMS_OUTPUT` lines of each
    /// statement, with [`Session::enable_server_output`](crate::Session::enable_server_output)
    /// or [`Session::disable_server_output`](crate::Session::disable_server_output).
    ServerOutput(ServerOutputSetting),
    /// Every other SQL*Plus command, and statements the reader leaves unrun.
    Command(SqlPlusCommand),
}

/// What SQL*Plus's `SET SERVEROUTPUT ON|OFF [SIZE n|UNLIMITED] [FORMAT ...]` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerOutputSetting {
    /// `ON`: keep the lines PL/SQL writes, up to `buffer_bytes` (`None` for `SIZE UNLIMITED`
    /// or no `SIZE`, which EggDB caps at 1 MiB).
    On {
        /// `SIZE n`, from 2,000 to 1,000,000 bytes.
        buffer_bytes: Option<usize>,
    },
    /// `OFF`: PL/SQL's `DBMS_OUTPUT` calls are ignored again.
    Off,
}

const SERVER_OUTPUT_FORM: &str =
    "the form is SET SERVEROUTPUT ON|OFF [SIZE 2000..1000000|UNLIMITED] [FORMAT WRAPPED|WORD_WRAPPED|TRUNCATED]";
const SERVER_OUTPUT_HINT: &str =
    "SET SERVEROUTPUT ON alone keeps every line, up to EggDB's 1 MiB buffer.";
const SMALLEST_SERVER_OUTPUT_SIZE: usize = 2_000;
const LARGEST_SERVER_OUTPUT_SIZE: usize = 1_000_000;

impl ServerOutputSetting {
    /// Reads what follows `SET SERVEROUTPUT` (or `SET SERVEROUT`): `ON` or `OFF`, then `SIZE`
    /// and `FORMAT` in any order. `FORMAT` is accepted and has no effect, since EggDB never
    /// wraps a line.
    ///
    /// # Errors
    /// A `SIZE` that is not a number from 2,000 to 1,000,000 or `UNLIMITED` (SQLSTATE 22023),
    /// and any other word or a missing one (42601), each with a hint.
    pub fn parse(argument: &str) -> SqlResult<Self> {
        let words: Vec<&str> = argument.split_whitespace().collect();
        let Some(first) = words.first() else {
            return Err(server_output_word_error(None, "SERVEROUTPUT"));
        };
        let mut setting = match first.to_ascii_lowercase().as_str() {
            "on" => Self::On { buffer_bytes: None },
            "off" => Self::Off,
            _ => return Err(server_output_word_error(Some(first), "SERVEROUTPUT")),
        };
        let mut index = 1;
        while let Some(word) = words.get(index) {
            let value = words.get(index + 1).copied();
            let lowered = word.to_ascii_lowercase();
            if lowered == "size" {
                let Some(value) = value else {
                    return Err(server_output_word_error(None, "SIZE"));
                };
                let bytes = server_output_size(value)?;
                if let Self::On { buffer_bytes } = &mut setting {
                    *buffer_bytes = bytes;
                }
            } else if abbreviates(&lowered, "format", 3) {
                let Some(value) = value else {
                    return Err(server_output_word_error(None, "FORMAT"));
                };
                let format = value.to_ascii_lowercase();
                let known = ["wrapped", "word_wrapped", "truncated"]
                    .iter()
                    .any(|name| abbreviates(&format, name, 3));
                if !known {
                    return Err(server_output_word_error(Some(value), "FORMAT"));
                }
            } else {
                return Err(server_output_word_error(Some(word), "SERVEROUTPUT"));
            }
            index += 2;
        }
        Ok(setting)
    }
}

fn abbreviates(word: &str, name: &str, minimum: usize) -> bool {
    word.len() >= minimum && name.starts_with(word)
}

fn server_output_size(value: &str) -> SqlResult<Option<usize>> {
    if abbreviates(&value.to_ascii_lowercase(), "unlimited", 3) {
        return Ok(None);
    }
    match value.parse::<usize>() {
        Ok(bytes) if (SMALLEST_SERVER_OUTPUT_SIZE..=LARGEST_SERVER_OUTPUT_SIZE).contains(&bytes) => {
            Ok(Some(bytes))
        }
        _ => Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!(
                "SET SERVEROUTPUT SIZE {value} is out of range: SIZE is 2000 to 1000000, or UNLIMITED"
            ),
        )
        .with_hint(SERVER_OUTPUT_HINT)),
    }
}

fn server_output_word_error(word: Option<&str>, after: &str) -> SqlError {
    let taken = match word {
        Some(word) => format!("'{word}'"),
        None => format!("nothing after {after}"),
    };
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("SET SERVEROUTPUT does not take {taken}: {SERVER_OUTPUT_FORM}"),
    )
    .with_hint(SERVER_OUTPUT_HINT)
}

/// Whether `text`, the start of a statement in Oracle's dialect, is a PL/SQL unit, which
/// SQL*Plus ends only at a line holding `/`: `Some(true)` for an anonymous block (`DECLARE`,
/// `BEGIN`, `<<label>>`) and `CREATE [OR REPLACE] PROCEDURE`, `FUNCTION`, `PACKAGE`,
/// `TRIGGER` or `TYPE`; `Some(false)` for SQL, which ends at `;`; `None` while too few words
/// have been typed to tell.
#[must_use]
pub fn starts_plsql_unit(text: &str) -> Option<bool> {
    match mode_of(text) {
        StatementMode::Undecided => None,
        StatementMode::Sql => Some(false),
        StatementMode::PlSql => Some(true),
    }
}

/// Reads a SQL*Plus script step by step for a caller that runs it itself, such as `eggdb sql
/// --dialect oracle`: the same reader, terminators, commands, substitution and limits as
/// [`restore_dump`](super::restore_dump) in Oracle's dialect.
pub struct SqlPlusScript<'r> {
    reader: SqlPlusReader<'r>,
}

impl std::fmt::Debug for SqlPlusScript<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SqlPlusScript")
            .field("line", &self.reader.line_number())
            .finish_non_exhaustive()
    }
}

impl<'r> SqlPlusScript<'r> {
    /// Starts reading `input`, opening the scripts it includes with `includes` (without it
    /// every include is an error) and starting with the substitution variables `defines`.
    ///
    /// # Errors
    /// A variable name or value `DEFINE` would refuse.
    pub fn new(
        input: &'r mut dyn BufRead,
        includes: Option<&'r dyn IncludeOpener>,
        defines: &[(String, String)],
    ) -> SqlResult<Self> {
        let mut reader = SqlPlusReader::new(input, includes);
        for (name, value) in defines {
            reader.define(name, value)?;
        }
        Ok(Self { reader })
    }

    /// The next step, or `None` at the end of the script.
    ///
    /// # Errors
    /// What stops the script: an unreadable or damaged script, a substitution variable with no
    /// value in a statement, a refused `SET`, an include that cannot be opened, or a limit.
    pub fn next_step(&mut self) -> SqlResult<Option<SqlPlusStep>> {
        match self.reader.next_item()? {
            ScriptItem::End => Ok(None),
            ScriptItem::Failed(failure) => Err(failure.error),
            ScriptItem::Statement(statement) => {
                if let Some(error) = statement.unresolved {
                    return Err(error);
                }
                let place = self.place(statement.script, statement.line);
                let text = match statement.text.strip_prefix("EXECUTE ") {
                    Some(call) => format!("BEGIN {call}; END;"),
                    None => statement.text,
                };
                Ok(Some(SqlPlusStep::Statement { text, place }))
            }
            ScriptItem::ClientCommand(command) => Ok(Some(self.command_step(command))),
        }
    }

    fn command_step(&self, command: ClientCommand) -> SqlPlusStep {
        let place = self.place(command.script, command.line);
        let name = command.name.clone();
        match name.as_str() {
            "prompt" => SqlPlusStep::Prompt(command.argument),
            "set serveroutput" => match ServerOutputSetting::parse(&command.argument) {
                Ok(setting) => SqlPlusStep::ServerOutput(setting),
                Err(_) => SqlPlusStep::Command(SqlPlusCommand {
                    name: command.name,
                    shown: command.shown,
                    argument: command.argument,
                    place,
                }),
            },
            "connect" => SqlPlusStep::Connect {
                statement: format!(
                    "ALTER SESSION SET CURRENT_SCHEMA = {}",
                    super::oracle_quoted(&command.argument)
                ),
            },
            "exit" => {
                let words: Vec<String> = command
                    .argument
                    .split_whitespace()
                    .map(str::to_ascii_uppercase)
                    .collect();
                let rollback = words.iter().any(|word| word == "ROLLBACK");
                let status: Vec<&str> = words
                    .iter()
                    .map(String::as_str)
                    .filter(|word| !matches!(*word, "COMMIT" | "ROLLBACK" | "SUCCESS" | "0"))
                    .collect();
                SqlPlusStep::Exit {
                    rollback,
                    status: (!status.is_empty()).then(|| status.join(" ")),
                    place,
                }
            }
            _ => SqlPlusStep::Command(SqlPlusCommand {
                name: command.name,
                shown: command.shown,
                argument: command.argument,
                place,
            }),
        }
    }

    fn place(&self, script: u32, line: u64) -> String {
        if script == 0 {
            return format!("line {line}");
        }
        let names = self.reader.script_names();
        match usize::try_from(script).ok().and_then(|index| names.get(index)) {
            Some(name) => format!("{name} line {line}"),
            None => format!("line {line}"),
        }
    }
}
