//! Small libarchive boundary: decode only; never ask libarchive to write paths,
//! restore ownership/permissions, follow links, or execute external filters.
use super::{AcquisitionRecord, Error, Limits, account, ensure_directories, safe_path, materialize_links};
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[link(name = "archive")]
unsafe extern "C" {
    fn archive_error_string(a: *mut c_void) -> *const c_char;
    fn archive_read_new() -> *mut c_void;
    fn archive_read_free(a: *mut c_void) -> c_int;
    fn archive_read_support_filter_gzip(a: *mut c_void) -> c_int;
    fn archive_read_support_filter_bzip2(a: *mut c_void) -> c_int;
    fn archive_read_support_filter_xz(a: *mut c_void) -> c_int;
    fn archive_read_support_filter_zstd(a: *mut c_void) -> c_int;
    fn archive_read_support_filter_lzip(a: *mut c_void) -> c_int;
    fn archive_read_support_format_tar(a: *mut c_void) -> c_int;
    fn archive_read_support_format_zip(a: *mut c_void) -> c_int;
    fn archive_read_open_filename(a: *mut c_void, name: *const c_char, block: usize) -> c_int;
    fn archive_read_next_header(a: *mut c_void, entry: *mut *mut c_void) -> c_int;
    fn archive_read_data(a: *mut c_void, data: *mut c_void, size: usize) -> isize;
    fn archive_entry_pathname(entry: *mut c_void) -> *const c_char;
    fn archive_entry_filetype(entry: *mut c_void) -> c_uint;
    fn archive_entry_mode(entry: *mut c_void) -> c_uint;
    fn archive_entry_size(entry: *mut c_void) -> i64;
    fn archive_entry_symlink(entry: *mut c_void) -> *const c_char;
    fn archive_entry_hardlink(entry: *mut c_void) -> *const c_char;
}
struct Reader(*mut c_void);
impl Drop for Reader {
    fn drop(&mut self) {
        unsafe {
            archive_read_free(self.0);
        }
    }
}

pub(super) fn extract(
    file: &Path,
    tree: &Path,
    limits: &Limits,
    record: &mut AcquisitionRecord,
) -> Result<(), Error> {
    // All pointers originate in a live reader. Entry strings are copied before
    // next_header invalidates them; data writes target a fixed stack buffer.
    unsafe {
        let reader = Reader(archive_read_new());
        if reader.0.is_null() {
            return Err(Error::Failed("cannot create archive reader"));
        }
        for register in [
            archive_read_support_filter_gzip,
            archive_read_support_filter_bzip2,
            archive_read_support_filter_xz,
            archive_read_support_filter_zstd,
            archive_read_support_filter_lzip,
            archive_read_support_format_tar,
            archive_read_support_format_zip,
        ] {
            // WARN can mean falling back to an external decompressor. Refuse it.
            if register(reader.0) != 0 {
                return Err(Error::Failed(
                    "required built-in archive decoder unavailable",
                ));
            }
        }
        let filename = CString::new(file.as_os_str().as_encoded_bytes())
            .map_err(|_| Error::Failed("invalid download path"))?;
        if archive_read_open_filename(reader.0, filename.as_ptr(), 65_536) != 0 {
            return Err(Error::Failed("invalid source archive"));
        }
        let mut buffer = [0u8; 65_536];
        let mut links = Vec::new();
        let mut hardlinks = Vec::new();
        loop {
            let mut entry = std::ptr::null_mut();
            match archive_read_next_header(reader.0, &mut entry) {
                1 => break,
                0 if !entry.is_null() => {}
                _ => {
                    let message = archive_error_string(reader.0);
                    let detail = if message.is_null() { "unknown decoder error".into() } else { CStr::from_ptr(message).to_string_lossy() };
                    return Err(Error::Detail(format!("malformed or truncated source archive: {detail}")));
                },
            }
            let raw = archive_entry_pathname(entry);
            if raw.is_null() {
                return Err(Error::Failed("archive entry has no path"));
            }
            let name = CStr::from_ptr(raw)
                .to_str()
                .map_err(|_| Error::Failed("archive path is not UTF-8"))?;
            if matches!(name, "." | "./") && archive_entry_filetype(entry) == 0o040000 {
                continue;
            }
            let path = safe_path(name).map_err(|e| Error::Detail(format!("archive entry {name:?}: {e:?}")))?;
            let destination = tree.join(&path);
            if !archive_entry_hardlink(entry).is_null() {
                let target = CStr::from_ptr(archive_entry_hardlink(entry)).to_str()
                    .map_err(|_| Error::Failed("hard link target is not UTF-8"))?;
                let target = safe_path(target).map_err(|e| Error::Detail(format!("archive hard link {name:?} -> {target:?}: {e:?}")))?;
                account(record, limits, 0)?;
                if let Some(parent) = path.parent() { ensure_directories(tree, parent, limits, record)?; }
                hardlinks.push((path, target));
                continue;
            }
            match archive_entry_filetype(entry) {
                0o040000 => {
                    ensure_directories(tree, &path, limits, record)?;
                }
                0o120000 => {
                    let value = archive_entry_symlink(entry);
                    if value.is_null() {
                        return Err(Error::Failed("invalid archive symlink"));
                    }
                    let value = CStr::from_ptr(value)
                        .to_str()
                        .map_err(|_| Error::Failed("symlink is not UTF-8"))?
                        .to_owned();
                    // Conservative relative links only; defer creation so no
                    // archive entry can traverse a previously written symlink.

                    account(record, limits, value.len() as u64)?;
                    if let Some(parent) = path.parent() {
                        ensure_directories(tree, parent, limits, record)?;
                    }
                    links.push((path, value));
                }
                0o100000 => {
                    let size = u64::try_from(archive_entry_size(entry))
                        .map_err(|_| Error::Failed("invalid archive entry size"))?;
                    account(record, limits, size)?;
                    if let Some(parent) = path.parent() {
                        ensure_directories(tree, parent, limits, record)?;
                    }
                    let mut output = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&destination)?;
                    let mut remaining = size;
                    loop {
                        let read =
                            archive_read_data(reader.0, buffer.as_mut_ptr().cast(), buffer.len());
                        if read < 0 {
                            return Err(Error::Failed("corrupt archive entry"));
                        }
                        if read == 0 {
                            break;
                        }
                        let read = read as usize;
                        if read as u64 > remaining {
                            return Err(Error::Failed("archive entry exceeds declared size"));
                        }
                        output.write_all(&buffer[..read])?;
                        remaining -= read as u64;
                    }
                    if remaining != 0 {
                        return Err(Error::Failed("truncated archive entry"));
                    }
                    fs::set_permissions(
                        destination,
                        fs::Permissions::from_mode(if archive_entry_mode(entry) & 0o111 != 0 {
                            0o755
                        } else {
                            0o644
                        }),
                    )?;
                }
                _ => {
                    return Err(Error::Failed(
                        "archive contains a device or unsupported entry",
                    ));
                }
            }
        }
        // Links are archive-root-relative; support forward references, but
        // never link through a symlink or to a directory/outside file.
        let targets: std::collections::HashMap<_, _> = hardlinks.iter().cloned().collect();
        for (path, target) in &hardlinks {
            let mut target = target;
            for _ in 0..40 {
                match targets.get(target) { Some(next) => target = next, None => break }
            }
            if targets.contains_key(target) { return Err(Error::Failed("cyclic or excessive hard link chain")); }
            let mut full = tree.to_path_buf();
            for component in target.components() {
                full.push(component);
                if fs::symlink_metadata(&full)?.file_type().is_symlink() { return Err(Error::Failed("hard link target traverses symlink")); }
            }
            let meta = fs::symlink_metadata(&full)?;
            if !meta.is_file() { return Err(Error::Failed("hard link target is not a regular file")); }
            if meta.len() > limits.max_extracted_bytes.saturating_sub(record.extracted_bytes) {
                return Err(Error::Limited("expanded source byte limit exceeded"));
            }
            record.extracted_bytes += meta.len();
            fs::hard_link(full, tree.join(path))?;
        }
        // Validate against the final root: the caller strips a sole wrapper
        // directory. Otherwise a link to its parent would escape after rename.
        let entries: Vec<_> = fs::read_dir(tree)?.collect::<Result<_, _>>()?;
        if entries.len() == 1 && entries[0].file_type()?.is_dir()
            && links.iter().all(|(path, _)| path.starts_with(entries[0].file_name()))
        {
            let wrapper = entries[0].file_name();
            let relative: Vec<_> = links.iter().map(|(path, value)|
                (path.strip_prefix(&wrapper).unwrap().to_path_buf(), value.clone())).collect();
            materialize_links(&tree.join(wrapper), &relative)?;
        } else {
            materialize_links(tree, &links)?;
        }
    }
    Ok(())
}
