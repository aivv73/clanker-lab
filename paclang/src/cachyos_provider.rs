//! Optional CachyOS recipes for packages the Arch provider could not resolve.
//! Metadata is read at one immutable repository commit; PKGBUILDs are never run.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};

use base64::Engine;
use serde::Deserialize;
use url::Url;

use crate::arch_provider::{
    RecipeEndpoint, http,
    recipe::{self, Recipe, RecipePackage},
};
use crate::inventory::InstalledPackage;
use crate::package::provider::{
    AttemptOutcome, EvidenceGrade, ProviderAttempt, RecipeProvenance, SourceProvider,
};
use crate::progress::Progress;
use crate::proxy::ProxyUrl;
use crate::storage::{RefreshTransaction, StorageError};

#[derive(Deserialize)]
struct Commit {
    sha: String,
}
#[derive(Deserialize)]
struct Tree {
    truncated: bool,
    tree: Vec<Entry>,
}
#[derive(Deserialize)]
struct Entry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}
#[derive(Deserialize)]
struct Blob {
    sha: String,
    encoding: String,
    content: String,
}

struct Repository {
    commit: String,
    bases: BTreeSet<String>,
    srcinfo: BTreeMap<String, (String, String)>,
    recipes: BTreeMap<String, Vec<u8>>,
}

fn endpoint_url(endpoint: &RecipeEndpoint, segments: &[&str]) -> Url {
    let mut url = endpoint.as_url().clone();
    url.path_segments_mut()
        .expect("validated HTTP URL")
        .pop_if_empty()
        .extend(segments.iter().copied());
    url
}

fn full_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn fetch(url: Url, proxy: Option<&ProxyUrl>) -> Result<Vec<u8>, http::FetchError> {
    http::fetch_all(&[url], proxy)?
        .pop()
        .expect("one request has one result")
}

// Git protocol v0 pkt-lines; only the exact branch ref is accepted, not HEAD,
// tags, capabilities, or a server-provided URL.
fn advertised_master(mut bytes: &[u8]) -> Result<String, &'static str> {
    let invalid = "invalid CachyOS Git reference advertisement";
    let mut master = None;
    while !bytes.is_empty() {
        let header = bytes.get(..4).ok_or(invalid)?;
        let length = usize::from_str_radix(std::str::from_utf8(header).map_err(|_| invalid)?, 16).map_err(|_| invalid)?;
        bytes = &bytes[4..];
        if length == 0 { continue; }
        if length < 4 { return Err(invalid); }
        let line = bytes.get(..length-4).ok_or(invalid)?;
        bytes = &bytes[length-4..];
        let line = std::str::from_utf8(line).map_err(|_| invalid)?.split('\0').next().ok_or(invalid)?.trim_end_matches('\n');
        if let Some((oid, name)) = line.split_once(' ')
            && name == "refs/heads/master"
            && (!full_oid(oid) || master.replace(oid.to_owned()).is_some())
        {
            return Err(invalid);
        }
    }
    master.ok_or("CachyOS master reference is missing")
}

impl Repository {
    fn load(endpoint: &RecipeEndpoint, proxy: Option<&ProxyUrl>) -> Result<Self, http::FetchError> {
        if endpoint.as_url().as_str().trim_end_matches('/') == "https://api.github.com/repos/CachyOS/CachyOS-PKGBUILDS" {
            return Self::load_snapshot(
                "https://github.com/CachyOS/CachyOS-PKGBUILDS.git/info/refs?service=git-upload-pack".parse().unwrap(),
                "https://codeload.github.com/CachyOS/CachyOS-PKGBUILDS/tar.gz".parse().unwrap(),
                proxy,
            );
        }
        let bytes = fetch(endpoint_url(endpoint, &["commits", "master"]), proxy)?;
        let commit: Commit = serde_json::from_slice(&bytes)
            .map_err(|_| http::FetchError::Failed("invalid CachyOS commit JSON"))?;
        if !full_oid(&commit.sha) {
            return Err(http::FetchError::Failed("invalid CachyOS recipe commit"));
        }
        let mut url = endpoint_url(endpoint, &["git", "trees", &commit.sha]);
        url.query_pairs_mut().append_pair("recursive", "1");
        let bytes = fetch(url, proxy)?;
        Self::decode_tree(&bytes, commit.sha).map_err(http::FetchError::Failed)
    }

    fn load_snapshot(refs_url: Url, mut archive_url: Url, proxy: Option<&ProxyUrl>) -> Result<Self, http::FetchError> {
        let refs = fetch(refs_url, proxy)?;
        let commit = advertised_master(&refs).map_err(http::FetchError::Failed)?;
        archive_url.path_segments_mut().expect("HTTP URL").push(&commit);
        let bytes = http::fetch_all_limited(&[archive_url], std::time::Duration::from_secs(60), proxy, Some(16 * 1024 * 1024))?
            .pop().expect("one archive result")?;
        Self::decode_snapshot(&bytes, commit)
    }

    fn decode_snapshot(bytes: &[u8], commit: String) -> Result<Self, http::FetchError> {
        let invalid = || http::FetchError::Failed("invalid CachyOS recipe snapshot");
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut expanded = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            if crate::interrupt::is_requested() { return Err(http::FetchError::Interrupted); }
            let count = decoder.read(&mut buffer).map_err(|_| invalid())?;
            if count == 0 { break; }
            if expanded.len() + count > 64 * 1024 * 1024 {
                return Err(http::FetchError::Failed("CachyOS recipe snapshot exceeds expansion limit"));
            }
            expanded.extend_from_slice(&buffer[..count]);
        }
        // Require the complete tar terminator before absence can enable fallback.
        if expanded.len() < 1024 || expanded.len() % 512 != 0 || expanded[expanded.len()-1024..].iter().any(|b| *b != 0) {
            return Err(invalid());
        }
        let mut archive = tar::Archive::new(expanded.as_slice());
        let mut entries = Vec::new();
        let mut recipes = BTreeMap::new();
        let top = format!("CachyOS-PKGBUILDS-{commit}");
        let prefix = format!("{top}/");
        for (index, entry) in archive.entries().map_err(|_| invalid())?.enumerate() {
            if crate::interrupt::is_requested() { return Err(http::FetchError::Interrupted); }
            if index >= 20000 { return Err(http::FetchError::Failed("too many CachyOS recipe snapshot entries")); }
            let mut entry = entry.map_err(|_| invalid())?;
            // Codeload archives carry pax metadata entries before the tree;
            // only files, directories, and links describe recipes.
            let entry_type = entry.header().entry_type();
            if !(entry_type.is_file() || entry_type.is_dir() || entry_type.is_symlink() || entry_type.is_hard_link()) {
                continue;
            }
            let raw = entry.path().map_err(|_| invalid())?.to_str().ok_or_else(invalid)?.to_owned();
            if raw == top || raw == prefix || raw == "pax_global_header" {
                continue;
            }
            let path = raw.strip_prefix(&prefix).ok_or_else(invalid)?.trim_end_matches('/');
            if path.is_empty() {
                continue;
            }
            let kind = if entry.header().entry_type().is_dir() { "tree" } else { "blob" };
            let mut sha = commit.clone();
            if path.ends_with("/.SRCINFO") && entry.header().entry_type().is_file() {
                if entry.size() > crate::srcinfo::MAX_SRCINFO_BYTES as u64 { return Err(http::FetchError::Failed("CachyOS .SRCINFO exceeds size limit")); }
                let mut content = Vec::new();
                entry.read_to_end(&mut content).map_err(|_| invalid())?;
                sha = git2::Oid::hash_object(git2::ObjectType::Blob, &content).map_err(|_| invalid())?.to_string();
                if recipes.insert(path.to_owned(), content).is_some() { return Err(invalid()); }
            } else if path.ends_with("/.SRCINFO") { return Err(invalid()); }
            entries.push(serde_json::json!({"path":path,"type":kind,"sha":sha}));
        }
        let tree = serde_json::to_vec(&serde_json::json!({"truncated":false,"tree":entries})).map_err(|_| invalid())?;
        let mut repo = Self::decode_tree(&tree, commit).map_err(http::FetchError::Failed)?;
        for (base, (directory, _)) in &repo.srcinfo {
            repo.recipes.insert(base.clone(), recipes.remove(&format!("{directory}/.SRCINFO")).ok_or_else(invalid)?);
        }
        Ok(repo)
    }

    fn decode_tree(bytes: &[u8], commit: String) -> Result<Self, &'static str> {
        let tree: Tree =
            serde_json::from_slice(bytes).map_err(|_| "invalid CachyOS recipe tree JSON")?;
        // A truncated index cannot establish that a custom recipe is absent.
        if tree.truncated {
            return Err("CachyOS recipe tree is truncated");
        }
        let mut bases = BTreeSet::new();
        let mut srcinfo = BTreeMap::new();
        for entry in tree.tree {
            if entry
                .path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
                || entry.path.chars().any(char::is_control)
            {
                return Err("invalid CachyOS recipe path");
            }
            if entry.kind == "tree" || entry.kind == "commit" {
                bases.insert(entry.path.rsplit('/').next().unwrap().to_owned());
            }
            if let Some((directory, filename)) = entry.path.rsplit_once('/') {
                let base = directory.rsplit('/').next().unwrap();
                bases.insert(base.to_owned());
                if filename == ".SRCINFO" && entry.kind == "blob" {
                    if !full_oid(&entry.sha) {
                        return Err("invalid CachyOS recipe blob ID");
                    }
                    if srcinfo
                        .insert(base.to_owned(), (directory.to_owned(), entry.sha))
                        .is_some()
                    {
                        return Err("ambiguous CachyOS recipe paths");
                    }
                }
            }
        }
        Ok(Self {
            commit,
            bases,
            srcinfo,
            recipes: BTreeMap::new(),
        })
    }

    fn decode_recipe(
        &self,
        bytes: &[u8],
        endpoint: &RecipeEndpoint,
        directory: &str,
        sha: &str,
    ) -> Result<Recipe, &'static str> {
        let blob: Blob =
            serde_json::from_slice(bytes).map_err(|_| "invalid CachyOS recipe blob JSON")?;
        if blob.sha != sha || blob.encoding != "base64" {
            return Err("invalid CachyOS recipe blob envelope");
        }
        // GitHub wraps base64 at 60 columns. No server strings enter diagnostics.
        let content: String = blob
            .content
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .collect();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(content)
            .map_err(|_| "invalid CachyOS recipe base64")?;
        let segments: Vec<_> = std::iter::once("contents")
            .chain(directory.split('/'))
            .chain(std::iter::once(".SRCINFO"))
            .collect();
        let mut location = endpoint_url(endpoint, &segments);
        location.query_pairs_mut().append_pair("ref", &self.commit);
        let provenance = RecipeProvenance::new(location.as_str(), &self.commit)?;
        Ok(Recipe::from_srcinfo(&bytes, provenance))
    }
}

/// Only an additional numeric pkgrel component is eligible, never pkgver or epoch.
/// This is a hypothesis about a rebuild and always produces fallback evidence.
/// Shared with the Arch provider, which skips fetching rebuild-suffixed tags
/// (they cannot exist upstream) when this mode is enabled.
pub(crate) fn rebuild_version(version: &str) -> Option<String> {
    let (version, release) = version.rsplit_once('-')?;
    let (release, rebuild) = release.split_once('.')?;
    let digits = |value: &str| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
    if !digits(release) || !digits(rebuild) {
        return None;
    }
    Some(format!("{version}-{release}"))
}

fn resolve(recipe: &Recipe, package: RecipePackage<'_>) -> AttemptOutcome {
    let exact = recipe.resolve(package);
    if !matches!(&exact, AttemptOutcome::Unresolved { reason } if reason == "recipe version does not match installed version")
    {
        return exact;
    }
    let Some(version) = rebuild_version(package.version) else {
        return exact;
    };
    match recipe.resolve(RecipePackage {
        version: &version,
        ..package
    }) {
        AttemptOutcome::Resolved { target, .. } => AttemptOutcome::Resolved {
            target,
            grade: EvidenceGrade::Fallback,
        },
        failure @ AttemptOutcome::Unresolved { .. } => failure,
        _ => exact,
    }
}

pub(crate) fn record_attempts<'a, W: Write>(
    refresh: &RefreshTransaction<'_>,
    packages: impl IntoIterator<Item = &'a InstalledPackage>,
    endpoint: &RecipeEndpoint,
    arch_endpoint: &RecipeEndpoint,
    progress: &mut Progress<W>,
    require_checksums: bool,
    proxy: Option<&ProxyUrl>,
) -> Result<(), StorageError> {
    let provider = SourceProvider::parse("cachyos").expect("static provider");
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for package in packages {
        let attempts = refresh.provider_attempts(package.name(), package.version())?;
        if attempts.iter().any(|attempt| matches!(attempt.outcome, AttemptOutcome::SourceFree) || matches!(&attempt.outcome,
            AttemptOutcome::Resolved { target, .. } if !require_checksums || target.revision.is_some() || target.checksum.is_some())) {
            continue;
        }
        groups
            .entry((
                package.package_base().unwrap_or(package.name()),
                package.version(),
            ))
            .or_default()
            .push(package);
    }
    progress.set_total(groups.len() as u64);
    if groups.is_empty() {
        return Ok(());
    }
    let repository = Repository::load(endpoint, proxy);
    if matches!(repository, Err(http::FetchError::Interrupted)) {
        return Err(StorageError::Interrupted);
    }
    let groups: Vec<_> = groups.into_iter().collect();
    for batch in groups.chunks(http::MAX_CONCURRENT_FETCHES) {
        if crate::interrupt::is_requested() {
            return Err(StorageError::Interrupted);
        }
        // None means no eligible metadata path; never invent an Arch mapping for
        // a custom directory whose .SRCINFO is missing or malformed.
        let urls: Vec<Option<Url>> = batch
            .iter()
            .map(|((base, version), members)| {
                if !members.iter().any(|p| p.origin() == "unknown") {
                    return None;
                }
                let repo = repository.as_ref().ok()?;
                if repo.recipes.contains_key(*base) { return None; }
                if let Some((_, sha)) = repo.srcinfo.get(*base) {
                    Some(endpoint_url(endpoint, &["git", "blobs", sha]))
                } else if !repo.bases.contains(*base) {
                    let version = rebuild_version(version)?;
                    let tag = version.replacen(':', "-", 1).replace('~', ".");
                    Some(arch_endpoint.file_url(
                        &format!(
                            "archlinux/packaging/packages/{}",
                            recipe::project_path(base)
                        ),
                        &tag,
                    ))
                } else {
                    None
                }
            })
            .collect();
        let requests: Vec<_> = urls.iter().flatten().cloned().collect();
        let fetched = match http::fetch_all(&requests, proxy) {
            Ok(results) => results,
            Err(http::FetchError::Interrupted) => return Err(StorageError::Interrupted),
            Err(error) => requests.iter().map(|_| Err(error.clone())).collect(),
        };
        let mut fetched = fetched.into_iter();
        for (((base, _), members), url) in batch.iter().zip(urls) {
            let mut downloaded = 0;
            let recipe: Result<Recipe, String> = match (&repository, url) {
                (Err(error), _) => Err(error.to_string()),
                (Ok(repo), Some(url)) => match fetched.next().expect("one result per URL") {
                    Ok(bytes) => {
                        downloaded = bytes.len() as u64;
                        if let Some((directory, sha)) = repo.srcinfo.get(*base) {
                            repo.decode_recipe(&bytes, endpoint, directory, sha)
                                .map_err(str::to_owned)
                        } else {
                            Recipe::decode(&bytes, &url).map_err(str::to_owned)
                        }
                    }
                    Err(http::FetchError::Interrupted) => return Err(StorageError::Interrupted),
                    Err(error) => Err(error.to_string()),
                },
                (Ok(repo), None) if repo.recipes.contains_key(*base) => {
                    let (directory, _) = &repo.srcinfo[*base];
                    let mut location: Url = format!("https://github.com/CachyOS/CachyOS-PKGBUILDS/blob/{}/", repo.commit).parse().unwrap();
                    location.path_segments_mut().unwrap().pop_if_empty().extend(directory.split('/')).push(".SRCINFO");
                    RecipeProvenance::new(location.as_str(), &repo.commit)
                        .map(|provenance| Recipe::from_srcinfo(&repo.recipes[*base], provenance)).map_err(str::to_owned)
                }
                (Ok(repo), None) if repo.bases.contains(*base) => {
                    Err("CachyOS recipe has no .SRCINFO".to_owned())
                }
                (Ok(_), None) => {
                    Err("no CachyOS recipe or eligible Arch rebuild mapping".to_owned())
                }
            };
            let mut resolved = 0;
            for package in members {
                let outcome = if package.origin() != "unknown" {
                    AttemptOutcome::Unresolved {
                        reason: "unsupported package origin".to_owned(),
                    }
                } else {
                    match &recipe {
                        Ok(recipe) => resolve(recipe, (*package).into()),
                        Err(reason) => AttemptOutcome::Unresolved {
                            reason: reason.clone(),
                        },
                    }
                };
                if matches!(
                    outcome,
                    AttemptOutcome::Resolved { .. } | AttemptOutcome::SourceFree
                ) {
                    resolved += 1;
                }
                refresh.record_provider_attempt(&ProviderAttempt {
                    package_name: package.name().to_owned(),
                    package_version: package.version().to_owned(),
                    provider: provider.clone(),
                    outcome,
                    recipe: recipe
                        .as_ref()
                        .ok()
                        .map(|recipe| recipe.provenance().clone()),
                })?;
            }
            progress.advance(1, downloaded, resolved, members.len() as u64 - resolved);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
