use std::collections::{BTreeMap, BTreeSet};

use clap::ValueEnum;
use serde::Serialize;

use super::{
    CountReport, Coverage, Format, PopulationSelection, RenderError, ReportKind,
    ReportSchemaVersion, SnapshotStaleness, ToonSpecificationVersion, format_tenths,
    percentage_tenths,
};
use crate::linguist::{LanguageBytes, ProfileSource, RevisionFidelity};
use crate::package::provider::AttemptOutcome;
use crate::storage::PublishedSnapshot;

/// Which revision fidelity the source-byte totals sum.
///
/// Profiles with different fidelity are never mixed as equal evidence:
/// `exact-packaged-source` profiles were analyzed from the packaged revision
/// itself, while `current-upstream` profiles approximate it from upstream
/// HEAD. The totals always sum one fidelity selection; the other fidelity
/// stays disclosed apart.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub(crate) enum FidelitySelection {
    /// Sum only `exact-packaged-source` profiles (stale fallbacks included,
    /// current-upstream approximations disclosed apart and never counted).
    #[default]
    Exact,
    /// Sum only `current-upstream` approximations (exact profiles disclosed
    /// apart and never counted).
    Approximate,
    /// Sum exact profiles where available and fall back to approximations
    /// where a selected package has no exact profile. The exact and
    /// approximate contributions stay disclosed separately.
    BestAvailable,
}

impl FidelitySelection {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Approximate => "approximate",
            Self::BestAvailable => "best-available",
        }
    }
}

impl std::fmt::Display for FidelitySelection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Default human-output row cap: the top 10 languages plus one aggregated
/// `Other` row, so the row count stays bounded regardless of the byte
/// distribution. Structured output always carries every language.
pub(crate) const DEFAULT_TOP_LANGUAGES: usize = 10;

/// Resolve the `--all`/`--top` flags to a human-output row cap: `None` shows
/// every language, `Some(n)` shows the top `n` plus `Other`.
pub(crate) fn display_limit(all: bool, top: Option<usize>) -> Option<usize> {
    if all {
        None
    } else {
        Some(top.unwrap_or(DEFAULT_TOP_LANGUAGES))
    }
}

/// SI byte size for human output (`623.7 KB`, `216.4 MB`). Structured output
/// keeps exact integer bytes; only presentation rounds.
pub(crate) fn format_human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn plural(count: u64, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

/// One language share of the deduplicated source-byte total.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct LanguageRow {
    pub language: String,
    /// Summed bytes across each unique analysis target counted once.
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct SourceBytesReport {
    schema_version: ReportSchemaVersion,
    report: ReportKind,
    pub(super) snapshot_staleness: SnapshotStaleness,
    selection: PopulationSelection,
    pub(super) fidelity: FidelitySelection,
    installed_total: u64,
    selected_population: u64,
    coverage: Coverage,
    /// Distinct analysis targets summed once under the selected fidelity,
    /// including stale fallbacks in the `exact` and `best-available` modes.
    unique_targets: u64,
    /// How many of the summed targets come from stale fallback profiles.
    /// Zero in `approximate` mode, which sums no exact profiles.
    stale_targets: u64,
    /// Distinct `exact-packaged-source` targets (including stale fallbacks),
    /// summed in `exact` and `best-available`, disclosed apart in
    /// `approximate`.
    exact_targets: u64,
    /// Summed bytes across each unique exact target counted once.
    exact_bytes: u64,
    /// Distinct current-upstream approximations, summed in `approximate` and
    /// `best-available`, disclosed apart and never counted in `exact`.
    approximate_targets: u64,
    /// Summed bytes across each unique approximate target counted once.
    approximate_bytes: u64,
    /// Summed source bytes across each unique target counted once under the
    /// selected fidelity.
    pub(super) total_source_bytes: u64,
    /// Stale contribution included in `total_source_bytes`, disclosed separately.
    /// Zero in `approximate` mode.
    pub(super) stale_source_bytes: u64,
    /// Per-language totals sorted by bytes descending and name ascending.
    /// Counts always sum to `total_source_bytes`.
    pub(super) languages: Vec<LanguageRow>,
}

impl SourceBytesReport {
    #[cfg(test)]
    pub(super) fn test_languages(languages: Vec<LanguageRow>) -> Self {
        Self {
            schema_version: ReportSchemaVersion::CURRENT,
            report: ReportKind::SourceBytes,
            snapshot_staleness: SnapshotStaleness::Current,
            selection: PopulationSelection::new(super::InstallReasonSelection::All, []),
            fidelity: FidelitySelection::Exact,
            installed_total: 0, selected_population: 0,
            coverage: Coverage::missing_only(0),
            unique_targets: 0, stale_targets: 0, exact_targets: 0, exact_bytes: 0,
            approximate_targets: 0, approximate_bytes: 0, stale_source_bytes: 0,
            total_source_bytes: languages.iter().map(|row| row.bytes).sum(),
            languages,
        }
    }

    /// Exact analyzed share in percent, mirroring `count` so `--min-coverage`
    /// gates both reports on the same denominator.
    pub fn analyzed_coverage_percent(&self) -> f64 {
        self.coverage.analyzed_percent()
    }

    pub(super) fn approximate_coverage_ratio(&self) -> f64 {
        if self.coverage.denominator == 0 {
            0.0
        } else {
            self.coverage.approximate as f64 / self.coverage.denominator as f64
        }
    }

    pub fn meets_coverage(&self, minimum: Option<f64>) -> bool {
        match minimum {
            Some(minimum) => self.analyzed_coverage_percent() >= minimum,
            None => true,
        }
    }

    pub(crate) fn render(
        &self,
        format: Format,
        display_limit: Option<usize>,
    ) -> Result<String, RenderError> {
        match format {
            // Structured output always carries every language with exact
            // integer bytes, independent of the human-output row cap.
            Format::Json => serde_json::to_string_pretty(self).map_err(RenderError::Json),
            Format::Toon => {
                #[derive(Serialize)]
                struct ToonReport<'a> {
                    toon_specification_version: ToonSpecificationVersion,
                    #[serde(flatten)]
                    report: &'a SourceBytesReport,
                }
                toon_format::encode_default(&ToonReport {
                    toon_specification_version: ToonSpecificationVersion::CURRENT,
                    report: self,
                })
                .map_err(RenderError::Toon)
            }
            Format::Terminal => Ok(self.render_terminal(display_limit)),
            Format::Markdown => Ok(self.render_markdown(display_limit)),
        }
    }

    /// Build from a published snapshot. Current-version profiles come from
    /// the snapshot itself; stale fallbacks are supplied by the caller
    /// (distinct exact profiles from older versions, keyed by package name)
    /// so this stays a pure offline aggregation over one inventory snapshot.
    /// `fidelity` selects which profiles the totals sum; the other fidelity
    /// stays disclosed apart and is never mixed in as equal evidence.
    pub(crate) fn from_snapshot_with_stale(
        snapshot: &PublishedSnapshot,
        staleness: SnapshotStaleness,
        selection: PopulationSelection,
        stale_by_package: &BTreeMap<String, Vec<(String, LanguageBytesVec)>>,
        fidelity: FidelitySelection,
    ) -> Self {
        // Reuse the measured coverage so source-bytes never disagrees with
        // count about the selected population under the same filters.
        let count = CountReport::from_provider_snapshot(snapshot, staleness, selection.clone());
        let (coverage_snapshot, analyzed, approximate, stale_packages) = (count, 0, 0, 0);
        let _ = (analyzed, approximate, stale_packages);
        let selection = coverage_snapshot.selection_clone();
        let installed_total = coverage_snapshot.installed_total();
        let selected_population = coverage_snapshot.selected_population();
        let coverage = coverage_snapshot.into_coverage();

        type TargetKey = (String, String, String, String);
        let mut exact: BTreeMap<TargetKey, Vec<LanguageBytes>> = BTreeMap::new();
        let mut stale_keys: BTreeSet<TargetKey> = BTreeSet::new();
        let mut approximate: BTreeMap<TargetKey, Vec<LanguageBytes>> = BTreeMap::new();

        for (package, evidence) in snapshot.packages.iter().zip(&snapshot.provider_packages) {
            if !selection.includes(package) {
                continue;
            }
            if crate::package::provider::source_free(&evidence.attempts) {
                continue;
            }
            let mut current_exact = Vec::new();
            let mut current_approximate = Vec::new();
            for (attempt, analysis) in evidence.attempts.iter().zip(&evidence.analyses) {
                let AttemptOutcome::Resolved { target, .. } = &attempt.outcome else {
                    continue;
                };
                let Some(record) = analysis else { continue };
                if record.state != "analyzed" {
                    continue;
                }
                let Ok(languages) = parse_languages(&record.languages_json) else {
                    continue;
                };
                let key: TargetKey = (
                    target.location.clone(),
                    target.revision.clone().unwrap_or_default(),
                    target.subdirectory.clone().unwrap_or_default(),
                    target.checksum.clone().unwrap_or_default(),
                );
                if record.fidelity == RevisionFidelity::ExactPackagedSource
                    && record.source == ProfileSource::Linguist
                {
                    current_exact.push((key, languages));
                } else if record.fidelity == RevisionFidelity::CurrentUpstream {
                    current_approximate.push((key, languages));
                }
            }
            if !current_exact.is_empty() {
                for (key, languages) in current_exact {
                    exact.entry(key).or_insert(languages);
                }
                continue;
            }
            if !current_approximate.is_empty() {
                for (key, languages) in current_approximate {
                    approximate.entry(key).or_insert(languages);
                }
                continue;
            }
            // No current exact or approximate profile: fall back to older
            // exact profiles as stale, mirroring package-level stale analysis.
            // A definitive empty current source suppresses the fallback; the
            // caller only supplies stale entries for packages in Stale state,
            // so an empty lookup naturally contributes nothing.
            if evidence.analysis_state == crate::linguist::AnalysisState::Stale {
                if let Some(entries) = stale_by_package.get(&evidence.name) {
                    for (fingerprint, breakdown) in entries {
                        let Some((location, revision, subdirectory, checksum)) =
                            split_fingerprint(fingerprint)
                        else {
                            continue;
                        };
                        let key: TargetKey = (location, revision, subdirectory, checksum);
                        if exact.contains_key(&key) {
                            stale_keys.insert(key.clone());
                            continue;
                        }
                        stale_keys.insert(key.clone());
                        exact.insert(key, breakdown.0.clone());
                    }
                }
            } else if evidence.analysis_state == crate::linguist::AnalysisState::Approximate {
                // Approximate state without a current-upstream record in this
                // snapshot (for example, stored under a different candidate):
                // keep the package out of the exact totals. Target-level
                // approximate keys were already collected above when visible.
            }
        }

        // Approximate targets that also have an exact profile elsewhere stay
        // exact: fidelity is never mixed as equal evidence.
        approximate.retain(|key, _| !exact.contains_key(key));

        let exact_bytes: u64 = exact
            .values()
            .map(|languages| languages.iter().map(|row| row.bytes).sum::<u64>())
            .sum();
        let approximate_bytes: u64 = approximate
            .values()
            .map(|languages| languages.iter().map(|row| row.bytes).sum::<u64>())
            .sum();
        let stale_targets = stale_keys
            .iter()
            .filter(|key| exact.contains_key(*key))
            .count() as u64;
        let stale_source_bytes: u64 = stale_keys
            .iter()
            .filter_map(|key| exact.get(key))
            .map(|languages| languages.iter().map(|row| row.bytes).sum::<u64>())
            .sum();

        // The totals sum one fidelity selection; the other fidelity stays
        // disclosed apart through the exact/approximate fields above.
        let summed: BTreeMap<&TargetKey, &Vec<LanguageBytes>> = match fidelity {
            FidelitySelection::Exact => exact.iter().collect(),
            FidelitySelection::Approximate => approximate.iter().collect(),
            FidelitySelection::BestAvailable => exact.iter().chain(approximate.iter()).collect(),
        };
        let mut totals: BTreeMap<String, u64> = BTreeMap::new();
        let mut total_source_bytes = 0u64;
        for languages in summed.values() {
            total_source_bytes = total_source_bytes
                .saturating_add(languages.iter().map(|row| row.bytes).sum::<u64>());
            for row in languages.iter() {
                *totals.entry(row.language.clone()).or_default() = totals
                    .get(&row.language)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(row.bytes);
            }
        }
        let mut languages: Vec<LanguageRow> = totals
            .into_iter()
            .map(|(language, bytes)| LanguageRow { language, bytes })
            .collect();
        languages.sort_by(|left, right| {
            right
                .bytes
                .cmp(&left.bytes)
                .then_with(|| left.language.cmp(&right.language))
        });
        debug_assert_eq!(
            languages.iter().map(|row| row.bytes).sum::<u64>(),
            total_source_bytes
        );

        // In approximate mode the totals sum no exact profiles, so no summed
        // target is stale.
        let (unique_targets, stale_targets, stale_source_bytes) = match fidelity {
            FidelitySelection::Approximate => (approximate.len() as u64, 0, 0),
            FidelitySelection::Exact | FidelitySelection::BestAvailable => {
                (summed.len() as u64, stale_targets, stale_source_bytes)
            }
        };

        Self {
            schema_version: ReportSchemaVersion::CURRENT,
            report: ReportKind::SourceBytes,
            snapshot_staleness: staleness,
            selection,
            fidelity,
            installed_total,
            selected_population,
            coverage,
            unique_targets,
            stale_targets,
            exact_targets: exact.len() as u64,
            exact_bytes,
            approximate_targets: approximate.len() as u64,
            approximate_bytes,
            total_source_bytes,
            stale_source_bytes,
            languages,
        }
    }

    fn render_terminal(&self, display_limit: Option<usize>) -> String {
        self.render_human(false, display_limit)
    }

    fn render_markdown(&self, display_limit: Option<usize>) -> String {
        self.render_human(true, display_limit)
    }

    fn render_human(&self, markdown: bool, display_limit: Option<usize>) -> String {
        let mut rendered = format!(
            "{}Snapshot: {}\nSelection: {}\nFidelity: {}\nInstalled packages: {}\nSelected population: {}\n\nCoverage denominator: {}\nAnalysis targets: {}\nSource bytes: {}",
            if markdown {
                "# Paclang source-bytes\n\n"
            } else {
                ""
            },
            self.snapshot_staleness.as_str(),
            self.selection.render(),
            self.fidelity.as_str(),
            self.installed_total,
            self.selected_population,
            plural(self.coverage.denominator, "package"),
            self.unique_targets,
            format_human_bytes(self.total_source_bytes),
        );
        if self.coverage.source_free_packages > 0 {
            rendered.push_str(&format!(
                "\nSource-free packages (excluded): {}",
                self.coverage.source_free_packages
            ));
        }
        rendered.push('\n');
        match self.fidelity {
            FidelitySelection::Exact => {
                if self.coverage.stale > 0 {
                    rendered.push_str(&format!(
                        "\nStale analysis (included in totals): {} package(s), {}, {}",
                        self.coverage.stale,
                        plural(self.stale_targets, "target"),
                        format_human_bytes(self.stale_source_bytes),
                    ));
                }
                if self.coverage.approximate > 0 {
                    rendered.push_str(&format!(
                        "\nApproximate analysis (current upstream, not counted in totals): {} package(s), {}, {}",
                        self.coverage.approximate,
                        plural(self.approximate_targets, "target"),
                        format_human_bytes(self.approximate_bytes),
                    ));
                }
            }
            FidelitySelection::Approximate => {
                if self.exact_targets > 0 {
                    rendered.push_str(&format!(
                        "\nExact analysis (packaged revision, not counted in totals): {}, {}",
                        plural(self.exact_targets, "target"),
                        format_human_bytes(self.exact_bytes),
                    ));
                }
            }
            FidelitySelection::BestAvailable => {
                rendered.push_str(&format!(
                    "\nExact:          {}, {}",
                    plural(self.exact_targets, "target"),
                    format_human_bytes(self.exact_bytes),
                ));
                if self.coverage.stale > 0 {
                    rendered.push_str(&format!(
                        "\n  (stale, included in exact totals): {}, {}",
                        plural(self.stale_targets, "target"),
                        format_human_bytes(self.stale_source_bytes),
                    ));
                }
                rendered.push_str(&format!(
                    "\nApproximate:    {}, {}",
                    plural(self.approximate_targets, "target"),
                    format_human_bytes(self.approximate_bytes),
                ));
            }
        }
        rendered.push_str("\n\nLanguages:");
        // Human output trims to the top N languages plus one aggregated
        // `Other` row. `Other` is presentation only (never part of the
        // report model): it aggregates every hidden row so that visible
        // language bytes plus `Other` always equal the total source bytes.
        let trimmed = trim_languages(&self.languages, self.total_source_bytes, display_limit);
        if trimmed.rows.is_empty() {
            rendered.push_str(if markdown {
                "\n- No analyzed source bytes in the selected population."
            } else {
                "\nNo analyzed source bytes in the selected population."
            });
            return rendered;
        }
        let name_width = trimmed
            .rows
            .iter()
            .map(|row| row.name.chars().count())
            .max()
            .unwrap_or(0);
        let bytes_width = trimmed
            .rows
            .iter()
            .map(|row| format_human_bytes(row.bytes).len())
            .max()
            .unwrap_or(0);
        let counts = trimmed.rows.iter().map(|row| row.bytes).collect::<Vec<_>>();
        let tenths = percentage_tenths(&counts);
        let percent_width = tenths
            .iter()
            .map(|share| format_tenths(*share).len())
            .max()
            .unwrap_or(0);
        for (index, (row, share)) in trimmed.rows.iter().zip(tenths).enumerate() {
            if trimmed.other_at == Some(index) {
                let rule = "─".repeat(name_width + bytes_width + percent_width + 4);
                rendered.push_str(&format!("\n  {rule}"));
            }
            let name = if markdown {
                escape_markdown(&row.name)
            } else {
                row.name.clone()
            };
            rendered.push_str(&format!(
                "\n  {:<name_width$}  {:>bytes_width$}  {:>percent_width$}",
                name,
                format_human_bytes(row.bytes),
                format_tenths(share),
                name_width = name_width,
                bytes_width = bytes_width,
                percent_width = percent_width,
            ));
        }
        rendered
    }
}

/// One human-output language row: either a language or the aggregated
/// `Other` presentation bucket.
struct DisplayRow {
    name: String,
    bytes: u64,
}

struct TrimmedLanguages {
    rows: Vec<DisplayRow>,
    /// Position of the aggregated `Other` row, if any. A separator rule is
    /// drawn above it to set it apart from real languages.
    other_at: Option<usize>,
}

/// Trim the sorted language totals to the top `limit` rows plus an aggregated
/// `Other` row. `None` shows every language. The visible bytes plus `Other`
/// always equal `total_source_bytes`.
fn trim_languages(
    languages: &[LanguageRow],
    total_source_bytes: u64,
    limit: Option<usize>,
) -> TrimmedLanguages {
    let visible: &[LanguageRow] = match limit {
        Some(limit) if languages.len() > limit => &languages[..limit],
        _ => languages,
    };
    let mut rows: Vec<DisplayRow> = visible
        .iter()
        .map(|row| DisplayRow {
            name: row.language.clone(),
            bytes: row.bytes,
        })
        .collect();
    let visible_bytes: u64 = rows.iter().map(|row| row.bytes).sum();
    let other_at = if visible.len() < languages.len() {
        let other_bytes = total_source_bytes.saturating_sub(visible_bytes);
        rows.push(DisplayRow {
            name: "Other".to_owned(),
            bytes: other_bytes,
        });
        Some(rows.len() - 1)
    } else {
        None
    };
    debug_assert_eq!(
        rows.iter().map(|row| row.bytes).sum::<u64>(),
        total_source_bytes
    );
    TrimmedLanguages { rows, other_at }
}

/// Serializable language breakdown for one stale target, paired with a
/// fingerprint string so the report builder stays independent of storage
/// types. See `stale_breakdown` for construction.
#[derive(Clone, Debug)]
pub(crate) struct LanguageBytesVec(pub Vec<LanguageBytes>);

/// Build the stale lookup consumed by `from_snapshot_with_stale` from
/// distinct exact profiles fetched through storage.
pub(crate) fn stale_breakdown(
    profiles: &[(
        crate::package::provider::AnalysisTarget,
        crate::linguist::LanguageRecord,
    )],
) -> Vec<(String, LanguageBytesVec)> {
    profiles
        .iter()
        .filter_map(|(target, record)| {
            let languages = parse_languages(&record.languages_json).ok()?;
            Some((fingerprint(target), LanguageBytesVec(languages)))
        })
        .collect()
}

fn fingerprint(target: &crate::package::provider::AnalysisTarget) -> String {
    format!(
        "{}\n{}\n{}\n{}",
        target.location,
        target.revision.as_deref().unwrap_or_default(),
        target.subdirectory.as_deref().unwrap_or_default(),
        target.checksum.as_deref().unwrap_or_default(),
    )
}

fn split_fingerprint(value: &str) -> Option<(String, String, String, String)> {
    let mut parts = value.splitn(4, '\n');
    Some((
        parts.next()?.to_owned(),
        parts.next()?.to_owned(),
        parts.next()?.to_owned(),
        parts.next()?.to_owned(),
    ))
}

pub(super) fn parse_languages(text: &str) -> Result<Vec<LanguageBytes>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "language profile is not JSON".to_owned())?;
    let entries = value
        .as_array()
        .ok_or_else(|| "language profile is not an array".to_owned())?;
    let mut languages = Vec::with_capacity(entries.len());
    for entry in entries {
        let language = entry
            .get("language")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "language entry has no name".to_owned())?;
        let bytes = entry
            .get("bytes")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| format!("language {language:?} has no byte count"))?;
        if language.is_empty() || language.len() > 64 || language.chars().any(char::is_control) {
            return Err(format!("language {language:?} is not a valid name"));
        }
        if bytes > 0 {
            languages.push(LanguageBytes {
                language: language.to_owned(),
                bytes,
            });
        }
    }
    languages.sort_by(|left: &LanguageBytes, right: &LanguageBytes| {
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.language.cmp(&right.language))
    });
    Ok(languages)
}

fn escape_markdown(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '&' => "&amp;".to_owned(),
            '<' => "&lt;".to_owned(),
            '>' => "&gt;".to_owned(),
            '\\' | '`' | '*' | '_' | '[' | ']' | '|' => format!("\\{ch}"),
            _ => ch.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_percentages_total_one_hundred() {
        let tenths = percentage_tenths(&[70, 30]);
        assert_eq!(tenths.iter().sum::<u64>(), 1000);
    }

    #[test]
    fn malformed_breakdowns_are_skipped() {
        assert!(parse_languages("[]").unwrap().is_empty());
        assert!(parse_languages("{}").is_err());
        assert!(parse_languages(r#"[{"language":"Rust"}]"#).is_err());
    }

    #[test]
    fn human_bytes_use_si_units_with_one_decimal() {
        assert_eq!(format_human_bytes(0), "0 B");
        assert_eq!(format_human_bytes(150), "150 B");
        assert_eq!(format_human_bytes(999), "999 B");
        assert_eq!(format_human_bytes(623_666), "623.7 KB");
        assert_eq!(format_human_bytes(216_376_939), "216.4 MB");
        assert_eq!(format_human_bytes(1_500_000_000), "1.5 GB");
    }

    #[test]
    fn display_limit_resolves_all_and_top() {
        assert_eq!(display_limit(true, None), None);
        assert_eq!(display_limit(true, Some(3)), None);
        assert_eq!(display_limit(false, None), Some(DEFAULT_TOP_LANGUAGES));
        assert_eq!(display_limit(false, Some(20)), Some(20));
    }

    fn language_rows(counts: &[(&str, u64)]) -> Vec<LanguageRow> {
        counts
            .iter()
            .map(|(language, bytes)| LanguageRow {
                language: (*language).to_owned(),
                bytes: *bytes,
            })
            .collect()
    }

    #[test]
    fn other_aggregates_hidden_rows_and_conserves_the_total() {
        let languages = language_rows(&[("C", 100), ("Rust", 60), ("Go", 40)]);
        let total = 200;
        let trimmed = trim_languages(&languages, total, Some(2));
        assert_eq!(trimmed.other_at, Some(2));
        assert_eq!(trimmed.rows.len(), 3);
        assert_eq!(trimmed.rows[2].name, "Other");
        assert_eq!(trimmed.rows[2].bytes, 40);
        // Visible language bytes plus Other equal the total source bytes.
        assert_eq!(trimmed.rows.iter().map(|row| row.bytes).sum::<u64>(), total);
    }

    #[test]
    fn no_other_row_when_everything_fits() {
        let languages = language_rows(&[("C", 100), ("Rust", 60)]);
        for limit in [None, Some(2), Some(10)] {
            let trimmed = trim_languages(&languages, 160, limit);
            assert_eq!(trimmed.other_at, None);
            assert_eq!(trimmed.rows.len(), 2);
        }
    }
}
