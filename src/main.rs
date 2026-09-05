mod acquisition;
mod arch_provider;
mod cachyos_provider;
mod cli;
mod expac_parser;
mod interrupt;
mod inventory;
mod package;
mod progress;
mod proxy;
mod report;
mod srcinfo;
mod storage;

use std::env;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use clap::{Parser, error::ErrorKind};

use crate::cli::{Cli, Command};
use crate::report::{AppError, CountReport, Format, SnapshotStaleness};

#[derive(Clone, Copy)]
enum FailureExit {
    Runtime,
    InvalidCli,
    Interrupted,
}

struct RunOutput {
    rendered: String,
    /// False when `--min-coverage` was not met. The rendered report facts are
    /// unchanged; only the exit status signals the unmet threshold.
    meets_coverage: bool,
    /// Holds the snapshot's shared sidecar lock until the response is written,
    /// so retention cannot prune the snapshot being reported. `None` on the
    /// refresh path, which publishes rather than reads. Never read by design;
    /// the field exists to extend the guard's lifetime to the write.
    #[allow(dead_code)]
    snapshot_read_guard: Option<storage::PublishedSnapshot>,
}

impl FailureExit {
    const fn code(self) -> u8 {
        match self {
            Self::Runtime => 1,
            Self::InvalidCli => 2,
            Self::Interrupted => 130,
        }
    }
}

fn main() -> ExitCode {
    let mut worker_args = env::args_os().skip(1);
    if worker_args.next().as_deref() == Some(std::ffi::OsStr::new("--acquisition-worker")) {
        return match (worker_args.next(), worker_args.next()) {
            (Some(path), None) if acquisition::worker(std::path::Path::new(&path)) => {
                ExitCode::SUCCESS
            }
            _ => ExitCode::from(1),
        };
    }

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
    let selection = cli.population_selection();
    if let Err(error) = interrupt::install() {
        return write_error(
            format,
            &AppError::inventory(format!("cannot install the interrupt handler: {error}")),
            FailureExit::Runtime,
        );
    }

    match run(
        cli.command.unwrap_or(Command::Count),
        format,
        selection,
        cli.min_coverage,
    ) {
        Ok(output) => {
            if write_rendered(&output.rendered) != ExitCode::SUCCESS {
                ExitCode::from(1)
            } else if output.meets_coverage {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            let failure = if matches!(error, AppError::Interrupted) {
                FailureExit::Interrupted
            } else {
                FailureExit::Runtime
            };
            write_error(format, &error, failure)
        }
    }
}

fn run(
    command: Command,
    format: Format,
    selection: report::PopulationSelection,
    min_coverage: Option<f64>,
) -> Result<RunOutput, AppError> {
    match command {
        Command::Count => render_snapshot_report(format, selection, min_coverage, false),
        Command::Providers => render_snapshot_report(format, selection, min_coverage, true),
        Command::Refresh {
            metadata_only: _,
            recipes_only,
            sources_only,
            require_complete,
            limits,
            arch_api_url,
            cachyos,
            cachyos_api_url,
            proxy_url,
        } => {
            let database_path = storage::database_path().map_err(AppError::storage)?;
            let mut database =
                storage::Database::open(&database_path).map_err(AppError::storage)?;
            let refresh = database.begin_refresh().map_err(map_storage_error)?;
            let inventory =
                inventory::capture_installed_inventory().map_err(map_inventory_error)?;
            let snapshot_packages = inventory.snapshot_packages();
            let mut source_acquisition = None;
            let staleness = if recipes_only || sources_only {
                let selected = inventory
                    .packages()
                    .iter()
                    .zip(&snapshot_packages)
                    .filter_map(|(installed, snapshot)| {
                        selection.includes(snapshot).then_some(installed)
                    });
                let mut progress =
                    progress::Progress::stderr("Fetching recipes", "Resolved", "Unresolved");
                let result = arch_provider::record_attempts(
                    &refresh,
                    selected,
                    &arch_api_url,
                    &mut progress,
                    sources_only,
                    proxy_url.as_deref(),
                )
                .map_err(map_storage_error);
                progress.finish();
                result?;
                if cachyos {
                    let selected = inventory
                        .packages()
                        .iter()
                        .zip(&snapshot_packages)
                        .filter_map(|(installed, snapshot)| {
                            selection.includes(snapshot).then_some(installed)
                        });
                    let mut progress =
                        progress::Progress::stderr("CachyOS recipes", "Resolved", "Unresolved");
                    let result = cachyos_provider::record_attempts(
                        &refresh,
                        selected,
                        &cachyos_api_url,
                        &arch_api_url,
                        &mut progress,
                        sources_only,
                        proxy_url.as_deref(),
                    )
                    .map_err(map_storage_error);
                    progress.finish();
                    result?;
                }
                if sources_only {
                    let selected = inventory
                        .packages()
                        .iter()
                        .zip(&snapshot_packages)
                        .filter_map(|(installed, snapshot)| {
                            selection.includes(snapshot).then_some(installed)
                        });
                    let mut progress =
                        progress::Progress::stderr("Acquiring sources", "Acquired", "Incomplete");
                    let result = acquisition::record_sources(
                        &refresh,
                        selected,
                        &limits,
                        &mut progress,
                        proxy_url.as_deref(),
                    )
                    .map_err(map_storage_error);
                    progress.finish();
                    source_acquisition = Some(result?);
                }
                if inventory::read_live_fingerprint().map_err(map_inventory_error)?
                    != *inventory.fingerprint()
                {
                    SnapshotStaleness::Stale
                } else {
                    SnapshotStaleness::Current
                }
            } else if inventory.is_stale() {
                SnapshotStaleness::Stale
            } else {
                SnapshotStaleness::Current
            };
            let sources_complete = source_acquisition
                .as_ref()
                .is_none_or(acquisition::AcquisitionSummary::complete);
            let mut source_free = 0;
            for (installed, snapshot) in inventory.packages().iter().zip(&snapshot_packages) {
                if selection.includes(snapshot)
                    && package::provider::source_free(
                        &refresh
                            .provider_attempts(installed.name(), installed.version())
                            .map_err(map_storage_error)?,
                    )
                {
                    source_free += 1;
                }
            }
            let mut report = CountReport::from_snapshot(&snapshot_packages, staleness, selection)
                .with_source_free(source_free);
            report.source_acquisition = source_acquisition;
            let rendered = format.render_count(&report).map_err(AppError::render)?;
            refresh
                .publish(inventory.packages(), inventory.fingerprint())
                .map_err(map_storage_error)?;
            Ok(RunOutput {
                rendered,
                meets_coverage: report.meets_coverage(min_coverage)
                    && (!require_complete || sources_complete),
                snapshot_read_guard: None,
            })
        }
    }
}

fn render_snapshot_report(
    format: Format,
    selection: report::PopulationSelection,
    min_coverage: Option<f64>,
    providers: bool,
) -> Result<RunOutput, AppError> {
    let database_path = storage::database_path().map_err(AppError::storage)?;
    let database = storage::Database::open(&database_path).map_err(AppError::storage)?;
    if providers {
        let snapshot = database
            .published_provider_snapshot()
            .map_err(AppError::storage)?
            .ok_or(AppError::NoSnapshot)?;
        let live = inventory::read_live_fingerprint().map_err(map_inventory_error)?;
        let staleness = match &snapshot.fingerprint {
            Some(value) if *value == live => SnapshotStaleness::Current,
            Some(_) => SnapshotStaleness::Stale,
            None => SnapshotStaleness::Unknown,
        };
        let meets_coverage =
            CountReport::from_provider_snapshot(&snapshot, staleness, selection.clone())
                .meets_coverage(min_coverage);
        let report =
            report::providers::ProviderReport::from_snapshot(&snapshot, staleness, selection);
        let rendered = report.render(format).map_err(AppError::render)?;
        return Ok(RunOutput {
            rendered,
            meets_coverage,
            snapshot_read_guard: Some(snapshot),
        });
    }
    render_count_from_database(format, &database, selection, min_coverage)
}

fn render_count_from_database(
    format: Format,
    database: &storage::Database,
    selection: report::PopulationSelection,
    min_coverage: Option<f64>,
) -> Result<RunOutput, AppError> {
    let snapshot = database
        .published_provider_snapshot()
        .map_err(AppError::storage)?
        .ok_or(AppError::NoSnapshot)?;
    let live_fingerprint = inventory::read_live_fingerprint().map_err(map_inventory_error)?;
    let staleness = match &snapshot.fingerprint {
        Some(fingerprint) if *fingerprint == live_fingerprint => SnapshotStaleness::Current,
        Some(_) => SnapshotStaleness::Stale,
        None => SnapshotStaleness::Unknown,
    };
    let report = CountReport::from_provider_snapshot(&snapshot, staleness, selection);
    let rendered = format.render_count(&report).map_err(AppError::render)?;
    let meets_coverage = report.meets_coverage(min_coverage);
    Ok(RunOutput {
        rendered,
        meets_coverage,
        snapshot_read_guard: Some(snapshot),
    })
}

fn map_inventory_error(error: inventory::InventoryError) -> AppError {
    if error.is_interrupted() {
        AppError::Interrupted
    } else {
        AppError::inventory(error)
    }
}

fn map_storage_error(error: storage::StorageError) -> AppError {
    if error.is_interrupted() {
        AppError::Interrupted
    } else {
        AppError::storage(error)
    }
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
