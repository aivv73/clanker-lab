//! Disposable source acquisition. Workers own untrusted decoding; the parent
//! owns deadlines, cancellation, publication, and temporary-directory lifetime.
mod archive;
mod git;
mod http;
#[cfg(test)]
mod tests;

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct AcquisitionSummary {
    pub targets: u64,
    pub acquired: u64,
    pub failed: u64,
    pub resource_limited: u64,
    pub unresolved_packages: u64,
}
impl AcquisitionSummary {
    pub(crate) fn complete(&self) -> bool {
        self.failed == 0 && self.resource_limited == 0 && self.unresolved_packages == 0
    }
    pub(crate) fn render(&self) -> String {
        format!(
            "Source targets: {}\nAcquired: {}\nFailed acquisition: {}\nResource limited: {}\nPackages without resolved sources: {}",
            self.targets,
            self.acquired,
            self.failed,
            self.resource_limited,
            self.unresolved_packages
        )
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

pub(crate) fn record_sources<'a, W: Write>(
    refresh: &RefreshTransaction<'_>,
    packages: impl IntoIterator<Item = &'a crate::inventory::InstalledPackage>,
    limits: &Limits,
    progress: &mut crate::progress::Progress<W>,
) -> Result<AcquisitionSummary, StorageError> {
    let mut summary = AcquisitionSummary::default();
    let mut targets = Vec::new();
    for package in packages {
        let mut resolved = false;
        for attempt in refresh.provider_attempts(package.name(), package.version())? {
            if let AttemptOutcome::Resolved { target, .. } = attempt.outcome {
                resolved = true;
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        if !resolved {
            summary.unresolved_packages += 1;
        }
    }
    summary.targets = targets.len() as u64;
    progress.set_total(targets.len() as u64);
    acquire_targets(&targets, limits, |target, result, materialized| {
        // This is the pipeline handoff: a later language analyzer consumes the
        // repository here, before the disposable handle leaves this callback.
        if let Some(source) = materialized {
            debug_assert!(source.repository().join(".git").is_dir());
        }
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
        let result = materialize(&job.target, &job.limits, directory, &mut record);
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
) -> Result<(), Error> {
    http::validate_url(&target.location)?;
    let tree = directory.join("tree");
    fs::create_dir(&tree)?;
    if target.revision.is_some() {
        git::fetch(target, limits, directory, &tree, record)?;
    } else {
        let checksum = target.checksum.as_deref().ok_or(Error::Failed(
            "archive has no supported recipe checksum; refresh recipe evidence",
        ))?;
        validate_checksum(checksum)?;
        let archive = directory.join("download");
        http::download(&target.location, &archive, limits, record)?;
        let (kind, expected) = checksum.split_once(':').expect("validated checksum");
        let mut file = File::open(&archive)?;
        let mut buffer = [0u8; 65_536];
        let mut sha256 = Sha256::new();
        let mut sha512 = sha2::Sha512::new();
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            sha256.update(&buffer[..n]);
            sha512.update(&buffer[..n]);
        }
        let digest = format!("{:x}", sha256.finalize());
        let actual = if kind == "sha256" {
            digest.clone()
        } else {
            format!("{:x}", sha512.finalize())
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

pub(super) fn validate_checksum(value: &str) -> Result<(), Error> {
    let (kind, hash) = value
        .split_once(':')
        .ok_or(Error::Failed("invalid source checksum"))?;
    let len = match kind {
        "sha256" => 64,
        "sha512" => 128,
        _ => return Err(Error::Failed("unsupported source checksum")),
    };
    if hash.len() != len || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Failed("invalid source checksum"));
    }
    Ok(())
}

pub(super) fn safe_path(value: &str) -> Result<PathBuf, Error> {
    if value.len() > 4096 || value.contains(['\\', ':']) || value.chars().any(char::is_control) {
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
