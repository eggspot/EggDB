use super::IncludeOpener;
use super::script::{ClientCommand, ScriptItem, ScriptSource};
use super::sqlplus_reader::SqlPlusReader;
use crate::error::SqlResult;
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
    /// Every other SQL*Plus command, and statements the reader leaves unrun.
    Command(SqlPlusCommand),
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
