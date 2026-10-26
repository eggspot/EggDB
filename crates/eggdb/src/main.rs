//! The `eggdb` command: create, read, write, scan, check, export and import an EggDB database
//! file from a shell or a script. `eggdb help` lists the commands; the guide is `docs/cli.md` in
//! the repository.

mod cli;

use eggdb_platform::terminal::{StandardStream, is_terminal, wants_color};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let terminal = cli::Terminal {
        stdin: is_terminal(StandardStream::Input),
        stdout: is_terminal(StandardStream::Output),
        stderr: is_terminal(StandardStream::Error),
        stdout_color: wants_color(StandardStream::Output),
        stderr_color: wants_color(StandardStream::Error),
    };
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    let mut console = cli::Console {
        stdin: &mut stdin,
        stdout: &mut stdout,
        stderr: &mut stderr,
        terminal,
    };
    ExitCode::from(cli::run(&args, &mut console))
}
