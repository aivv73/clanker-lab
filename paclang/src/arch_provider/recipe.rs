use base64::Engine;
use serde::Deserialize;
use url::Url;

use crate::inventory::InstalledPackage;
use crate::package::provider::{AnalysisTarget, AttemptOutcome, EvidenceGrade, RecipeProvenance};
use crate::srcinfo::{self, ParsedSrcinfo};

#[derive(Clone, Copy)]
pub(crate) struct RecipePackage<'a> {
    pub(crate) name: &'a str,
    pub(crate) base: &'a str,
    pub(crate) version: &'a str,
}

impl<'a> From<&'a InstalledPackage> for RecipePackage<'a> {
    fn from(package: &'a InstalledPackage) -> Self {
        Self {
            name: package.name(),
            base: package.package_base().unwrap_or(package.name()),
            version: package.version(),
        }
    }
}

pub(crate) struct Recipe {
    document: Result<ParsedSrcinfo, &'static str>,
    provenance: RecipeProvenance,
}

#[derive(Deserialize)]
struct RepositoryFile {
    file_path: String,
    encoding: String,
    content: String,
    commit_id: String,
}

impl Recipe {
    pub(crate) fn decode(bytes: &[u8], url: &Url) -> Result<Self, &'static str> {
        let file: RepositoryFile =
            serde_json::from_slice(bytes).map_err(|_| "invalid recipe API JSON")?;
        if file.file_path != ".SRCINFO" || file.encoding != "base64" {
            return Err("recipe API did not return base64 .SRCINFO");
        }
        let mut location = url.clone();
        location
            .query_pairs_mut()
            .clear()
            .append_pair("ref", &file.commit_id);
        let provenance = RecipeProvenance::new(location.as_str(), &file.commit_id)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(file.content)
            .map_err(|_| "invalid recipe API base64")?;
        let document =
            srcinfo::parse_srcinfo_bytes(&bytes).map_err(|_| "invalid or oversized .SRCINFO");
        Ok(Self {
            document,
            provenance,
        })
    }

    pub(crate) fn from_srcinfo(bytes: &[u8], provenance: RecipeProvenance) -> Self {
        Self {
            document: srcinfo::parse_srcinfo_bytes(bytes)
                .map_err(|_| "invalid or oversized .SRCINFO"),
            provenance,
        }
    }

    pub(crate) fn provenance(&self) -> &RecipeProvenance {
        &self.provenance
    }

    pub(crate) fn resolve(&self, package: RecipePackage<'_>) -> AttemptOutcome {
        let unresolved = |reason: &str| AttemptOutcome::Unresolved {
            reason: reason.to_owned(),
        };
        let document = match &self.document {
            Ok(document) => document,
            Err(reason) => return unresolved(reason),
        };
        if document.has_arch_specific_sources {
            return unresolved("architecture-specific sources need architecture-aware resolution");
        }
        if document.pkgbase != package.base {
            return unresolved("recipe package base does not match installed metadata");
        }
        let Some(member) = document
            .packages
            .iter()
            .find(|member| member.name == package.name)
        else {
            return unresolved("installed package is absent from the recipe");
        };
        let version = match document.epoch.as_deref() {
            None | Some("0") => format!("{}-{}", document.pkgver, document.pkgrel),
            Some(epoch) => format!("{epoch}:{}-{}", document.pkgver, document.pkgrel),
        };
        if version != package.version {
            return unresolved("recipe version does not match installed version");
        }
        if document.sources.is_empty()
            && member.sources.is_empty()
            && document.sha256sums.is_empty()
            && document.sha512sums.is_empty()
            && document.b2sums.is_empty()
            && member.sha256sums.is_empty()
            && member.sha512sums.is_empty()
            && member.b2sums.is_empty()
        {
            return AttemptOutcome::SourceFree;
        }
        let mut targets = Vec::new();
        let mut unpinned = Vec::new();
        for (sources, sha256, sha512, b2) in [
            (
                &document.sources,
                &document.sha256sums,
                &document.sha512sums,
                &document.b2sums,
            ),
            (
                &member.sources,
                &member.sha256sums,
                &member.sha512sums,
                &member.b2sums,
            ),
        ] {
            if [sha256, sha512, b2]
                .iter()
                .any(|sums| !sums.is_empty() && sums.len() != sources.len())
            {
                return unresolved("recipe checksum count does not match source count");
            }
            for (index, source) in sources.iter().enumerate() {
                // A positional digest in the matching, pinned recipe binds
                // archive content even when upstream encodes versions oddly.
                // Acquisition still verifies that digest before extraction.
                let digest_bound = [(sha256, 64), (sha512, 128), (b2, 128)]
                    .iter()
                    .any(|(sums, length)| sums.get(index).is_some_and(|sum|
                        sum.len() == *length && sum.bytes().all(|b| b.is_ascii_hexdigit())));
                match source_target_with_evidence(source, &document.pkgver, digest_bound) {
                    Ok(Some(mut target)) => {
                        if target.revision.is_none() {
                            // Strongest verification evidence wins: SHA-512 and
                            // BLAKE2 (both 128 hex digits) outrank SHA-256.
                            for (kind, length, sums) in
                                [("sha512", 128, sha512), ("b2", 128, b2), ("sha256", 64, sha256)]
                            {
                                if let Some(sum) =
                                    sums.get(index).filter(|sum| sum.as_str() != "SKIP")
                                {
                                    if sum.len() != length
                                        || !sum.bytes().all(|b| b.is_ascii_hexdigit())
                                    {
                                        return unresolved("invalid source checksum");
                                    }
                                    if target.checksum.is_none() {
                                        target.checksum =
                                            Some(format!("{kind}:{}", sum.to_ascii_lowercase()));
                                    }
                                }
                            }
                        }
                        if !targets.contains(&target) {
                            targets.push(target);
                        }
                    }
                    Ok(None) => {}
                    Err("unpinned Git source is unsupported") => {
                        // Validate transport/query syntax using the normal parser,
                        // but never turn this temporary selector into evidence.
                        // Role classification requires the entire recipe, not
                        // whichever source happened to occur first.
                        match source_target_with_evidence(&format!("{source}#tag=paclang-role-validation"), &document.pkgver, false) {
                            Ok(Some(mut target)) => {
                                target.revision = None;
                                unpinned.push(target);
                            }
                            Err(reason) => return unresolved(reason),
                            _ => return unresolved("invalid unpinned Git input"),
                        }
                    }
                    Err(reason) => return unresolved(reason),
                }
            }
        }
        if !unpinned.is_empty() {
            let primary = targets.iter().filter(|target| known_primary(package.base, target)).collect::<Vec<_>>();
            let Some(rule) = super::source_roles::rule(package.base).filter(|_| primary.len() == 1) else {
                return unresolved("unpinned Git source is unsupported: no unique recognized primary tree");
            };
            for input in &unpinned {
                if !rule.supplemental.iter().any(|url| super::source_roles::repository_matches(&input.location, url)) {
                    let location: String = input.location.chars().take(700).collect();
                    return unresolved(&format!("unclassified unpinned secondary Git source: {location}; full inputs in pinned recipe"));
                }
            }
        }
        if targets.is_empty() {
            let sources: Vec<_> = document.sources.iter().chain(&member.sources).collect();
            if !sources.is_empty() && sources.iter().all(|source| !source.contains("://")) {
                for source in &sources {
                    if source.contains(['$', '`']) || crate::acquisition::safe_path(source).is_err() {
                        return unresolved("invalid recipe-local source path");
                    }
                }
                match packaging_target(&self.provenance, package.base) {
                    Some(target) => targets.push(target),
                    None => return unresolved("recipe-local sources require a supported pinned packaging repository"),
                }
            } else {
                return unresolved("no primary source candidate: only auxiliary source inputs");
            }
        }
        if targets.len() > 1 {
            let primary: Vec<_> = targets.iter().filter(|target| !known_auxiliary(package.base, target, &document.pkgver)).cloned().collect();
            if primary.len() == 1 && known_primary(package.base, &primary[0]) {
                targets = primary;
            } else {
                let mut candidates = targets.iter().map(|target| {
                    format!("{}{}", target.location, target.revision.as_ref().map(|r| format!("#{r}")).unwrap_or_default())
                }).collect::<Vec<_>>().join(", ");
                if candidates.len() > 3800 {
                    let mut boundary = 3800;
                    while !candidates.is_char_boundary(boundary) { boundary -= 1; }
                    candidates.truncate(boundary);
                    candidates.push_str(" ...; full inputs in pinned recipe");
                }
                return unresolved(&format!("ambiguous primary source: {} candidates [{}]", targets.len(), candidates));
            }
        }
        AttemptOutcome::Resolved {
            target: targets.remove(0),
            // Matching an Arch recipe reconstructs evidence for unknown origin;
            // it cannot prove which recipe produced the installed binary.
            grade: EvidenceGrade::Reconstructed,
        }
    }
}

// Explicit source-role policy. A rule applies only when every other candidate
// is recognized as auxiliary and the sole remaining target is the expected
// upstream. Unknown additions keep the recipe ambiguous.
fn known_primary(base: &str, target: &AnalysisTarget) -> bool {
    if let Some(rule) = super::source_roles::rule(base)
        && target.revision.is_some() && super::source_roles::repository_matches(&target.location, rule.primary) {
        return true;
    }
    let Ok(url) = Url::parse(&target.location) else { return false; };
    let host = url.host_str().unwrap_or("");
    let path = url.path().trim_end_matches('/').trim_end_matches(".git");
    match base {
        "gstreamer" => host == "gitlab.freedesktop.org" && path == "/gstreamer/gstreamer",
        "libxml2" | "lib32-libxml2" => host == "gitlab.gnome.org" && path == "/GNOME/libxml2",
        "poppler" => host == "gitlab.freedesktop.org" && path == "/poppler/poppler",
        "nlohmann-json" => host == "github.com" && path == "/nlohmann/json",
        "dav1d" => host == "downloads.videolan.org" && path.starts_with("/pub/videolan/dav1d/"),
        "mpdecimal" => host == "www.bytereef.org" && path.starts_with("/software/mpdecimal/releases/mpdecimal-"),
        "sqlite" => host == "www.sqlite.org" && path.rsplit('/').next().is_some_and(|p| p.starts_with("sqlite-src-")),
        _ => false,
    }
}

fn known_auxiliary(base: &str, target: &AnalysisTarget, version: &str) -> bool {
    if let Some(rule) = super::source_roles::rule(base)
        && (target.revision.is_some() && rule.supplemental.iter().any(|url| super::source_roles::repository_matches(&target.location, url))
            || target.revision.is_none() && super::source_roles::alternate_distribution(base, &target.location, version))
    {
        return true;
    }
    let Ok(url) = Url::parse(&target.location) else { return false; };
    let host = url.host_str().unwrap_or("");
    let path = url.path().trim_end_matches('/').trim_end_matches(".git");
    match base {
        "gstreamer" => host == "gstreamer.freedesktop.org" && path.starts_with("/src/gstreamer-docs/"),
        "libxml2" | "lib32-libxml2" => host == "www.w3.org" && path.starts_with("/XML/Test/"),
        "poppler" => host == "gitlab.freedesktop.org" && path == "/poppler/test",
        "nlohmann-json" => host == "github.com" && path.starts_with("/nlohmann/json_test_data/archive/"),
        "dav1d" => host == "code.videolan.org" && path.starts_with("/videolan/dav1d-test-data/-/archive/"),
        "mpdecimal" => host == "speleotrove.com" && path == "/decimal/dectest.zip",
        "sqlite" => host == "www.sqlite.org" && path.rsplit('/').next().is_some_and(|p| p.starts_with("sqlite-doc-")),
        _ => false,
    }
}

fn packaging_target(provenance: &RecipeProvenance, base: &str) -> Option<AnalysisTarget> {
    let url = Url::parse(provenance.location()).ok()?;
    let parts: Vec<_> = url.path_segments()?.collect();
    let (location, subdirectory) = match url.host_str()? {
        "gitlab.archlinux.org" if parts.len() == 7 && parts[..3] == ["api", "v4", "projects"]
            && parts[4..] == ["repository", "files", ".SRCINFO"] => {
            let encoded = format!("project={}", parts[3]);
            let project = url::form_urlencoded::parse(encoded.as_bytes()).next()?.1.into_owned();
            if project != format!("archlinux/packaging/packages/{}", project_path(base)) { return None; }
            (format!("https://gitlab.archlinux.org/{project}.git"), None)
        }
        "github.com" if parts.len() >= 5 && parts[..3] == ["CachyOS", "CachyOS-PKGBUILDS", "blob"]
            && parts[3] == provenance.commit() && parts.last() == Some(&".SRCINFO") => {
            let directory = parts[4..parts.len()-1].join("/");
            if directory.is_empty() || crate::acquisition::safe_path(&directory).is_err() { return None; }
            ("https://github.com/CachyOS/CachyOS-PKGBUILDS.git".to_owned(), Some(directory))
        }
        _ => return None,
    };
    Some(AnalysisTarget { location, revision: Some(format!("commit:{}", provenance.commit())), subdirectory, checksum: None })
}

#[cfg(test)]
fn source_target(source: &str, version: &str) -> Result<Option<AnalysisTarget>, &'static str> {
    source_target_with_evidence(source, version, false)
}

fn source_target_with_evidence(source: &str, version: &str, digest_bound: bool) -> Result<Option<AnalysisTarget>, &'static str> {
    if source.contains(['$', '`']) {
        return Err("dynamic recipe source is unsupported");
    }
    let (alias, source) = match source.split_once("::") {
        Some((alias, source)) => (Some(alias), source),
        None => (None, source),
    };
    let is_git = source.starts_with("git+");
    let location = source.strip_prefix("git+").unwrap_or(source);
    // Local recipe files (patches, launchers, etc.) are not primary trees.
    if !location.contains("://") {
        return Ok(None);
    }
    if location.chars().any(char::is_whitespace) {
        return Err("source URL contains whitespace");
    }
    let mut url = Url::parse(location).map_err(|_| "invalid source URL")?;
    let loopback = matches!(url.host(), Some(url::Host::Ipv4(ip)) if ip.is_loopback())
        || matches!(url.host(), Some(url::Host::Ipv6(ip)) if ip.is_loopback());
    if !(url.scheme() == "https" || url.scheme() == "http" && loopback)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("source URL requires HTTPS without credentials");
    }
    let revision = if is_git {
        let fragment = url.fragment().ok_or("unpinned Git source is unsupported")?;
        // makepkg permits ?signed after the revision as well as before '#'.
        // It is recipe signature policy, not part of a Git ref or fetch URL.
        // The pinned recipe provenance retains that policy; resolving a tag
        // does not assert that its signature has been verified.
        let fragment = fragment.strip_suffix("?signed").unwrap_or(fragment);
        if url.query().is_some_and(|query| query != "signed") {
            return Err("unsupported Git source query");
        }
        let (kind, value) = fragment
            .split_once('=')
            .ok_or("unsupported Git revision selector")?;
        if !matches!(kind, "commit" | "tag")
            || value.is_empty()
            || value.len() > 1000
            || value.contains(['?', '#', '&'])
        {
            return Err("unsupported Git revision selector");
        }
        if kind == "commit"
            && !(matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err("Git source needs a full commit ID");
        }
        let revision = Some(format!("{kind}:{value}"));
        url.set_fragment(None);
        url.set_query(None);
        revision
    } else {
        let filename = alias.unwrap_or_else(|| url.path().rsplit('/').next().unwrap_or(""));
        // Auxiliary downloads do not identify a primary source tree.
        if [".sig", ".asc", ".patch", ".diff"]
            .iter()
            .any(|suffix| filename.ends_with(suffix))
        {
            return Ok(None);
        }
        if url.fragment().is_some()
            || ![
                ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar.lz", ".tgz", ".zip", ".tar",
            ]
            .iter()
            .any(|suffix| filename.ends_with(suffix))
        {
            return Err("source is not a supported archive or pinned Git tree");
        }
        let versioned = url.path().split('/').any(|segment| {
            let stem = [
                ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar.lz", ".tgz", ".zip", ".tar",
            ]
            .iter()
            .find_map(|suffix| segment.strip_suffix(suffix))
            .unwrap_or(segment);
            stem == version
                || stem == format!("v{version}")
                || stem.ends_with(&format!("-{version}"))
                || stem.ends_with(&format!("_{version}"))
        });
        if !versioned && !digest_bound {
            return Err("archive URL does not identify the packaged version");
        }
        // Archive URL is the recipe's packaged source; its bytes are not fetched
        // or verified in this slice. No invented VCS revision is attached.
        None
    };
    if url.as_str().len() > 4096 {
        return Err("source URL exceeds storage limit");
    }
    Ok(Some(AnalysisTarget {
        location: url.to_string(),
        revision,
        subdirectory: None,
        checksum: None,
    }))
}

/// Arch devtools' project-name mapping, including GitLab's reserved `tree`.
/// See src/lib/api/gitlab.sh::gitlab_project_name_to_path in archlinux/devtools.
pub(crate) fn project_path(base: &str) -> String {
    let chars: Vec<_> = base.chars().collect();
    let mut path = String::new();
    let mut index = 0;
    while index < chars.len() {
        // Consume nonoverlapping left-word + right-word matches as devtools does.
        if chars[index].is_ascii_alphanumeric() {
            while chars.get(index).is_some_and(char::is_ascii_alphanumeric) {
                path.push(chars[index]);
                index += 1;
            }
            if chars.get(index) == Some(&'+')
                && chars.get(index + 1).is_some_and(char::is_ascii_alphabetic)
            {
                path.push('-');
                index += 1;
                while chars.get(index).is_some_and(char::is_ascii_alphabetic) {
                    path.push(chars[index]);
                    index += 1;
                }
            }
            continue;
        }
        match chars[index] {
            '+' => path.push_str("plus"),
            ch @ ('_' | '-' | '.') => path.push(ch),
            _ => path.push('-'),
        }
        index += 1;
    }
    let mut collapsed = String::new();
    let mut separators = String::new();
    for ch in path.chars().chain(std::iter::once('\0')) {
        if matches!(ch, '_' | '-') {
            separators.push(ch);
            continue;
        }
        if separators.len() > 1 {
            collapsed.push('-');
        } else {
            collapsed.push_str(&separators);
        }
        separators.clear();
        if ch != '\0' {
            collapsed.push(ch);
        }
    }
    if collapsed == "tree" {
        "unix-tree".to_owned()
    } else {
        collapsed
    }
}


#[cfg(test)]
mod selector_tests {
    use super::*;

    #[test]
    fn signed_metadata_does_not_change_pinned_revision_or_fetch_url() {
        for (selector, expected) in [
            ("tag=v1.2.0", "tag:v1.2.0".to_owned()),
            ("tag=releases/1.2", "tag:releases/1.2".to_owned()),
            ("commit=0123456789012345678901234567890123456789", "commit:0123456789012345678901234567890123456789".to_owned()),
        ] {
            for suffix in [format!("#{selector}?signed"), format!("?signed#{selector}"), format!("#{selector}")] {
                let target = source_target(&format!("alias::git+https://example.org/repo.git{suffix}"), "1.2.0").unwrap().unwrap();
                assert_eq!(target.location, "https://example.org/repo.git");
                assert_eq!(target.revision.as_deref(), Some(expected.as_str()));
            }
        }
    }

    #[test]
    fn signed_marker_does_not_relax_selector_validation() {
        for suffix in ["#branch=main?signed", "#tag=?signed", "#commit=abcdef?signed",
            "#tag=v1?unknown", "#tag=v1?signed&other", "#tag=v1?signed?signed",
            "?unknown#tag=v1", "?signed", "#tag=v1&other?signed"] {
            assert!(source_target(&format!("git+https://example.org/repo.git{suffix}"), "1").is_err(), "{suffix}");
        }
    }
}
