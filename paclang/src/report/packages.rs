use std::collections::BTreeMap;

use serde::Serialize;

use super::{AppError, Format, PopulationSelection, RenderError, SnapshotStaleness};
use crate::linguist::{
    AnalysisState, LanguageBytes, LanguageProfile, ProfileSource, RevisionFidelity,
};
use crate::package::provider::AttemptOutcome;
use crate::storage::{Database, PublishedSnapshot};

#[derive(Clone, Copy, Debug, clap::ValueEnum, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Fidelity {
    Exact,
    BestAvailable,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Sort {
    Name,
    Share,
    Bytes,
}

impl Sort {
    fn compare(self, a: &Row, b: &Row) -> std::cmp::Ordering {
        match self {
            Self::Name => std::cmp::Ordering::Equal,
            // Compare exact ratios without floating-point rounding or u64 overflow.
            Self::Share => (u128::from(b.language_bytes) * u128::from(a.total_bytes))
                .cmp(&(u128::from(a.language_bytes) * u128::from(b.total_bytes))),
            Self::Bytes => b.language_bytes.cmp(&a.language_bytes),
        }
        .then_with(|| a.name.cmp(&b.name))
    }
}

#[derive(Serialize)]
pub(crate) struct PackagesReport {
    schema_version: super::ReportSchemaVersion,
    report: &'static str,
    snapshot_staleness: SnapshotStaleness,
    selection: PopulationSelection,
    language: String,
    mode: &'static str,
    min_share: Option<f64>,
    fidelity: Fidelity,
    sort: Sort,
    selected_population: usize,
    matched_packages: usize,
    pub(super) packages: Vec<Row>,
}

#[derive(Serialize)]
pub(super) struct Row {
    pub(super) name: String,
    pub(super) version: String,
    primary_language: Option<String>,
    pub(super) language_bytes: u64,
    pub(super) total_bytes: u64,
    share: f64,
    pub(super) fidelity: RevisionFidelity,
    pub(super) stale: bool,
}

fn matching(
    profile: &LanguageProfile,
    language: &str,
    contains: bool,
    min_share: Option<f64>,
) -> Option<(u64, f64)> {
    if profile.total_bytes == 0 {
        return None;
    }
    let bytes = profile
        .languages
        .iter()
        .filter(|row| row.language.to_lowercase() == language.to_lowercase())
        .map(|row| row.bytes)
        .sum::<u64>();
    let share = bytes as f64 / profile.total_bytes as f64;
    let matches = if let Some(min) = min_share {
        share >= min
    } else if contains {
        bytes > 0
    } else {
        profile
            .primary_language
            .as_ref()
            .is_some_and(|name| name.to_lowercase() == language.to_lowercase())
    };
    matches.then_some((bytes, share))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    snapshot: &PublishedSnapshot,
    database: &Database,
    snapshot_staleness: SnapshotStaleness,
    selection: PopulationSelection,
    language: String,
    contains: bool,
    min_share: Option<f64>,
    fidelity: Fidelity,
    sort: Sort,
) -> Result<PackagesReport, AppError> {
    let mut report = PackagesReport {
        schema_version: super::ReportSchemaVersion::CURRENT,
        report: "packages",
        snapshot_staleness,
        selection,
        language,
        mode: if min_share.is_some() {
            "min-share"
        } else if contains {
            "contains"
        } else {
            "primary"
        },
        min_share,
        fidelity,
        sort,
        selected_population: 0,
        matched_packages: 0,
        packages: Vec::new(),
    };
    for (package, evidence) in snapshot.packages.iter().zip(&snapshot.provider_packages) {
        if !report.selection.includes(package) {
            continue;
        }
        report.selected_population += 1;
        if crate::package::provider::source_free(&evidence.attempts) {
            continue;
        }
        let mut exact = BTreeMap::new();
        let mut approximate = BTreeMap::new();
        for (attempt, record) in evidence.attempts.iter().zip(&evidence.analyses) {
            let AttemptOutcome::Resolved { target, .. } = &attempt.outcome else {
                continue;
            };
            let Some(record) = record else { continue };
            if record.state != "analyzed" {
                continue;
            }
            let Ok(rows) = super::source_bytes::parse_languages(&record.languages_json) else {
                continue;
            };
            let key = (
                target.location.clone(),
                target.revision.clone(),
                target.subdirectory.clone(),
                target.checksum.clone(),
            );
            if record.fidelity == RevisionFidelity::ExactPackagedSource
                && record.source == ProfileSource::Linguist
            {
                exact.entry(key).or_insert(rows);
            } else if record.fidelity == RevisionFidelity::CurrentUpstream {
                approximate.entry(key).or_insert(rows);
            }
        }
        let mut stale = false;
        if exact.is_empty()
            && approximate.is_empty()
            && evidence.analysis_state == AnalysisState::Stale
        {
            for (target, record) in database
                .stale_exact_profiles(&evidence.name)
                .map_err(AppError::storage)?
            {
                if let Ok(rows) = super::source_bytes::parse_languages(&record.languages_json) {
                    let key = (
                        target.location,
                        target.revision,
                        target.subdirectory,
                        target.checksum,
                    );
                    exact.entry(key).or_insert(rows);
                    stale = true;
                }
            }
        }
        let (profiles, row_fidelity) = if !exact.is_empty() {
            (exact, RevisionFidelity::ExactPackagedSource)
        } else if fidelity == Fidelity::BestAvailable {
            (approximate, RevisionFidelity::CurrentUpstream)
        } else {
            continue;
        };
        let mut bytes = BTreeMap::<String, u64>::new();
        for rows in profiles.into_values() {
            for row in rows {
                let value = bytes.entry(row.language).or_default();
                *value = value
                    .checked_add(row.bytes)
                    .ok_or_else(|| AppError::storage("language byte count overflow"))?;
            }
        }
        let rows: Vec<_> = bytes
            .into_iter()
            .map(|(language, bytes)| LanguageBytes { language, bytes })
            .collect();
        if rows
            .iter()
            .try_fold(0u64, |sum, row| sum.checked_add(row.bytes))
            .is_none()
        {
            return Err(AppError::storage("total language byte count overflow"));
        }
        let profile = LanguageProfile::from_bytes(rows);
        if let Some((language_bytes, share)) =
            matching(&profile, &report.language, contains, min_share)
        {
            report.packages.push(Row {
                name: evidence.name.clone(),
                version: evidence.version.clone(),
                primary_language: profile.primary_language,
                language_bytes,
                total_bytes: profile.total_bytes,
                share,
                fidelity: row_fidelity,
                stale,
            });
        }
    }
    report.packages.sort_by(|a, b| sort.compare(a, b));
    report.matched_packages = report.packages.len();
    Ok(report)
}

impl PackagesReport {
    pub(crate) fn render(&self, format: Format) -> Result<String, RenderError> {
        match format {
            Format::Json => serde_json::to_string_pretty(self).map_err(RenderError::Json),
            Format::Toon => {
                let mut value = serde_json::to_value(self).map_err(RenderError::Json)?;
                value["toon_specification_version"] = serde_json::json!("3.0");
                toon_format::encode_default(&value).map_err(RenderError::Toon)
            }
            Format::Markdown => {
                let mut output = format!(
                    "Matched packages: {} / {}\n\n| Package | Version | Share | Bytes | Fidelity | Stale |\n| --- | --- | ---: | ---: | --- | --- |\n",
                    self.matched_packages, self.selected_population
                );
                for row in &self.packages {
                    output.push_str(&format!(
                        "| {} | {} | {:.2}% | {}/{} | {} | {} |\n",
                        row.name.replace('|', "&#124;"),
                        row.version.replace('|', "&#124;"),
                        row.share * 100.0,
                        row.language_bytes,
                        row.total_bytes,
                        row.fidelity.as_str(),
                        row.stale
                    ));
                }
                Ok(output)
            }
            Format::Terminal => {
                let mut output = format!(
                    "Packages matching {:?} ({}): {} / {}\n",
                    self.language, self.mode, self.matched_packages, self.selected_population
                );
                for row in &self.packages {
                    output.push_str(&format!(
                        "{} {} — {:.2}% ({}/{} bytes), {}{}\n",
                        row.name,
                        row.version,
                        row.share * 100.0,
                        row.language_bytes,
                        row.total_bytes,
                        row.fidelity.as_str(),
                        if row.stale { ", stale" } else { "" }
                    ));
                }
                Ok(output)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_matching_modes_and_boundaries() {
        let profile = LanguageProfile::from_bytes(vec![
            LanguageBytes {
                language: "C++".into(),
                bytes: 60,
            },
            LanguageBytes {
                language: "Rust".into(),
                bytes: 40,
            },
        ]);
        assert!(matching(&profile, "rust", false, None).is_none());
        assert!(matching(&profile, "RUST", true, None).is_some());
        assert!(matching(&profile, "rust", false, Some(0.4)).is_some());
        assert!(matching(&profile, "rust", false, Some(0.5)).is_none());
        assert!(matching(&profile, "c++", false, None).is_some());
        assert!(matching(&profile, "Python", true, None).is_none());
        assert!(matching(&profile, "Python", false, Some(0.0)).is_some());
        assert!(matching(&LanguageProfile::default(), "Rust", false, Some(0.0)).is_none());
    }
}
