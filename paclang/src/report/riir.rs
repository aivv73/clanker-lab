use std::collections::BTreeMap;

use serde::Serialize;

use super::packages::{self, Fidelity, Sort};
use super::source_bytes::{FidelitySelection, SourceBytesReport};
use super::{AppError, Format, PopulationSelection, RenderError, SnapshotStaleness};
use crate::linguist::RevisionFidelity;
use crate::storage::{Database, PublishedSnapshot};

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub(crate) enum Bar {
    Blocks,
    Crabs,
    Ascii,
}

#[derive(Serialize)]
pub(crate) struct Candidate {
    name: String,
    version: String,
    c_cpp_bytes: u64,
    total_source_bytes: u64,
    c_cpp_ratio: f64,
    fidelity: RevisionFidelity,
    stale: bool,
}

pub(crate) fn candidates(
    snapshot: &PublishedSnapshot,
    database: &Database,
    staleness: SnapshotStaleness,
    selection: PopulationSelection,
    fidelity: FidelitySelection,
) -> Result<Vec<Candidate>, AppError> {
    let fidelity = match fidelity {
        FidelitySelection::Exact => Fidelity::Exact,
        _ => Fidelity::BestAvailable,
    };
    let mut rows = BTreeMap::<String, Candidate>::new();
    for language in ["C", "C++"] {
        let report = packages::build(
            snapshot,
            database,
            staleness,
            selection.clone(),
            language.into(),
            true,
            None,
            fidelity,
            Sort::Name,
        )?;
        for row in report.packages {
            let entry = rows.entry(row.name.clone()).or_insert(Candidate {
                name: row.name,
                version: row.version,
                c_cpp_bytes: 0,
                total_source_bytes: row.total_bytes,
                c_cpp_ratio: 0.0,
                fidelity: row.fidelity,
                stale: row.stale,
            });
            entry.c_cpp_bytes += row.language_bytes;
            entry.c_cpp_ratio = entry.c_cpp_bytes as f64 / entry.total_source_bytes as f64;
        }
    }
    let mut rows: Vec<_> = rows.into_values().collect();
    // Rank by descending C/C++ bytes, then package name.
    rows.sort_by(|a, b| {
        b.c_cpp_bytes
            .cmp(&a.c_cpp_bytes)
            .then_with(|| a.name.cmp(&b.name))
    });
    rows.truncate(3);
    Ok(rows)
}

#[derive(Serialize)]
pub(crate) struct RiirReport<'a> {
    schema_version: super::ReportSchemaVersion,
    report: &'static str,
    metric: &'static str,
    oxidation_ratio: Option<f64>,
    riir_remaining_ratio: Option<f64>,
    rust_bytes: u64,
    c_bytes: u64,
    cpp_bytes: u64,
    other_bytes: u64,
    unknown_source_bytes: Option<u64>,
    riir_remaining_bytes: u64,
    coverage_ratio: f64,
    exact_coverage_ratio: f64,
    approximate_fallback_coverage_ratio: f64,
    top_rewrite_candidates: Vec<Candidate>,
    /// Preserve source-byte provenance, selection, coverage and fidelity counts.
    source_analysis: &'a SourceBytesReport,
}

impl<'a> RiirReport<'a> {
    pub(crate) fn new(source: &'a SourceBytesReport, candidates: Vec<Candidate>) -> Self {
        let bytes = |name| {
            source
                .languages
                .iter()
                .filter(|row| row.language == name)
                .map(|row| row.bytes)
                .sum::<u64>()
        };
        let rust = bytes("Rust");
        let c = bytes("C");
        let cpp = bytes("C++");
        let denominator = rust + c + cpp;
        let exact_coverage_ratio = source.analyzed_coverage_percent() / 100.0;
        let approximate_fallback_coverage_ratio =
            if source.fidelity == FidelitySelection::BestAvailable {
                source.approximate_coverage_ratio()
            } else {
                0.0
            };
        Self {
            schema_version: super::ReportSchemaVersion::CURRENT,
            report: "riir",
            metric: "rust / (rust + c + cpp) source bytes",
            oxidation_ratio: (denominator > 0).then(|| rust as f64 / denominator as f64),
            riir_remaining_ratio: (denominator > 0).then(|| (c + cpp) as f64 / denominator as f64),
            rust_bytes: rust,
            c_bytes: c,
            cpp_bytes: cpp,
            other_bytes: source.total_source_bytes - denominator,
            unknown_source_bytes: None,
            riir_remaining_bytes: c + cpp,
            coverage_ratio: exact_coverage_ratio + approximate_fallback_coverage_ratio,
            exact_coverage_ratio,
            approximate_fallback_coverage_ratio,
            top_rewrite_candidates: candidates,
            source_analysis: source,
        }
    }

    pub(crate) fn render(&self, format: Format, bar: Bar) -> Result<String, RenderError> {
        match format {
            Format::Json => serde_json::to_string_pretty(self).map_err(RenderError::Json),
            Format::Toon => {
                #[derive(Serialize)]
                struct Toon<'a, 'b> {
                    toon_specification_version: super::ToonSpecificationVersion,
                    #[serde(flatten)]
                    report: &'a RiirReport<'b>,
                }
                toon_format::encode_default(&Toon {
                    toon_specification_version: super::ToonSpecificationVersion::CURRENT,
                    report: self,
                })
                .map_err(RenderError::Toon)
            }
            Format::Terminal => Ok(self.human(bar)),
            Format::Markdown => Ok(format!("```text\n{}```\n", self.human(bar))),
        }
    }

    fn coverage_text(&self) -> String {
        if self.source_analysis.fidelity == FidelitySelection::BestAvailable {
            format!(
                "Coverage: {:.1}% best-available\n  exact: {:.1}%\n  approximate fallback: {:.1}%",
                self.coverage_ratio * 100.0,
                self.exact_coverage_ratio * 100.0,
                self.approximate_fallback_coverage_ratio * 100.0,
            )
        } else {
            format!(
                "Coverage: {:.1}% (exact package analysis)",
                self.exact_coverage_ratio * 100.0
            )
        }
    }

    fn human(&self, bar: Bar) -> String {
        let mut text = format!(
            "🦀 Rewrite It In Rust 🦀\n\nOxidation progress\n{}\n\n🦀 Rust             {}\n💀 C                {}\n💀 C++              {}\n   Other (excluded) {}\n?  Unknown          not measured\n\n🦀 Oxidized          {}\n🔥 RIIR remaining    {}\n\n{}\n\n",
            progress(self.oxidation_ratio, bar),
            size(self.rust_bytes),
            size(self.c_bytes),
            size(self.cpp_bytes),
            size(self.other_bytes),
            percent(self.oxidation_ratio),
            percent(self.riir_remaining_ratio),
            self.coverage_text(),
        );
        text.push_str(&format!(
            "Metric: Rust / (Rust + C + C++), source bytes\nFidelity: {}; snapshot: {}; stale source bytes: {}\n\nTop rewrite candidates (C/C++ bytes):\n",
            self.source_analysis.fidelity.as_str(),
            self.source_analysis.snapshot_staleness.as_str(),
            size(self.source_analysis.stale_source_bytes),
        ));
        if self.top_rewrite_candidates.is_empty() {
            text.push_str("  None in analyzed data.\n");
        }
        for row in &self.top_rewrite_candidates {
            text.push_str(&format!(
                "  {:<20} {} C/C++ ({:.1}%, {}{})\n",
                row.name,
                size(row.c_cpp_bytes),
                row.c_cpp_ratio * 100.0,
                row.fidelity.as_str(),
                if row.stale { ", stale" } else { "" }
            ));
        }
        text.push_str(&format!(
            "\n{}\nThe crabs demand {} more.\n",
            message(self.rust_bytes, self.riir_remaining_bytes),
            size(self.riir_remaining_bytes)
        ));
        text
    }
}

fn size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{value:.1} {unit}")
}

fn percent(ratio: Option<f64>) -> String {
    ratio.map_or_else(|| "N/A".into(), |ratio| format!("{:.1}%", ratio * 100.0))
}

fn progress(ratio: Option<f64>, bar: Bar) -> String {
    let filled = (ratio.unwrap_or(0.0) * 20.0).round() as usize;
    let (fill, empty) = match bar {
        Bar::Blocks => ("█", "░"),
        // Each segment occupies two cells in terminals with standard emoji width.
        Bar::Crabs => ("🦀", "░░"),
        Bar::Ascii => ("#", "-"),
    };
    format!(
        "[{}{}] {}",
        fill.repeat(filled),
        empty.repeat(20 - filled),
        percent(ratio)
    )
}

fn message(rust: u64, remaining: u64) -> &'static str {
    if rust == 0 && remaining == 0 {
        return "🦀 No Rust/C/C++ source bytes measured yet.";
    }
    if remaining == 0 {
        return "🦀 EVERYTHING IS RUST 🦀";
    }
    let percent = u128::from(rust) * 100 / (u128::from(rust) + u128::from(remaining));
    match percent {
        0..20 => "🦀 The crabs are starving.",
        20..40 => "🦀 Oxidation has begun.",
        40..60 => "🦀 The crabs are advancing.",
        60..80 => "🦀 Ferris grows stronger.",
        _ => "🦀 C is an endangered species.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_cover_boundaries_without_false_completion() {
        for (rust, expected) in [
            (0, "starving"),
            (20, "begun"),
            (40, "advancing"),
            (60, "stronger"),
            (80, "endangered"),
            (100, "EVERYTHING"),
        ] {
            assert!(message(rust, 100 - rust).contains(expected));
        }
        assert!(message(0, 0).contains("No Rust"));
        assert!(message(u64::MAX, 1).contains("endangered"));
    }

    #[test]
    fn bars_have_fixed_segment_counts_and_handle_missing_data() {
        assert_eq!(
            progress(Some(0.5), Bar::Ascii),
            "[##########----------] 50.0%"
        );
        let crabs = progress(Some(0.5), Bar::Crabs);
        assert_eq!(crabs.matches('🦀').count(), 10);
        assert_eq!(crabs.matches('░').count(), 20);
        assert!(progress(None, Bar::Blocks).ends_with("N/A"));
    }
}
