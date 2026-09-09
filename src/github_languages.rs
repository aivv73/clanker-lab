//! Fast language approximation through a GitHub-compatible Languages API.
//!
//! Unlike the local Linguist adapter, this provider downloads nothing: it
//! reads byte counts from `{api-root}/repos/{owner}/{repo}/languages`. The
//! endpoint always describes upstream HEAD, never the packaged revision, so
//! every stored profile carries `current-upstream` fidelity and reports count
//! it as approximate evidence, never as exact analysis. Paclang never
//! overwrites an exact profile with an approximation.

use std::io::Write;

use crate::arch_provider::http::{FetchError, MAX_CONCURRENT_FETCHES, fetch_all_api};
use crate::interrupt;
use crate::package::provider::AnalysisTarget;
use crate::storage::{RefreshTransaction, StorageError};
use clap::ValueEnum;
use url::Url;

/// Language-analysis policy for a sources-only refresh.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum AnalysisMode {
    /// Never download sources; approximate GitHub-hosted targets through the
    /// Languages API and leave everything else missing.
    Fast,
    /// Always acquire packaged revisions and analyze them locally.
    Exact,
    /// Reuse stored exact profiles, approximate GitHub-hosted targets without
    /// one, and acquire plus analyze everything else.
    Auto,
}

impl AnalysisMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Exact => "exact",
            Self::Auto => "auto",
        }
    }
}

impl std::fmt::Display for AnalysisMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// GitHub-compatible API root for fast language analysis. HTTPS is required
/// except for literal loopback HTTP addresses used by local fixtures.
/// Redirects and URL credentials are not used.
#[derive(Clone, Debug)]
pub(crate) struct GithubApiEndpoint(Url);

impl std::str::FromStr for GithubApiEndpoint {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let url = Url::parse(value).map_err(|_| "GitHub API must be an absolute URL")?;
        let loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "GitHub API requires HTTPS without credentials, query, or fragment (HTTP is allowed only for literal loopback addresses)",
            );
        }
        Ok(Self(url))
    }
}

impl GithubApiEndpoint {
    fn languages_url(&self, owner: &str, repository: &str) -> Url {
        let mut url = self.0.clone();
        // Constructor permits only hierarchical HTTP(S) URLs.
        url.path_segments_mut()
            .expect("HTTP URL has path segments")
            .pop_if_empty()
            .extend(["repos", owner, repository, "languages"]);
        url
    }
}

/// Split a source location into a GitHub `(owner, repository)` pair. Only
/// plain `https://github.com/owner/repo` locations (with an optional `.git`
/// suffix) qualify; tarballs, subpaths, credentials, and other hosts never
/// become API queries.
pub(super) fn parse_github_repo(location: &str) -> Option<(String, String)> {
    let url = Url::parse(location).ok()?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    if !url
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case("github.com"))
    {
        return None;
    }
    let mut segments = url.path_segments()?;
    let owner = segments.next()?;
    let mut repository = segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    repository = repository.strip_suffix(".git").unwrap_or(repository);
    if !valid_name(owner) || !valid_name(repository) {
        return None;
    }
    Some((owner.to_owned(), repository.to_owned()))
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Per-pass statistics for the fast language provider. `attempted` counts
/// HTTP requests actually fired; breaker-skipped targets never become
/// requests and carry no stored record, so the next refresh retries them.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct ApiSummary {
    pub attempted: u64,
    pub approximated: u64,
    pub failed: u64,
    pub skipped_exact: u64,
    pub skipped_rate_limited: u64,
}

impl ApiSummary {
    pub(crate) fn is_empty(&self) -> bool {
        self.attempted == 0
            && self.approximated == 0
            && self.failed == 0
            && self.skipped_exact == 0
            && self.skipped_rate_limited == 0
    }
}

/// GitHub rate limiting leaves no room for doubt: the first throttled
/// response trips the circuit breaker and every unfired request is skipped
/// without a stored record.
const RATE_LIMIT_REASON: &str = "GitHub language API rate limit exceeded";

/// Approximate every GitHub-hosted target through the Languages API and store
/// the result. Returns the targets that gained a usable approximation
/// alongside the pass statistics. Targets with a stored exact profile are
/// never touched; failures are data recorded per target, while interruption
/// aborts the whole refresh.
pub(crate) fn record_api_profiles<W: Write>(
    refresh: &RefreshTransaction<'_>,
    targets: &[AnalysisTarget],
    api: &GithubApiEndpoint,
    proxy: Option<&crate::proxy::ProxyUrl>,
    auth: Option<&str>,
    progress: &mut crate::progress::Progress<W>,
) -> Result<(Vec<AnalysisTarget>, ApiSummary), StorageError> {
    let mut summary = ApiSummary::default();
    let mut work = Vec::new();
    for target in targets {
        if interrupt::is_requested() {
            return Err(StorageError::Interrupted);
        }
        if exact_profile_present(refresh, target)? {
            summary.skipped_exact += 1;
            continue;
        }
        // The Languages API describes a whole repository, never a selected
        // packaging directory within it.
        if target.subdirectory.is_some() { continue; }
        let Some((owner, repository)) = parse_github_repo(&target.location) else {
            continue;
        };
        work.push((target.clone(), api.languages_url(&owner, &repository)));
    }
    progress.set_total(work.len() as u64);
    if work.is_empty() {
        return Ok((Vec::new(), summary));
    }
    let mut approximated = Vec::new();
    let mut tripped = false;
    // Redirect hops re-enter the queue; three hops settle renames, anything
    // deeper is recorded as a failure instead of looping forever.
    let mut round = 0u8;
    let mut current = work;
    while !current.is_empty() {
        round += 1;
        if round > 4 {
            for (target, _) in &current {
                refresh.record_language_profile(
                    target,
                    &api_failure("GitHub language redirect did not settle", api),
                )?;
                summary.failed += 1;
                progress.advance(1, 0, 0, 1);
            }
            break;
        }
        let mut next = Vec::new();
        for chunk in current.chunks(MAX_CONCURRENT_FETCHES) {
            if tripped {
                for _ in chunk {
                    summary.skipped_rate_limited += 1;
                    progress.advance(1, 0, 0, 1);
                }
                continue;
            }
            if interrupt::is_requested() {
                return Err(StorageError::Interrupted);
            }
            let urls: Vec<Url> = chunk.iter().map(|(_, url)| url.clone()).collect();
            summary.attempted += urls.len() as u64;
            // Only interruption escapes the batch; any other outer error
            // becomes a per-target failure like every other provider error.
            let responses = match fetch_all_api(&urls, proxy, auth) {
                Ok(results) => results,
                Err(FetchError::Interrupted) => return Err(StorageError::Interrupted),
                Err(_) => urls
                    .iter()
                    .map(|_| Err(FetchError::Failed("language API batch failed")))
                    .collect(),
            };
            for ((target, url), response) in chunk.iter().zip(responses) {
                if interrupt::is_requested() {
                    return Err(StorageError::Interrupted);
                }
                // Re-check: an exact profile may have landed while fetching.
                if exact_profile_present(refresh, target)? {
                    summary.skipped_exact += 1;
                    progress.advance(1, 0, 0, 1);
                    continue;
                }
                match response {
                    Ok(bytes) => {
                        let record = api_record(&bytes, api);
                        let usable = record.state == "analyzed";
                        refresh.record_language_profile(target, &record)?;
                        if usable {
                            summary.approximated += 1;
                            approximated.push(target.clone());
                        } else {
                            summary.failed += 1;
                        }
                        progress.advance(1, 0, u64::from(usable), u64::from(!usable));
                    }
                    Err(FetchError::RateLimited(_) | FetchError::Http(403)) => {
                        tripped = true;
                        refresh.record_language_profile(
                            target,
                            &api_failure(RATE_LIMIT_REASON, api),
                        )?;
                        summary.failed += 1;
                        progress.advance(1, 0, 0, 1);
                    }
                    Err(FetchError::Redirect(location)) => {
                        match resolve_redirect(api, url, &location) {
                            Ok(resolved) => next.push((target.clone(), resolved)),
                            Err(reason) => {
                                refresh
                                    .record_language_profile(target, &api_failure(reason, api))?;
                                summary.failed += 1;
                                progress.advance(1, 0, 0, 1);
                            }
                        }
                    }
                    Err(FetchError::Http(404)) => {
                        refresh.record_language_profile(
                            target,
                            &api_failure("GitHub repository is not public or does not exist", api),
                        )?;
                        summary.failed += 1;
                        progress.advance(1, 0, 0, 1);
                    }
                    Err(FetchError::Http(status)) => {
                        refresh.record_language_profile(
                            target,
                            &api_failure(
                                &format!("GitHub language API returned HTTP {status}"),
                                api,
                            ),
                        )?;
                        summary.failed += 1;
                        progress.advance(1, 0, 0, 1);
                    }
                    Err(FetchError::Curl(code)) => {
                        refresh.record_language_profile(
                            target,
                            &api_failure(
                                &format!("GitHub language transfer failed (curl code {code})"),
                                api,
                            ),
                        )?;
                        summary.failed += 1;
                        progress.advance(1, 0, 0, 1);
                    }
                    Err(FetchError::Failed(reason)) => {
                        refresh.record_language_profile(target, &api_failure(reason, api))?;
                        summary.failed += 1;
                        progress.advance(1, 0, 0, 1);
                    }
                    Err(FetchError::Interrupted) => return Err(StorageError::Interrupted),
                }
            }
        }
        if tripped {
            // Breaker-skipped redirects never fire: they stay unrecorded so
            // the next refresh retries them instead of fossilizing a failure.
            for _ in next {
                summary.skipped_rate_limited += 1;
                progress.advance(1, 0, 0, 1);
            }
            break;
        }
        current = next;
    }
    Ok((approximated, summary))
}

/// Follow a Languages API redirect only within the configured API root.
/// Anything leaving the scheme and host (login pages, asset mirrors, renamed
/// hosts) stays a failure rather than leaking the request elsewhere.
fn resolve_redirect(
    api: &GithubApiEndpoint,
    current: &Url,
    location: &str,
) -> Result<Url, &'static str> {
    let resolved = current
        .join(location)
        .map_err(|_| "GitHub language redirect is not a valid URL")?;
    if resolved.scheme() != api.0.scheme() || resolved.host_str() != api.0.host_str() {
        return Err("GitHub language redirect leaves the API host");
    }
    Ok(resolved)
}

pub(crate) fn exact_profile_present(
    refresh: &RefreshTransaction<'_>,
    target: &AnalysisTarget,
) -> Result<bool, StorageError> {
    Ok(refresh.language_profile(target)?.is_some_and(|profile| {
        profile.state == "analyzed"
            && profile.fidelity == crate::linguist::RevisionFidelity::ExactPackagedSource
    }))
}

fn api_record(bytes: &[u8], api: &GithubApiEndpoint) -> crate::linguist::LanguageRecord {
    let adapter = api.0.as_str().to_owned();
    match std::str::from_utf8(bytes) {
        Ok(text) => match crate::linguist::parse_breakdown(text) {
            Ok(languages) if languages.is_empty() => crate::linguist::LanguageRecord {
                state: "no_language".to_owned(),
                reason: None,
                total_bytes: 0,
                primary_language: None,
                languages_json: "[]".to_owned(),
                adapter,
                source: crate::linguist::ProfileSource::GithubApi,
                fidelity: crate::linguist::RevisionFidelity::CurrentUpstream,
            },
            Ok(languages) => {
                let profile = crate::linguist::LanguageProfile::from_bytes(languages);
                crate::linguist::LanguageRecord {
                    state: "analyzed".to_owned(),
                    reason: None,
                    total_bytes: profile.total_bytes,
                    primary_language: profile.primary_language.clone(),
                    languages_json: serde_json::to_string(&profile.languages)
                        .unwrap_or_else(|_| "[]".to_owned()),
                    adapter,
                    source: crate::linguist::ProfileSource::GithubApi,
                    fidelity: crate::linguist::RevisionFidelity::CurrentUpstream,
                }
            }
            Err(reason) => api_failure(&reason, api),
        },
        Err(_) => api_failure("GitHub language response is not UTF-8", api),
    }
}

fn api_failure(reason: &str, api: &GithubApiEndpoint) -> crate::linguist::LanguageRecord {
    crate::linguist::LanguageRecord {
        state: "failed".to_owned(),
        reason: Some(reason.chars().take(512).collect()),
        total_bytes: 0,
        primary_language: None,
        languages_json: "[]".to_owned(),
        adapter: api.0.as_str().to_owned(),
        source: crate::linguist::ProfileSource::GithubApi,
        fidelity: crate::linguist::RevisionFidelity::CurrentUpstream,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_github_repo_locations_qualify() {
        assert_eq!(
            parse_github_repo("https://github.com/CachyOS/CachyOS-PKGBUILDS"),
            Some(("CachyOS".to_owned(), "CachyOS-PKGBUILDS".to_owned()))
        );
        assert_eq!(
            parse_github_repo("https://github.com/owner/repo.git"),
            Some(("owner".to_owned(), "repo".to_owned()))
        );
        for location in [
            "https://github.com/owner/repo/tree/main/src",
            "https://github.com/owner",
            "https://github.com/",
            "https://user:pass@github.com/owner/repo",
            "http://github.com/owner/repo",
            "https://gitlab.com/owner/repo",
            "https://github.com.evil.invalid/owner/repo",
            "https://example.invalid/owner/repo.tar.gz",
            "https://github.com/owner/repo?tab=readme",
            "not a url",
            "",
        ] {
            assert_eq!(parse_github_repo(location), None, "{location}");
        }
    }

    #[test]
    fn languages_url_appends_the_api_path() {
        let api: GithubApiEndpoint = "https://api.github.com".parse().unwrap();
        assert_eq!(
            api.languages_url("owner", "repo").as_str(),
            "https://api.github.com/repos/owner/repo/languages"
        );
        assert!(
            "https://api.github.com/"
                .parse::<GithubApiEndpoint>()
                .is_ok()
        );
        assert!(
            "http://api.github.com"
                .parse::<GithubApiEndpoint>()
                .is_err()
        );
    }

    #[test]
    fn redirects_stay_within_the_api_root() {
        let api: GithubApiEndpoint = "https://api.github.com".parse().unwrap();
        let current: Url = "https://api.github.com/repos/example/old/languages"
            .parse()
            .unwrap();
        assert_eq!(
            resolve_redirect(&api, &current, "/repos/example/new/languages").unwrap(),
            "https://api.github.com/repos/example/new/languages"
                .parse()
                .unwrap()
        );
        for location in [
            "https://example.invalid/repos/evil/repo/languages",
            "http://api.github.com/repos/example/new/languages",
            "https://api.github.com.evil.invalid/repos/example/new/languages",
        ] {
            assert!(resolve_redirect(&api, &current, location).is_err());
        }
    }
}
