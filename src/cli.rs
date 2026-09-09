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

    /// Select only this installed package by name. Repeat to select several
    /// packages; every other filter still applies.
    #[arg(long, global = true, value_name = "NAME", value_parser = parse_package_name)]
    package: Vec<String>,

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
            .with_packages(self.package.iter().cloned())
    }
}

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Report the selected installed-package count and analysis coverage.
    Count,

    /// Report summed source-language bytes over unique analysis targets.
    SourceBytes {
        /// Which revision fidelity the totals sum. Exact sums only
        /// `exact-packaged-source` profiles; approximate sums only
        /// `current-upstream` profiles; best-available sums exact where
        /// available and falls back to approximations. Fidelities are never
        /// mixed as equal evidence; the other fidelity stays disclosed apart.
        /// `--min-coverage` still gates on the exact analyzed share.
        #[arg(long, value_enum, default_value_t = crate::report::source_bytes::FidelitySelection::Exact)]
        fidelity: crate::report::source_bytes::FidelitySelection,

        /// Show every language in human output instead of the top 10 plus
        /// `Other`. JSON and TOON always carry every language.
        #[arg(long, conflicts_with = "top")]
        all: bool,

        /// Show the top N languages plus an aggregated `Other` row in human
        /// output, so the row count stays bounded regardless of the byte
        /// distribution. JSON and TOON always carry every language.
        #[arg(long, value_name = "N", conflicts_with = "all", value_parser = parse_top)]
        top: Option<usize>,
    },

    /// List installed packages matching any stored language name (case-insensitive).
    Packages {
        #[arg(value_parser = parse_language)]
        language: String,
        /// Match positive language bytes instead of the primary language.
        #[arg(long, conflicts_with = "min_share")]
        contains: bool,
        /// Match a language share at least this fraction, from 0 to 1.
        #[arg(long, value_parser = parse_share)]
        min_share: Option<f64>,
        #[arg(long, value_enum, default_value = "exact")]
        fidelity: crate::report::packages::Fidelity,

        /// Sort by name, or descending selected-language share or bytes.
        #[arg(long, value_enum, default_value = "name")]
        sort: crate::report::packages::Sort,
    },

    /// Show Rust oxidation of analyzed Rust/C/C++ source bytes.
    Riir {
        #[arg(long, value_enum, default_value = "exact")]
        fidelity: crate::report::packages::Fidelity,
        /// Progress-bar style; choose crabs when emoji occupy two terminal cells.
        #[arg(long, value_enum, default_value = "blocks")]
        bar: crate::report::riir::Bar,
    },

    /// Apply the subjective, versioned clanker language policy to measured source bytes.
    ClankerIndex {
        #[arg(long, value_enum, default_value = "exact")]
        fidelity: crate::report::packages::Fidelity,
        /// Show every programming language in human output. Machine output is complete.
        #[arg(long, conflicts_with = "top")]
        all: bool,
        /// Show the top N programming languages plus a display-only remainder.
        #[arg(long, value_name = "N", conflicts_with = "all", value_parser = parse_top)]
        top: Option<usize>,
    },

    /// Report installed-payload visibility and confirmed prebuilt binary matches.
    Blobs {
        /// Show all packages, including unresolved provenance.
        #[arg(long, conflicts_with = "top")]
        all: bool,
        /// Show the top N packages by confirmed opaque installed bytes.
        #[arg(long, conflicts_with = "all", value_parser = parse_top)]
        top: Option<usize>,
    },

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

        /// Language-analysis policy for sources-only refresh.
        #[arg(long, requires = "sources_only", value_enum, env = "PACLANG_ANALYSIS_MODE", default_value_t = crate::github_languages::AnalysisMode::Exact)]
        analysis_mode: crate::github_languages::AnalysisMode,

        /// GitHub-compatible API root for fast language approximation.
        /// Boxed to keep the CLI enum small; see `parse_github_api_url`.
        #[arg(long, requires = "sources_only", default_value = "https://api.github.com", value_parser = parse_github_api_url)]
        github_api_url: Box<crate::github_languages::GithubApiEndpoint>,

        #[command(flatten)]
        limits: crate::acquisition::Limits,

        #[command(flatten)]
        linguist: crate::linguist::LinguistConfig,

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

fn parse_package_name(value: &str) -> Result<String, String> {
    crate::expac_parser::validate_package_name(value)
        .map(|()| value.to_owned())
        .map_err(|message| format!("{value:?} is not a valid package name: {message}"))
}

fn parse_top(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(top) if top >= 1 => Ok(top),
        _ => Err(format!("{value:?} is not an integer 1 or greater")),
    }
}

fn parse_github_api_url(
    value: &str,
) -> Result<Box<crate::github_languages::GithubApiEndpoint>, String> {
    value
        .parse()
        .map(Box::new)
        .map_err(|reason: &'static str| reason.to_owned())
}

fn parse_cachyos_api_url(value: &str) -> Result<Box<crate::arch_provider::RecipeEndpoint>, String> {
    value
        .parse()
        .map(Box::new)
        .map_err(|reason: &'static str| reason.to_owned())
}

fn parse_share(value: &str) -> Result<f64, String> {
    let share = value
        .parse::<f64>()
        .map_err(|_| "expected a fraction from 0 to 1")?;
    if !share.is_finite() || !(0.0..=1.0).contains(&share) {
        return Err("expected a finite fraction from 0 to 1".into());
    }
    Ok(share)
}

fn parse_language(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err("expected a nonempty language name without control characters".into());
    }
    Ok(value.to_owned())
}
