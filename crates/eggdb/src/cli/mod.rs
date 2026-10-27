mod args;
mod byte_argument;
mod commands;
mod failure;
mod help;
mod jsonl;
mod output;
mod render;
mod reports;
pub(crate) mod sql_store;
mod ui;

use args::{CommandName, Invocation};
use failure::{Exit, Failure};
use output::{Output, Style};
use std::ffi::OsString;
use std::io::{Read, Write};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Terminal {
    pub(crate) stdin: bool,
    pub(crate) stdout: bool,
    pub(crate) stderr: bool,
    pub(crate) stdout_color: bool,
    pub(crate) stderr_color: bool,
}

pub(crate) struct Console<'a> {
    pub(crate) stdin: &'a mut dyn Read,
    pub(crate) stdout: &'a mut dyn Write,
    pub(crate) stderr: &'a mut dyn Write,
    pub(crate) terminal: Terminal,
}

pub(crate) fn run(args: &[OsString], console: &mut Console<'_>) -> u8 {
    let exit = match args::parse(args) {
        Ok(Invocation::Help(None)) => print(console.stdout, help::MAIN),
        Ok(Invocation::Help(Some(command))) => print(console.stdout, &help::command_help(command)),
        Ok(Invocation::NoArguments) => {
            let _ = console.stderr.write_all(help::MAIN.as_bytes());
            Exit::Usage
        }
        Ok(Invocation::Version) => print(console.stdout, &format!("eggdb {}\n", eggdb::VERSION)),
        Ok(Invocation::Run(request)) => {
            let color = request.color;
            match commands::execute(*request, console) {
                Ok(exit) => exit,
                Err(failure) => report(&failure, console, color),
            }
        }
        Err(failure) => report(&failure, console, args::color_choice_of(args)),
    };
    exit.code()
}

fn print(stdout: &mut dyn Write, text: &str) -> Exit {
    let mut out = Output::new(stdout, false);
    out.text(text);
    match out.finish() {
        Ok(()) | Err(Failure::OutputClosed) => Exit::Done,
        Err(_) => Exit::DatabaseError,
    }
}

fn report(failure: &Failure, console: &mut Console<'_>, color: args::ColorChoice) -> Exit {
    let color = commands::uses_color(color, console.terminal.stderr_color);
    let mut messages = Output::new(&mut *console.stderr, color);
    let try_line = match failure {
        Failure::Usage { message, command } => {
            error_line(&mut messages, message);
            if let Some(command) = command {
                usage_line(&mut messages, *command);
            }
            Some(match command {
                Some(command) => format!("eggdb help {}", command.name()),
                None => "eggdb help".to_owned(),
            })
        }
        Failure::Unavailable { message } | Failure::OutputFailed { message } => {
            error_line(&mut messages, message);
            None
        }
        Failure::Database {
            message,
            try_command,
            ..
        } => {
            error_line(&mut messages, message);
            try_command.clone()
        }
        Failure::OutputClosed => None,
    };
    if let Some(command) = try_line {
        messages.text("  ");
        messages.styled(Style::Bold, "try:");
        messages.text(" ");
        messages.text(&command);
        messages.text("\n");
    }
    let _ = messages.finish();
    failure.exit()
}

pub(crate) fn error_line(messages: &mut Output<'_>, message: &str) {
    messages.styled(Style::Red, "eggdb:");
    messages.text(" ");
    messages.text(message);
    messages.text("\n");
}

fn usage_line(messages: &mut Output<'_>, command: CommandName) {
    messages.text("  ");
    messages.styled(Style::Bold, "usage:");
    messages.text(" ");
    messages.text(help::usage_line(command));
    messages.text("\n");
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod transfer_tests;
