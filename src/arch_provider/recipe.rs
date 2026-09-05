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
            && member.sha256sums.is_empty()
            && member.sha512sums.is_empty()
        {
            return AttemptOutcome::SourceFree;
        }
        let mut targets = Vec::new();
        for (sources, sha256, sha512) in [
            (
                &document.sources,
                &document.sha256sums,
                &document.sha512sums,
            ),
            (&member.sources, &member.sha256sums, &member.sha512sums),
        ] {
            if [sha256, sha512]
                .iter()
                .any(|sums| !sums.is_empty() && sums.len() != sources.len())
            {
                return unresolved("recipe checksum count does not match source count");
            }
            for (index, source) in sources.iter().enumerate() {
                match source_target(source, &document.pkgver) {
                    Ok(Some(mut target)) => {
                        if target.revision.is_none() {
                            for (kind, length, sums) in
                                [("sha512", 128, sha512), ("sha256", 64, sha256)]
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
                    Err(reason) => return unresolved(reason),
                }
            }
        }
        if targets.len() != 1 {
            return unresolved("primary source is missing or ambiguous in recipe metadata");
        }
        AttemptOutcome::Resolved {
            target: targets.remove(0),
            // Matching an Arch recipe reconstructs evidence for unknown origin;
            // it cannot prove which recipe produced the installed binary.
            grade: EvidenceGrade::Reconstructed,
        }
    }
}

fn source_target(source: &str, version: &str) -> Result<Option<AnalysisTarget>, &'static str> {
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
        if !versioned {
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
