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

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Report the selected installed-package count and analysis coverage.
    Count,

    /// Inspect stored source-provider outcomes for the selected package versions.
    Providers,

    /// Refresh package metadata and source-language analysis.
    Refresh {
        /// Publish installed-package metadata without analyzing source trees.
        #[arg(
            long,
            required_unless_present_any = ["recipes_only", "sources_only"],
            conflicts_with_all = ["recipes_only", "sources_only"]
        )]
        metadata_only: bool,

        /// Fetch source recipes and record evidence without downloading source trees.
        #[arg(long, conflicts_with = "sources_only")]
        recipes_only: bool,

        /// Fetch recipes and verify/materialize sources without language analysis.
        #[arg(long)]
        sources_only: bool,

        /// Exit 1 after publishing if any selected source is incomplete.
        #[arg(long, requires = "sources_only")]
        require_complete: bool,

        #[command(flatten)]
        limits: crate::acquisition::Limits,

        /// Arch-compatible GitLab API endpoint used for recipe metadata.
        #[arg(
            long,
            conflicts_with = "metadata_only",
            default_value = "https://gitlab.archlinux.org/api/v4"
        )]
        arch_api_url: crate::arch_provider::RecipeEndpoint,

        /// Try CachyOS recipes and graded Arch rebuild fallbacks for unresolved packages.
        #[arg(long, conflicts_with = "metadata_only")]
        cachyos: bool,

        /// GitHub-compatible API for the CachyOS recipe repository.
        #[arg(
            long,
            requires = "cachyos",
            default_value = "https://api.github.com/repos/CachyOS/CachyOS-PKGBUILDS",
            value_parser = parse_cachyos_api_url
        )]
        cachyos_api_url: Box<crate::arch_provider::RecipeEndpoint>,

        /// Explicit HTTP(S) proxy for recipe, archive, and Git requests.
        /// Proxy environment variables are never read implicitly. Boxed to
        /// keep the CLI enum small; see `parse_proxy_url`.
        #[arg(long, conflicts_with = "metadata_only", value_name = "URL", value_parser = parse_proxy_url)]
        proxy_url: Option<Box<crate::proxy::ProxyUrl>>,
    },
}

fn parse_proxy_url(value: &str) -> Result<Box<crate::proxy::ProxyUrl>, String> {
    value
        .parse()
        .map(Box::new)
        .map_err(|reason: &'static str| reason.to_owned())
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

fn parse_cachyos_api_url(value: &str) -> Result<Box<crate::arch_provider::RecipeEndpoint>, String> {
    value
        .parse()
        .map(Box::new)
        .map_err(|reason: &'static str| reason.to_owned())
}
