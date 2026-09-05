mod http;
mod recipe;
#[cfg(test)]
mod tests;

pub(crate) use http::RecipeEndpoint;

use std::collections::BTreeMap;

use crate::interrupt;
use crate::inventory::InstalledPackage;
use crate::package::provider::{AttemptOutcome, ProviderAttempt, SourceProvider};
use crate::storage::{RefreshTransaction, StorageError};

/// Fetch once per package base/version, then record each split-package member.
/// Only the bounded document for the current group is retained in memory.
pub(crate) fn record_attempts<'a>(
    refresh: &RefreshTransaction<'_>,
    packages: impl IntoIterator<Item = &'a InstalledPackage>,
    endpoint: &RecipeEndpoint,
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
    for ((base, version), members) in groups {
        if interrupt::is_requested() {
            return Err(StorageError::Interrupted);
        }
        let eligible = members.iter().any(|package| package.origin() == "unknown");
        let fetched = if eligible {
            let project = format!(
                "archlinux/packaging/packages/{}",
                recipe::project_path(base)
            );
            let tag = version.replacen(':', "-", 1).replace('~', ".");
            let url = endpoint.file_url(&project, &tag);
            match http::fetch(&url) {
                Ok(bytes) => recipe::Recipe::decode(&bytes, &url).map_err(str::to_owned),
                Err(http::FetchError::Interrupted) => return Err(StorageError::Interrupted),
                Err(error) => Err(error.to_string()),
            }
        } else {
            Err("unsupported package origin".to_owned())
        };
        for package in members {
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
            refresh.record_provider_attempt(&ProviderAttempt {
                package_name: package.name().to_owned(),
                package_version: package.version().to_owned(),
                provider: provider.clone(),
                outcome,
                recipe,
            })?;
        }
    }
    Ok(())
}
