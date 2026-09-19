//! Analysis-mode policy for a sources-only refresh.
//!
//! `exact` acquires every resolved target and analyzes the packaged revision
//! locally. `fast` downloads nothing and approximates GitHub-hosted targets
//! through the Languages API. `auto` reuses stored exact profiles, fills
//! GitHub-hosted gaps through the API, and acquires plus analyzes the rest.
//! Approximations never overwrite exact profiles, in any mode.

use std::io::Write;

use crate::acquisition::{self, AcquisitionSummary};
use crate::github_languages::{self, AnalysisMode, GithubApiEndpoint};
use crate::interrupt;
use crate::storage::{RefreshTransaction, StorageError};

pub(crate) struct Policy<'a> {
    pub limits: &'a acquisition::Limits,
    pub linguist: &'a crate::linguist::LinguistConfig,
    pub mode: AnalysisMode,
    pub github_api: &'a GithubApiEndpoint,
    /// Bearer token for the Languages API, read once from
    /// `PACLANG_GITHUB_TOKEN`. Never a CLI flag: process arguments stay
    /// out of the secret's path.
    pub github_token: Option<String>,
    pub proxy: Option<&'a crate::proxy::ProxyUrl>,
}

/// The refresh-stage outcome: acquisition results plus the optional fast
/// approximation statistics. Approximations are reported even when nothing
/// was downloaded, so both summaries travel together.
pub(crate) struct AnalysisOutcome {
    pub acquisition: AcquisitionSummary,
    pub approximation: Option<crate::github_languages::ApiSummary>,
}

pub(crate) fn run<'a, W: Write>(
    refresh: &RefreshTransaction<'_>,
    packages: impl IntoIterator<Item = &'a crate::inventory::InstalledPackage>,
    policy: Policy<'_>,
    progress: &mut crate::progress::Progress<W>,
) -> Result<AnalysisOutcome, StorageError> {
    let packages: Vec<&crate::inventory::InstalledPackage> = packages.into_iter().collect();
    match policy.mode {
        AnalysisMode::Fast => {
            let (targets, unresolved_packages, source_free_packages) =
                acquisition::collect_targets(refresh, packages.iter().copied())?;
            let mut api_progress = crate::progress::Progress::stderr(
                "Approximating languages",
                "Approximated",
                "Incomplete",
            );
            let result = github_languages::record_api_profiles(
                refresh,
                &targets,
                policy.github_api,
                policy.proxy,
                policy.github_token.as_deref(),
                &mut api_progress,
            );
            api_progress.finish();
            let (_, api_summary) = result?;
            Ok(AnalysisOutcome {
                acquisition: AcquisitionSummary {
                    targets: targets.len() as u64,
                    unresolved_packages,
                    source_free_packages,
                    ..Default::default()
                },
                approximation: Some(api_summary).filter(|summary| !summary.is_empty()),
            })
        }
        AnalysisMode::Exact => Ok(AnalysisOutcome {
            acquisition: acquisition::record_sources(
                refresh,
                packages.iter().copied(),
                policy.limits,
                policy.linguist,
                &[],
                progress,
                policy.proxy,
            )?,
            approximation: None,
        }),
        AnalysisMode::Auto => {
            let (targets, _, _) = acquisition::collect_targets(refresh, packages.iter().copied())?;
            let mut skip = Vec::new();
            for target in &targets {
                if interrupt::is_requested() {
                    return Err(StorageError::Interrupted);
                }
                if github_languages::exact_profile_present(refresh, target)? {
                    skip.push(target.clone());
                }
            }
            let mut api_progress = crate::progress::Progress::stderr(
                "Approximating languages",
                "Approximated",
                "Incomplete",
            );
            let result = github_languages::record_api_profiles(
                refresh,
                &targets,
                policy.github_api,
                policy.proxy,
                policy.github_token.as_deref(),
                &mut api_progress,
            );
            api_progress.finish();
            let (fresh, api_summary) = result?;
            skip.extend(fresh);
            Ok(AnalysisOutcome {
                acquisition: acquisition::record_sources(
                    refresh,
                    packages.iter().copied(),
                    policy.limits,
                    policy.linguist,
                    &skip,
                    progress,
                    policy.proxy,
                )?,
                approximation: Some(api_summary).filter(|summary| !summary.is_empty()),
            })
        }
    }
}
