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
