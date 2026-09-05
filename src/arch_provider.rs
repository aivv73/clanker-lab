mod http;
mod recipe;
#[cfg(test)]
mod tests;

pub(crate) use http::RecipeEndpoint;

use std::collections::BTreeMap;
use std::io;

use crate::interrupt;
use crate::inventory::InstalledPackage;
use crate::package::provider::{AttemptOutcome, ProviderAttempt, SourceProvider};
use crate::progress::Progress;
use crate::storage::{RefreshTransaction, StorageError};

/// Groups fetched, decoded, and recorded per batch. Bounds retained
/// response bytes alongside [`http::MAX_CONCURRENT_FETCHES`].
const GROUP_BATCH: usize = 32;

/// Fetch once per package base/version, then record each split-package member.
/// Members with an already-resolved attempt reuse the stored evidence without
/// refetching; unresolved attempts are retried. At most [`GROUP_BATCH`] fetched
/// documents are retained in memory at once.
pub(crate) fn record_attempts<'a, W: io::Write>(
    refresh: &RefreshTransaction<'_>,
    packages: impl IntoIterator<Item = &'a InstalledPackage>,
    endpoint: &RecipeEndpoint,
    progress: &mut Progress<W>,
    require_checksums: bool,
) -> Result<(), StorageError> {
    let provider = SourceProvider::parse("arch").expect("static provider name is valid");
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for package in packages {
        groups
            .entry((
                package.package_base().unwrap_or(package.name()),
                package.version(),
            ))
            .or_default()
            .push(package);
    }
    // Partition first so one batch fetch covers every group that needs it.
    // `fresh` holds members without stored resolved evidence; resolved
    // members keep their rows untouched.
    let mut pending = Vec::new();
    let mut skipped = 0u64;
    for ((base, version), members) in &groups {
        if interrupt::is_requested() {
            return Err(StorageError::Interrupted);
        }
        let mut fresh = Vec::new();
        for package in members {
            if refresh.has_resolved_attempt(package.name(), package.version(), &provider)?
                && !(require_checksums && refresh.provider_attempts(package.name(), package.version())?.iter().any(|attempt|
                    attempt.provider == provider && matches!(&attempt.outcome, AttemptOutcome::Resolved { target, .. } if target.revision.is_none() && target.checksum.is_none()))) {
                continue;
            }
            fresh.push(*package);
        }
        if fresh.is_empty() {
            skipped += 1;
            continue;
        }
        let project = format!(
            "archlinux/packaging/packages/{}",
            recipe::project_path(base)
        );
        let tag = version.replacen(':', "-", 1).replace('~', ".");
        pending.push((endpoint.file_url(&project, &tag), fresh));
    }
    progress.set_total(groups.len() as u64);
    progress.advance(skipped, 0, 0, 0);
    // Groups whose only fresh members have unsupported origins need no fetch.
    // Batches bound the retained documents; interruption aborts the refresh
    // and rolls every batch back with the surrounding transaction.
    while !pending.is_empty() {
        let take = pending.len().min(GROUP_BATCH);
        let batch: Vec<_> = pending.drain(..take).collect();
        let mut urls = Vec::with_capacity(batch.len());
        let mut fetch_index: Vec<Option<usize>> = Vec::with_capacity(batch.len());
        for (url, fresh) in &batch {
            if fresh.iter().any(|package| package.origin() == "unknown") {
                fetch_index.push(Some(urls.len()));
                urls.push(url.clone());
            } else {
                fetch_index.push(None);
            }
        }
        // Only interruption escapes the batch; any other outer error is
        // recorded as an unresolved reason for every group in the batch.
        let fetched = match http::fetch_all(&urls) {
            Ok(results) => results,
            Err(http::FetchError::Interrupted) => return Err(StorageError::Interrupted),
            Err(error) => urls.iter().map(|_| Err(error.clone())).collect(),
        };
        for ((url, fresh), index) in batch.into_iter().zip(fetch_index) {
            if interrupt::is_requested() {
                return Err(StorageError::Interrupted);
            }
            let mut downloaded = 0u64;
            let fetched: Result<recipe::Recipe, String> = match index {
                None => Err("unsupported package origin".to_owned()),
                Some(position) => match &fetched[position] {
                    Ok(bytes) => {
                        downloaded = bytes.len() as u64;
                        recipe::Recipe::decode(bytes, &url).map_err(str::to_owned)
                    }
                    Err(http::FetchError::Interrupted) => return Err(StorageError::Interrupted),
                    Err(error) => Err(error.to_string()),
                },
            };
            let mut resolved = 0u64;
            let mut unresolved = 0u64;
            for package in fresh {
                let (outcome, recipe) = if package.origin() != "unknown" {
                    (
                        AttemptOutcome::Unresolved {
                            reason: "unsupported package origin".to_owned(),
                        },
                        None,
                    )
                } else {
                    match &fetched {
                        Ok(recipe) => (
                            recipe.resolve(package.into()),
                            Some(recipe.provenance().clone()),
                        ),
                        Err(reason) => (
                            AttemptOutcome::Unresolved {
                                reason: reason.clone(),
                            },
                            None,
                        ),
                    }
                };
                match &outcome {
                    AttemptOutcome::Resolved { .. } => resolved += 1,
                    AttemptOutcome::Unresolved { .. } => unresolved += 1,
                }
                refresh.record_provider_attempt(&ProviderAttempt {
                    package_name: package.name().to_owned(),
                    package_version: package.version().to_owned(),
                    provider: provider.clone(),
                    outcome,
                    recipe,
                })?;
            }
            progress.advance(1, downloaded, resolved, unresolved);
        }
    }
    Ok(())
}
