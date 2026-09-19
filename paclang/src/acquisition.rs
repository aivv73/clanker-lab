//! Disposable source acquisition. Workers own untrusted decoding; the parent
//! owns deadlines, cancellation, publication, and temporary-directory lifetime.
mod archive;
mod git;
mod git_redirects;
mod http;
pub(crate) fn initialize_analysis_tree(tree: &Path) -> Result<(), String> {
    git::initialize(tree).map_err(|error| format!("cannot initialize analysis tree: {error:?}"))
}
#[cfg(test)]
mod tests;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use blake2::Blake2b512;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::interrupt;
use crate::package::provider::{AnalysisTarget, AttemptOutcome};
use crate::storage::{RefreshTransaction, StorageError};

#[derive(Clone, Debug, clap::Args, Serialize, Deserialize)]
pub(crate) struct Limits {
    /// Maximum simultaneous source targets (ADR 0008 default: four).
    #[arg(long, env = "PACLANG_SOURCE_JOBS", default_value_t = 4, value_parser = clap::value_parser!(u16).range(1..=64))]
    pub source_jobs: u16,
    /// Maximum downloaded bytes per source target.
    #[arg(long, env = "PACLANG_MAX_DOWNLOAD_BYTES", default_value_t = 1_073_741_824, value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
    pub max_download_bytes: u64,
    /// Maximum expanded source bytes per target.
    #[arg(long, env = "PACLANG_MAX_EXTRACTED_BYTES", default_value_t = 4_294_967_296, value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
    pub max_extracted_bytes: u64,
    /// Maximum source entries per target, including directories and links.
    #[arg(long, env = "PACLANG_MAX_SOURCE_FILES", default_value_t = 250_000, value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
    pub max_source_files: u64,
    /// Wall-clock budget per target, including acquisition and materialization.
    #[arg(long, env = "PACLANG_SOURCE_TIMEOUT_SECONDS", default_value_t = 900, value_parser = clap::value_parser!(u64).range(1..=86_400))]
    pub source_timeout_seconds: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AcquisitionRecord {
    pub state: String,
    pub reason: Option<String>,
    pub downloaded_bytes: u64,
    pub extracted_bytes: u64,
    pub files: u64,
    pub verified_revision: Option<String>,
    pub content_sha256: Option<String>,
    #[serde(default)]
    pub submodules: Vec<SubmoduleRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SubmoduleRecord {
    pub path: String,
    pub url: String,
    pub verified_revision: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct AcquisitionSummary {
    pub targets: u64,
    pub acquired: u64,
    pub failed: u64,
    pub resource_limited: u64,
    pub unresolved_packages: u64,
    pub source_free_packages: u64,
    /// Distinct targets skipped because a usable profile was already stored;
    /// nothing was downloaded for them in this refresh.
    pub reused: u64,
}
impl AcquisitionSummary {
    pub(crate) fn complete(&self) -> bool {
        self.failed == 0 && self.resource_limited == 0 && self.unresolved_packages == 0
    }
    pub(crate) fn render(&self) -> String {
        let mut rendered = format!(
            "Source targets: {}\nAcquired: {}\nFailed acquisition: {}\nResource limited: {}\nPackages without resolved sources: {}\nPackages requiring no sources: {}",
            self.targets,
            self.acquired,
            self.failed,
            self.resource_limited,
            self.unresolved_packages,
            self.source_free_packages
        );
        if self.reused > 0 {
            rendered.push_str(&format!("\nReused stored profiles: {}", self.reused));
        }
        rendered
    }
}

#[derive(Debug)]
pub(super) enum Error {
    Failed(&'static str),
    Detail(String),
    Limited(&'static str),
}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::Failed("source filesystem operation failed")
    }
}
impl From<git2::Error> for Error {
    fn from(error: git2::Error) -> Self {
        Self::Detail(format!(
            "source Git operation failed: {}",
            error
                .message()
                .chars()
                .filter(|c| !c.is_control())
                .take(512)
                .collect::<String>()
        ))
    }
}

#[derive(Serialize, Deserialize)]
struct Job {
    target: AnalysisTarget,
    limits: Limits,
    /// Explicit proxy URL. Workers run with a cleared environment, so this
    /// serialized value is the only proxy configuration they can see.
    proxy: Option<String>,
}

/// A verified Git worktree remains available until this handle is dropped.
/// The analysis stage can consume `repository()` inside the completion callback.
pub(crate) struct MaterializedSource {
    directory: TempDir,
}
impl MaterializedSource {
    pub(crate) fn repository(&self) -> PathBuf {
        self.directory.path().join("tree")
    }
}

struct Running {
    target: AnalysisTarget,
    directory: TempDir,
    child: Child,
    started: Instant,
}
impl Drop for Running {
    fn drop(&mut self) {
        // Reap even on a storage error or Ctrl-C before deleting the worktree.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Distinct resolved targets across the selected packages, plus the
/// package counts the refresh summary needs. Shared by the acquisition and
/// fast-analysis paths so both report the same population.
pub(crate) fn collect_targets<'a>(
    refresh: &RefreshTransaction<'_>,
    packages: impl IntoIterator<Item = &'a crate::inventory::InstalledPackage>,
) -> Result<(Vec<AnalysisTarget>, u64, u64), StorageError> {
    let mut targets = Vec::new();
    let mut unresolved_packages = 0;
    let mut source_free_packages = 0;
    for package in packages {
        let mut resolved = false;
        let mut source_free = false;
        for attempt in refresh.provider_attempts(package.name(), package.version())? {
            if matches!(attempt.outcome, AttemptOutcome::SourceFree) {
                source_free = true;
            }
            if let AttemptOutcome::Resolved { target, .. } = attempt.outcome {
                resolved = true;
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        if !resolved && source_free {
            source_free_packages += 1;
        }
        if !resolved && !source_free {
            unresolved_packages += 1;
        }
    }
    Ok((targets, unresolved_packages, source_free_packages))
}

pub(crate) fn record_sources<'a, W: Write>(
    refresh: &RefreshTransaction<'_>,
    packages: impl IntoIterator<Item = &'a crate::inventory::InstalledPackage>,
    limits: &Limits,
    linguist: &crate::linguist::LinguistConfig,
    skip: &[AnalysisTarget],
    progress: &mut crate::progress::Progress<W>,
    proxy: Option<&crate::proxy::ProxyUrl>,
) -> Result<AcquisitionSummary, StorageError> {
    let mut summary = AcquisitionSummary::default();
    let (targets, unresolved_packages, source_free_packages) = collect_targets(refresh, packages)?;
    let attempted: Vec<AnalysisTarget> = targets
        .iter()
        .filter(|target| !skip.contains(target))
        .cloned()
        .collect();
    summary.targets = targets.len() as u64;
    summary.unresolved_packages = unresolved_packages;
    summary.source_free_packages = source_free_packages;
    // Skipped targets keep their stored profiles untouched.
    summary.reused = summary.targets.saturating_sub(attempted.len() as u64);
    progress.set_total(attempted.len() as u64);
    acquire_targets(&attempted, limits, proxy, |target, result, materialized| {
        // Pipeline handoff: the language analyzer consumes the disposable
        // repository here, before the handle leaves this callback.
        let mut source_representation = None;
        if let Some(source) = materialized {
            debug_assert!(source.repository().join(".git").is_dir());
            if let Some(executable) = linguist.linguist_executable.as_deref() {
                let outcome = crate::linguist::analyze_target(
                    &source.repository(),
                    target.subdirectory.as_deref(),
                    executable,
                    std::time::Duration::from_secs(linguist.linguist_timeout_seconds),
                    limits.max_source_files,
                );
                let record = language_record(outcome, executable);
                source_representation = match record.state.as_str() {
                    "analyzed" if record.total_bytes > 0 => Some(true),
                    "no_language" => Some(false),
                    _ => None,
                };
                refresh.record_language_profile(target, &record)?;
            }
        }
        let mut manifest = match materialized {
            Some(source) => crate::payload::inspect_source(
                &source.repository(), target.subdirectory.as_deref(), limits,
            ),
            None => crate::payload::SourceManifest::unavailable("source acquisition incomplete"),
        };
        manifest.source_representation = source_representation;
        refresh.record_source_manifest(target, &manifest)?;
        match result.state.as_str() {
            "acquired" => summary.acquired += 1,
            "resource_limited" => summary.resource_limited += 1,
            _ => summary.failed += 1,
        }
        refresh.record_acquisition(target, result)?;
        progress.advance(
            1,
            result.downloaded_bytes,
            u64::from(result.state == "acquired"),
            u64::from(result.state != "acquired"),
        );
        Ok(())
    })?;
    Ok(summary)
}

fn language_record(
    outcome: crate::linguist::Outcome,
    executable: &Path,
) -> crate::linguist::LanguageRecord {
    let adapter = executable.to_string_lossy().into_owned();
    match outcome {
        crate::linguist::Outcome::Analyzed(profile) => crate::linguist::LanguageRecord {
            state: "analyzed".to_owned(),
            reason: None,
            total_bytes: profile.total_bytes,
            primary_language: profile.primary_language.clone(),
            languages_json: serde_json::to_string(&profile.languages)
                .unwrap_or_else(|_| "[]".to_owned()),
            adapter,
            source: crate::linguist::ProfileSource::Linguist,
            fidelity: crate::linguist::RevisionFidelity::ExactPackagedSource,
        },
        crate::linguist::Outcome::NoLanguage => crate::linguist::LanguageRecord {
            state: "no_language".to_owned(),
            reason: None,
            total_bytes: 0,
            primary_language: None,
            languages_json: "[]".to_owned(),
            adapter,
            source: crate::linguist::ProfileSource::Linguist,
            fidelity: crate::linguist::RevisionFidelity::ExactPackagedSource,
        },
        crate::linguist::Outcome::Failed(reason) => crate::linguist::LanguageRecord {
            state: "failed".to_owned(),
            reason: Some(reason),
            total_bytes: 0,
            primary_language: None,
            languages_json: "[]".to_owned(),
            adapter,
            source: crate::linguist::ProfileSource::Linguist,
            fidelity: crate::linguist::RevisionFidelity::ExactPackagedSource,
        },
        crate::linguist::Outcome::Limited(reason) => crate::linguist::LanguageRecord {
            state: "resource_limited".to_owned(),
            reason: Some(reason),
            total_bytes: 0,
            primary_language: None,
            languages_json: "[]".to_owned(),
            adapter,
            source: crate::linguist::ProfileSource::Linguist,
            fidelity: crate::linguist::RevisionFidelity::ExactPackagedSource,
        },
    }
}

fn cache_directory() -> Result<PathBuf, StorageError> {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or(StorageError::DataDirectoryUnavailable)?;
    let path = root.join("paclang/sources");
    fs::create_dir_all(&path).map_err(StorageError::CreateDirectory)?;
    Ok(path)
}

fn acquire_targets(
    targets: &[AnalysisTarget],
    limits: &Limits,
    proxy: Option<&crate::proxy::ProxyUrl>,
    mut complete: impl FnMut(
        &AnalysisTarget,
        &AcquisitionRecord,
        Option<&MaterializedSource>,
    ) -> Result<(), StorageError>,
) -> Result<(), StorageError> {
    if targets.is_empty() {
        return Ok(());
    }
    let cache = cache_directory()?;
    for batch in targets.chunks(usize::from(limits.source_jobs)) {
        let mut running = Vec::new();
        for target in batch {
            if interrupt::is_requested() {
                return Err(StorageError::Interrupted);
            }
            let directory = tempfile::Builder::new()
                .prefix("target-")
                .tempdir_in(&cache)
                .map_err(StorageError::CreateDirectory)?;
            let job = serde_json::to_vec(&Job {
                target: target.clone(),
                limits: limits.clone(),
                proxy: proxy.map(|url| url.as_str().to_owned()),
            })
            .expect("serializable job");
            fs::write(directory.path().join("job.json"), job)
                .map_err(StorageError::CreateDirectory)?;
            let child =
                Command::new(std::env::current_exe().map_err(StorageError::CreateDirectory)?)
                    .arg("--acquisition-worker")
                    .arg(directory.path())
                    .env_clear()
                    .env("HOME", directory.path())
                    .env("XDG_CONFIG_HOME", directory.path())
                    .env("TMPDIR", directory.path())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            match child {
                Ok(child) => running.push(Running {
                    target: target.clone(),
                    directory,
                    child,
                    started: Instant::now(),
                }),
                Err(_) => complete(
                    target,
                    &AcquisitionRecord {
                        state: "failed".to_owned(),
                        reason: Some("cannot start source worker".to_owned()),
                        ..Default::default()
                    },
                    None,
                )?,
            }
        }
        while !running.is_empty() {
            if interrupt::is_requested() {
                return Err(StorageError::Interrupted);
            }
            let mut index = 0;
            while index < running.len() {
                let timed_out = running[index].started.elapsed()
                    >= Duration::from_secs(limits.source_timeout_seconds);
                let exited = running[index]
                    .child
                    .try_wait()
                    .map_err(StorageError::CreateDirectory)?;
                if !timed_out && exited.is_none() {
                    index += 1;
                    continue;
                }
                let mut task = running.swap_remove(index);
                let record = if timed_out && exited.is_none() {
                    let _ = task.child.kill();
                    let _ = task.child.wait();
                    AcquisitionRecord {
                        state: "resource_limited".to_owned(),
                        reason: Some("source deadline exceeded".to_owned()),
                        ..Default::default()
                    }
                } else {
                    File::open(task.directory.path().join("result.json"))
                        .ok()
                        .and_then(|file| serde_json::from_reader(file.take(16_384)).ok())
                        .unwrap_or_else(|| AcquisitionRecord {
                            state: "failed".to_owned(),
                            reason: Some("source worker did not complete".to_owned()),
                            ..Default::default()
                        })
                };
                // Running's Drop still owns the parent directory; borrow its
                // completed repository for the callback, then clean everything.
                if record.state == "acquired" {
                    let source_dir = tempfile::Builder::new()
                        .prefix("ready-")
                        .tempdir_in(&cache)
                        .map_err(StorageError::CreateDirectory)?;
                    fs::rename(
                        task.directory.path().join("tree"),
                        source_dir.path().join("tree"),
                    )
                    .map_err(StorageError::CreateDirectory)?;
                    let source = MaterializedSource {
                        directory: source_dir,
                    };
                    complete(&task.target, &record, Some(&source))?;
                } else {
                    complete(&task.target, &record, None)?;
                }
            }
            if !running.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
    Ok(())
}

/// Internal worker protocol is intercepted before CLI parsing and never runs a
/// refresh or publishes database state. The supervisor enforces wall time.
pub(crate) fn worker(directory: &Path) -> bool {
    let outcome = (|| -> Result<(), Box<dyn std::error::Error>> {
        let job: Job =
            serde_json::from_reader(File::open(directory.join("job.json"))?.take(65_536))?;
        // libarchive converts UTF-8 archive names using LC_CTYPE. The isolated
        // worker otherwise inherits the C locale and rejects valid names.
        // This process is single-threaded; never change locale in the parent.
        if unsafe { nix::libc::setlocale(nix::libc::LC_CTYPE, c"C.UTF-8".as_ptr()) }.is_null() {
            return Err("source worker requires the C.UTF-8 locale".into());
        }
        // The worker is single-threaded and has not opened a repository yet.
        // Disable user/system URL rewrites, proxies, templates and filters.
        let empty_config = directory.join("empty-config");
        fs::create_dir(&empty_config)?;
        for level in [
            git2::ConfigLevel::System,
            git2::ConfigLevel::Global,
            git2::ConfigLevel::XDG,
        ] {
            unsafe {
                git2::opts::set_search_path(level, empty_config.as_os_str())?;
            }
        }
        let mut record = AcquisitionRecord::default();
        let result = materialize(
            &job.target,
            &job.limits,
            directory,
            &mut record,
            job.proxy.as_deref(),
        );
        match result {
            Ok(()) => record.state = "acquired".to_owned(),
            Err(Error::Detail(reason)) => {
                record.state = "failed".to_owned();
                record.reason = Some(reason);
            }
            Err(Error::Failed(reason)) => {
                record.state = "failed".to_owned();
                record.reason = Some(reason.to_owned());
            }
            Err(Error::Limited(reason)) => {
                record.state = "resource_limited".to_owned();
                record.reason = Some(reason.to_owned());
            }
        }
        fs::write(directory.join("result.json"), serde_json::to_vec(&record)?)?;
        Ok(())
    })();
    outcome.is_ok()
}

fn materialize(
    target: &AnalysisTarget,
    limits: &Limits,
    directory: &Path,
    record: &mut AcquisitionRecord,
    proxy: Option<&str>,
) -> Result<(), Error> {
    http::validate_url(&target.location)?;
    let tree = directory.join("tree");
    fs::create_dir(&tree)?;
    if target.revision.is_some() {
        git::fetch(target, limits, directory, &tree, record, proxy)?;
    } else {
        let checksum = target.checksum.as_deref().ok_or(Error::Failed(
            "archive has no supported recipe checksum; refresh recipe evidence",
        ))?;
        validate_checksum(checksum)?;
        let archive = directory.join("download");
        http::download(&target.location, &archive, limits, record, proxy)?;
        let (kind, expected) = checksum.split_once(':').expect("validated checksum");
        let (digest, sha512_digest, b2_digest) = archive_digests(&archive)?;
        let actual = match kind {
            "sha256" => digest.clone(),
            "sha512" => sha512_digest,
            _ => b2_digest,
        };
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(Error::Failed("archive checksum mismatch"));
        }
        record.content_sha256 = Some(digest);
        archive::extract(&archive, &tree, limits, record)?;
        // Normalize the usual single archive wrapper directory.
        let entries: Vec<_> = fs::read_dir(&tree)?.collect::<Result<_, _>>()?;
        if entries.len() == 1 && entries[0].file_type()?.is_dir() {
            let wrapper = directory.join("wrapper");
            fs::rename(&tree, &wrapper)?;
            fs::rename(wrapper.join(entries[0].file_name()), &tree)?;
        }
        git::initialize(&tree)?;
    }
    if let Some(subdirectory) = &target.subdirectory {
        let path = safe_path(subdirectory)?;
        let selected = fs::canonicalize(tree.join(path))?;
        if !selected.starts_with(fs::canonicalize(&tree)?) || !selected.is_dir() {
            return Err(Error::Failed("invalid source subdirectory"));
        }
    }
    Ok(())
}

/// SHA-256, SHA-512, and BLAKE2b-512 hex digests of one downloaded archive.
/// A single streaming pass feeds all three hashers; verification selects its
/// checksum kind afterwards.
fn archive_digests(archive: &Path) -> Result<(String, String, String), Error> {
    let mut file = File::open(archive)?;
    let mut buffer = [0u8; 65_536];
    let mut sha256 = Sha256::new();
    let mut sha512 = sha2::Sha512::new();
    let mut b2 = Blake2b512::new();
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        sha256.update(&buffer[..n]);
        sha512.update(&buffer[..n]);
        b2.update(&buffer[..n]);
    }
    Ok((
        format!("{:x}", sha256.finalize()),
        format!("{:x}", sha512.finalize()),
        format!("{:x}", b2.finalize()),
    ))
}

pub(crate) fn validate_checksum(value: &str) -> Result<(), Error> {
    let (kind, hash) = value
        .split_once(':')
        .ok_or(Error::Failed("invalid source checksum"))?;
    let len = match kind {
        "sha256" => 64,
        "sha512" => 128,
        "b2" => 128,
        _ => return Err(Error::Failed("unsupported source checksum")),
    };
    if hash.len() != len || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Failed("invalid source checksum"));
    }
    Ok(())
}

pub(crate) fn safe_path(value: &str) -> Result<PathBuf, Error> {
    if value.len() > 4096 || value.contains('\\') || value.chars().any(char::is_control) {
        return Err(Error::Failed("unsafe source path"));
    }
    let mut path = PathBuf::new();
    for component in Path::new(value).components() {
        match component {
            Component::Normal(name) if !name.eq_ignore_ascii_case(".git") => path.push(name),
            Component::CurDir => {}
            _ => return Err(Error::Failed("unsafe source path")),
        }
    }
    if path.components().count() > 128 {
        return Err(Error::Limited("source tree depth exceeded"));
    }
    if path.as_os_str().is_empty() {
        return Err(Error::Failed("empty source path"));
    }
    Ok(path)
}

// Resolve link components in filesystem order, expanding other pending links
// before processing '..'. Lexical normalization alone misses chain escapes.
pub(super) fn materialize_links(root: &Path, links: &[(PathBuf, String)]) -> Result<(), Error> {
    use std::collections::{HashMap, VecDeque};
    let map: HashMap<_, _> = links.iter().map(|(p, v)| (p.clone(), v)).collect();
    if map.len() != links.len() { return Err(Error::Failed("duplicate source symlink")); }
    for (path, target) in links {
        let check = || -> Result<(), Error> {
            let mut pending: VecDeque<String> = path.parent().unwrap_or(Path::new("")).components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            pending.extend(target.split('/').map(str::to_owned));
            let mut resolved = PathBuf::new();
            let mut expansions = 0;
            if target.is_empty() || target.starts_with('/') || target.len() > 4096 {
                return Err(Error::Failed("invalid relative symlink target"));
            }
            while let Some(part) = pending.pop_front() {
                match part.as_str() {
                    "" | "." => continue,
                    ".." => {
                        if !resolved.pop() { return Err(Error::Failed("symlink escapes source root")); }
                    }
                    _ => {
                        safe_path(&part)?;
                        resolved.push(&part);
                        if let Some(value) = map.get(&resolved) {
                            expansions += 1;
                            if expansions > 40 { return Err(Error::Failed("cyclic or excessive symlink chain")); }
                            if value.is_empty() || value.starts_with('/') || value.len() > 4096 {
                                return Err(Error::Failed("invalid relative symlink target"));
                            }
                            resolved.pop();
                            for component in value.split('/').rev() { pending.push_front(component.to_owned()); }
                        }
                    }
                }
                if resolved.components().count() > 128 { return Err(Error::Limited("source tree depth exceeded")); }
            }
            // No symlink may be used as the parent of another entry.
            let mut parent = root.to_path_buf();
            for part in path.parent().unwrap_or(Path::new("")).components() {
                parent.push(part);
                if !fs::symlink_metadata(&parent)?.is_dir() { return Err(Error::Failed("symlink parent is not a directory")); }
            }
            Ok(())
        };
        check().map_err(|e| Error::Detail(format!("source symlink {:?} -> {:?}: {e:?}", path, target)))?;
    }
    for (path, target) in links { std::os::unix::fs::symlink(target, root.join(path))?; }
    Ok(())
}

pub(super) fn account(
    record: &mut AcquisitionRecord,
    limits: &Limits,
    bytes: u64,
) -> Result<(), Error> {
    if record.files >= limits.max_source_files {
        return Err(Error::Limited("source file count exceeded"));
    }
    if bytes
        > limits
            .max_extracted_bytes
            .saturating_sub(record.extracted_bytes)
    {
        return Err(Error::Limited("expanded source byte limit exceeded"));
    }
    record.files += 1;
    record.extracted_bytes += bytes;
    Ok(())
}

pub(super) fn ensure_directories(
    root: &Path,
    relative: &Path,
    limits: &Limits,
    record: &mut AcquisitionRecord,
) -> Result<(), Error> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(Error::Failed(
                    "source directory collides with a file or link",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                account(record, limits, 0)?;
                fs::create_dir(&current)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
