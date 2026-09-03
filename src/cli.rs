use clap::{Parser, Subcommand};

use crate::report::Format;

#[derive(Debug, Parser)]
#[command(name = "paclang", version, about)]
pub struct Cli {
    /// Select the report format instead of using terminal detection.
    #[arg(long, value_enum, global = true)]
    pub format: Option<Format>,

    #[command(subcommand)]
    pub command: Option<Command>,
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
