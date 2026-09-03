mod cli;
mod inventory;
mod report;
mod storage;

use std::env;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use clap::{Parser, error::ErrorKind};

use crate::cli::{Cli, Command};
use crate::report::{AppError, CountReport, Format};

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

    match run(cli.command.unwrap_or(Command::Count), format) {
        Ok(rendered) => write_rendered(&rendered),
        Err(error) => write_error(format, &error, FailureExit::Runtime),
    }
}

fn run(command: Command, format: Format) -> Result<String, AppError> {
    match command {
        Command::Count => render_count(format),
        Command::Refresh { metadata_only: _ } => {
            let packages = inventory::read_installed_packages().map_err(AppError::inventory)?;
            let database_path = storage::database_path().map_err(AppError::storage)?;
            let mut database =
                storage::Database::open(&database_path).map_err(AppError::storage)?;
            database
                .publish_snapshot(&packages)
                .map_err(AppError::storage)?;
            render_count_from_database(format, &database)
        }
    }
}

fn render_count(format: Format) -> Result<String, AppError> {
    let database_path = storage::database_path().map_err(AppError::storage)?;
    let database = storage::Database::open(&database_path).map_err(AppError::storage)?;
    render_count_from_database(format, &database)
}

fn render_count_from_database(
    format: Format,
    database: &storage::Database,
) -> Result<String, AppError> {
    let installed_total = database
        .published_package_count()
        .map_err(AppError::storage)?
        .ok_or(AppError::NoSnapshot)?;
    format
        .render_count(&CountReport::new(installed_total))
        .map_err(AppError::render)
}

fn write_rendered(rendered: &str) -> ExitCode {
    let stdout = io::stdout();
    if writeln!(stdout.lock(), "{rendered}").is_err() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
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
    if write_rendered(&rendered) == ExitCode::SUCCESS {
        ExitCode::from(failure.code())
    } else {
        ExitCode::from(1)
    }
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
