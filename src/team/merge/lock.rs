//! File-based merge lock and merge outcome types.
//!
//! `MergeLock` serializes merge attempts with an advisory `flock` on
//! `.batty/merge.lock`. The kernel releases the lock when the holder exits,
//! so a lock left by a killed process never needs reclaiming, and two
//! waiters can never both "reclaim" a stale lock and end up as concurrent
//! holders. The file is never deleted while in use (deleting a locked file
//! lets a later opener lock a fresh inode alongside the old holder); it is
//! only truncated on release. The holder's PID is written into the file for
//! diagnostics and for `merge_lock_is_held` callers in other processes.
//! `MergeOutcome` encodes the three possible results of a merge attempt.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A non-empty lock file without a readable PID (written by an older batty
/// or another tool) is treated as held until it is this old.
const UNOWNED_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

pub(crate) struct MergeLock {
    // Keeping the file open keeps the flock; dropping it releases the lock.
    file: File,
}

pub(crate) fn merge_lock_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("merge.lock")
}

/// True if a merge lock is currently held: either some open file holds the
/// flock, or the file carries a holder marker that still looks live (a PID
/// of a running process, or an unparseable marker younger than 30s).
pub(crate) fn merge_lock_is_held(project_root: &Path) -> bool {
    let path = merge_lock_path(project_root);
    let Ok(file) = OpenOptions::new().read(true).open(&path) else {
        return false;
    };
    match try_flock(&file, false) {
        Ok(true) => unlock(&file),
        Ok(false) => return true,
        Err(_) => {}
    }
    legacy_marker_is_live(&path)
}

fn legacy_marker_is_live(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let content = content.trim();
    if content.is_empty() {
        return false;
    }
    match content.parse::<u32>().ok().filter(|pid| *pid > 0) {
        Some(pid) => crate::team::daemon_mgmt::process_exists(pid),
        None => std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_none_or(|age| age <= UNOWNED_LOCK_STALE_AFTER),
    }
}

/// Try to take the advisory lock without blocking. `Ok(false)` means another
/// open file description holds it.
#[cfg(unix)]
fn try_flock(file: &File, exclusive: bool) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let mode = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    loop {
        // SAFETY: flock on a valid file descriptor owned by `file`.
        let rc = unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if code == libc::EWOULDBLOCK => return Ok(false),
            Some(libc::EINTR) => continue,
            _ => return Err(error),
        }
    }
}

#[cfg(unix)]
fn unlock(file: &File) {
    use std::os::unix::io::AsRawFd;
    // SAFETY: flock on a valid file descriptor owned by `file`.
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

#[cfg(not(unix))]
fn try_flock(_file: &File, _exclusive: bool) -> std::io::Result<bool> {
    Ok(true)
}

#[cfg(not(unix))]
fn unlock(_file: &File) {}

impl MergeLock {
    /// Take the merge lock if it is free. Returns `Ok(None)` when another
    /// holder has it; poll-loop callers retry on a later tick instead of
    /// blocking the daemon.
    pub(crate) fn try_acquire(project_root: &Path) -> Result<Option<Self>> {
        let path = merge_lock_path(project_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("failed to open merge lock {}", path.display()))?;
        if !try_flock(&file, true)
            .with_context(|| format!("failed to lock merge lock {}", path.display()))?
        {
            return Ok(None);
        }
        // We hold the lock: replace any marker left by a dead holder.
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        file.flush()?;
        Ok(Some(Self { file }))
    }
}

impl Drop for MergeLock {
    fn drop(&mut self) {
        // Clear the PID marker while still holding the lock; closing the
        // file releases it. Never unlink: see the module docs.
        let _ = self.file.set_len(0);
        unlock(&self.file);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MergeMode {
    DirectRoot,
    IsolatedIntegration,
}

impl MergeMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::DirectRoot => "direct_root",
            Self::IsolatedIntegration => "isolated_integration",
        }
    }
}

pub(crate) fn infer_merge_mode_from_failure(message: &str) -> Option<MergeMode> {
    if message.starts_with("direct merge path failed") {
        Some(MergeMode::DirectRoot)
    } else if message.starts_with("isolated merge path failed") {
        Some(MergeMode::IsolatedIntegration)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MergeSuccess {
    pub(crate) mode: MergeMode,
    pub(crate) reason: Option<String>,
}

#[derive(Debug)]
pub(crate) enum MergeOutcome {
    Success(MergeSuccess),
    RebaseConflict(String),
    MergeFailure(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    use std::thread;

    #[test]
    fn merge_lock_records_pid_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = MergeLock::try_acquire(tmp.path()).unwrap().unwrap();
        let content = std::fs::read_to_string(merge_lock_path(tmp.path())).unwrap();
        assert_eq!(content.trim(), std::process::id().to_string());
        assert!(merge_lock_is_held(tmp.path()));
        drop(lock);
        assert!(!merge_lock_is_held(tmp.path()));
        // The file is kept (never unlinked) but its marker is cleared.
        let content = std::fs::read_to_string(merge_lock_path(tmp.path())).unwrap();
        assert!(content.trim().is_empty());
    }

    #[test]
    fn merge_lock_reclaims_lock_left_by_dead_process() {
        let tmp = tempfile::tempdir().unwrap();
        let path = merge_lock_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A finished child's PID is no longer alive.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        std::fs::write(&path, format!("{dead_pid}\n")).unwrap();
        assert!(!merge_lock_is_held(tmp.path()));

        let lock = MergeLock::try_acquire(tmp.path()).unwrap();
        assert!(lock.is_some(), "dead holder's marker must not block");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.trim(), std::process::id().to_string());
    }

    #[test]
    fn fresh_unowned_marker_is_treated_as_held() {
        let tmp = tempfile::tempdir().unwrap();
        let path = merge_lock_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "active merge\n").unwrap();
        assert!(merge_lock_is_held(tmp.path()));
    }

    #[test]
    fn empty_unlocked_lock_file_is_not_held() {
        let tmp = tempfile::tempdir().unwrap();
        let path = merge_lock_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        assert!(!merge_lock_is_held(tmp.path()));
    }

    #[test]
    fn try_acquire_returns_none_while_held_and_some_after_release() {
        let tmp = tempfile::tempdir().unwrap();
        let first = MergeLock::try_acquire(tmp.path()).unwrap().unwrap();
        assert!(MergeLock::try_acquire(tmp.path()).unwrap().is_none());
        drop(first);
        assert!(MergeLock::try_acquire(tmp.path()).unwrap().is_some());
    }

    #[test]
    fn holder_drop_does_not_release_another_holders_lock() {
        // Old design: a waiter that reclaimed a "stale" lock could delete a
        // fresh holder's file, and Drop removed the file unconditionally.
        let tmp = tempfile::tempdir().unwrap();
        let held = MergeLock::try_acquire(tmp.path()).unwrap().unwrap();
        // A stale-looking marker from a dead process must not let anyone in.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        std::fs::write(merge_lock_path(tmp.path()), format!("{dead_pid}\n")).unwrap();
        assert!(MergeLock::try_acquire(tmp.path()).unwrap().is_none());
        assert!(merge_lock_is_held(tmp.path()));
        drop(held);
        assert!(MergeLock::try_acquire(tmp.path()).unwrap().is_some());
    }

    #[test]
    fn concurrent_try_acquire_has_exactly_one_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let path = merge_lock_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Start from a stale marker so every contender sees a "dead" holder.
        std::fs::write(&path, "999999999\n").unwrap();
        let threads = 8;
        let barrier = Arc::new(Barrier::new(threads));
        let holders = Arc::new(AtomicUsize::new(0));
        let max_holders = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let root = tmp.path().to_path_buf();
                let barrier = Arc::clone(&barrier);
                let holders = Arc::clone(&holders);
                let max_holders = Arc::clone(&max_holders);
                thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..20 {
                        if let Some(lock) = MergeLock::try_acquire(&root).unwrap() {
                            let now = holders.fetch_add(1, Ordering::SeqCst) + 1;
                            max_holders.fetch_max(now, Ordering::SeqCst);
                            thread::sleep(Duration::from_millis(1));
                            holders.fetch_sub(1, Ordering::SeqCst);
                            drop(lock);
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(max_holders.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn merge_mode_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_string(&MergeMode::IsolatedIntegration).unwrap(),
            "\"isolated_integration\""
        );
    }

    #[test]
    fn infer_merge_mode_from_failure_prefix() {
        assert_eq!(
            infer_merge_mode_from_failure("direct merge path failed: boom"),
            Some(MergeMode::DirectRoot)
        );
        assert_eq!(
            infer_merge_mode_from_failure("isolated merge path failed: boom"),
            Some(MergeMode::IsolatedIntegration)
        );
        assert_eq!(
            infer_merge_mode_from_failure("verification detected regressions"),
            None
        );
    }
}
