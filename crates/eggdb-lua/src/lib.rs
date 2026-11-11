//! EggDB's own interpreter of Lua 5.1, the language Redis scripts (`EVAL`, `FUNCTION`) are
//! written in (D27, D193). It reads source into a syntax tree and runs it with a step budget, so
//! a hostile script is an error, never a hang, a crash or unbounded memory. No third-party crate
//! and no C library is involved.

mod ast;
mod host;
mod interpreter;
mod lexer;
mod parser;
mod stdlib;
mod value;

pub use host::{Host, NoHost, Reply};
pub use interpreter::{Frame, Interpreter, Native, Unwind};
pub use value::{Closure, Function, Table, TableRef, Value, number_text, parse_number};

use std::fmt;

/// Whether an error came from reading the source or from running it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LuaErrorKind {
    /// The source does not parse.
    Syntax,
    /// The script raised an error, called `error`, or ran out of its budget.
    Runtime,
}

/// An error of a Lua script, with the line it happened on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LuaError {
    /// Whether the source did not parse or the script failed while running.
    pub kind: LuaErrorKind,
    /// The line, from 1; 0 when no line applies.
    pub line: u32,
    /// The message, without the `user_script:LINE:` prefix.
    pub message: String,
}

impl LuaError {
    pub(crate) fn syntax(line: u32, message: &str) -> Self {
        Self {
            kind: LuaErrorKind::Syntax,
            line,
            message: message.to_owned(),
        }
    }

    pub(crate) fn runtime(line: u32, message: impl Into<String>) -> Self {
        Self {
            kind: LuaErrorKind::Runtime,
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for LuaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            formatter.write_str(&self.message)
        } else {
            write!(formatter, "user_script:{}: {}", self.line, self.message)
        }
    }
}

impl std::error::Error for LuaError {}

/// Checks that `source` is a Lua chunk that parses, as `SCRIPT LOAD` does before keeping it.
pub fn check_syntax(source: &[u8]) -> Result<(), LuaError> {
    parser::parse(source).map(drop)
}

#[cfg(test)]
mod tests;
