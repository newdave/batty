//! File-based merge lock and merge outcome types.
//!
//! `MergeLock` serializes merge attempts on `.batty/merge.lock`.
//!
//! - **flock mode (normal):** the holder keeps an advisory `flock` on the
//!   file and writes its PID into it. The kernel releases the flock when the
//!   holder exits, so a lock left by a killed process never blocks anyone.
//!   On release the holder truncates and unlinks the file *while still
//!   holding the flock*, so older batty binaries (which treat an existing
//!   file as held) see a free lock again. Because the file can be unlinked,
//!   a waiter that locked an inode which is no longer at the lock path
//!   retries instead of holding a lock nobody else can see.
//! - **Compatibility with older binaries:** those use exclusive create plus
//!   a PID marker and never `flock`. A file that is not flocked but carries
//!   a live foreign PID (or is empty or unparseable and young, i.e. being
//!   written) is therefore treated as held.
//! - **Fallback mode:** on filesystems without `flock` support (`ENOLCK`,
//!   `ENOTSUP`), the lock falls back to that same exclusive-create scheme
//!   with dead-PID reclaim.
//!
//! `MergeOutcome` encodes the three possible results of a merge attempt.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// A non-empty lock file without a readable PID (written by an older batty
/// or another tool) is treated as held until it is this old.
const UNOWNED_LOCK_STALE_AFTER: Duration = Duration::from_secs(30);
/// An empty, unlocked lock file younger than this may belong to an older
/// binary that created it and has not written its PID yet.
const EMPTY_LOCK_GRACE: Duration = Duration::from_secs(5);
/// Attempts to lock the file currently at the lock path before giving up
/// for this call (another holder unlinked it between our open and flock).
const ACQUIRE_ATTEMPTS: usize = 4;

/// Lock paths held in fallback (exclusive-create) mode by this process. The
/// PID marker cannot tell two holders in one process apart.
static FALLBACK_HELD: LazyLock<Mutex<BTreeSet<PathBuf>>> =
    LazyLock::new(|| Mutex::new(BTreeSet::new()));

pub(crate) struct MergeLock {
    path: PathBuf,
    mode: LockMode,
}

enum LockMode {
    /// Keeping the file open keeps the flock; dropping it releases the lock.
    Flock(File),
    /// The file's existence (with our PID inside) is the lock.
    ExclusiveCreate,
}

impl std::fmt::Debug for MergeLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mode = match self.mode {
            LockMode::Flock(_) => "flock",
            LockMode::ExclusiveCreate => "exclusive-create",
        };
        write!(f, "MergeLock({}, {mode})", self.path.display())
    }
}

pub(crate) fn merge_lock_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("merge.lock")
}

/// True if a merge lock is currently held: either some open file holds the
/// flock, or the file carries a holder marker that still looks live (a live
/// PID, or an empty or unparseable marker that is still young).
pub(crate) fn merge_lock_is_held(project_root: &Path) -> bool {
    let path = merge_lock_path(project_root);
    if fallback_held_in_process(&path) {
        return true;
    }
    let Ok(file) = OpenOptions::new().read(true).open(&path) else {
        return false;
    };
    match try_flock(&file, false) {
        Ok(true) => unlock(&file),
        Ok(false) => return true,
        Err(_) => {}
    }
    marker_is_live(&path)
}

fn fallback_held_in_process(path: &Path) -> bool {
    FALLBACK_HELD
        .lock()
        .map(|held| held.contains(path))
        .unwrap_or(true)
}

/// Whether the marker in the lock file belongs to a live holder that does
/// not use `flock` (an older batty, or a holder in fallback mode). Our own
/// PID never counts: a marker with it was left behind by this process
/// before an `exec` (hot reload) or by a holder that is gone.
fn marker_is_live(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    marker_content_is_live(path, &content)
}

fn marker_content_is_live(path: &Path, content: &str) -> bool {
    let content = content.trim();
    let age = || {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
    };
    if content.is_empty() {
        return age().is_none_or(|age| age <= EMPTY_LOCK_GRACE);
    }
    match content.parse::<u32>().ok().filter(|pid| *pid > 0) {
        Some(pid) if pid == std::process::id() => false,
        Some(pid) => crate::team::daemon_mgmt::process_exists(pid),
        None => age().is_none_or(|age| age <= UNOWNED_LOCK_STALE_AFTER),
    }
}

#[cfg(test)]
thread_local! {
    /// Test hook: make `try_flock` on this thread fail with this errno.
    static FORCED_FLOCK_ERROR: std::cell::Cell<Option<i32>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(crate) fn force_flock_error_for_test(errno: Option<i32>) {
    FORCED_FLOCK_ERROR.with(|forced| forced.set(errno));
}

/// Try to take the advisory lock without blocking. `Ok(false)` means another
/// open file description holds it.
#[cfg(unix)]
fn try_flock(file: &File, exclusive: bool) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    #[cfg(test)]
    if let Some(errno) = FORCED_FLOCK_ERROR.with(std::cell::Cell::get) {
        return Err(std::io::Error::from_raw_os_error(errno));
    }
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

#[cfg(unix)]
fn flock_unsupported(error: &std::io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        code == libc::ENOLCK || code == libc::ENOTSUP || code == libc::EOPNOTSUPP
    })
}

/// True when `file` is still the inode at `path`.
#[cfg(unix)]
fn is_file_at_path(file: &File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (file.metadata(), std::fs::metadata(path)) {
        (Ok(open), Ok(at_path)) => open.dev() == at_path.dev() && open.ino() == at_path.ino(),
        _ => false,
    }
}

#[cfg(not(unix))]
fn try_flock(_file: &File, _exclusive: bool) -> std::io::Result<bool> {
    Ok(true)
}

#[cfg(not(unix))]
fn unlock(_file: &File) {}

#[cfg(not(unix))]
fn flock_unsupported(_error: &std::io::Error) -> bool {
    false
}

#[cfg(not(unix))]
fn is_file_at_path(_file: &File, path: &Path) -> bool {
    path.exists()
}

enum FlockAttempt {
    Acquired(MergeLock),
    Busy,
    /// The file was unlinked or replaced by its holder; try again.
    Replaced,
    Unsupported,
}

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
        if fallback_held_in_process(&path) {
            return Ok(None);
        }
        for _ in 0..ACQUIRE_ATTEMPTS {
            match try_acquire_flock(&path)? {
                FlockAttempt::Acquired(lock) => return Ok(Some(lock)),
                FlockAttempt::Busy => return Ok(None),
                FlockAttempt::Replaced => continue,
                FlockAttempt::Unsupported => {
                    warn!(
                        path = %path.display(),
                        "flock is not supported for the merge lock; using exclusive-create locking"
                    );
                    return try_acquire_exclusive_create(&path);
                }
            }
        }
        Ok(None)
    }
}

fn try_acquire_flock(path: &Path) -> Result<FlockAttempt> {
    let open_error = |error: std::io::Error| {
        anyhow::Error::new(error).context(format!("failed to open merge lock {}", path.display()))
    };
    let (mut file, created) = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Never create here: a file that vanished was released by its
            // holder, and creating it would look like an old binary's
            // half-written marker to everyone else.
            match OpenOptions::new().read(true).write(true).open(path) {
                Ok(file) => (file, false),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(FlockAttempt::Replaced);
                }
                Err(error) => return Err(open_error(error)),
            }
        }
        Err(error) => return Err(open_error(error)),
    };
    match try_flock(&file, true) {
        Ok(true) => {}
        Ok(false) => return Ok(FlockAttempt::Busy),
        Err(error) if flock_unsupported(&error) => {
            if created {
                let _ = std::fs::remove_file(path);
            }
            return Ok(FlockAttempt::Unsupported);
        }
        Err(error) => {
            return Err(anyhow::Error::new(error)
                .context(format!("failed to lock merge lock {}", path.display())));
        }
    }
    if !is_file_at_path(&file, path) {
        return Ok(FlockAttempt::Replaced);
    }
    if !created {
        let mut content = String::new();
        file.read_to_string(&mut content)
            .with_context(|| format!("failed to read merge lock {}", path.display()))?;
        if marker_content_is_live(path, &content) {
            // An older binary (no flock) holds it.
            return Ok(FlockAttempt::Busy);
        }
    }
    // We hold the lock: replace any marker left by a dead holder.
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(FlockAttempt::Acquired(MergeLock {
        path: path.to_path_buf(),
        mode: LockMode::Flock(file),
    }))
}

/// Fallback for filesystems without `flock`: the lock is the file itself,
/// created exclusively with our PID. A marker whose PID is dead (or an old
/// unparseable one) is reclaimed. Two waiters reclaiming the same stale
/// marker at once can race; `flock` mode has no such window.
fn try_acquire_exclusive_create(path: &Path) -> Result<Option<MergeLock>> {
    let mut held = FALLBACK_HELD
        .lock()
        .map_err(|_| anyhow::anyhow!("merge lock registry poisoned"))?;
    if held.contains(path) {
        return Ok(None);
    }
    for _ in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())
                    .and_then(|()| file.flush())
                    .with_context(|| format!("failed to write merge lock {}", path.display()))?;
                held.insert(path.to_path_buf());
                return Ok(Some(MergeLock {
                    path: path.to_path_buf(),
                    mode: LockMode::ExclusiveCreate,
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if marker_is_live(path) {
                    return Ok(None);
                }
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(anyhow::Error::new(error).context(format!(
                            "failed to reclaim stale merge lock {}",
                            path.display()
                        )));
                    }
                }
            }
            Err(error) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("failed to create merge lock {}", path.display())));
            }
        }
    }
    Ok(None)
}

/// Test helper: take the lock, retrying briefly. A child that another test
/// thread forks keeps a duplicate of any open lock descriptor (and so the
/// flock) until it execs, which can make a free lock look busy for a moment.
#[cfg(test)]
pub(crate) fn acquire_eventually_for_test(project_root: &Path) -> Option<MergeLock> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(lock) = MergeLock::try_acquire(project_root).ok().flatten() {
            return Some(lock);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Drop for MergeLock {
    fn drop(&mut self) {
        match &self.mode {
            LockMode::Flock(file) => {
                // Clear the marker and unlink while still holding the flock,
                // so no one can lock this inode and see it as theirs. A
                // waiter that opened it earlier notices it is gone and
                // retries on the path.
                let _ = file.set_len(0);
                if is_file_at_path(file, &self.path) {
                    let _ = std::fs::remove_file(&self.path);
                }
                unlock(file);
            }
            LockMode::ExclusiveCreate => {
                let ours = std::fs::read_to_string(&self.path)
                    .is_ok_and(|content| content.trim() == std::process::id().to_string());
                if ours {
                    let _ = std::fs::remove_file(&self.path);
                }
                if let Ok(mut held) = FALLBACK_HELD.lock() {
                    held.remove(&self.path);
                }
            }
        }
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
        // Released locks remove the file, so older binaries that treat an
        // existing file as held see a free lock.
        assert!(!merge_lock_path(tmp.path()).exists());
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

    fn age_file(path: &Path, age: Duration) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::now() - age)
            .unwrap();
    }

    #[test]
    fn old_empty_unlocked_lock_file_is_not_held() {
        let tmp = tempfile::tempdir().unwrap();
        let path = merge_lock_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        age_file(&path, Duration::from_secs(60));
        assert!(!merge_lock_is_held(tmp.path()));
        assert!(acquire_eventually_for_test(tmp.path()).is_some());
    }

    #[test]
    fn young_empty_unlocked_lock_file_is_held() {
        // An older binary creates the file first and writes its PID after.
        let tmp = tempfile::tempdir().unwrap();
        let path = merge_lock_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        assert!(merge_lock_is_held(tmp.path()));
        assert!(MergeLock::try_acquire(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn old_scheme_lock_with_live_pid_blocks_acquisition() {
        // Older binaries hold the lock by exclusive create + PID, no flock.
        let tmp = tempfile::tempdir().unwrap();
        let path = merge_lock_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        std::fs::write(&path, format!("{}\n", child.id())).unwrap();
        age_file(&path, Duration::from_secs(600));

        let blocked = MergeLock::try_acquire(tmp.path()).unwrap();
        let held = merge_lock_is_held(tmp.path());
        let content = std::fs::read_to_string(&path).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();

        assert!(blocked.is_none(), "a live old-scheme holder must block");
        assert!(held);
        assert_eq!(content.trim(), child.id().to_string(), "marker untouched");
        // Once that holder is gone its marker is reclaimed.
        assert!(acquire_eventually_for_test(tmp.path()).is_some());
    }

    #[test]
    fn marker_with_own_pid_left_before_exec_is_reclaimed() {
        // Hot reload execs in place: the PID survives but the flock does not.
        let tmp = tempfile::tempdir().unwrap();
        let path = merge_lock_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        assert!(!merge_lock_is_held(tmp.path()));
        assert!(acquire_eventually_for_test(tmp.path()).is_some());
    }

    #[test]
    fn falls_back_to_exclusive_create_when_flock_is_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        let path = merge_lock_path(tmp.path());
        force_flock_error_for_test(Some(libc::ENOLCK));
        let first = MergeLock::try_acquire(tmp.path()).unwrap();
        let second = MergeLock::try_acquire(tmp.path()).unwrap();
        let held = merge_lock_is_held(tmp.path());
        let marker = std::fs::read_to_string(&path).unwrap();
        force_flock_error_for_test(None);

        let first = first.expect("fallback lock must be acquired");
        assert!(format!("{first:?}").contains("exclusive-create"));
        assert!(
            second.is_none(),
            "fallback lock must exclude a second holder"
        );
        assert!(held);
        assert_eq!(marker.trim(), std::process::id().to_string());
        drop(first);
        assert!(!path.exists());
        assert!(!merge_lock_is_held(tmp.path()));
    }

    #[test]
    fn other_flock_errors_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        force_flock_error_for_test(Some(libc::EIO));
        let result = MergeLock::try_acquire(tmp.path());
        force_flock_error_for_test(None);
        assert!(result.is_err());
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
