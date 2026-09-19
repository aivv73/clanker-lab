//! Native `github-linguist` adapter.
//!
//! Paclang never downloads or installs the adapter. A configured executable is
//! invoked as `<executable> --json` with its working directory set to the
//! materialized source tree (plus the target subdirectory). Its JSON breakdown
//! becomes one stored source-language profile per analysis target.

use std::path::{Path, PathBuf};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::interrupt;

const MAX_JSON_BYTES: u64 = 4_096_000;
const MAX_LANGUAGES: usize = 256;
const MAX_LANGUAGE_NAME_BYTES: usize = 64;

#[derive(Clone, Debug, clap::Args)]
pub(crate) struct LinguistConfig {
    /// Native `github-linguist` executable used for language analysis.
    /// Absent means sources-only refresh acquires trees without analyzing
    /// them; coverage stays missing.
    #[arg(long, env = "PACLANG_LINGUIST_EXECUTABLE", value_name = "PATH")]
    pub linguist_executable: Option<PathBuf>,

    /// Per-target wall-clock budget for the Linguist invocation.
    #[arg(long, env = "PACLANG_LINGUIST_TIMEOUT_SECONDS", default_value_t = 300, value_parser = clap::value_parser!(u64).range(1..=86_400))]
    pub linguist_timeout_seconds: u64,
}

/// One detected language share. Bytes come from Linguist's `size` field (or a
/// plain integer value in fixture output); rows sort by bytes descending and
/// name ascending so reports are stable and diffable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LanguageBytes {
    pub language: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LanguageProfile {
    pub languages: Vec<LanguageBytes>,
    pub total_bytes: u64,
    pub primary_language: Option<String>,
}

impl LanguageProfile {
    pub(crate) fn from_bytes(mut languages: Vec<LanguageBytes>) -> Self {
        languages.sort_by(|left, right| {
            right
                .bytes
                .cmp(&left.bytes)
                .then_with(|| left.language.cmp(&right.language))
        });
        let total_bytes = languages.iter().map(|row| row.bytes).sum();
        let primary_language = languages.first().map(|row| row.language.clone());
        Self {
            languages,
            total_bytes,
            primary_language,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LanguageRecord {
    pub state: String,
    pub reason: Option<String>,
    pub total_bytes: u64,
    pub primary_language: Option<String>,
    pub languages_json: String,
    pub adapter: String,
    pub source: ProfileSource,
    pub fidelity: RevisionFidelity,
}

/// Where a stored source-language profile came from. Profiles from
/// different sources are never mixed as equal evidence: only `Linguist`
/// profiles describe the packaged revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ProfileSource {
    Linguist,
    GithubApi,
}

impl ProfileSource {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Linguist => "linguist",
            Self::GithubApi => "github-api",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "linguist" => Ok(Self::Linguist),
            "github-api" => Ok(Self::GithubApi),
            _ => Err("profile source is neither linguist nor github-api"),
        }
    }
}

/// How closely a stored profile tracks the packaged source revision.
/// `ExactPackagedSource` profiles were analyzed from the packaged revision
/// itself; `CurrentUpstream` profiles approximate it from upstream HEAD.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RevisionFidelity {
    ExactPackagedSource,
    CurrentUpstream,
}

impl RevisionFidelity {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ExactPackagedSource => "exact-packaged-source",
            Self::CurrentUpstream => "current-upstream",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, &'static str> {
        match value {
            "exact-packaged-source" => Ok(Self::ExactPackagedSource),
            "current-upstream" => Ok(Self::CurrentUpstream),
            _ => Err("revision fidelity is neither exact-packaged-source nor current-upstream"),
        }
    }
}

/// Package-level analysis state derived from stored profiles.
///
/// `Current` means the installed version's own target has an exact analyzed
/// profile. `Approximate` means it has only a current-upstream approximation:
/// different evidence, counted and disclosed separately, never mixed with
/// exact coverage. `Stale` means only an older version's exact target has
/// one: the cached profile does not match the installed package version, but
/// reports include it by default and disclose it separately. `Missing` covers
/// everything else (unresolved source, failed analysis, or a definitive
/// `no_language` result on the current target, which suppresses older exact
/// profiles so a stale language never overrides a known-empty current
/// source).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnalysisState {
    Current,
    Approximate,
    Stale,
    Missing,
}

pub(super) enum Outcome {
    Analyzed(LanguageProfile),
    NoLanguage,
    Failed(String),
    Limited(String),
}

/// Run the configured adapter against one materialized repository. The caller
/// owns timeouts, cancellation, and persistence; this only maps process
/// results to analysis states.
pub(crate) fn analyze_target(
    repository: &Path,
    subdirectory: Option<&str>,
    executable: &Path,
    timeout: Duration,
    max_source_files: u64,
) -> Outcome {
    // Linguist returns an empty map at >= its tree limit. Acquisition allows
    // exactly max_source_files entries, so the adapter needs a strict upper
    // bound one greater than that budget (including custom CLI limits).
    let Some(tree_limit) = max_source_files.checked_add(1) else {
        return Outcome::Limited("language analysis tree limit overflow".to_owned());
    };
    let started = Instant::now();
    let isolated;
    let directory = match subdirectory {
        None => repository.to_path_buf(),
        Some(subdirectory) => match crate::acquisition::safe_path(subdirectory) {
            Ok(relative) => {
                isolated = match isolate_subdirectory(repository, &relative, started, timeout) {
                    Ok(tree) => tree,
                    Err(reason) if reason == "language analysis deadline exceeded" || reason.starts_with("analysis subtree ") => return Outcome::Limited(reason),
                    Err(reason) => return Outcome::Failed(reason),
                };
                isolated.path().to_path_buf()
            }
            Err(_) => return Outcome::Failed("invalid source subdirectory".to_owned()),
        },
    };
    if interrupt::is_requested() {
        return Outcome::Failed("inventory refresh was interrupted".to_owned());
    }
    let mut child = match Command::new(executable)
        .arg("--json")
        .arg(format!("--tree-size={tree_limit}"))
        .current_dir(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Outcome::Failed("cannot start the Linguist adapter".to_owned()),
    };
    // Drain both pipes concurrently: a verbose adapter must not deadlock while
    // the supervisor is waiting. Retain bounded output, discard the remainder.
    fn collect(mut pipe: impl Read + Send + 'static, limit: usize) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut kept = Vec::new();
            let mut buffer = [0; 8192];
            while let Ok(count) = pipe.read(&mut buffer) {
                if count == 0 { break; }
                let take = count.min(limit.saturating_sub(kept.len()));
                kept.extend_from_slice(&buffer[..take]);
            }
            let _ = sender.send(kept);
        });
        receiver
    }
    let stdout = collect(child.stdout.take().unwrap(), MAX_JSON_BYTES as usize + 1);
    let stderr = collect(child.stderr.take().unwrap(), 2048);
    let _guard = interrupt::ActiveProcessGroup::register(child.id());
    loop {
        if interrupt::is_requested() {
            let _ = child.kill();
            let _ = child.wait();
            return Outcome::Failed("inventory refresh was interrupted".to_owned());
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Outcome::Limited("language analysis deadline exceeded".to_owned());
        }
        match child.try_wait() {
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Ok(Some(status)) => {
                let output = stdout.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
                let error = stderr.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
                drop(_guard);
                if !status.success() && !error.is_empty() {
                    let detail: String = String::from_utf8_lossy(&error).chars().take(800)
                        .map(|c| if c.is_control() { ' ' } else { c }).collect();
                    return Outcome::Failed(format!("Linguist adapter failed ({status}): {}", detail.trim()));
                }
                return interpret_output(status.success(), &output);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Outcome::Failed("language analysis did not complete".to_owned());
            }
        }
    }
}

fn isolate_subdirectory(repository: &Path, relative: &Path, started: Instant, timeout: Duration) -> Result<tempfile::TempDir, String> {
    let root = repository.canonicalize().map_err(|e| e.to_string())?;
    let selected = root.join(relative).canonicalize().map_err(|e| e.to_string())?;
    if !selected.starts_with(&root) || !selected.is_dir() { return Err("invalid source subdirectory".into()); }
    let temporary = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut pending = vec![PathBuf::new()];
    let mut links = Vec::new();
    let mut bytes = 0u64;
    let mut files = 0u64;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(selected.join(&directory)).map_err(|e| e.to_string())? {
            if interrupt::is_requested() { return Err("inventory refresh was interrupted".into()); }
            if started.elapsed() >= timeout { return Err("language analysis deadline exceeded".into()); }
            let entry = entry.map_err(|e| e.to_string())?;
            let path = directory.join(entry.file_name());
            let text = path.to_str().ok_or("source path is not UTF-8")?;
            crate::acquisition::safe_path(text).map_err(|e| format!("invalid selected source path: {e:?}"))?;
            files += 1;
            if files > 250_000 { return Err("analysis subtree file limit exceeded".into()); }
            let meta = entry.path().symlink_metadata().map_err(|e| e.to_string())?;
            let destination = temporary.path().join(&path);
            if meta.is_dir() {
                std::fs::create_dir(&destination).map_err(|e| e.to_string())?;
                pending.push(path);
            } else if meta.file_type().is_symlink() {
                let value = std::fs::read_link(entry.path()).map_err(|e| e.to_string())?;
                links.push((path, value.to_str().ok_or("symlink is not UTF-8")?.to_owned()));
            } else if meta.is_file() {
                bytes = bytes.checked_add(meta.len()).ok_or("analysis subtree byte limit exceeded")?;
                if bytes > 4_294_967_296 { return Err("analysis subtree byte limit exceeded".into()); }
                std::fs::copy(entry.path(), &destination).map_err(|e| e.to_string())?;
            } else { return Err("unsupported analysis subtree entry".into()); }
        }
    }
    crate::acquisition::materialize_links(temporary.path(), &links).map_err(|e| format!("invalid analysis subtree link: {e:?}"))?;
    crate::acquisition::initialize_analysis_tree(temporary.path())?;
    Ok(temporary)
}

fn interpret_output(success: bool, stdout: &[u8]) -> Outcome {
    if !success {
        return Outcome::Failed("Linguist adapter reported a failure".to_owned());
    }
    if stdout.len() as u64 > MAX_JSON_BYTES {
        return Outcome::Failed("Linguist output exceeds the size limit".to_owned());
    }
    let text = match std::str::from_utf8(stdout) {
        Ok(text) => text,
        Err(_) => return Outcome::Failed("Linguist output is not UTF-8".to_owned()),
    };
    match parse_breakdown(text) {
        Ok(languages) if languages.is_empty() => Outcome::NoLanguage,
        Ok(languages) => Outcome::Analyzed(LanguageProfile::from_bytes(languages)),
        Err(reason) => Outcome::Failed(reason),
    }
}

/// Accept both real `github-linguist --json` output (`{"Rust": {"size": 1}}`)
/// and plain integer shares (`{"Rust": 1}`) used by fixtures.
pub(crate) fn parse_breakdown(text: &str) -> Result<Vec<LanguageBytes>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "Linguist output is not JSON".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "Linguist output is not a language map".to_owned())?;
    if object.len() > MAX_LANGUAGES {
        return Err("Linguist reported too many languages".to_owned());
    }
    let mut languages = Vec::with_capacity(object.len());
    for (name, entry) in object {
        validate_language_name(name)?;
        let bytes = match entry {
            serde_json::Value::Number(number) => number
                .as_u64()
                .ok_or_else(|| format!("language {name:?} has no byte count"))?,
            serde_json::Value::Object(fields) => fields
                .get("size")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| format!("language {name:?} has no byte count"))?,
            _ => return Err(format!("language {name:?} has no byte count")),
        };
        languages.push(LanguageBytes {
            language: name.clone(),
            bytes,
        });
    }
    // A zero-byte profile carries no detected language.
    languages.retain(|row| row.bytes > 0);
    Ok(languages)
}

fn validate_language_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_LANGUAGE_NAME_BYTES {
        return Err(format!("language {name:?} is not a valid name"));
    }
    if name.chars().any(char::is_control) {
        return Err(format!("language {name:?} is not a valid name"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use super::*;

    #[test]
    fn adapter_scans_trees_at_the_acquisition_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = directory.path().join("adapter");
        // Model Linguist's exclusive cutoff using a fixture's tree size.
        // The default and a custom acquisition cap must both be forwarded.
        std::fs::write(&adapter, "#!/bin/sh\nlimit=100000\nfor arg in \"$@\"; do case \"$arg\" in --tree-size=*) limit=${arg#*=};; esac; done\nread count < tree-count\nif [ \"$count\" -ge \"$limit\" ]; then echo '{}'; else echo '{\"Rust\":42}'; fi\n").unwrap();
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
        for count in [100_000, 250_000, 400_000] {
            std::fs::write(directory.path().join("tree-count"), format!("{count}\n")).unwrap();
            assert!(matches!(analyze_target(directory.path(), None, &adapter, Duration::from_secs(5), count), Outcome::Analyzed(profile) if profile.total_bytes == 42));
        }
        assert!(matches!(analyze_target(directory.path(), None, &adapter, Duration::from_secs(5), u64::MAX), Outcome::Limited(_)));
    }

    #[test]
    fn subdirectory_adapter_gets_its_own_tracked_repository() {
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir(source.path().join("selected")).unwrap();
        std::fs::write(source.path().join("outside.rs"), "fn outside() {}").unwrap();
        std::fs::write(source.path().join("selected/local.sh"), "echo hello\n").unwrap();
        std::fs::write(source.path().join("selected/.gitignore"), "local.sh\n").unwrap();
        let adapter_dir = tempfile::tempdir().unwrap();
        let adapter = adapter_dir.path().join("adapter");
        std::fs::write(&adapter, "#!/bin/sh\n[ -d .git ] && [ -f local.sh ] && [ ! -e outside.rs ] || exit 2\nprintf '{\"Shell\":11}'\n").unwrap();
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(analyze_target(source.path(), Some("selected"), &adapter, Duration::from_secs(5), 250_000), Outcome::Analyzed(profile) if profile.total_bytes == 11));
        assert!(!source.path().join("selected/.git").exists());
        let isolated = isolate_subdirectory(source.path(), Path::new("selected"), Instant::now(), Duration::from_secs(5)).unwrap();
        let repo = git2::Repository::open(isolated.path()).unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        assert!(tree.get_name("local.sh").is_some());
        assert!(tree.get_name("outside.rs").is_none());
    }

    #[test]
    fn subdirectory_isolation_rejects_links_to_outside_files() {
        let source = tempfile::tempdir().unwrap();
        std::fs::create_dir(source.path().join("selected")).unwrap();
        std::os::unix::fs::symlink("../outside", source.path().join("selected/link")).unwrap();
        assert!(isolate_subdirectory(source.path(), Path::new("selected"), Instant::now(), Duration::from_secs(5)).is_err());
        assert!(isolate_subdirectory(source.path(), Path::new(".."), Instant::now(), Duration::from_secs(5)).is_err());
    }

    #[test]
    fn verbose_adapter_failure_retains_bounded_diagnostics_without_deadlock() {
        let directory = tempfile::tempdir().unwrap();
        let adapter = directory.path().join("adapter");
        std::fs::write(&adapter, "#!/bin/sh\ni=0\nwhile [ $i -lt 10000 ]; do echo 'repository unavailable diagnostic text' >&2; i=$((i+1)); done\nexit 7\n").unwrap();
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
        let Outcome::Failed(reason) = analyze_target(directory.path(), None, &adapter, Duration::from_secs(5), 250_000) else { panic!("expected adapter failure"); };
        assert!(reason.contains("repository unavailable"));
        assert!(reason.len() < 4096);
        assert!(!reason.chars().any(char::is_control));
    }

    #[test]
    fn parses_object_and_integer_breakdowns() {
        let object = parse_breakdown(r#"{"Rust": {"size": 10}, "C": {"size": 4}}"#).unwrap();
        assert_eq!(
            LanguageProfile::from_bytes(object)
                .primary_language
                .as_deref(),
            Some("Rust")
        );
        let integers = parse_breakdown(r#"{"Rust": 10, "C": 0}"#).unwrap();
        assert_eq!(integers.len(), 1);
        assert_eq!(integers[0].language, "Rust");
    }

    #[test]
    fn rejects_malformed_breakdowns() {
        for text in [
            "[]",
            r#"{"Rust": "lots"}"#,
            r#"{"Rust": -1}"#,
            r#"{"Rust": {}}"#,
            "{not json",
        ] {
            assert!(parse_breakdown(text).is_err(), "{text}");
        }
        assert!(parse_breakdown(&format!("{{\"{}\": 1}}", "x".repeat(65))).is_err());
    }

    #[test]
    fn profiles_sort_and_total() {
        let profile = LanguageProfile::from_bytes(vec![
            LanguageBytes {
                language: "C".to_owned(),
                bytes: 5,
            },
            LanguageBytes {
                language: "Rust".to_owned(),
                bytes: 5,
            },
        ]);
        assert_eq!(profile.total_bytes, 10);
        // Ties break toward the earlier language name.
        assert_eq!(profile.primary_language.as_deref(), Some("C"));
    }
}
