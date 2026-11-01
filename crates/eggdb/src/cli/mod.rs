mod args;
mod byte_argument;
mod commands;
mod failure;
mod help;
mod jsonl;
mod output;
mod render;
mod reports;
mod sqlite_import;
mod ui;

use args::{CommandName, Format, Invocation};
use failure::{Exit, Failure};
use output::{Output, Style};
use render::JsonObject;
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
        Ok(Invocation::HelpJson(command, verb)) => {
            print(console.stdout, &help::registry_json(command, verb))
        }
        Ok(Invocation::NoArguments) => {
            let _ = console.stderr.write_all(help::MAIN.as_bytes());
            Exit::Usage
        }
        Ok(Invocation::Version) => print(console.stdout, &format!("eggdb {}\n", eggdb::VERSION)),
        Ok(Invocation::VersionJson) => print(console.stdout, &help::version_json()),
        Ok(Invocation::Run(request)) => {
            let color = request.color;
            let json = request.format == Some(Format::Json);
            match commands::execute(*request, console) {
                Ok(exit) => exit,
                Err(failure) => report_as(&failure, console, color, json),
            }
        }
        Err(failure) => report_as(
            &failure,
            console,
            args::color_choice_of(args),
            args::json_errors_requested(args),
        ),
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

pub(crate) fn report(
    failure: &Failure,
    console: &mut Console<'_>,
    color: args::ColorChoice,
) -> Exit {
    report_as(failure, console, color, false)
}

fn report_as(
    failure: &Failure,
    console: &mut Console<'_>,
    color: args::ColorChoice,
    json: bool,
) -> Exit {
    if json {
        if *failure != Failure::OutputClosed {
            let mut messages = Output::new(&mut *console.stderr, false);
            messages.text(&error_json(failure));
            let _ = messages.finish();
        }
        return failure.exit();
    }
    let color = commands::uses_color(color, console.terminal.stderr_color);
    let mut messages = Output::new(&mut *console.stderr, color);
    match failure {
        Failure::Usage {
            message, command, ..
        } => {
            error_line(&mut messages, message);
            if let Some(command) = command {
                usage_line(&mut messages, *command);
            }
        }
        Failure::OutputFailed { message } | Failure::Database { message, .. } => {
            error_line(&mut messages, message);
        }
        Failure::OutputClosed => {}
    }
    if let Some(command) = fix_command(failure) {
        messages.text("  ");
        messages.styled(Style::Bold, "try:");
        messages.text(" ");
        messages.text(&command);
        messages.text("\n");
    }
    let _ = messages.finish();
    failure.exit()
}

fn fix_command(failure: &Failure) -> Option<String> {
    match failure {
        Failure::Usage { command, .. } => Some(match command {
            Some(command) => format!("eggdb help {}", command.name()),
            None => "eggdb help".to_owned(),
        }),
        Failure::Database { try_command, .. } => try_command.clone(),
        Failure::OutputFailed { .. } | Failure::OutputClosed => None,
    }
}

fn error_json(failure: &Failure) -> String {
    let message = match failure {
        Failure::Usage { message, .. }
        | Failure::Database { message, .. }
        | Failure::OutputFailed { message } => message.as_str(),
        Failure::OutputClosed => "",
    };
    let mut error = JsonObject::new()
        .string("code", failure.code())
        .string("category", failure.category().name())
        .string("message", message);
    if let Failure::Usage {
        command: Some(command),
        ..
    } = failure
    {
        error = error.string("usage", &help::usage_line(*command));
    }
    if let Some(command) = fix_command(failure) {
        error = error.string("fix_command", &command);
    }
    JsonObject::new().raw("error", &error.object()).line()
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
    messages.text(&help::usage_line(command));
    messages.text("\n");
}

#[cfg(test)]
mod collection_tests;
#[cfg(test)]
mod command_model_tests;
#[cfg(test)]
mod copy_tests;
#[cfg(test)]
mod expiry_tests;
#[cfg(test)]
mod shell_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod transfer_tests;
