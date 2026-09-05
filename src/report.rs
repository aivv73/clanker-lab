pub(crate) mod providers;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fmt;

use clap::ValueEnum;
use serde::Serialize;

use crate::package::{InstallReason, PackageOrigin, SnapshotPackage};

#[derive(Clone, Copy, Debug, Serialize)]
enum ReportSchemaVersion {
    #[serde(rename = "1.3.0")]
    V1_3_0,
}

impl ReportSchemaVersion {
    const CURRENT: Self = Self::V1_3_0;

    const fn as_str(self) -> &'static str {
        match self {
            Self::V1_3_0 => "1.3.0",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
enum ToonSpecificationVersion {
    #[serde(rename = "3.0")]
    V3_0,
}

impl ToonSpecificationVersion {
    const CURRENT: Self = Self::V3_0;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum Format {
    Terminal,
    Toon,
    Json,
    Markdown,
}

impl Format {
    pub fn for_terminal(is_terminal: bool) -> Self {
        if is_terminal {
            Self::Terminal
        } else {
            Self::Toon
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "terminal" => Some(Self::Terminal),
            "toon" => Some(Self::Toon),
            "json" => Some(Self::Json),
            "markdown" => Some(Self::Markdown),
            _ => None,
        }
    }

    pub fn render_error(self, error: &AppError) -> Result<String, RenderError> {
        let report = ErrorReport::from(error);
        match self {
            Self::Terminal => Ok(report.render_terminal()),
            Self::Toon => toon_format::encode_default(&ToonErrorReport::from(report))
                .map_err(RenderError::Toon),
            Self::Json => serde_json::to_string_pretty(&report).map_err(RenderError::Json),
            Self::Markdown => Ok(report.render_markdown()),
        }
    }

    pub fn render_count(self, report: &CountReport) -> Result<String, RenderError> {
        match self {
            Self::Terminal => Ok(report.render_terminal()),
            Self::Toon => toon_format::encode_default(&ToonCountReport::from(report))
                .map_err(RenderError::Toon),
            Self::Json => serde_json::to_string_pretty(report).map_err(RenderError::Json),
            Self::Markdown => Ok(report.render_markdown()),
        }
    }
}

#[derive(Debug)]
pub enum AppError {
    NoSnapshot,
    Interrupted,
    Inventory(String),
    Storage(String),
    Render(String),
    InvalidCli(String),
}

impl AppError {
    pub fn storage(error: impl fmt::Display) -> Self {
        Self::Storage(error.to_string())
    }

    pub fn inventory(error: impl fmt::Display) -> Self {
        Self::Inventory(error.to_string())
    }

    pub fn render(error: impl fmt::Display) -> Self {
        Self::Render(error.to_string())
    }

    pub fn invalid_cli(message: impl Into<String>) -> Self {
        Self::InvalidCli(message.into().trim().to_owned())
    }

    fn code(&self) -> ErrorCode {
        match self {
            Self::NoSnapshot => ErrorCode::NoSnapshot,
            Self::Interrupted => ErrorCode::Interrupted,
            Self::Inventory(_) => ErrorCode::Inventory,
            Self::Storage(_) => ErrorCode::Storage,
            Self::Render(_) => ErrorCode::Render,
            Self::InvalidCli(_) => ErrorCode::InvalidCli,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::NoSnapshot => "No inventory snapshot is available.",
            Self::Interrupted => "Inventory refresh was interrupted.",
            Self::Inventory(message)
            | Self::Storage(message)
            | Self::Render(message)
            | Self::InvalidCli(message) => message,
        }
    }

    fn next_command(&self) -> Option<&'static str> {
        match self {
            Self::NoSnapshot => Some("paclang refresh --metadata-only"),
            Self::Interrupted
            | Self::Inventory(_)
            | Self::Storage(_)
            | Self::Render(_)
            | Self::InvalidCli(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ErrorCode {
    NoSnapshot,
    Interrupted,
    Inventory,
    Storage,
    Render,
    InvalidCli,
}

impl ErrorCode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NoSnapshot => "no_snapshot",
            Self::Interrupted => "interrupted",
            Self::Inventory => "inventory",
            Self::Storage => "storage",
            Self::Render => "render",
            Self::InvalidCli => "invalid_cli",
        }
    }
}

/// How the published snapshot used by a report relates to the live inventory.
///
/// Distinct from stale analysis (a cached source-language profile that no
/// longer matches its package); this describes the snapshot as a whole.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SnapshotStaleness {
    /// The snapshot fingerprint matches the live `pacman -Q` fingerprint.
    Current,
    /// The snapshot is coherent but no longer matches live inventory.
    Stale,
    /// No fingerprint was recorded (snapshots published before migration 3);
    /// re-evaluate after the next refresh.
    Unknown,
}

impl SnapshotStaleness {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Stale => "stale",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReportKind {
    Count,
    Providers,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InstallReasonSelection {
    All,
    Explicit,
    Dependency,
}

impl InstallReasonSelection {
    const fn includes(self, reason: InstallReason) -> bool {
        matches!(
            (self, reason),
            (Self::All, _)
                | (Self::Explicit, InstallReason::Explicit)
                | (Self::Dependency, InstallReason::Dependency)
        )
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Explicit => "explicit",
            Self::Dependency => "dependency",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct PopulationSelection {
    install_reason: InstallReasonSelection,
    origins: BTreeSet<PackageOrigin>,
}

impl PopulationSelection {
    pub(crate) fn new(
        install_reason: InstallReasonSelection,
        origins: impl IntoIterator<Item = PackageOrigin>,
    ) -> Self {
        Self {
            install_reason,
            origins: origins.into_iter().collect(),
        }
    }

    pub(crate) fn includes(&self, package: &SnapshotPackage) -> bool {
        self.install_reason.includes(package.install_reason)
            && (self.origins.is_empty() || self.origins.contains(&package.origin))
    }

    fn render(&self) -> String {
        let origins = if self.origins.is_empty() {
            "any".to_owned()
        } else {
            let origins = self
                .origins
                .iter()
                .map(PackageOrigin::as_str)
                .collect::<Vec<_>>();
            format!("{origins:?}")
        };
        format!(
            "install reason = {}; origins = {origins}",
            self.install_reason.as_str()
        )
    }
}

#[derive(Debug, Serialize)]
pub struct CountReport {
    schema_version: ReportSchemaVersion,
    report: ReportKind,
    snapshot_staleness: SnapshotStaleness,
    selection: PopulationSelection,
    installed_total: u64,
    selected_population: u64,
    coverage: Coverage,
}

impl CountReport {
    pub fn from_snapshot(
        packages: &[SnapshotPackage],
        snapshot_staleness: SnapshotStaleness,
        selection: PopulationSelection,
    ) -> Self {
        let installed_total = packages.len() as u64;
        let selected_population = packages
            .iter()
            .filter(|package| selection.includes(package))
            .count() as u64;
        Self {
            schema_version: ReportSchemaVersion::CURRENT,
            report: ReportKind::Count,
            snapshot_staleness,
            selection,
            installed_total,
            selected_population,
            coverage: Coverage::missing_only(selected_population),
        }
    }

    /// Exact analyzed share in percent. An empty selection is vacuously
    /// complete so `--min-coverage` does not fail explicit zero reports.
    pub fn analyzed_coverage_percent(&self) -> f64 {
        self.coverage.analyzed_percent()
    }

    pub fn meets_coverage(&self, minimum: Option<f64>) -> bool {
        match minimum {
            Some(minimum) => self.analyzed_coverage_percent() >= minimum,
            None => true,
        }
    }

    fn render_terminal(&self) -> String {
        let mut rendered = format!(
            "Snapshot: {}\nSelection: {}\nInstalled packages: {}\nSelected population: {}\nCoverage denominator: {}",
            self.snapshot_staleness.as_str(),
            self.selection.render(),
            self.installed_total,
            self.selected_population,
            self.coverage.denominator,
        );
        let tenths = self.coverage.percentage_tenths();
        for (row, share) in self.coverage.rows.iter().zip(tenths) {
            rendered.push_str(&format!(
                "\n{}: {} ({})",
                row.state.human_label(),
                row.count,
                format_tenths(share)
            ));
        }
        rendered
    }

    fn render_markdown(&self) -> String {
        let mut rendered = format!(
            "# Paclang count\n\n- Schema version: `{}`\n- Snapshot: {}\n- Selection: {}\n- Installed packages: {}\n- Selected population: {}\n- Coverage denominator: {}",
            self.schema_version.as_str(),
            self.snapshot_staleness.as_str(),
            self.selection.render(),
            self.installed_total,
            self.selected_population,
            self.coverage.denominator,
        );
        let tenths = self.coverage.percentage_tenths();
        for (row, share) in self.coverage.rows.iter().zip(tenths) {
            rendered.push_str(&format!(
                "\n- {}: {} ({})",
                row.state.human_label(),
                row.count,
                format_tenths(share)
            ));
        }
        rendered
    }
}

#[derive(Debug, Serialize)]
struct Coverage {
    denominator: u64,
    analyzed: u64,
    missing_source_language_analysis: u64,
    /// Per-state breakdown sorted by count descending and state name
    /// ascending. The counts always sum to `denominator`.
    rows: Vec<CoverageRow>,
}

/// Canonical coverage-state names shared by every output format.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
enum CoverageState {
    Analyzed,
    MissingSourceLanguageAnalysis,
}

impl CoverageState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Analyzed => "analyzed",
            Self::MissingSourceLanguageAnalysis => "missing_source_language_analysis",
        }
    }

    const fn human_label(self) -> &'static str {
        match self {
            Self::Analyzed => "Analyzed",
            Self::MissingSourceLanguageAnalysis => "Missing source-language analysis",
        }
    }
}

#[derive(Debug, Serialize)]
struct CoverageRow {
    state: CoverageState,
    /// Integer numerator; the denominator is [`Coverage::denominator`].
    count: u64,
}

impl Coverage {
    fn missing_only(denominator: u64) -> Self {
        let mut rows = vec![
            CoverageRow {
                state: CoverageState::Analyzed,
                count: 0,
            },
            CoverageRow {
                state: CoverageState::MissingSourceLanguageAnalysis,
                count: denominator,
            },
        ];
        // Rows sort by metric descending and canonical name ascending so
        // repeated reports are stable and diffable.
        rows.sort_by(|left, right| {
            right
                .count
                .cmp(&left.count)
                .then_with(|| left.state.as_str().cmp(right.state.as_str()))
        });
        debug_assert_eq!(rows.iter().map(|row| row.count).sum::<u64>(), denominator);
        Self {
            denominator,
            analyzed: 0,
            missing_source_language_analysis: denominator,
            rows,
        }
    }

    fn analyzed_percent(&self) -> f64 {
        if self.denominator == 0 {
            100.0
        } else {
            self.analyzed as f64 * 100.0 / self.denominator as f64
        }
    }

    /// Human percentages in tenths of a percent for each row, deterministically
    /// rounded with the largest-remainder method so they total exactly 100.0%
    /// (or 0.0% for an empty selection). Row order is preserved.
    fn percentage_tenths(&self) -> Vec<u64> {
        percentage_tenths(&self.rows.iter().map(|row| row.count).collect::<Vec<_>>())
    }
}

/// Largest-remainder rounding of `counts` to tenths of a percent. The result
/// sums to exactly 1000 tenths when the total is nonzero and to 0 otherwise.
/// Ties break toward the earlier row, so callers must pre-sort rows for
/// deterministic output.
fn percentage_tenths(counts: &[u64]) -> Vec<u64> {
    let total = counts.iter().sum::<u64>();
    if total == 0 {
        return vec![0; counts.len()];
    }
    let total = u128::from(total);
    let mut bases = Vec::with_capacity(counts.len());
    let mut remainders = Vec::with_capacity(counts.len());
    for count in counts {
        let scaled = u128::from(*count) * 1000;
        bases.push(scaled / total);
        remainders.push(scaled % total);
    }
    let mut tenths = bases
        .iter()
        .map(|base| u64::try_from(*base).unwrap_or(u64::MAX))
        .collect::<Vec<_>>();
    let mut leftover = 1000_u64.saturating_sub(tenths.iter().sum::<u64>());
    let mut order = (0..counts.len()).collect::<Vec<_>>();
    // Largest fractional remainder first; row order breaks ties.
    order.sort_by(|left, right| {
        remainders[*right]
            .cmp(&remainders[*left])
            .then_with(|| left.cmp(right))
    });
    for index in order {
        if leftover == 0 {
            break;
        }
        tenths[index] = tenths[index].saturating_add(1);
        leftover -= 1;
    }
    tenths
}

fn format_tenths(tenths: u64) -> String {
    format!("{}.{}%", tenths / 10, tenths % 10)
}

#[derive(Debug, Serialize)]
struct ToonCountReport<'a> {
    schema_version: ReportSchemaVersion,
    toon_specification_version: ToonSpecificationVersion,
    report: ReportKind,
    snapshot_staleness: SnapshotStaleness,
    selection: &'a PopulationSelection,
    installed_total: u64,
    selected_population: u64,
    coverage: &'a Coverage,
}

impl<'a> From<&'a CountReport> for ToonCountReport<'a> {
    fn from(report: &'a CountReport) -> Self {
        Self {
            schema_version: report.schema_version,
            toon_specification_version: ToonSpecificationVersion::CURRENT,
            report: report.report,
            snapshot_staleness: report.snapshot_staleness,
            selection: &report.selection,
            installed_total: report.installed_total,
            selected_population: report.selected_population,
            coverage: &report.coverage,
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorReport<'a> {
    schema_version: ReportSchemaVersion,
    error: ErrorBody<'a>,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    code: ErrorCode,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_command: Option<&'static str>,
}

impl<'a> From<&'a AppError> for ErrorReport<'a> {
    fn from(error: &'a AppError) -> Self {
        Self {
            schema_version: ReportSchemaVersion::CURRENT,
            error: ErrorBody {
                code: error.code(),
                message: error.message(),
                next_command: error.next_command(),
            },
        }
    }
}

impl ErrorReport<'_> {
    fn render_terminal(&self) -> String {
        let mut rendered = format!(
            "Error: {}\nCode: {}",
            self.error.message,
            self.error.code.as_str()
        );
        if let Some(command) = self.error.next_command {
            rendered.push_str(&format!("\nNext: {command}"));
        }
        rendered
    }

    fn render_markdown(&self) -> String {
        let mut rendered = format!(
            "# Paclang error\n\n- Schema version: `{}`\n- Code: `{}`\n- Message: {}",
            self.schema_version.as_str(),
            self.error.code.as_str(),
            self.error.message
        );
        if let Some(command) = self.error.next_command {
            rendered.push_str(&format!("\n- Next command: `{command}`"));
        }
        rendered
    }
}

#[derive(Debug, Serialize)]
struct ToonErrorReport<'a> {
    schema_version: ReportSchemaVersion,
    toon_specification_version: ToonSpecificationVersion,
    error: ErrorBody<'a>,
}

impl<'a> From<ErrorReport<'a>> for ToonErrorReport<'a> {
    fn from(report: ErrorReport<'a>) -> Self {
        Self {
            schema_version: report.schema_version,
            toon_specification_version: ToonSpecificationVersion::CURRENT,
            error: report.error,
        }
    }
}

#[derive(Debug)]
pub enum RenderError {
    Json(serde_json::Error),
    Toon(toon_format::ToonError),
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(formatter, "JSON serializer error: {error}"),
            Self::Toon(error) => write!(formatter, "TOON serializer error: {error}"),
        }
    }
}
