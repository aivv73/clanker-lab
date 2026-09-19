//! A kernel-held per-user lock prevents independent controllers bypassing admission.
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
};
pub struct TurnLease(File);
impl TurnLease {
    pub fn acquire() -> Result<Self, String> {
        // This fixed namespace intentionally does not depend on adapter, workspace,
        // environment, or binary path. The lock file must never be unlinked.
        let uid = unsafe { libc::geteuid() };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(format!("/tmp/psi-active-turn-{uid}.lock"))
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
