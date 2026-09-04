use clap::{Parser, Subcommand};

use crate::package::PackageOrigin;
use crate::report::{Format, InstallReasonSelection, PopulationSelection};

#[derive(Debug, Parser)]
#[command(name = "paclang", version, about)]
pub struct Cli {
    /// Select the report format instead of using terminal detection.
    #[arg(long, value_enum, global = true)]
    pub format: Option<Format>,

    /// Select explicitly installed packages.
    #[arg(long, global = true, conflicts_with = "dependency")]
    explicit: bool,

    /// Select packages installed as dependencies.
    #[arg(long, global = true, conflicts_with = "explicit")]
    dependency: bool,

    /// Select packages from this origin. Repeat to select several origins.
    #[arg(long, global = true, value_name = "NAME")]
    origin: Vec<PackageOrigin>,

    /// Exit 1 when analyzed coverage is below this percent. The report facts
    /// are unchanged; only the exit status signals the unmet threshold.
    #[arg(long, global = true, value_name = "PERCENT", value_parser = parse_min_coverage)]
    pub min_coverage: Option<f64>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    pub fn population_selection(&self) -> PopulationSelection {
        let install_reason = match (self.explicit, self.dependency) {
            (true, false) => InstallReasonSelection::Explicit,
            (false, true) => InstallReasonSelection::Dependency,
            (false, false) => InstallReasonSelection::All,
            (true, true) => unreachable!("clap rejects conflicting install-reason filters"),
        };
        PopulationSelection::new(install_reason, self.origin.iter().cloned())
    }
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub enum Command {
    /// Report the selected installed-package count and analysis coverage.
    Count,

    /// Refresh package metadata and source-language analysis.
    Refresh {
        /// Publish installed-package metadata without analyzing source trees.
        #[arg(long, required = true)]
        metadata_only: bool,
    },
}

fn parse_min_coverage(value: &str) -> Result<f64, String> {
    let percent = value
        .parse::<f64>()
        .map_err(|_| format!("{value:?} is not a number between 0 and 100"))?;
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err(format!("{value:?} is not a number between 0 and 100"));
    }
    Ok(percent)
}
