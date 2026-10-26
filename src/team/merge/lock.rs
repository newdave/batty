//! File-based merge lock and merge outcome types.
//!
//! `MergeLock` serializes concurrent merge attempts using an O_EXCL lock file
//! that records the holder's PID, so a lock left by a killed process can be
//! detected and reclaimed.
//! `MergeOutcome` encodes the three possible results of a merge attempt.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use tracing::warn;

/// A lock file with no readable PID is only treated as stale once it is this
/// old, so a holder that has created the file but not yet written its PID is
/// not mistaken for a dead one.
const UNOWNED_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);

pub(crate) struct MergeLock {
    path: PathBuf,
}

pub(crate) fn merge_lock_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("merge.lock")
}

/// True if a merge lock exists and its holder still appears to be alive.
pub(crate) fn merge_lock_is_held(project_root: &Path) -> bool {
    let path = merge_lock_path(project_root);
    path.exists() && !merge_lock_is_stale(&path)
}

fn merge_lock_is_stale(path: &Path) -> bool {
    let holder = std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse::<u32>().ok())
        .filter(|pid| *pid > 0);
    match holder {
        Some(pid) => !crate::team::daemon_mgmt::process_exists(pid),
        None => std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > UNOWNED_LOCK_STALE_AFTER),
    }
}

impl MergeLock {
    pub fn acquire(project_root: &Path) -> Result<Self> {
        let path = merge_lock_path(project_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let start = std::time::Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let lock = Self { path };
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(lock);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if merge_lock_is_stale(&path) {
                        warn!(path = %path.display(), "removing stale merge lock left by a dead process");
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if start.elapsed() > Duration::from_secs(60) {
                        bail!("merge lock timeout after 60s: {}", path.display());
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(error) => bail!("failed to acquire merge lock: {error}"),
            }
        }
    }
}

impl Drop for MergeLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    #[test]
    fn merge_lock_records_pid_and_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = MergeLock::acquire(tmp.path()).unwrap();
        let content = std::fs::read_to_string(merge_lock_path(tmp.path())).unwrap();
        assert_eq!(content.trim(), std::process::id().to_string());
        assert!(merge_lock_is_held(tmp.path()));
        drop(lock);
        assert!(!merge_lock_path(tmp.path()).exists());
        assert!(!merge_lock_is_held(tmp.path()));
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

        let start = std::time::Instant::now();
        let _lock = MergeLock::acquire(tmp.path()).unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.trim(), std::process::id().to_string());
    }

    #[test]
    fn fresh_lock_without_pid_is_treated_as_held() {
        let tmp = tempfile::tempdir().unwrap();
        let path = merge_lock_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        assert!(merge_lock_is_held(tmp.path()));
    }

    #[test]
    fn lock_held_by_live_process_is_not_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let path = merge_lock_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        assert!(merge_lock_is_held(tmp.path()));
    }
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicBool, Ordering},
    };
    use std::thread;
    use std::time::Duration;

    #[test]
    fn merge_lock_acquire_release() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();
        let lock_path = tmp.path().join(".batty").join("merge.lock");

        {
            let lock = MergeLock::acquire(tmp.path()).unwrap();
            assert!(lock_path.exists());
            drop(lock);
        }
        assert!(!lock_path.exists());
    }

    #[test]
    fn merge_lock_second_acquire_waits_for_release() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty")).unwrap();

        let first_lock = MergeLock::acquire(tmp.path()).unwrap();
        let project_root = tmp.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));
        let acquired = Arc::new(AtomicBool::new(false));

        let thread_barrier = Arc::clone(&barrier);
        let thread_acquired = Arc::clone(&acquired);
        let handle = thread::spawn(move || {
            thread_barrier.wait();
            let second_lock = MergeLock::acquire(&project_root).unwrap();
            thread_acquired.store(true, Ordering::SeqCst);
            drop(second_lock);
        });

        barrier.wait();
        thread::sleep(Duration::from_millis(600));
        assert!(!acquired.load(Ordering::SeqCst));

        drop(first_lock);
        handle.join().unwrap();
        assert!(acquired.load(Ordering::SeqCst));
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
