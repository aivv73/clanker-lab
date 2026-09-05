use super::*;
use crate::package::provider::AttemptOutcome;
use crate::storage::PublishedSnapshot;

#[derive(Debug, Serialize)]
pub(crate) struct ProviderReport {
    schema_version: ReportSchemaVersion,
    report: ReportKind,
    snapshot_staleness: SnapshotStaleness,
    selection: PopulationSelection,
    installed_total: usize,
    selected_population: usize,
    resolved: usize,
    unresolved: usize,
    not_attempted: usize,
    packages: Vec<PackageRow>,
}

#[derive(Debug, Serialize)]
struct PackageRow {
    name: String,
    version: String,
    outcome: &'static str,
    attempts: Vec<AttemptRow>,
}

#[derive(Debug, Serialize)]
struct AttemptRow {
    provider: String,
    outcome: &'static str,
    grade: Option<&'static str>,
    failure_reason: Option<String>,
    target: Option<TargetRow>,
    recipe: Option<RecipeRow>,
    acquisition: Option<crate::acquisition::AcquisitionRecord>,
}

#[derive(Debug, Serialize)]
struct TargetRow {
    location: String,
    revision: Option<String>,
    subdirectory: Option<String>,
    checksum: Option<String>,
}

#[derive(Debug, Serialize)]
struct RecipeRow {
    location: String,
    commit: String,
}

impl ProviderReport {
    pub(crate) fn from_snapshot(
        snapshot: &PublishedSnapshot,
        snapshot_staleness: SnapshotStaleness,
        selection: PopulationSelection,
    ) -> Self {
        let mut report = Self {
            schema_version: ReportSchemaVersion::CURRENT,
            report: ReportKind::Providers,
            snapshot_staleness,
            selection,
            installed_total: snapshot.packages.len(),
            selected_population: 0,
            resolved: 0,
            unresolved: 0,
            not_attempted: 0,
            packages: Vec::new(),
        };
        for (package, evidence) in snapshot.packages.iter().zip(&snapshot.provider_packages) {
            if !report.selection.includes(package) {
                continue;
            }
            let mut attempts = Vec::new();
            for (attempt, acquisition) in evidence.attempts.iter().zip(&evidence.acquisitions) {
                let (outcome, grade, failure_reason, target) = match &attempt.outcome {
                    AttemptOutcome::Resolved { target, grade } => (
                        "resolved",
                        Some(grade.as_str()),
                        None,
                        Some(TargetRow {
                            location: target.location.clone(),
                            revision: target.revision.clone(),
                            subdirectory: target.subdirectory.clone(),
                            checksum: target.checksum.clone(),
                        }),
                    ),
                    AttemptOutcome::Unresolved { reason } => {
                        ("unresolved", None, Some(reason.clone()), None)
                    }
                };
                attempts.push(AttemptRow {
                    acquisition: acquisition.clone(),
                    provider: attempt.provider.as_str().to_owned(),
                    outcome,
                    grade,
                    failure_reason,
                    target,
                    recipe: attempt.recipe.as_ref().map(|recipe| RecipeRow {
                        location: recipe.location().to_owned(),
                        commit: recipe.commit().to_owned(),
                    }),
                });
            }
            // Any resolved candidate makes the package resolved; every provider
            // stays visible, without selecting a winner among candidates.
            let outcome = if attempts.iter().any(|attempt| attempt.outcome == "resolved") {
                report.resolved += 1;
                "resolved"
            } else if attempts.is_empty() {
                report.not_attempted += 1;
                "not_attempted"
            } else {
                report.unresolved += 1;
                "unresolved"
            };
            report.packages.push(PackageRow {
                name: evidence.name.clone(),
                version: evidence.version.clone(),
                outcome,
                attempts,
            });
        }
        report.selected_population = report.packages.len();
        report
    }

    pub(crate) fn render(&self, format: Format) -> Result<String, RenderError> {
        match format {
            Format::Json => serde_json::to_string_pretty(self).map_err(RenderError::Json),
            Format::Toon => {
                #[derive(Serialize)]
                struct ToonReport<'a> {
                    toon_specification_version: ToonSpecificationVersion,
                    #[serde(flatten)]
                    report: &'a ProviderReport,
                }
                toon_format::encode_default(&ToonReport {
                    toon_specification_version: ToonSpecificationVersion::CURRENT,
                    report: self,
                })
                .map_err(RenderError::Toon)
            }
            Format::Terminal | Format::Markdown => {
                Ok(self.render_human(format == Format::Markdown))
            }
        }
    }

    fn render_human(&self, markdown: bool) -> String {
        // Quote externally supplied strings: terminal controls cannot execute,
        // and Markdown metacharacters cannot alter the report structure.
        let quote = |value: &str| {
            let value = format!("{value:?}");
            if markdown {
                value
                    .chars()
                    .map(|ch| match ch {
                        '&' => "&amp;".to_owned(),
                        '<' => "&lt;".to_owned(),
                        '>' => "&gt;".to_owned(),
                        '\\' | '`' | '*' | '_' | '[' | ']' | '|' => format!("\\{ch}"),
                        _ => ch.to_string(),
                    })
                    .collect::<String>()
            } else {
                value
            }
        };
        let mut text = format!(
            "{}Snapshot: {}\nSelection: {}\nInstalled packages: {}\nSelected population: {}\nResolved: {}\nUnresolved: {}\nNot attempted: {}",
            if markdown {
                "# Paclang providers\n\n"
            } else {
                ""
            },
            self.snapshot_staleness.as_str(),
            quote(&self.selection.render()),
            self.installed_total,
            self.selected_population,
            self.resolved,
            self.unresolved,
            self.not_attempted,
        );
        for package in &self.packages {
            text.push_str(&format!(
                "\n\n{}{} {}: {}",
                if markdown { "## " } else { "" },
                quote(&package.name),
                quote(&package.version),
                package.outcome
            ));
            for attempt in &package.attempts {
                text.push_str(&format!(
                    "\n\n- Provider: {}\n- Outcome: {}",
                    quote(&attempt.provider),
                    attempt.outcome
                ));
                if let Some(grade) = attempt.grade {
                    text.push_str(&format!("\n- Grade: {grade}"));
                }
                if let Some(reason) = &attempt.failure_reason {
                    text.push_str(&format!("\n- Reason: {}", quote(reason)));
                }
                if let Some(target) = &attempt.target {
                    text.push_str(&format!("\n- Target: {}", quote(&target.location)));
                    if let Some(checksum) = &target.checksum {
                        text.push_str(&format!("\n- Recipe checksum: {}", quote(checksum)));
                    }
                    text.push_str(&format!(
                        "\n- Revision: {}\n- Subdirectory: {}",
                        target
                            .revision
                            .as_deref()
                            .map(&quote)
                            .unwrap_or_else(|| "none".to_owned()),
                        target
                            .subdirectory
                            .as_deref()
                            .map(&quote)
                            .unwrap_or_else(|| "none".to_owned())
                    ));
                }
                if let Some(acquisition) = &attempt.acquisition {
                    text.push_str(&format!("\n- Acquisition: {}\n- Downloaded bytes: {}\n- Extracted bytes: {}\n- Source entries: {}", acquisition.state, acquisition.downloaded_bytes, acquisition.extracted_bytes, acquisition.files));
                    if let Some(reason) = &acquisition.reason {
                        text.push_str(&format!("\n- Acquisition reason: {}", quote(reason)));
                    }
                    if let Some(revision) = &acquisition.verified_revision {
                        text.push_str(&format!("\n- Verified revision: {}", quote(revision)));
                    }
                    if let Some(digest) = &acquisition.content_sha256 {
                        text.push_str(&format!("\n- Content SHA-256: {}", quote(digest)));
                    }
                } else {
                    text.push_str("\n- Acquisition: not attempted");
                }
                match &attempt.recipe {
                    Some(recipe) => text.push_str(&format!(
                        "\n- Recipe: {}\n- Recipe commit: {}",
                        quote(&recipe.location),
                        recipe.commit
                    )),
                    None => text.push_str("\n- Recipe: unavailable"),
                }
            }
        }
        text
    }
}
