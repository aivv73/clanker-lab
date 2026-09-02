mod cli;
mod report;
mod storage;

use std::env;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use clap::{Parser, error::ErrorKind};

use crate::cli::{Cli, Command};
use crate::report::{AppError, Format};

#[derive(Clone, Copy)]
enum FailureExit {
    Runtime,
    InvalidCli,
}

impl FailureExit {
    const fn code(self) -> u8 {
        match self {
            Self::Runtime => 1,
            Self::InvalidCli => 2,
        }
    }
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            return match error.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                    if error.print().is_ok() {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(1)
                    }
                }
                _ => {
                    let format = format_from_arguments()
                        .unwrap_or_else(|| Format::for_terminal(io::stdout().is_terminal()));
                    write_error(
                        format,
                        &AppError::invalid_cli(error.to_string()),
                        FailureExit::InvalidCli,
                    )
                }
            };
        }
    };

    let format = cli
        .format
        .unwrap_or_else(|| Format::for_terminal(io::stdout().is_terminal()));

    match run(cli.command.unwrap_or(Command::Count)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => write_error(format, &error, FailureExit::Runtime),
    }
}

fn run(command: Command) -> Result<(), AppError> {
    match command {
        Command::Count => {
            let database_path = storage::database_path().map_err(AppError::storage)?;
            let database = storage::Database::open(&database_path).map_err(AppError::storage)?;

            if database
                .has_published_snapshot()
                .map_err(AppError::storage)?
            {
                return Err(AppError::UnsupportedSnapshot);
            }

            Err(AppError::NoSnapshot)
        }
    }
}

fn write_error(format: Format, error: &AppError, failure: FailureExit) -> ExitCode {
    let rendered = match format.render_error(error) {
        Ok(rendered) => rendered,
        Err(render_error) => {
            eprintln!("cannot render the error report: {render_error}");
            return ExitCode::from(1);
        }
    };
    let stdout = io::stdout();
    if writeln!(stdout.lock(), "{rendered}").is_err() {
        return ExitCode::from(1);
    }

    ExitCode::from(failure.code())
}

fn format_from_arguments() -> Option<Format> {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    for (index, argument) in arguments.iter().enumerate() {
        let argument = argument.to_string_lossy();
        if let Some(value) = argument.strip_prefix("--format=") {
            return Format::parse(value);
        }
        if argument == "--format" {
            return arguments
                .get(index + 1)
                .and_then(|value| Format::parse(&value.to_string_lossy()));
        }
    }
    None
}
