use super::{AcquisitionRecord, Error, Limits, account, safe_path};
use crate::package::provider::AnalysisTarget;
use git2::{
    AutotagOption, FetchOptions, IndexEntry, IndexTime, ObjectType, Oid, RemoteCallbacks,
    RemoteRedirect, Repository, RepositoryInitOptions, Signature,
};
use std::cell::Cell;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub(super) fn fetch(
    target: &AnalysisTarget,
    limits: &Limits,
    directory: &Path,
    tree: &Path,
    record: &mut AcquisitionRecord,
) -> Result<(), Error> {
    git2::opts::strict_hash_verification(true);
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
        directory.join("upstream"),
        RepositoryInitOptions::new()
            .bare(true)
            .external_template(false),
    )?;
    let mut config = repo.config()?;
    config.set_str("http.proxy", "")?;
    config.set_bool("http.sslVerify", true)?;
    let limited = Cell::new(false);
    let downloaded = Cell::new(0u64);
    let mut callbacks = RemoteCallbacks::new();
    callbacks.transfer_progress(|stats| {
        downloaded.set(stats.received_bytes() as u64);
        if downloaded.get() > limits.max_download_bytes {
            limited.set(true);
            return false;
        }
        true
    });
    let mut options = FetchOptions::new();
    options
        .remote_callbacks(callbacks)
        .depth(1)
        .download_tags(AutotagOption::None)
        .follow_redirects(RemoteRedirect::None);
    let mut remote = repo.remote_anonymous(&target.location)?;
    let result = remote.fetch(
        &[format!("+{reference}:refs/paclang/source")],
        Some(&mut options),
        None,
    );
    record.downloaded_bytes = downloaded.get();
    if limited.get() {
        return Err(Error::Limited("source download byte limit exceeded"));
    }
    result?;
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
    let mut expanded = 0u64;
    let mut object_error = None;
    let verified = odb.foreach(|oid| {
        let checked = (|| -> Result<(), Error> {
            let (size, _) = odb.read_header(*oid)?;
            expanded = expanded
                .checked_add(size as u64)
                .ok_or(Error::Limited("Git object expansion limit exceeded"))?;
            if expanded > limits.max_extracted_bytes {
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
    record.verified_revision = Some(commit.id().to_string());
    write_tree(
        &repo,
        &commit.tree()?,
        tree,
        Path::new(""),
        limits,
        record,
        0,
    )?;
    initialize(tree)
}

fn write_tree(
    repo: &Repository,
    source: &git2::Tree<'_>,
    destination: &Path,
    prefix: &Path,
    limits: &Limits,
    record: &mut AcquisitionRecord,
    depth: usize,
) -> Result<(), Error> {
    if depth > 128 {
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
                account(record, limits, 0)?;
                fs::create_dir(&output)?;
                write_tree(
                    repo,
                    &repo.find_tree(entry.id())?,
                    destination,
                    &path,
                    limits,
                    record,
                    depth + 1,
                )?;
            }
            Some(ObjectType::Blob) => {
                let (size, _) = repo.odb()?.read_header(entry.id())?;
                account(record, limits, size as u64)?;
                let blob = repo.find_blob(entry.id())?;
                if entry.filemode() == 0o120000 {
                    let value = std::str::from_utf8(blob.content())
                        .map_err(|_| Error::Failed("Git symlink is not UTF-8"))?;
                    safe_path(value)?;
                    std::os::unix::fs::symlink(value, output)?;
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
            _ => {
                return Err(Error::Failed(
                    "Git submodules are unsupported; source tree is incomplete",
                ));
            }
        }
    }
    Ok(())
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
