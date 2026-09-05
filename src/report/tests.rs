use proptest::prelude::*;

use super::{
    AppError, CountReport, Format, InstallReasonSelection, PopulationSelection, SnapshotStaleness,
    format_tenths, percentage_tenths,
};
use crate::package::{InstallReason, PackageOrigin, SnapshotPackage};

fn package(origin: &str, install_reason: InstallReason) -> SnapshotPackage {
    SnapshotPackage {
        origin: PackageOrigin::parse(origin).unwrap(),
        install_reason,
    }
}

#[test]
fn human_percentages_render_with_one_decimal() {
    assert_eq!(format_tenths(1000), "100.0%");
    assert_eq!(format_tenths(0), "0.0%");
    assert_eq!(format_tenths(333), "33.3%");
}

#[test]
fn redirected_stdout_selects_toon() {
    assert_eq!(Format::for_terminal(false), Format::Toon);
}

#[test]
fn tty_stdout_selects_terminal() {
    assert_eq!(Format::for_terminal(true), Format::Terminal);
}

#[test]
fn json_error_has_stable_schema() {
    let rendered = Format::Json.render_error(&AppError::NoSnapshot).unwrap();
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

    assert_eq!(value["schema_version"], "1.4.0");
    assert_eq!(value["error"]["code"], "no_snapshot");
    assert_eq!(
        value["error"]["next_command"],
        "paclang refresh --metadata-only"
    );
}

#[test]
fn toon_serializer_escapes_grammar_characters() {
    let error = AppError::storage("line one:\n\"quoted\" \\ path");
    let rendered = Format::Toon.render_error(&error).unwrap();
    let value: serde_json::Value = toon_format::decode_default(&rendered).unwrap();

    assert_eq!(value["error"]["message"], "line one:\n\"quoted\" \\ path");
}

#[test]
fn json_count_has_honest_missing_analysis_coverage() {
    let packages = (0..7)
        .map(|_| package("unknown", InstallReason::Explicit))
        .collect::<Vec<_>>();
    let report = CountReport::from_snapshot(
        &packages,
        SnapshotStaleness::Current,
        PopulationSelection::new(InstallReasonSelection::All, []),
    );
    let rendered = Format::Json.render_count(&report).unwrap();
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

    assert_eq!(value["report"], "count");
    assert_eq!(value["snapshot_staleness"], "current");
    assert_eq!(value["installed_total"], 7);
    assert_eq!(value["selected_population"], 7);
    assert_eq!(value["selection"]["install_reason"], "all");
    assert_eq!(value["selection"]["origins"], serde_json::json!([]));
    assert_eq!(value["coverage"]["denominator"], 7);
    assert_eq!(value["coverage"]["analyzed"], 0);
    assert_eq!(value["coverage"]["missing_source_language_analysis"], 7);
    assert_eq!(
        value["coverage"]["rows"],
        serde_json::json!([
            {"state": "missing_source_language_analysis", "count": 7},
            {"state": "analyzed", "count": 0},
        ])
    );
    assert!(value.get("languages").is_none());
}

#[test]
fn structured_count_rows_sort_by_count_then_name() {
    let report = CountReport::from_snapshot(
        &[],
        SnapshotStaleness::Current,
        PopulationSelection::new(InstallReasonSelection::All, []),
    );
    let rendered = Format::Json.render_count(&report).unwrap();
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

    // Both rows are zero, so the canonical name order decides.
    assert_eq!(
        value["coverage"]["rows"],
        serde_json::json!([
            {"state": "analyzed", "count": 0},
            {"state": "missing_source_language_analysis", "count": 0},
        ])
    );
}

#[test]
fn terminal_percentages_total_one_hundred() {
    let packages = (0..3)
        .map(|_| package("unknown", InstallReason::Explicit))
        .collect::<Vec<_>>();
    let report = CountReport::from_snapshot(
        &packages,
        SnapshotStaleness::Current,
        PopulationSelection::new(InstallReasonSelection::All, []),
    );
    let rendered = Format::Terminal.render_count(&report).unwrap();

    assert!(rendered.contains("Missing source-language analysis: 3 (100.0%)"));
    assert!(rendered.contains("Analyzed: 0 (0.0%)"));
}

#[test]
fn empty_selection_reports_zero_percentages() {
    let packages = vec![package("unknown", InstallReason::Explicit)];
    let report = CountReport::from_snapshot(
        &packages,
        SnapshotStaleness::Current,
        PopulationSelection::new(InstallReasonSelection::Dependency, []),
    );
    let rendered = Format::Json.render_count(&report).unwrap();
    let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();

    assert_eq!(value["selected_population"], 0);
    assert_eq!(value["coverage"]["denominator"], 0);
    let terminal = Format::Terminal.render_count(&report).unwrap();
    assert!(terminal.contains("(0.0%)"));
    assert!(!terminal.contains("(100.0%)"));
    // An empty selection is vacuously complete.
    assert!(report.meets_coverage(Some(100.0)));
}

#[test]
fn toon_report_declares_the_pinned_specification_version() {
    let packages = vec![package("unknown", InstallReason::Explicit)];
    let report = CountReport::from_snapshot(
        &packages,
        SnapshotStaleness::Current,
        PopulationSelection::new(InstallReasonSelection::All, []),
    );
    let rendered = Format::Toon.render_count(&report).unwrap();
    let value: serde_json::Value = toon_format::decode_default(&rendered).unwrap();

    assert_eq!(value["schema_version"], "1.4.0");
    assert_eq!(value["toon_specification_version"], "3.0");
    assert_eq!(
        value["coverage"]["rows"],
        serde_json::json!([
            {"state": "missing_source_language_analysis", "count": 1},
            {"state": "analyzed", "count": 0},
        ])
    );
}

#[test]
fn coverage_threshold_compares_the_exact_analyzed_share() {
    let packages = (0..4)
        .map(|_| package("unknown", InstallReason::Explicit))
        .collect::<Vec<_>>();
    let report = CountReport::from_snapshot(
        &packages,
        SnapshotStaleness::Current,
        PopulationSelection::new(InstallReasonSelection::All, []),
    );

    assert_eq!(report.analyzed_coverage_percent(), 0.0);
    assert!(report.meets_coverage(None));
    assert!(report.meets_coverage(Some(0.0)));
    assert!(!report.meets_coverage(Some(0.1)));
}

fn selection_count(
    packages: &[SnapshotPackage],
    install_reason: InstallReasonSelection,
    origins: impl IntoIterator<Item = PackageOrigin>,
) -> u64 {
    CountReport::from_snapshot(
        packages,
        SnapshotStaleness::Current,
        PopulationSelection::new(install_reason, origins),
    )
    .selected_population
}

proptest! {
    #[test]
    fn filters_conserve_the_selected_population(
        packages in proptest::collection::vec((any::<bool>(), 0_u8..4), 0..100),
        selected_origins in proptest::collection::vec(0_u8..4, 0..10),
    ) {
        let packages = packages
            .into_iter()
            .map(|(explicit, origin)| {
                let reason = if explicit {
                    InstallReason::Explicit
                } else {
                    InstallReason::Dependency
                };
                package(&format!("origin-{origin}"), reason)
            })
            .collect::<Vec<_>>();
        let origins = selected_origins
            .into_iter()
            .map(|origin| PackageOrigin::parse(&format!("origin-{origin}")).unwrap())
            .collect::<Vec<_>>();

        let all = selection_count(&packages, InstallReasonSelection::All, origins.clone());
        let explicit = selection_count(
            &packages,
            InstallReasonSelection::Explicit,
            origins.clone(),
        );
        let dependency = selection_count(
            &packages,
            InstallReasonSelection::Dependency,
            origins,
        );

        prop_assert!(all <= packages.len() as u64);
        prop_assert_eq!(all, explicit + dependency);
    }

    #[test]
    fn filtering_is_invariant_to_package_and_origin_input_permutations(
        packages in proptest::collection::vec((any::<bool>(), 0_u8..4, any::<u64>()), 0..100),
        selected_origins in proptest::collection::vec((0_u8..4, any::<u64>()), 0..10),
    ) {
        let original_packages = packages
            .iter()
            .map(|(explicit, origin, _)| {
                let reason = if *explicit {
                    InstallReason::Explicit
                } else {
                    InstallReason::Dependency
                };
                package(&format!("origin-{origin}"), reason)
            })
            .collect::<Vec<_>>();
        let original_origins = selected_origins
            .iter()
            .map(|(origin, _)| PackageOrigin::parse(&format!("origin-{origin}")).unwrap())
            .collect::<Vec<_>>();

        let mut permuted_packages = packages;
        permuted_packages.sort_by_key(|(_, _, key)| *key);
        let permuted_packages = permuted_packages
            .into_iter()
            .map(|(explicit, origin, _)| {
                let reason = if explicit {
                    InstallReason::Explicit
                } else {
                    InstallReason::Dependency
                };
                package(&format!("origin-{origin}"), reason)
            })
            .collect::<Vec<_>>();
        let mut permuted_origins = selected_origins;
        permuted_origins.sort_by_key(|(_, key)| *key);
        let permuted_origins = permuted_origins
            .into_iter()
            .map(|(origin, _)| PackageOrigin::parse(&format!("origin-{origin}")).unwrap())
            .collect::<Vec<_>>();
        let first = selection_count(
            &original_packages,
            InstallReasonSelection::All,
            original_origins,
        );
        let permuted = selection_count(
            &permuted_packages,
            InstallReasonSelection::All,
            permuted_origins,
        );

        prop_assert_eq!(first, permuted);
    }

    #[test]
    fn rounded_percentages_conserve_the_population_and_total_one_hundred(
        counts in proptest::collection::vec(0_u64..10_000, 0..12),
    ) {
        let tenths = percentage_tenths(&counts);
        let total: u64 = counts.iter().sum();

        prop_assert_eq!(tenths.len(), counts.len());
        if total == 0 {
            prop_assert!(tenths.iter().all(|share| *share == 0));
        } else {
            prop_assert_eq!(tenths.iter().sum::<u64>(), 1000);
            // Deterministic: the same input always rounds the same way.
            prop_assert_eq!(percentage_tenths(&counts), tenths.clone());
            // Largest remainder never moves a share more than one tenth away
            // from its exact value.
            for (count, share) in counts.iter().zip(&tenths) {
                let exact = *count as f64 * 1000.0 / total as f64;
                prop_assert!((*share as f64 - exact).abs() < 1.0);
            }
        }
    }

    #[test]
    fn coverage_rows_conserve_the_selected_population_and_stay_sorted(
        packages in proptest::collection::vec(any::<bool>(), 0..100),
    ) {
        let packages = packages
            .into_iter()
            .map(|explicit| {
                package(
                    "unknown",
                    if explicit {
                        InstallReason::Explicit
                    } else {
                        InstallReason::Dependency
                    },
                )
            })
            .collect::<Vec<_>>();
        let report = CountReport::from_snapshot(
            &packages,
            SnapshotStaleness::Current,
            PopulationSelection::new(InstallReasonSelection::All, []),
        );
        let rendered = Format::Json.render_count(&report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let rows = value["coverage"]["rows"].as_array().unwrap();

        let total: u64 = rows
            .iter()
            .map(|row| row["count"].as_u64().unwrap())
            .sum();
        prop_assert_eq!(total, value["coverage"]["denominator"].as_u64().unwrap());
        prop_assert_eq!(total, packages.len() as u64);
        let mut sorted = rows.clone();
        sorted.sort_by(|left, right| {
            right["count"]
                .as_u64()
                .cmp(&left["count"].as_u64())
                .then_with(|| left["state"].as_str().cmp(&right["state"].as_str()))
        });
        prop_assert_eq!(rows, &sorted);
    }
}

#[test]
fn human_selection_rendering_distinguishes_special_origin_names() {
    let selection = PopulationSelection::new(
        InstallReasonSelection::All,
        [
            PackageOrigin::parse("all").unwrap(),
            PackageOrigin::parse("core, extra").unwrap(),
        ],
    );

    assert_eq!(
        selection.render(),
        "install reason = all; origins = [\"all\", \"core, extra\"]"
    );
}
