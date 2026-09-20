//! A kernel-held per-user lock prevents independent controllers bypassing admission.
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
};
pub struct TurnLease(File);
impl TurnLease {
    pub fn acquire() -> Result<Self, String> {
        let primary = Self::named("active-turn")?;
        let _external = Self::external()?;
        Ok(primary)
    }
    /// The systemd supervisor keeps this independent lease across controller death.
    pub fn external() -> Result<Self, String> {
        use std::io::Read;
        let lease = Self::named("external-command")?;
        let mut path = String::new();
        (&lease.0)
            .take(4096)
            .read_to_string(&mut path)
            .map_err(|_| "cleanup_registry_unreadable")?;
        if !path.is_empty() {
            let path = std::path::Path::new(&path);
            if !path.starts_with("/sys/fs/cgroup") {
                return Err("cleanup_registry_invalid".into());
            }
            match std::fs::read_to_string(path.join("cgroup.events")) {
                Ok(events) if events.lines().any(|l| l == "populated 0") => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && !path.exists() => {}
                _ => return Err("external_cleanup_unconfirmed".into()),
            }
            lease
                .0
                .set_len(0)
                .map_err(|_| "cleanup_registry_unwritable")?;
        }
        Ok(lease)
    }
    pub fn record_cgroup(&self, path: &std::path::Path) -> Result<(), String> {
        use std::os::unix::fs::FileExt;
        self.0
            .set_len(0)
            .map_err(|_| "cleanup_registry_unwritable")?;
        self.0
            .write_all_at(path.as_os_str().as_encoded_bytes(), 0)
            .map_err(|_| "cleanup_registry_unwritable")?;
        self.0
            .sync_all()
            .map_err(|_| "cleanup_registry_unwritable".into())
    }
    pub fn confirm_cleanup(&self) -> Result<(), String> {
        self.0
            .set_len(0)
            .and_then(|()| self.0.sync_all())
            .map_err(|_| "cleanup_registry_unwritable".into())
    }
    fn named(name: &str) -> Result<Self, String> {
        // This fixed namespace intentionally does not depend on adapter, workspace,
        // environment, or binary path. The lock file must never be unlinked.
        let uid = unsafe { libc::geteuid() };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(format!("/tmp/psi-{name}-{uid}.lock"))
            .map_err(|_| "admission_unavailable")?;
        let metadata = file.metadata().map_err(|_| "admission_unavailable")?;
        if !metadata.is_file()
            || metadata.uid() != uid
            || metadata.nlink() != 1
            || metadata.mode() & 0o077 != 0
        {
            return Err("admission_unavailable".into());
        }
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => "busy".to_string(),
            _ => "admission_unavailable".to_string(),
        })?;
        Ok(Self(file))
    }
}
impl Drop for TurnLease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
