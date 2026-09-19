use super::{Format, PopulationSelection, RenderError, SnapshotStaleness};
use crate::payload::{PackageRow, Report, Summary};
use serde::Serialize;

#[derive(Serialize)]
struct Structured<'a> {
    schema_version: &'static str,
    report: &'static str,
    snapshot_staleness: SnapshotStaleness,
    selection: &'a PopulationSelection,
    #[serde(flatten)]
    summary: &'a Summary,
    packages: &'a [PackageRow],
}

pub(crate) fn render(
    report: &Report,
    format: Format,
    staleness: SnapshotStaleness,
    selection: &PopulationSelection,
    all: bool,
    top: Option<usize>,
) -> Result<String, RenderError> {
    let structured = Structured {
        schema_version: "1.0.0",
        report: "blobs",
        snapshot_staleness: staleness,
        selection,
        summary: &report.summary,
        packages: &report.packages,
    };
    match format {
        Format::Json => serde_json::to_string_pretty(&structured).map_err(RenderError::Json),
        Format::Toon => {
            #[derive(Serialize)]
            struct Toon<'a, 'b> {
                toon_specification_version: &'static str,
                #[serde(flatten)]
                report: &'a Structured<'b>,
            }
            toon_format::encode_default(&Toon {
                toon_specification_version: "3.0",
                report: &structured,
            })
            .map_err(RenderError::Toon)
        }
        Format::Terminal | Format::Markdown => {
            let mut text = format!(
                "Opaque installed payload: {}\n\n{}Snapshot: {}\n\n",
                super::source_bytes::format_human_bytes(report.summary.opaque_bytes),
                report.summary.human(),
                staleness.as_str()
            );
            let rows: Vec<_> = report
                .packages
                .iter()
                .filter(|r| all || r.opaque_bytes > 0)
                .collect();
            let limit = if all {
                rows.len()
            } else {
                top.unwrap_or(10).min(rows.len())
            };
            for row in &rows[..limit] {
                text.push_str(&format!(
                    "{}  opaque: {} / installed: {}  {} ({})\n",
                    row.name,
                    super::source_bytes::format_human_bytes(row.opaque_bytes),
                    super::source_bytes::format_human_bytes(row.installed_bytes),
                    row.classification.as_str(),
                    row.provenance_state
                ));
                for reason in &row.reasons {
                    text.push_str(&format!("  {reason}\n"));
                }
            }
            if rows.is_empty() {
                text.push_str("No confirmed opaque bytes in the recorded evidence.\n");
            }
            if rows.len() > limit {
                text.push_str(&format!(
                    "... {} more packages; use --all.\n",
                    rows.len() - limit
                ));
            }
            text.push_str("\nConfirmed: installed ELF matches a verified prebuilt archive, with no recorded source representation.\nSuspected bytes stay unknown. Prebuilt does not imply proprietary.\nCollect evidence with refresh --sources-only --analysis-mode exact and a configured Linguist.\n");
            if format == Format::Markdown {
                Ok(format!("```text\n{text}```\n"))
            } else {
                Ok(text)
            }
        }
    }
}
