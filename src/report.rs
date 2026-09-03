#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fmt;

use clap::ValueEnum;
use serde::Serialize;

use crate::package::{InstallReason, PackageOrigin, SnapshotPackage};

#[derive(Clone, Copy, Debug, Serialize)]
enum ReportSchemaVersion {
    #[serde(rename = "1.1.0")]
    V1_1_0,
}

impl ReportSchemaVersion {
    const CURRENT: Self = Self::V1_1_0;

    const fn as_str(self) -> &'static str {
        match self {
            Self::V1_1_0 => "1.1.0",
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
            Self::Inventory(_) => ErrorCode::Inventory,
            Self::Storage(_) => ErrorCode::Storage,
            Self::Render(_) => ErrorCode::Render,
            Self::InvalidCli(_) => ErrorCode::InvalidCli,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::NoSnapshot => "No inventory snapshot is available.",
            Self::Inventory(message)
            | Self::Storage(message)
            | Self::Render(message)
            | Self::InvalidCli(message) => message,
        }
    }

    fn next_command(&self) -> Option<&'static str> {
        match self {
            Self::NoSnapshot => Some("paclang refresh --metadata-only"),
            Self::Inventory(_) | Self::Storage(_) | Self::Render(_) | Self::InvalidCli(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ErrorCode {
    NoSnapshot,
    Inventory,
    Storage,
    Render,
    InvalidCli,
}

impl ErrorCode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NoSnapshot => "no_snapshot",
            Self::Inventory => "inventory",
            Self::Storage => "storage",
            Self::Render => "render",
            Self::InvalidCli => "invalid_cli",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReportKind {
    Count,
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

    fn includes(&self, package: &SnapshotPackage) -> bool {
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
    selection: PopulationSelection,
    installed_total: u64,
    selected_population: u64,
    coverage: Coverage,
}

impl CountReport {
    pub fn from_snapshot(packages: &[SnapshotPackage], selection: PopulationSelection) -> Self {
        let installed_total = packages.len() as u64;
        let selected_population = packages
            .iter()
            .filter(|package| selection.includes(package))
            .count() as u64;
        Self {
            schema_version: ReportSchemaVersion::CURRENT,
            report: ReportKind::Count,
            selection,
            installed_total,
            selected_population,
            coverage: Coverage {
                denominator: selected_population,
                analyzed: 0,
                missing_source_language_analysis: selected_population,
            },
        }
    }

    fn render_terminal(&self) -> String {
        format!(
            "Selection: {}\nInstalled packages: {}\nSelected population: {}\nCoverage denominator: {}\nAnalyzed: {}\nMissing source-language analysis: {}",
            self.selection.render(),
            self.installed_total,
            self.selected_population,
            self.coverage.denominator,
            self.coverage.analyzed,
            self.coverage.missing_source_language_analysis
        )
    }

    fn render_markdown(&self) -> String {
        format!(
            "# Paclang count\n\n- Schema version: `{}`\n- Selection: {}\n- Installed packages: {}\n- Selected population: {}\n- Coverage denominator: {}\n- Analyzed: {}\n- Missing source-language analysis: {}",
            self.schema_version.as_str(),
            self.selection.render(),
            self.installed_total,
            self.selected_population,
            self.coverage.denominator,
            self.coverage.analyzed,
            self.coverage.missing_source_language_analysis
        )
    }
}

#[derive(Debug, Serialize)]
struct Coverage {
    denominator: u64,
    analyzed: u64,
    missing_source_language_analysis: u64,
}

#[derive(Debug, Serialize)]
struct ToonCountReport<'a> {
    schema_version: ReportSchemaVersion,
    toon_specification_version: ToonSpecificationVersion,
    report: ReportKind,
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
