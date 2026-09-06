//! Derived policy report over the existing deduplicated source-byte report.
use serde::Serialize;

use super::clanker_policy::{self as policy, LanguageType, Tier};
use super::clanker_presentation as presentation;
use super::source_bytes::{FidelitySelection, SourceBytesReport, format_human_bytes};
use super::{Format, RenderError};

#[derive(Serialize)]
struct MeasuredLanguage {
    language: String,
    bytes: u64,
    /// Fraction of ALL measured source bytes, including ancillary bytes.
    share: f64,
    language_type: Option<LanguageType>,
}

#[derive(Serialize)]
struct Language {
    #[serde(flatten)]
    measured: MeasuredLanguage,
    tier: Option<Tier>,
    weight: Option<u8>,
    inherited_from: Option<&'static str>,
}

#[derive(Serialize)]
struct RegistryIdentity {
    id: &'static str,
    source_sha256: &'static str,
}

#[derive(Serialize)]
pub(crate) struct ClankerIndexReport<'a> {
    // Only this derived report changes schema; other reports remain unchanged.
    schema_version: &'static str,
    report: &'static str,
    policy: &'static str,
    language_registry: RegistryIdentity,
    fidelity: FidelitySelection,
    grade: Option<Tier>,
    gradeable_bytes: u64,
    ungraded_programming_bytes: u64,
    ancillary_bytes: u64,
    /// A profile name absent from the pinned registry is not silently classified.
    unknown_type_bytes: u64,
    total_source_bytes: u64,
    coverage_ratio: f64,
    exact_coverage_ratio: f64,
    approximate_fallback_coverage_ratio: f64,
    languages: Vec<Language>,
    ancillary_languages: Vec<MeasuredLanguage>,
    unknown_type_languages: Vec<MeasuredLanguage>,
    /// Complete measurement provenance, including stale contributions and selection.
    source_analysis: &'a SourceBytesReport,
}

impl<'a> ClankerIndexReport<'a> {
    pub(crate) fn new(source: &'a SourceBytesReport) -> Self {
        let mut weighted_sum = 0u128;
        let mut gradeable_bytes = 0;
        let mut ungraded_programming_bytes = 0;
        let mut ancillary_bytes = 0;
        let mut unknown_type_bytes = 0;
        let mut languages = Vec::new();
        let mut ancillary_languages = Vec::new();
        let mut unknown_type_languages = Vec::new();
        for row in &source.languages {
            let assignment = policy::lookup(&row.language);
            let measured = MeasuredLanguage {
                language: row.language.clone(),
                bytes: row.bytes,
                share: if source.total_source_bytes == 0 {
                    0.0
                } else {
                    row.bytes as f64 / source.total_source_bytes as f64
                },
                language_type: assignment.language_type,
            };
            match assignment.language_type {
                Some(LanguageType::Programming) => {
                    if let Some(tier) = assignment.tier {
                        weighted_sum += u128::from(row.bytes) * u128::from(tier.weight());
                        gradeable_bytes += row.bytes;
                    } else {
                        ungraded_programming_bytes += row.bytes;
                    }
                    languages.push(Language {
                        measured,
                        tier: assignment.tier,
                        weight: assignment.tier.map(Tier::weight),
                        inherited_from: assignment.inherited_from,
                    });
                }
                Some(_) => {
                    ancillary_bytes += row.bytes;
                    ancillary_languages.push(measured);
                }
                None => {
                    unknown_type_bytes += row.bytes;
                    unknown_type_languages.push(measured);
                }
            }
        }
        let exact_coverage_ratio = source.analyzed_coverage_percent() / 100.0;
        let approximate_fallback_coverage_ratio =
            if source.fidelity == FidelitySelection::BestAvailable {
                source.approximate_coverage_ratio()
            } else {
                0.0
            };
        Self {
            schema_version: "2.0.0",
            report: "clanker-index",
            policy: policy::ID,
            language_registry: RegistryIdentity {
                id: &policy::registry().id,
                source_sha256: &policy::registry().source_sha256,
            },
            fidelity: source.fidelity,
            grade: policy::aggregate(weighted_sum, u128::from(gradeable_bytes)),
            gradeable_bytes,
            ungraded_programming_bytes,
            ancillary_bytes,
            unknown_type_bytes,
            total_source_bytes: source.total_source_bytes,
            coverage_ratio: exact_coverage_ratio + approximate_fallback_coverage_ratio,
            exact_coverage_ratio,
            approximate_fallback_coverage_ratio,
            languages,
            ancillary_languages,
            unknown_type_languages,
            source_analysis: source,
        }
    }

    pub(crate) fn render(
        &self,
        format: Format,
        limit: Option<usize>,
    ) -> Result<String, RenderError> {
        match format {
            Format::Json => serde_json::to_string_pretty(self).map_err(RenderError::Json),
            Format::Toon => {
                #[derive(Serialize)]
                struct Toon<'a, 'b> {
                    toon_specification_version: super::ToonSpecificationVersion,
                    #[serde(flatten)]
                    report: &'a ClankerIndexReport<'b>,
                }
                toon_format::encode_default(&Toon {
                    toon_specification_version: super::ToonSpecificationVersion::CURRENT,
                    report: self,
                })
                .map_err(RenderError::Toon)
            }
            Format::Terminal => Ok(self.human(limit)),
            Format::Markdown => Ok(format!("```text\n{}```\n", self.human(limit))),
        }
    }

    fn human(&self, limit: Option<usize>) -> String {
        let mut output = format!(
            "🤖 Clanker Index\n\nSystem clanker grade: {}\n{}\n\nPolicy: {} (subjective)\n\n",
            self.grade.map_or("???", Tier::label),
            presentation::system_grade(self.grade),
            self.policy,
        );
        let visible = limit
            .unwrap_or(self.languages.len())
            .min(self.languages.len());
        for row in &self.languages[..visible] {
            let (emoji, verdict) = presentation::lookup(&row.measured.language);
            output.push_str(&format!(
                "{:<4} {} {:<20} {:>10} {:>5.1}%  {}\n",
                row.tier.map_or("???", Tier::label),
                emoji,
                row.measured.language,
                format_human_bytes(row.measured.bytes),
                row.measured.share * 100.0,
                verdict,
            ));
        }
        if visible < self.languages.len() {
            let remaining: u64 = self.languages[visible..]
                .iter()
                .map(|row| row.measured.bytes)
                .sum();
            output.push_str(&format!(
                "\nOther programming languages (display-only, mixed tiers; {} languages): {}\n",
                self.languages.len() - visible,
                format_human_bytes(remaining),
            ));
        }
        output.push_str(&format!(
            "\nAncillary source bytes:       {}\nGradeable programming bytes:  {}\nUngraded programming bytes:   {}\nTotal source bytes:           {}\n",
            format_human_bytes(self.ancillary_bytes),
            format_human_bytes(self.gradeable_bytes),
            format_human_bytes(self.ungraded_programming_bytes),
            format_human_bytes(self.total_source_bytes),
        ));
        if self.unknown_type_bytes > 0 {
            output.push_str(&format!(
                "Unknown language type (excluded): {}\n",
                format_human_bytes(self.unknown_type_bytes)
            ));
        }
        output.push_str(
            "Shares: all measured source bytes; grade: gradeable programming bytes only.\n",
        );
        if self.fidelity == FidelitySelection::BestAvailable {
            output.push_str(&format!(
                "\nCoverage: {:.1}% best-available\n  exact:                {:.1}%\n  approximate fallback: {:.1}%\n",
                self.coverage_ratio * 100.0, self.exact_coverage_ratio * 100.0,
                self.approximate_fallback_coverage_ratio * 100.0,
            ));
        } else {
            output.push_str(&format!(
                "\nCoverage: {:.1}% (exact package analysis)\n",
                self.exact_coverage_ratio * 100.0
            ));
        }
        output.push_str(&format!(
            "Fidelity: {}; snapshot: {}; stale source bytes (included): {}\n",
            self.fidelity.as_str(),
            self.source_analysis.snapshot_staleness.as_str(),
            format_human_bytes(self.source_analysis.stale_source_bytes),
        ));
        let anomalies: Vec<_> = self
            .languages
            .iter()
            .filter(|row| row.tier.is_none())
            .collect();
        if !anomalies.is_empty() {
            output.push_str("\nUnjudged by the Clanker Council (??? is not worse than F):\n");
            let shown = limit
                .map_or(anomalies.len(), |limit| limit.min(3))
                .min(anomalies.len());
            for row in &anomalies[..shown] {
                let (emoji, verdict) = presentation::lookup(&row.measured.language);
                output.push_str(&format!(
                    "{} {:<20} {:>10}  {}\n",
                    emoji,
                    row.measured.language,
                    format_human_bytes(row.measured.bytes),
                    verdict
                ));
            }
            if shown < anomalies.len() {
                output.push_str(&format!(
                    "  ... {} more unjudged programming languages; use --all.\n",
                    anomalies.len() - shown
                ));
            }
        }
        if !self.unknown_type_languages.is_empty() {
            output.push_str("\nNames absent from the language registry (not graded):\n");
            let shown = limit
                .map_or(self.unknown_type_languages.len(), |limit| limit.min(3))
                .min(self.unknown_type_languages.len());
            for row in &self.unknown_type_languages[..shown] {
                output.push_str(&format!(
                    "  {}  {}\n",
                    row.language,
                    format_human_bytes(row.bytes)
                ));
            }
            if shown < self.unknown_type_languages.len() {
                output.push_str(&format!(
                    "  ... {} more names; use --all.\n",
                    self.unknown_type_languages.len() - shown
                ));
            }
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::source_bytes::LanguageRow;

    fn source(rows: &[(&str, u64)]) -> SourceBytesReport {
        SourceBytesReport::test_languages(
            rows.iter()
                .map(|(language, bytes)| LanguageRow {
                    language: (*language).into(),
                    bytes: *bytes,
                })
                .collect(),
        )
    }

    #[test]
    fn programming_ancillary_and_unknown_types_partition_measured_bytes() {
        let source = source(&[
            ("Perl", 900),
            ("Rust", 10),
            ("C", 90),
            ("Prolog", 100),
            ("HTML", 1000),
            ("CSS", 2000),
            ("MDX", 3000),
            ("Roff", 4000),
            ("JSON", 5000),
            ("Markdown", 6000),
            ("New Language", 7000),
        ]);
        let report = ClankerIndexReport::new(&source);
        assert_eq!(report.grade, Some(Tier::C));
        assert_eq!(report.gradeable_bytes, 100);
        assert_eq!(report.ungraded_programming_bytes, 1000);
        assert_eq!(report.ancillary_bytes, 21_000);
        assert_eq!(report.unknown_type_bytes, 7000);
        assert_eq!(
            report.total_source_bytes,
            report.gradeable_bytes
                + report.ungraded_programming_bytes
                + report.ancillary_bytes
                + report.unknown_type_bytes
        );
        assert_eq!(report.languages.len(), 4);
        assert_eq!(report.ancillary_languages.len(), 6);
        assert_eq!(report.unknown_type_languages.len(), 1);
        assert_eq!(report.languages[0].weight, None);
        let text = report.human(None);
        assert!(text.contains("Unjudged by the Clanker Council"));
        assert!(text.contains("Perl"));
        assert!(text.contains("Prolog"));
        for ancillary in ["HTML", "CSS", "MDX", "Roff", "JSON", "Markdown"] {
            assert!(!text.contains(ancillary));
        }
        assert!(text.contains("New Language"));
        assert!(text.contains("Unknown language type (excluded)"));
    }

    #[test]
    fn no_grade_for_empty_unjudged_or_ancillary_only_but_f_is_real() {
        for rows in [
            vec![],
            vec![("Perl", 100)],
            vec![("Omgrofl", 42)],
            vec![("HTML", 100)],
            vec![("Future Language", 100)],
        ] {
            let source = source(&rows);
            let report = ClankerIndexReport::new(&source);
            assert_eq!(report.grade, None);
            assert!(
                report
                    .human(None)
                    .contains("The council declines to issue a rating")
            );
        }
        let source = source(&[("Ruby", 42), ("HTML", 100_000)]);
        let report = ClankerIndexReport::new(&source);
        assert_eq!(report.grade, Some(Tier::F));
        assert_eq!(report.gradeable_bytes, 42);
        assert!(report.human(None).contains("Clanker-hostile environment"));
    }

    #[test]
    fn terminal_lore_is_separate_from_complete_machine_data() {
        let source = source(&[
            ("HTML", 100_000),
            ("Rust", 600),
            ("Perl", 300),
            ("C", 100),
            ("TSX", 50),
        ]);
        let report = ClankerIndexReport::new(&source);
        let text = report.render(Format::Terminal, Some(1)).unwrap();
        assert!(text.contains("🦀 Rust"));
        assert!(text.contains("REWRITE EVERYTHING"));
        assert!(!text.contains("🏭 C "));
        assert!(text.contains(
            "Other programming languages (display-only, mixed tiers; 3 languages): 450 B"
        ));
        assert!(text.contains("Gradeable programming bytes:  750 B"));
        assert!(text.contains("Ungraded programming bytes:   300 B"));
        let all = report.render(Format::Terminal, None).unwrap();
        assert!(!all.contains("🏭")); // C uses its own icon, not the C++ factory.
        assert!(all.contains("🗿 C"));
        assert!(all.contains("foundation of civilization"));
        let json_text = report.render(Format::Json, Some(1)).unwrap();
        let json: serde_json::Value = serde_json::from_str(&json_text).unwrap();
        assert_eq!(json["schema_version"], "2.0.0");
        assert_eq!(json["languages"].as_array().unwrap().len(), 4);
        assert_eq!(json["ancillary_languages"][0]["language"], "HTML");
        assert_eq!(json["languages"][3]["inherited_from"], "TypeScript");
        assert_eq!(json["languages"][3]["language_type"], "programming");
        assert!(json["languages"][0].get("emoji").is_none());
        assert!(json["languages"][0].get("verdict").is_none());
        assert!(!json_text.contains("REWRITE"));
        assert!(!json_text.contains("🦀"));
        let toon = report.render(Format::Toon, Some(1)).unwrap();
        assert!(!toon.contains("REWRITE"));
        assert!(!toon.contains("🦀"));
        assert!(toon.contains("TSX"));
    }
}
