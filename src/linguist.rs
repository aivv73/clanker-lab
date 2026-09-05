//! Native `github-linguist` adapter.
//!
//! Paclang never downloads or installs the adapter. A configured executable is
//! invoked as `<executable> --json` with its working directory set to the
//! materialized source tree (plus the target subdirectory). Its JSON breakdown
//! becomes one stored source-language profile per analysis target.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::interrupt;

const MAX_JSON_BYTES: u64 = 4_096_000;
const MAX_LANGUAGES: usize = 256;
const MAX_LANGUAGE_NAME_BYTES: usize = 64;

#[derive(Clone, Debug, clap::Args)]
pub(crate) struct LinguistConfig {
    /// Native `github-linguist` executable used for language analysis.
    /// Absent means sources-only refresh acquires trees without analyzing
    /// them; coverage stays missing.
    #[arg(long, env = "PACLANG_LINGUIST_EXECUTABLE", value_name = "PATH")]
    pub linguist_executable: Option<PathBuf>,

    /// Per-target wall-clock budget for the Linguist invocation.
    #[arg(long, env = "PACLANG_LINGUIST_TIMEOUT_SECONDS", default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=86_400))]
    pub linguist_timeout_seconds: u64,
}

/// One detected language share. Bytes come from Linguist's `size` field (or a
/// plain integer value in fixture output); rows sort by bytes descending and
/// name ascending so reports are stable and diffable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LanguageBytes {
    pub language: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LanguageProfile {
    pub languages: Vec<LanguageBytes>,
    pub total_bytes: u64,
    pub primary_language: Option<String>,
}

impl LanguageProfile {
    pub(crate) fn from_bytes(mut languages: Vec<LanguageBytes>) -> Self {
        languages.sort_by(|left, right| {
            right
                .bytes
                .cmp(&left.bytes)
                .then_with(|| left.language.cmp(&right.language))
        });
        let total_bytes = languages.iter().map(|row| row.bytes).sum();
        let primary_language = languages.first().map(|row| row.language.clone());
        Self {
            languages,
            total_bytes,
            primary_language,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LanguageRecord {
    pub state: String,
    pub reason: Option<String>,
    pub total_bytes: u64,
    pub primary_language: Option<String>,
    pub languages_json: String,
    pub adapter: String,
    pub source: ProfileSource,
    pub fidelity: RevisionFidelity,
}

/// Where a stored source-language profile came from. Profiles from
/// different sources are never mixed as equal evidence: only `Linguist`
/// profiles describe the packaged revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ProfileSource {
    Linguist,
    GithubApi,
}

impl ProfileSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Linguist => "linguist",
            Self::GithubApi => "github-api",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "linguist" => Ok(Self::Linguist),
            "github-api" => Ok(Self::GithubApi),
            _ => Err("profile source is neither linguist nor github-api"),
        }
    }
}

/// How closely a stored profile tracks the packaged source revision.
/// `ExactPackagedSource` profiles were analyzed from the packaged revision
/// itself; `CurrentUpstream` profiles approximate it from upstream HEAD.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RevisionFidelity {
    ExactPackagedSource,
    CurrentUpstream,
}

impl RevisionFidelity {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ExactPackagedSource => "exact-packaged-source",
            Self::CurrentUpstream => "current-upstream",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "exact-packaged-source" => Ok(Self::ExactPackagedSource),
            "current-upstream" => Ok(Self::CurrentUpstream),
            _ => Err("revision fidelity is neither exact-packaged-source nor current-upstream"),
        }
    }
}

/// Package-level analysis state derived from stored profiles.
///
/// `Current` means the installed version's own target has an exact analyzed
/// profile. `Approximate` means it has only a current-upstream approximation:
/// different evidence, counted and disclosed separately, never mixed with
/// exact coverage. `Stale` means only an older version's exact target has
/// one: the cached profile does not match the installed package version, but
/// reports include it by default and disclose it separately. `Missing` covers
/// everything else (unresolved source, failed analysis, or a definitive
/// `no_language` result on the current target, which suppresses older exact
/// profiles so a stale language never overrides a known-empty current
/// source).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnalysisState {
    Current,
    Approximate,
    Stale,
    Missing,
}

pub(super) enum Outcome {
    Analyzed(LanguageProfile),
    NoLanguage,
    Failed(String),
    Limited(String),
}

/// Run the configured adapter against one materialized repository. The caller
/// owns timeouts, cancellation, and persistence; this only maps process
/// results to analysis states.
pub(crate) fn analyze_target(
    repository: &Path,
    subdirectory: Option<&str>,
    executable: &Path,
    timeout: Duration,
) -> Outcome {
    let directory = match subdirectory {
        None => repository.to_path_buf(),
        Some(subdirectory) => match crate::acquisition::safe_path(subdirectory) {
            Ok(relative) => repository.join(relative),
            Err(_) => return Outcome::Failed("invalid source subdirectory".to_owned()),
        },
    };
    if interrupt::is_requested() {
        return Outcome::Failed("inventory refresh was interrupted".to_owned());
    }
    let mut child = match Command::new(executable)
        .arg("--json")
        .current_dir(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Outcome::Failed("cannot start the Linguist adapter".to_owned()),
    };
    let _guard = interrupt::ActiveProcessGroup::register(child.id());
    let started = Instant::now();
    loop {
        if interrupt::is_requested() {
            let _ = child.kill();
            let _ = child.wait();
            return Outcome::Failed("inventory refresh was interrupted".to_owned());
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Outcome::Limited("language analysis deadline exceeded".to_owned());
        }
        match child.try_wait() {
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .unwrap_or_else(|_| std::process::Output {
                        status,
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    });
                drop(_guard);
                return interpret_output(status.success(), &output.stdout);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Outcome::Failed("language analysis did not complete".to_owned());
            }
        }
    }
}

fn interpret_output(success: bool, stdout: &[u8]) -> Outcome {
    if !success {
        return Outcome::Failed("Linguist adapter reported a failure".to_owned());
    }
    if stdout.len() as u64 > MAX_JSON_BYTES {
        return Outcome::Failed("Linguist output exceeds the size limit".to_owned());
    }
    let text = match std::str::from_utf8(stdout) {
        Ok(text) => text,
        Err(_) => return Outcome::Failed("Linguist output is not UTF-8".to_owned()),
    };
    match parse_breakdown(text) {
        Ok(languages) if languages.is_empty() => Outcome::NoLanguage,
        Ok(languages) => Outcome::Analyzed(LanguageProfile::from_bytes(languages)),
        Err(reason) => Outcome::Failed(reason),
    }
}

/// Accept both real `github-linguist --json` output (`{"Rust": {"size": 1}}`)
/// and plain integer shares (`{"Rust": 1}`) used by fixtures.
pub(crate) fn parse_breakdown(text: &str) -> Result<Vec<LanguageBytes>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "Linguist output is not JSON".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "Linguist output is not a language map".to_owned())?;
    if object.len() > MAX_LANGUAGES {
        return Err("Linguist reported too many languages".to_owned());
    }
    let mut languages = Vec::with_capacity(object.len());
    for (name, entry) in object {
        validate_language_name(name)?;
        let bytes = match entry {
            serde_json::Value::Number(number) => number
                .as_u64()
                .ok_or_else(|| format!("language {name:?} has no byte count"))?,
            serde_json::Value::Object(fields) => fields
                .get("size")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| format!("language {name:?} has no byte count"))?,
            _ => return Err(format!("language {name:?} has no byte count")),
        };
        languages.push(LanguageBytes {
            language: name.clone(),
            bytes,
        });
    }
    // A zero-byte profile carries no detected language.
    languages.retain(|row| row.bytes > 0);
    Ok(languages)
}

fn validate_language_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_LANGUAGE_NAME_BYTES {
        return Err(format!("language {name:?} is not a valid name"));
    }
    if name.chars().any(char::is_control) {
        return Err(format!("language {name:?} is not a valid name"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_object_and_integer_breakdowns() {
        let object = parse_breakdown(r#"{"Rust": {"size": 10}, "C": {"size": 4}}"#).unwrap();
        assert_eq!(
            LanguageProfile::from_bytes(object)
                .primary_language
                .as_deref(),
            Some("Rust")
        );
        let integers = parse_breakdown(r#"{"Rust": 10, "C": 0}"#).unwrap();
        assert_eq!(integers.len(), 1);
        assert_eq!(integers[0].language, "Rust");
    }

    #[test]
    fn rejects_malformed_breakdowns() {
        for text in [
            "[]",
            r#"{"Rust": "lots"}"#,
            r#"{"Rust": -1}"#,
            r#"{"Rust": {}}"#,
            "{not json",
        ] {
            assert!(parse_breakdown(text).is_err(), "{text}");
        }
        assert!(parse_breakdown(&format!("{{\"{}\": 1}}", "x".repeat(65))).is_err());
    }

    #[test]
    fn profiles_sort_and_total() {
        let profile = LanguageProfile::from_bytes(vec![
            LanguageBytes {
                language: "C".to_owned(),
                bytes: 5,
            },
            LanguageBytes {
                language: "Rust".to_owned(),
                bytes: 5,
            },
        ]);
        assert_eq!(profile.total_bytes, 10);
        // Ties break toward the earlier language name.
        assert_eq!(profile.primary_language.as_deref(), Some("C"));
    }
}
