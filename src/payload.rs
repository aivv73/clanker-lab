//! Installed-payload visibility. No language score, licensing inference or URL heuristics.
//! Confirmed opaque bytes require an installed ELF identical to an acquired,
//! checksum-verified prebuilt input for which no source-language representation is recorded.
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::linguist::{ProfileSource, RevisionFidelity};
use crate::package::provider::{AttemptOutcome, EvidenceGrade};
use crate::report::{PopulationSelection, SnapshotStaleness};
use crate::storage::PublishedSnapshot;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct BinaryFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SourceManifest {
    pub complete: bool,
    pub reason: Option<String>,
    pub binaries: Vec<BinaryFile>,
    /// Language inspection of this very materialization, never an older cached profile.
    #[serde(default)]
    pub source_representation: Option<bool>,
}

impl SourceManifest {
    pub(crate) fn unavailable(reason: &str) -> Self {
        Self {
            complete: false,
            reason: Some(reason.into()),
            binaries: Vec::new(),
            source_representation: None,
        }
    }
}

fn regular_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    Ok(file)
}

fn read_header(file: &mut File) -> io::Result<Vec<u8>> {
    let mut header = Vec::with_capacity(64);
    file.take(64).read_to_end(&mut header)?;
    Ok(header)
}

fn is_elf(header: &[u8]) -> bool {
    if header.len() < 52
        || &header[..4] != b"\x7fELF"
        || !matches!(header[4], 1 | 2)
        || !matches!(header[5], 1 | 2)
        || header[6] != 1
    {
        return false;
    }
    let kind = if header[5] == 1 {
        u16::from_le_bytes([header[16], header[17]])
    } else {
        u16::from_be_bytes([header[16], header[17]])
    };
    matches!(kind, 2 | 3) // ET_EXEC, ET_DYN; not arbitrary binary assets or relocatable objects.
}

fn digest(
    mut file: File,
    prefix: &[u8],
    budget: &mut u64,
    deadline: Instant,
) -> io::Result<(u64, String)> {
    let before = file.metadata()?;
    let bytes = before.len();
    if bytes > *budget {
        return Err(io::Error::other("payload hash byte limit exceeded"));
    }
    *budget -= bytes;
    let mut hash = Sha256::new();
    hash.update(prefix);
    let mut read = prefix.len() as u64;
    let mut buffer = [0u8; 65536];
    loop {
        if crate::interrupt::is_requested() || Instant::now() > deadline {
            return Err(io::Error::other(
                "payload inspection interrupted or timed out",
            ));
        }
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        read += n as u64;
        if read > bytes {
            return Err(io::Error::other("payload changed during inspection"));
        }
        hash.update(&buffer[..n]);
    }
    let after = file.metadata()?;
    if read != bytes || before.modified()? != after.modified()? || after.len() != bytes {
        return Err(io::Error::other("payload changed during inspection"));
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}

pub(crate) fn inspect_source(
    root: &Path,
    subdirectory: Option<&str>,
    limits: &crate::acquisition::Limits,
) -> SourceManifest {
    let root = subdirectory.map_or_else(|| root.to_path_buf(), |sub| root.join(sub));
    let mut manifest = SourceManifest {
        complete: true,
        reason: None,
        binaries: Vec::new(),
        source_representation: None,
    };
    let result = (|| -> io::Result<()> {
        let mut dirs = vec![root.clone()];
        let mut entries = 0u64;
        let mut budget = limits.max_extracted_bytes;
        let deadline = Instant::now() + Duration::from_secs(limits.source_timeout_seconds.min(30));
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(dir)? {
                if crate::interrupt::is_requested() || Instant::now() > deadline {
                    return Err(io::Error::other(
                        "source payload inspection interrupted or timed out",
                    ));
                }
                let entry = entry?;
                if entry.file_name() == ".git" {
                    continue;
                }
                entries += 1;
                if entries > limits.max_source_files {
                    return Err(io::Error::other("source payload entry limit exceeded"));
                }
                let kind = entry.file_type()?;
                if kind.is_dir() {
                    dirs.push(entry.path());
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let mut file = regular_file(&entry.path())?;
                let header = read_header(&mut file)?;
                if !is_elf(&header) {
                    continue;
                }
                let (bytes, sha256) = digest(file, &header, &mut budget, deadline)?;
                manifest.binaries.push(BinaryFile {
                    path: entry
                        .path()
                        .strip_prefix(&root)
                        .expect("source path")
                        .to_string_lossy()
                        .into(),
                    bytes,
                    sha256,
                });
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        manifest.complete = false;
        manifest.reason = Some(error.to_string());
    }
    manifest.binaries.sort_by(|a, b| a.path.cmp(&b.path));
    manifest
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Classification {
    SourceBacked,
    SourceFree,
    OpaqueBinary,
    Mixed,
    Unknown,
}

impl Classification {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SourceBacked => "source-backed",
            Self::SourceFree => "source-free",
            Self::OpaqueBinary => "opaque-binary",
            Self::Mixed => "mixed",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Match {
    pub installed_path: String,
    pub source_path: String,
    pub source_location: String,
    pub source_checksum: Option<String>,
    pub sha256: String,
    pub bytes: u64,
    pub state: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PackageRow {
    pub name: String,
    pub version: String,
    pub installed_bytes: u64,
    pub classification: Classification,
    pub provenance_state: &'static str,
    pub source_visible_bytes: u64,
    pub opaque_bytes: u64,
    pub unknown_bytes: u64,
    pub evidence: Vec<Match>,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Default)]
pub(crate) struct Summary {
    pub policy: &'static str,
    pub denominator: &'static str,
    pub selected_installed_bytes: u64,
    pub source_free_bytes: u64,
    pub denominator_bytes: u64,
    pub source_visible_bytes: u64,
    pub opaque_bytes: u64,
    pub unknown_bytes: u64,
    pub source_visible_ratio: Option<f64>,
    pub opaque_ratio: Option<f64>,
    pub unknown_ratio: Option<f64>,
}

pub(crate) struct Report {
    pub summary: Summary,
    pub packages: Vec<PackageRow>,
}

#[derive(Clone)]
struct Candidate {
    binary: BinaryFile,
    location: String,
    checksum: Option<String>,
    confirmed: bool,
}

fn installed_matches(name: &str, candidates: &[Candidate]) -> Result<Vec<Match>, String> {
    let output = crate::inventory::run_command("pacman", &["-Qql", "--", name])
        .map_err(|e| e.to_string())?;
    let paths = std::str::from_utf8(&output.stdout).map_err(|_| "non-UTF8 package file list")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut budget = 4_294_967_296;
    let mut seen = BTreeSet::new();
    let mut matches = Vec::new();
    let sizes: BTreeSet<_> = candidates.iter().map(|c| c.binary.bytes).collect();
    for path in paths.lines() {
        if seen.len() >= 250_000 || Instant::now() > deadline || crate::interrupt::is_requested() {
            return Err("installed payload inspection limit exceeded or interrupted".into());
        }
        let path_obj = Path::new(path);
        if !path_obj.is_absolute()
            || path_obj
                .components()
                .any(|c| matches!(c, Component::ParentDir))
            || path.chars().any(char::is_control)
        {
            return Err("unsafe installed package path".into());
        }
        if !seen.insert(path) {
            continue;
        }
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| format!("cannot inspect installed file {path}"))?;
        if !metadata.is_file() || !sizes.contains(&metadata.len()) {
            continue;
        }
        let mut file = regular_file(path_obj).map_err(|e| e.to_string())?;
        let header = read_header(&mut file).map_err(|e| e.to_string())?;
        if !is_elf(&header) {
            continue;
        }
        let (bytes, hash) =
            digest(file, &header, &mut budget, deadline).map_err(|e| e.to_string())?;
        // A source-bearing alternative prevents an opaque confirmation for identical content.
        let matching: Vec<_> = candidates
            .iter()
            .filter(|c| c.binary.bytes == bytes && c.binary.sha256 == hash)
            .collect();
        if let Some(candidate) = matching
            .iter()
            .find(|c| !c.confirmed)
            .or_else(|| matching.first())
        {
            matches.push(Match {
                installed_path: path.into(),
                source_path: candidate.binary.path.clone(),
                source_location: candidate.location.clone(),
                source_checksum: candidate.checksum.clone(),
                sha256: hash,
                bytes,
                state: if candidate.confirmed {
                    "opaque-confirmed"
                } else {
                    "opaque-suspected"
                },
            });
        }
    }
    Ok(matches)
}

pub(crate) fn build(
    snapshot: &PublishedSnapshot,
    selection: &PopulationSelection,
    staleness: SnapshotStaleness,
) -> Report {
    build_with(snapshot, selection, staleness, installed_matches)
}

fn build_with(
    snapshot: &PublishedSnapshot,
    selection: &PopulationSelection,
    staleness: SnapshotStaleness,
    inspect: impl Fn(&str, &[Candidate]) -> Result<Vec<Match>, String>,
) -> Report {
    let mut packages = Vec::new();
    for (package, stored) in snapshot.packages.iter().zip(&snapshot.provider_packages) {
        if !selection.includes(package) {
            continue;
        }
        let mut row = PackageRow {
            name: stored.name.clone(),
            version: stored.version.clone(),
            installed_bytes: stored.installed_bytes,
            classification: Classification::Unknown,
            provenance_state: "source-unresolved",
            source_visible_bytes: 0,
            opaque_bytes: 0,
            unknown_bytes: stored.installed_bytes,
            evidence: Vec::new(),
            reasons: Vec::new(),
        };
        if crate::package::provider::source_free(&stored.attempts) {
            row.classification = Classification::SourceFree;
            row.provenance_state = "source-free";
            row.unknown_bytes = 0;
            packages.push(row);
            continue;
        }
        let mut backed = false;
        let mut candidates = Vec::new();
        let mut resolved = false;
        let mut uncertain_target = false;
        for (i, attempt) in stored.attempts.iter().enumerate() {
            let AttemptOutcome::Resolved { target, grade } = &attempt.outcome else {
                if let AttemptOutcome::Unresolved { reason } = &attempt.outcome {
                    row.reasons.push(reason.clone());
                }
                continue;
            };
            resolved = true;
            if !matches!(grade, EvidenceGrade::Exact | EvidenceGrade::Reconstructed)
                || attempt.recipe.is_none()
            {
                uncertain_target = true;
                row.reasons
                    .push("source mapping lacks packaged-revision recipe provenance".into());
                continue;
            }
            let acquisition = stored.acquisitions.get(i).and_then(Option::as_ref);
            let manifest = stored.payload_manifests.get(i).and_then(Option::as_ref);
            let profile = stored.analyses.get(i).and_then(Option::as_ref);
            let exact = profile.filter(|p| {
                p.fidelity == RevisionFidelity::ExactPackagedSource
                    && p.source == ProfileSource::Linguist
            });
            let acquired = acquisition.is_some_and(|a| {
                a.state == "acquired"
                    && if target.revision.is_some() {
                        a.verified_revision.is_some()
                    } else {
                        a.content_sha256.is_some() && target.checksum.is_some()
                    }
            });
            let Some(manifest) = manifest.filter(|m| m.complete && acquired) else {
                uncertain_target = true;
                row.reasons.push("verified source payload inspection unavailable; refresh --sources-only --analysis-mode exact".into());
                continue;
            };
            let has_source = manifest.source_representation == Some(true)
                && exact.is_some_and(|p| {
                    p.state == "analyzed"
                        && p.total_bytes > 0
                        && serde_json::from_str::<Vec<crate::linguist::LanguageBytes>>(
                            &p.languages_json,
                        )
                        .is_ok_and(|rows| rows.iter().any(|row| row.bytes > 0))
                });
            // This is absence of a recorded representation in this verified input,
            // not a claim that upstream source is closed or that its license is proprietary.
            let no_representation = manifest.source_representation == Some(false)
                && exact.is_some_and(|p| p.state == "no_language");
            backed |= has_source;
            uncertain_target |= !has_source && !no_representation;
            for binary in &manifest.binaries {
                candidates.push(Candidate {
                    binary: binary.clone(),
                    location: target.location.clone(),
                    checksum: target.checksum.clone(),
                    confirmed: no_representation && target.revision.is_none(),
                });
            }
        }
        if !candidates.is_empty() {
            if matches!(staleness, SnapshotStaleness::Current) {
                match inspect(&row.name, &candidates) {
                    Ok(evidence) => row.evidence = evidence,
                    Err(reason) => {
                        uncertain_target = true;
                        row.reasons.push(reason);
                    }
                }
            } else {
                uncertain_target = true;
                row.reasons.push("live payload cannot be matched against a stale or unverified inventory snapshot".into());
            }
        }
        if candidates.iter().any(|candidate| {
            candidate.confirmed
                && !row.evidence.iter().any(|e| {
                    e.sha256 == candidate.binary.sha256 && e.bytes == candidate.binary.bytes
                })
        }) {
            uncertain_target = true;
            row.reasons.push("prebuilt input has no byte-identical installed match; remaining bytes stay unknown".into());
        }
        row.opaque_bytes = row
            .evidence
            .iter()
            .filter(|e| e.state == "opaque-confirmed")
            .map(|e| e.bytes)
            .sum();
        let suspected: u64 = row
            .evidence
            .iter()
            .filter(|e| e.state == "opaque-suspected")
            .map(|e| e.bytes)
            .sum();
        if row.opaque_bytes.saturating_add(suspected) > row.installed_bytes {
            row.opaque_bytes = 0;
            row.evidence.clear();
            uncertain_target = true;
            row.reasons.push(
                "matched file bytes exceed recorded installed size; refresh inventory".into(),
            );
        }
        if backed && !uncertain_target {
            row.source_visible_bytes = row.installed_bytes - row.opaque_bytes - suspected;
        }
        row.unknown_bytes = row.installed_bytes - row.opaque_bytes - row.source_visible_bytes;
        row.classification = if row.opaque_bytes > 0 && row.source_visible_bytes > 0 {
            Classification::Mixed
        } else if row.opaque_bytes > 0 {
            Classification::OpaqueBinary
        } else if backed && !uncertain_target && suspected == 0 {
            Classification::SourceBacked
        } else {
            Classification::Unknown
        };
        row.provenance_state = if row.opaque_bytes > 0 {
            "opaque-confirmed"
        } else if suspected > 0 {
            "opaque-suspected"
        } else if row.classification == Classification::SourceBacked {
            "source-backed"
        } else if resolved {
            "source-evidence-incomplete"
        } else {
            "source-unresolved"
        };
        if row.reasons.is_empty() && row.classification == Classification::Unknown {
            row.reasons
                .push("no sufficient payload provenance recorded".into());
        }
        row.reasons.sort();
        row.reasons.dedup();
        packages.push(row);
    }
    let mut summary = Summary {
        policy: "installed-payload-visibility-v1",
        denominator: "installed package bytes excluding source-free packages",
        ..Default::default()
    };
    for row in &packages {
        summary.selected_installed_bytes += row.installed_bytes;
        if row.classification == Classification::SourceFree {
            summary.source_free_bytes += row.installed_bytes;
        }
        summary.source_visible_bytes += row.source_visible_bytes;
        summary.opaque_bytes += row.opaque_bytes;
        summary.unknown_bytes += row.unknown_bytes;
    }
    summary.denominator_bytes = summary.selected_installed_bytes - summary.source_free_bytes;
    if summary.denominator_bytes > 0 {
        let denominator = summary.denominator_bytes as f64;
        summary.source_visible_ratio = Some(summary.source_visible_bytes as f64 / denominator);
        summary.opaque_ratio = Some(summary.opaque_bytes as f64 / denominator);
        summary.unknown_ratio = Some(summary.unknown_bytes as f64 / denominator);
    }
    packages.sort_by(|a, b| {
        b.opaque_bytes
            .cmp(&a.opaque_bytes)
            .then_with(|| a.name.cmp(&b.name))
    });
    Report { summary, packages }
}

impl Summary {
    pub(crate) fn human(&self) -> String {
        fn percent(value: Option<f64>) -> String {
            value.map_or_else(|| "N/A".into(), |v| format!("{:.1}%", v * 100.0))
        }
        format!(
            "Source-visible payload: {}\nOpaque payload:         {}\nUnknown provenance:     {}\nPayload denominator: {} B installed (source-free excluded: {} B)\n",
            percent(self.source_visible_ratio),
            percent(self.opaque_ratio),
            percent(self.unknown_ratio),
            self.denominator_bytes,
            self.source_free_bytes
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elf() -> Vec<u8> {
        let mut bytes = vec![0; 128];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        bytes[16] = 3;
        bytes
    }

    #[test]
    fn binary_evidence_uses_content_not_filenames() {
        assert!(is_elf(&elf()));
        assert!(!is_elf(b"foo-1.2.3-linux-x86_64.tar.gz"));
        assert!(!is_elf(b"#!/bin/sh\necho ELF\n"));
        let mut object = elf();
        object[16] = 1;
        assert!(!is_elf(&object));
        assert!(!is_elf(b"\x7fELF"));
    }

    #[test]
    fn source_inspection_hashes_elf_and_respects_limits_and_links() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("app"), elf()).unwrap();
        fs::write(directory.path().join("readme"), b"hello").unwrap();
        std::os::unix::fs::symlink("app", directory.path().join("alias")).unwrap();
        let mut limits = crate::acquisition::Limits {
            source_jobs: 1,
            max_download_bytes: 1024,
            max_extracted_bytes: 1024,
            max_source_files: 10,
            source_timeout_seconds: 5,
        };
        let manifest = inspect_source(directory.path(), None, &limits);
        assert!(manifest.complete);
        assert_eq!(manifest.binaries.len(), 1);
        assert_eq!(manifest.binaries[0].bytes, 128);
        assert_eq!(
            manifest.binaries[0].sha256,
            format!("{:x}", Sha256::digest(elf()))
        );
        assert!(regular_file(&directory.path().join("alias")).is_err());
        limits.max_extracted_bytes = 64;
        assert!(!inspect_source(directory.path(), None, &limits).complete);
    }
}
