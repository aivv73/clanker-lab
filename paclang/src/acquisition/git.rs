use super::{AcquisitionRecord, Error, Limits, account, materialize_links, safe_path};
use crate::package::provider::AnalysisTarget;
use git2::{
    AutotagOption, FetchOptions, IndexEntry, IndexTime, ObjectType, Oid, ProxyOptions,
    RemoteCallbacks, RemoteRedirect, Repository, RepositoryInitOptions, Signature,
};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

struct Context<'a> {
    limits: &'a Limits,
    directory: &'a Path,
    record: &'a mut AcquisitionRecord,
    proxy: Option<&'a str>,
    expanded: u64,
    repositories: usize,
}

pub(super) fn fetch(
    target: &AnalysisTarget,
    limits: &Limits,
    directory: &Path,
    tree: &Path,
    record: &mut AcquisitionRecord,
    proxy: Option<&str>,
) -> Result<(), Error> {
    git2::opts::strict_hash_verification(true);
    let mut context = Context {
        limits,
        directory,
        record,
        proxy,
        expanded: 0,
        repositories: 0,
    };
    let (repo, id, location) = fetch_repository(target, &mut context)?;
    context.record.verified_revision = Some(id.to_string());
    let mut links = Vec::new();
    materialize_repository(
        &repo,
        id,
        &location,
        tree,
        Path::new(""),
        &mut context,
        &mut links,
        0,
    )?;
    materialize_links(tree, &links)?;
    initialize(tree)
}

fn fetch_repository(
    target: &AnalysisTarget,
    context: &mut Context<'_>,
) -> Result<(Repository, Oid, String), Error> {
    let limits = context.limits;
    let proxy = context.proxy;
    if context.repositories >= 129 {
        return Err(Error::Limited("Git submodule count exceeded"));
    }
    let directory = context
        .directory
        .join(format!("upstream-{}", context.repositories));
    context.repositories += 1;
    super::http::validate_url(&target.location)?;
    let revision = target
        .revision
        .as_deref()
        .ok_or(Error::Failed("Git source has no revision"))?;
    let (kind, value) = revision
        .split_once(':')
        .ok_or(Error::Failed("invalid Git revision"))?;
    let reference = match kind {
        "commit" if value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()) => {
            value.to_owned()
        }
        "tag" if git2::Reference::is_valid_name(&format!("refs/tags/{value}")) => {
            format!("refs/tags/{value}")
        }
        "commit" => {
            return Err(Error::Failed(
                "Git transport supports full SHA-1 commit IDs only",
            ));
        }
        _ => return Err(Error::Failed("Git source must pin a tag or full commit")),
    };
    let repo = Repository::init_opts(
        directory,
        RepositoryInitOptions::new()
            .bare(true)
            .external_template(false),
    )?;
    let mut config = repo.config()?;
    // Explicit proxy only: the worker environment carries no proxy
    // variables, and user/system git configs are neutralized at startup.
    config.set_str("http.proxy", proxy.unwrap_or(""))?;
    config.set_bool("http.sslVerify", true)?;
    let mut location = target.location.clone();
    if let Err(error) = fetch_once(&repo, &reference, &location, context) {
        // Retry discovery only for libgit2 redirect refusals.
        if !matches!(&error, Error::Detail(reason) if reason.contains("redirect")) {
            return Err(error);
        }
        let redirected = super::git_redirects::resolve(&location, proxy, limits)?;
        if redirected == location {
            return Err(error);
        }
        location = redirected;
        fetch_once(&repo, &reference, &location, context)?;
    }
    let commit = repo
        .find_reference("refs/paclang/source")?
        .peel_to_commit()?;
    if kind == "commit" && commit.id() != Oid::from_str(value)? {
        return Err(Error::Failed(
            "fetched Git commit does not match pinned revision",
        ));
    }
    // Verify every fetched object's hash. Check expanded object sizes before
    // reading bodies; reject a pack whose expansion exceeds the source budget.
    let odb = repo.odb()?;
    let mut object_error = None;
    let verified = odb.foreach(|oid| {
        let checked = (|| -> Result<(), Error> {
            let (size, _) = odb.read_header(*oid)?;
            context.expanded = context
                .expanded
                .checked_add(size as u64)
                .ok_or(Error::Limited("Git object expansion limit exceeded"))?;
            if context.expanded > limits.max_extracted_bytes {
                return Err(Error::Limited("Git object expansion limit exceeded"));
            }
            let object = odb.read(*oid)?;
            if Oid::hash_object(object.kind(), object.data())? != *oid {
                return Err(Error::Failed("Git object hash mismatch"));
            }
            Ok(())
        })();
        if let Err(error) = checked {
            object_error = Some(error);
            return false;
        }
        true
    });
    if let Some(error) = object_error {
        return Err(error);
    }
    verified?;
    let id = commit.id();
    drop(commit);
    drop(odb);
    Ok((repo, id, location))
}

fn fetch_once(
    repo: &Repository,
    reference: &str,
    location: &str,
    context: &mut Context<'_>,
) -> Result<(), Error> {
    let result = fetch_refspecs(
        repo, &[format!("+{reference}:refs/paclang/source")], 1, location, context,
    );
    if reference.len() == 40
        && reference.bytes().all(|b| b.is_ascii_hexdigit())
        && matches!(&result, Err(Error::Detail(reason)) if reason.contains("cannot fetch a specific object from the remote repository"))
    {
        // Some servers refuse SHA wants even for reachable commits. Fetch a
        // bounded window of advertised history, never substitute a ref's tip.
        fetch_refspecs(
            repo,
            &[
                "+refs/heads/*:refs/paclang/heads/*".to_owned(),
                "+refs/tags/*:refs/paclang/tags/*".to_owned(),
            ],
            256,
            location,
            context,
        )?;
        let id = Oid::from_str(reference)?;
        repo.find_commit(id).map_err(|_| Error::Failed(
            "pinned Git commit absent from bounded advertised history (depth 256)",
        ))?;
        repo.reference("refs/paclang/source", id, true, "verified pinned commit")?;
        return Ok(());
    }
    result
}

fn fetch_refspecs(
    repo: &Repository,
    refspecs: &[String],
    depth: i32,
    location: &str,
    context: &mut Context<'_>,
) -> Result<(), Error> {
    let limits = context.limits;
    let proxy = context.proxy;
    let limited = Cell::new(false);
    let downloaded = Cell::new(0u64);
    let mut callbacks = RemoteCallbacks::new();
    callbacks.transfer_progress(|stats| {
        downloaded.set(stats.received_bytes() as u64);
        if downloaded.get()
            > limits
                .max_download_bytes
                .saturating_sub(context.record.downloaded_bytes)
        {
            limited.set(true);
            return false;
        }
        true
    });
    // libgit2 defaults to GIT_PROXY_NONE, which ignores http.proxy config.
    // Pass the explicit URL in fetch options; retain NONE without --proxy-url.
    let mut proxy_options = ProxyOptions::new();
    if let Some(url) = proxy {
        proxy_options.url(url);
    }
    let mut options = FetchOptions::new();
    options
        .proxy_options(proxy_options)
        .remote_callbacks(callbacks)
        .depth(depth)
        .download_tags(AutotagOption::None)
        .follow_redirects(RemoteRedirect::None);
    let mut remote = repo.remote_anonymous(location)?;
    let result = remote.fetch(
        refspecs,
        Some(&mut options),
        None,
    );
    drop(options);
    context.record.downloaded_bytes = context
        .record
        .downloaded_bytes
        .saturating_add(downloaded.get());
    if limited.get() {
        return Err(Error::Limited("source download byte limit exceeded"));
    }
    result?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn materialize_repository(
    repo: &Repository,
    id: Oid,
    url: &str,
    destination: &Path,
    prefix: &Path,
    context: &mut Context<'_>,
    links: &mut Vec<(std::path::PathBuf, String)>,
    depth: usize,
) -> Result<(), Error> {
    if depth > 8 {
        return Err(Error::Limited("Git submodule nesting limit exceeded"));
    }
    let source = repo.find_commit(id)?.tree()?;
    let modules = submodule_urls(repo, &source)?;
    write_tree(
        repo,
        &source,
        destination,
        prefix,
        prefix,
        url,
        &modules,
        context,
        links,
        depth,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_tree(
    repo: &Repository,
    source: &git2::Tree<'_>,
    destination: &Path,
    prefix: &Path,
    repository_prefix: &Path,
    repository_url: &str,
    modules: &BTreeMap<std::path::PathBuf, String>,
    context: &mut Context<'_>,
    links: &mut Vec<(std::path::PathBuf, String)>,
    depth: usize,
) -> Result<(), Error> {
    if prefix.components().count() > 128 {
        return Err(Error::Limited("source tree depth exceeded"));
    }
    for entry in source {
        let name = entry.name().ok_or(Error::Failed("Git path is not UTF-8"))?;
        let name = safe_path(name)?;
        if name.components().count() != 1 {
            return Err(Error::Failed(
                "Git tree entry is not a single path component",
            ));
        }
        let path = prefix.join(name);
        let output = destination.join(&path);
        match entry.kind() {
            Some(ObjectType::Tree) => {
                account(context.record, context.limits, 0)?;
                fs::create_dir(&output)?;
                write_tree(
                    repo,
                    &repo.find_tree(entry.id())?,
                    destination,
                    &path,
                    repository_prefix,
                    repository_url,
                    modules,
                    context,
                    links,
                    depth,
                )?;
            }
            Some(ObjectType::Blob) => {
                let (size, _) = repo.odb()?.read_header(entry.id())?;
                account(context.record, context.limits, size as u64)?;
                let blob = repo.find_blob(entry.id())?;
                if entry.filemode() == 0o120000 {
                    let value = std::str::from_utf8(blob.content())
                        .map_err(|_| Error::Failed("Git symlink is not UTF-8"))?;
                    links.push((path, value.to_owned()));
                } else {
                    fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&output)?
                        .write_all(blob.content())?;
                    fs::set_permissions(
                        output,
                        fs::Permissions::from_mode(if entry.filemode() == 0o100755 {
                            0o755
                        } else {
                            0o644
                        }),
                    )?;
                }
            }
            Some(ObjectType::Commit) if entry.filemode() == 0o160000 => {
                let relative = path
                    .strip_prefix(repository_prefix)
                    .map_err(|_| Error::Failed("invalid submodule path"))?;
                let location = modules.get(relative).ok_or(Error::Failed(
                    "Git gitlink has no root .gitmodules mapping; source tree is incomplete",
                ))?;
                let location = resolve_submodule_url(repository_url, location)?;
                if depth >= 8 {
                    return Err(Error::Limited("Git submodule nesting limit exceeded"));
                }
                let target = AnalysisTarget {
                    location: location.clone(),
                    revision: Some(format!("commit:{}", entry.id())),
                    subdirectory: None,
                    checksum: None,
                };
                let (child, id, location) = fetch_repository(&target, context)?;
                account(context.record, context.limits, 0)?;
                fs::create_dir(&output)?;
                context.record.submodules.push(super::SubmoduleRecord {
                    path: path.to_string_lossy().into_owned(),
                    url: location.clone(),
                    verified_revision: id.to_string(),
                });
                materialize_repository(
                    &child,
                    id,
                    &location,
                    destination,
                    &path,
                    context,
                    links,
                    depth + 1,
                )?;
            }
            _ => return Err(Error::Failed("unsupported Git tree object")),
        }
    }
    Ok(())
}

fn resolve_submodule_url(parent: &str, child: &str) -> Result<String, Error> {
    if child.len() > 4096 || child.contains('\\') || child.chars().any(char::is_control) {
        return Err(Error::Failed("invalid Git submodule URL"));
    }
    // Savannah documents these git:// aliases and the /git/ HTTPS endpoint:
    // https://savannah.gnu.org/maintenance/UsingGit
    // Restrict this transport substitution to the two audited repositories.
    // The caller still fetches and verifies the original gitlink commit.
    let child = match child {
        "git://git.sv.gnu.org/gnulib.git" | "git://git.sv.gnu.org/gnulib"
        | "git://git.savannah.gnu.org/gnulib.git" | "git://git.savannah.gnu.org/gnulib" =>
            "https://git.savannah.gnu.org/git/gnulib.git",
        "git://git.sv.gnu.org/paxutils.git" | "git://git.sv.gnu.org/paxutils"
        | "git://git.savannah.gnu.org/paxutils.git" | "git://git.savannah.gnu.org/paxutils" =>
            "https://git.savannah.gnu.org/git/paxutils.git",
        _ => child,
    };
    let resolved = if child.starts_with("../") || child.starts_with("./") {
        let mut base = super::http::validate_url(parent)?;
        base.set_query(None);
        // Git resolves relative URLs from the repository directory itself.
        base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
        base.join(child)
            .map_err(|_| Error::Failed("invalid relative submodule URL"))?
            .to_string()
    } else {
        child.to_owned()
    };
    let url = super::http::validate_url(&resolved)?;
    if url.query().is_some() {
        return Err(Error::Failed("Git submodule URL query is unsupported"));
    }
    Ok(url.to_string())
}

/// Parse a bounded, deliberately restricted Git config subset, never loading
/// includes or running update commands. Unsupported syntax fails explicitly.
fn submodule_urls(
    repo: &Repository,
    tree: &git2::Tree<'_>,
) -> Result<BTreeMap<std::path::PathBuf, String>, Error> {
    let Some(entry) = tree.get_name(".gitmodules") else {
        return Ok(BTreeMap::new());
    };
    if entry.filemode() != 0o100644 && entry.filemode() != 0o100755 {
        return Err(Error::Failed(".gitmodules must be a regular file"));
    }
    let (size, _) = repo.odb()?.read_header(entry.id())?;
    if size > 65536 {
        return Err(Error::Limited(".gitmodules size limit exceeded"));
    }
    let blob = repo.find_blob(entry.id())?;
    let text = std::str::from_utf8(blob.content())
        .map_err(|_| Error::Failed(".gitmodules is not UTF-8"))?;
    parse_modules(text)
}

fn config_value(input: &str) -> Result<String, Error> {
    let mut result = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in input.trim().chars() {
        if escaped {
            result.push(match ch {
                'n' => '\n',
                't' => '\t',
                'b' => '\u{8}',
                '\\' => '\\',
                '"' => '"',
                _ => return Err(Error::Failed("unsupported .gitmodules escape")),
            });
            escaped = false;
        } else {
            match ch {
                '\\' => escaped = true,
                '"' => quoted = !quoted,
                '#' | ';' if !quoted => break,
                _ => result.push(ch),
            }
        }
    }
    if quoted || escaped {
        return Err(Error::Failed(
            "unsupported .gitmodules quoting or continuation",
        ));
    }
    Ok(result.trim().to_owned())
}

fn parse_modules(text: &str) -> Result<BTreeMap<std::path::PathBuf, String>, Error> {
    let mut sections = BTreeMap::<String, BTreeMap<String, String>>::new();
    let mut current = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let end = line
                .rfind(']')
                .ok_or(Error::Failed("invalid .gitmodules section"))?;
            let tail = line[end + 1..].trim();
            if !tail.is_empty() && !tail.starts_with('#') && !tail.starts_with(';') {
                return Err(Error::Failed("invalid .gitmodules section suffix"));
            }
            let section = line[1..end].trim();
            let (kind, name) = section
                .split_once(char::is_whitespace)
                .ok_or(Error::Failed("unsupported .gitmodules section"))?;
            let name = name.trim();
            if !kind.eq_ignore_ascii_case("submodule")
                || !name.starts_with('"')
                || !name.ends_with('"')
            {
                return Err(Error::Failed(
                    "unsupported .gitmodules section; includes are not allowed",
                ));
            }
            let name = config_value(name)?;
            if name.is_empty() || sections.contains_key(&name) {
                return Err(Error::Failed("duplicate or empty .gitmodules section"));
            }
            sections.insert(name.clone(), BTreeMap::new());
            if sections.len() > 128 {
                return Err(Error::Limited("Git submodule count exceeded"));
            }
            current = Some(name);
        } else {
            let section = current
                .as_ref()
                .ok_or(Error::Failed(".gitmodules key outside section"))?;
            let (key, value) = line.split_once('=').unwrap_or((line, "true"));
            let key = key.trim().to_ascii_lowercase();
            if !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') || key.is_empty() {
                return Err(Error::Failed("invalid .gitmodules key"));
            }
            let value = config_value(value)?;
            if sections
                .get_mut(section)
                .unwrap()
                .insert(key, value)
                .is_some()
            {
                return Err(Error::Failed("duplicate .gitmodules key"));
            }
        }
    }
    let mut result = BTreeMap::new();
    for values in sections.values() {
        let path = values
            .get("path")
            .ok_or(Error::Failed("submodule path missing"))?;
        let url = values
            .get("url")
            .ok_or(Error::Failed("submodule URL missing"))?;
        let path = safe_path(path)?;
        if result.insert(path, url.clone()).is_some() {
            return Err(Error::Failed("duplicate submodule path"));
        }
    }
    Ok(result)
}

/// Build a fresh repository without config, hooks, filters, ignored-file rules,
/// or a working-tree checkout supplied by upstream. Track every source file.
pub(super) fn initialize(tree: &Path) -> Result<(), Error> {
    let repo = Repository::init_opts(tree, RepositoryInitOptions::new().external_template(false))?;
    let mut index = repo.index()?;
    let mut pending = vec![tree.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.path() == tree.join(".git") {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.is_dir() {
                pending.push(entry.path());
                continue;
            }
            let path = entry.path();
            let relative = path
                .strip_prefix(tree)
                .map_err(|_| Error::Failed("invalid materialized path"))?;
            let id = if metadata.file_type().is_symlink() {
                repo.blob(fs::read_link(&path)?.as_os_str().as_encoded_bytes())?
            } else {
                repo.blob_path(&path)?
            };
            index.add(&IndexEntry {
                ctime: IndexTime::new(0, 0),
                mtime: IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: if metadata.file_type().is_symlink() {
                    0o120000
                } else if metadata.permissions().mode() & 0o111 != 0 {
                    0o100755
                } else {
                    0o100644
                },
                uid: 0,
                gid: 0,
                file_size: metadata.len().min(u32::MAX as u64) as u32,
                id,
                flags: 0,
                flags_extended: 0,
                path: relative.as_os_str().as_encoded_bytes().to_vec(),
            })?;
        }
    }
    index.write()?;
    let tree_id = index.write_tree()?;
    let signature = Signature::new("Paclang", "paclang@localhost", &git2::Time::new(0, 0))?;
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "Verified packaged source",
        &repo.find_tree(tree_id)?,
        &[],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audited_savannah_submodule_aliases_use_https_only() {
        let parent = "https://git.savannah.gnu.org/git/tar.git";
        for host in ["git.sv.gnu.org", "git.savannah.gnu.org"] {
            for repo in ["gnulib", "paxutils"] {
                for suffix in ["", ".git"] {
                    assert_eq!(resolve_submodule_url(parent, &format!("git://{host}/{repo}{suffix}")).unwrap(), format!("https://git.savannah.gnu.org/git/{repo}.git"));
                }
            }
        }
        for input in [
            "git://example.org/gnulib.git",
            "git://git.sv.gnu.org.attacker.example/gnulib.git",
            "git://git.sv.gnu.org/unknown.git",
            "git://git.sv.gnu.org/gnulib.git?branch=main",
            "git://git.sv.gnu.org/gnulib.git#main",
            "git://user@git.sv.gnu.org/gnulib.git",
            "git://git.sv.gnu.org:9418/gnulib.git",
            "git://git.sv.gnu.org/%67nulib.git",
            "git://git.sv.gnu.org/../gnulib.git",
            "http://git.sv.gnu.org/gnulib.git",
        ] {
            assert!(resolve_submodule_url(parent, input).is_err(), "{input}");
        }
    }

    #[test]
    fn submodule_urls_follow_repository_relative_semantics_and_reject_other_transports() {
        let parent = "https://code.qt.io/qt/qtquick3d";
        assert_eq!(
            resolve_submodule_url(parent, "../qtquick3d-assimp.git").unwrap(),
            "https://code.qt.io/qt/qtquick3d-assimp.git"
        );
        assert_eq!(
            resolve_submodule_url(parent, "./child.git").unwrap(),
            "https://code.qt.io/qt/qtquick3d/child.git"
        );
        for url in [
            "git@github.com:owner/repo.git",
            "ssh://host/repo",
            "file:///tmp/repo",
            "/tmp/repo",
            "http://example.org/repo",
            "https://user:pass@example.org/repo",
            "https://example.org/repo#main",
            "https://example.org/repo?branch=main",
            "../bad\\repo",
        ] {
            assert!(resolve_submodule_url(parent, url).is_err(), "{url}");
        }
    }

    #[test]
    fn submodule_metadata_is_bounded_unambiguous_and_never_loads_includes() {
        let modules = parse_modules("[submodule \"gir\"] # comment\n path = \"vendor/gir\"\n url = ../gir.git # comment\n update = none\n branch = main\n").unwrap();
        assert_eq!(modules[Path::new("vendor/gir")], "../gir.git");
        for text in [
            "[include]\npath = /etc/gitconfig",
            "[submodule \"x\"]\npath = ../escape\nurl = ../x.git",
            "[submodule \"x\"]\npath = .git/x\nurl = ../x.git",
            "[submodule \"x\"]\npath = x\npath = y\nurl = ../x.git",
            "[submodule \"x\"]\npath = x",
            "[submodule \"x\"]\npath = x\nurl = ../x.git\n[submodule \"y\"]\npath = x\nurl = ../y.git",
            "[submodule \"x\"]\npath = x\nurl = \"unterminated",
        ] {
            assert!(parse_modules(text).is_err(), "{text}");
        }
        let many = (0..129)
            .map(|i| format!("[submodule \"{i}\"]\npath = p{i}\nurl = ../x.git\n"))
            .collect::<String>();
        assert!(matches!(parse_modules(&many), Err(Error::Limited(_))));
    }

    #[test]
    fn gitlinks_without_root_metadata_fail_instead_of_silently_omitting_source() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repository::init_bare(directory.path().join("repo")).unwrap();
        let mut builder = repo.treebuilder(None).unwrap();
        builder
            .insert("missing", Oid::from_str(&"a".repeat(40)).unwrap(), 0o160000)
            .unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let signature = Signature::now("Fixture", "fixture@example.org").unwrap();
        let id = repo
            .commit(None, &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        let limits = Limits {
            source_jobs: 1,
            max_download_bytes: 1024,
            max_extracted_bytes: 4096,
            max_source_files: 100,
            source_timeout_seconds: 15,
        };
        let mut record = AcquisitionRecord::default();
        let mut context = Context {
            limits: &limits,
            directory: directory.path(),
            record: &mut record,
            proxy: None,
            expanded: 0,
            repositories: 0,
        };
        let result = materialize_repository(
            &repo,
            id,
            "https://example.org/root.git",
            directory.path(),
            Path::new(""),
            &mut context,
            &mut Vec::new(),
            0,
        );
        assert!(
            matches!(result, Err(Error::Failed(reason)) if reason.contains("no root .gitmodules mapping"))
        );
        assert_eq!(context.repositories, 0);
    }
}
