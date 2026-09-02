#[cfg(test)]
mod tests;

use std::fmt;

use clap::ValueEnum;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize)]
enum ReportSchemaVersion {
    #[serde(rename = "1.0.0")]
    V1_0_0,
}

impl ReportSchemaVersion {
    const CURRENT: Self = Self::V1_0_0;

    const fn as_str(self) -> &'static str {
        match self {
            Self::V1_0_0 => "1.0.0",
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
}

#[derive(Debug)]
pub enum AppError {
    NoSnapshot,
    Storage(String),
    UnsupportedSnapshot,
    InvalidCli(String),
}

impl AppError {
    pub fn storage(error: impl fmt::Display) -> Self {
        Self::Storage(error.to_string())
    }

    pub fn invalid_cli(message: impl Into<String>) -> Self {
        Self::InvalidCli(message.into().trim().to_owned())
    }

    fn code(&self) -> ErrorCode {
        match self {
            Self::NoSnapshot => ErrorCode::NoSnapshot,
            Self::Storage(_) => ErrorCode::Storage,
            Self::UnsupportedSnapshot => ErrorCode::UnsupportedSnapshot,
            Self::InvalidCli(_) => ErrorCode::InvalidCli,
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::NoSnapshot => "No inventory snapshot is available.",
            Self::Storage(message) | Self::InvalidCli(message) => message,
            Self::UnsupportedSnapshot => {
                "The published inventory snapshot is not supported by this build."
            }
        }
    }

    fn next_command(&self) -> Option<&'static str> {
        match self {
            Self::NoSnapshot => Some("paclang refresh --metadata-only"),
            Self::Storage(_) | Self::UnsupportedSnapshot | Self::InvalidCli(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ErrorCode {
    NoSnapshot,
    Storage,
    UnsupportedSnapshot,
    InvalidCli,
}

impl ErrorCode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NoSnapshot => "no_snapshot",
            Self::Storage => "storage",
            Self::UnsupportedSnapshot => "unsupported_snapshot",
            Self::InvalidCli => "invalid_cli",
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
