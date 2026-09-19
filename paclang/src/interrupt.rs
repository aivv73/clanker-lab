use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::Pid;

static REQUESTED: AtomicBool = AtomicBool::new(false);
static ACTIVE_PROCESS_GROUP: AtomicI32 = AtomicI32::new(0);

pub(crate) fn install() -> io::Result<()> {
    let (mut notifications, signal_writer) = UnixStream::pair()?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGINT, signal_writer)?;
    std::thread::Builder::new()
        .name("paclang-signal-handler".to_owned())
        .spawn(move || {
            let mut notification = [0_u8; 1];
            while notifications.read_exact(&mut notification).is_ok() {
                REQUESTED.store(true, Ordering::Relaxed);
                terminate_active_process_group();
            }
        })?;
    Ok(())
}

pub(crate) fn is_requested() -> bool {
    REQUESTED.load(Ordering::Relaxed)
}

pub(crate) struct ActiveProcessGroup(i32);

impl ActiveProcessGroup {
    pub(crate) fn register(id: u32) -> Self {
        let id = i32::try_from(id).unwrap_or(0);
        debug_assert!(id > 0, "child process ID fits in i32");
        ACTIVE_PROCESS_GROUP.store(id, Ordering::Relaxed);
        let group = Self(id);
        if is_requested() {
            group.terminate();
        }
        group
    }

    pub(crate) fn terminate(&self) {
        terminate_process_tree(self.0);
    }
}

impl Drop for ActiveProcessGroup {
    fn drop(&mut self) {
        let _ =
            ACTIVE_PROCESS_GROUP.compare_exchange(self.0, 0, Ordering::Relaxed, Ordering::Relaxed);
    }
}

fn terminate_active_process_group() {
    let id = ACTIVE_PROCESS_GROUP.load(Ordering::Relaxed);
    if id > 0 {
        terminate_process_tree(id);
    }
}

/// Terminates the whole process tree, not just the direct child: refresh can
/// spawn helpers or containers (Linguist, OCI runtimes), and a child-only
/// kill would leave grandchildren running after timeout or cancel.
fn terminate_process_tree(root: i32) {
    for descendant in descendants(root).into_iter().rev() {
        let _ = kill(Pid::from_raw(descendant), Signal::SIGKILL);
    }
    let _ = killpg(Pid::from_raw(root), Signal::SIGKILL);
    let _ = kill(Pid::from_raw(root), Signal::SIGKILL);
}

fn descendants(parent: i32) -> Vec<i32> {
    let path = format!("/proc/{parent}/task/{parent}/children");
    let Ok(children) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for child in children
        .split_whitespace()
        .filter_map(|value| value.parse::<i32>().ok())
    {
        result.push(child);
        result.extend(descendants(child));
    }
    result
}
