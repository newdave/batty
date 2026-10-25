//! Task writes coordinated with kanban-md's board `.lock` advisory flock.
//!
//! Protocol verified against kanban-md 0.39.0 internal/filelock: open/create
//! `.lock` read-write, mode 0600, acquire exclusive flock, unlock and close.
//! Its create command and TUI use this lock; its standalone edit/move commands
//! do not. Batty wraps those commands with the same lock (board_cmd). External
//! raw CLI edits must therefore be coordinated by the caller as well.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_yaml::{Mapping, Value};

fn reject_symlinks(path: &Path) -> Result<()> {
    // Check the task, tasks directory and board root. System aliases above the
    // board (for example /var on macOS) are not task-controlled symlinks.
    for component in path.ancestors().take(3) {
        if fs::symlink_metadata(component)?.file_type().is_symlink() {
            bail!("board mutation refuses symlink paths: {}", path.display());
        }
    }
    Ok(())
}

fn frontmatter(content: &str) -> Result<Mapping> {
    let rest = content
        .trim_start()
        .strip_prefix("---\n")
        .context("board mutation requires YAML frontmatter")?;
    let end = rest
        .find("\n---")
        .context("board mutation requires closing frontmatter delimiter")?;
    Ok(serde_yaml::from_str(&rest[..end])?)
}

/// The lock inode must never be removed or replaced: Go processes may already
/// hold descriptors to it. Closing this descriptor also releases flock on error.
#[cfg(unix)]
pub(crate) struct BoardLock(File);

#[cfg(unix)]
impl BoardLock {
    pub(crate) fn acquire(directory: &Path) -> Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let path = directory.join(".lock");
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("board lock refuses symlink: {}", path.display());
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        if !file.metadata()?.is_file() {
            bail!("board lock must be a regular file");
        }
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
        Ok(Self(file))
    }

    pub(crate) fn release(self) -> Result<()> {
        use std::os::fd::AsRawFd;
        if unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}

#[cfg(not(unix))]
pub(crate) struct BoardLock;
#[cfg(not(unix))]
impl BoardLock {
    pub(crate) fn acquire(_: &Path) -> Result<Self> {
        bail!("native board flock mutation is unsupported on this platform")
    }
    pub(crate) fn release(self) -> Result<()> {
        Ok(())
    }
}

fn finish<T>(operation: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(anyhow::anyhow!(
            "{error:#}; cleanup also failed: {cleanup:#}"
        )),
    }
}

/// `expected` is the caller's original snapshot, not a reread after computing
/// the desired mutation. Detect stale callers, changed claims and replacements
/// under the lock before modifying permissions or any bytes. Structured YAML is
/// written intact; there is no conversion through the scalar-only CLI API.
pub(crate) fn write_task_file(path: &Path, expected: &str, contents: &str) -> Result<()> {
    reject_symlinks(path)?;
    let directory = path.parent().context("task has no parent directory")?;
    let board = if directory.file_name().is_some_and(|name| name == "tasks") {
        directory
            .parent()
            .context("tasks directory has no board root")?
    } else {
        directory
    };
    let lock = BoardLock::acquire(board)?;
    let result = write_locked(path, expected, contents);
    finish(result, lock.release())
}

#[cfg(unix)]
fn write_locked(path: &Path, expected: &str, contents: &str) -> Result<()> {
    write_locked_with(path, expected, contents, |writer, contents| {
        writer.seek(SeekFrom::Start(0))?;
        writer.write_all(contents.as_bytes())?;
        writer.set_len(contents.len() as u64)?;
        writer.sync_all()?;
        Ok(())
    })
}

#[cfg(unix)]
fn write_locked_with(
    path: &Path,
    expected: &str,
    contents: &str,
    write: impl FnOnce(&mut File, &str) -> Result<()>,
) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    reject_symlinks(path)?;
    let original = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = original.metadata()?;
    if !metadata.is_file() {
        bail!("task must be a regular file");
    }
    let mut actual = String::new();
    (&original).read_to_string(&mut actual)?;
    if actual != expected {
        bail!("task changed since mutation was prepared; retry with a fresh snapshot");
    }
    let old = frontmatter(expected)?;
    let new = frontmatter(contents)?;
    if old.get("id") != new.get("id") {
        bail!("board mutation cannot change task identity");
    }
    let owner = |mapping: &Mapping| -> Result<Option<String>> {
        match mapping.get("claimed_by") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(name)) => Ok((!name.is_empty()).then(|| name.clone())),
            _ => bail!("invalid task claim owner"),
        }
    };
    // The original snapshot includes the expected claim owner. A competing
    // claim change fails the exact reread above. Explicit owner assignment or
    // release by the calling task API remains supported after that validation.
    owner(&old)?;
    owner(&new)?;
    if actual == contents {
        return Ok(());
    }
    let permissions = metadata.permissions();
    // Match the backend write protocol only inside its exclusive shared lock.
    // Restore the exact original mode even when opening/writing/flushing fails.
    if permissions.readonly() {
        original.set_permissions(fs::Permissions::from_mode(permissions.mode() | 0o200))?;
    }
    let operation = (|| -> Result<()> {
        let mut writer = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let current = writer.metadata()?;
        if current.dev() != metadata.dev() || current.ino() != metadata.ino() {
            bail!("task inode changed during board mutation");
        }
        let written = write(&mut writer, contents);
        if written.is_err() {
            // Keep the same inode/claim lock. A partial I/O failure gets a
            // best-effort rollback under lock; surface both errors if rollback
            // itself fails rather than reporting a successful restoration.
            let rollback = (|| -> Result<()> {
                writer.seek(SeekFrom::Start(0))?;
                writer.write_all(expected.as_bytes())?;
                writer.set_len(expected.len() as u64)?;
                writer.sync_all()?;
                Ok(())
            })()
            .context("failed to roll back task after write failure");
            return finish(written, rollback);
        }
        written
    })();
    let restored = if permissions.readonly() {
        original
            .set_permissions(permissions)
            .context("failed to restore exact task permissions")
    } else {
        Ok(())
    };
    finish(operation, restored)
}

#[cfg(not(unix))]
fn write_locked(_: &Path, _: &str, _: &str) -> Result<()> {
    bail!("native board flock mutation is unsupported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_metadata_and_claimed_standalone_are_supported() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("task.md");
        let old = "---\nid: 1\nclaimed_by: role-a\n---\nbody\n";
        let new = "---\nid: 1\nclaimed_by: role-a\nchanged_paths: [src/a.rs]\ntest_results: {passed: 2, failed: 0}\n---\nbody\n";
        fs::write(&path, old).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions();
        write_task_file(&path, old, new).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), new);
        assert_eq!(fs::metadata(path).unwrap().permissions(), mode);
    }

    #[test]
    fn stale_snapshot_and_invalid_claim_leave_card_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("task.md");
        let before = "---\nid: 1\nclaimed_by: role-a\n---\nbody\n";
        fs::write(&path, before).unwrap();
        assert!(write_task_file(&path, "stale", before).is_err());
        assert!(
            write_task_file(
                &path,
                before,
                &before.replace("claimed_by: role-a", "claimed_by: [role-b]")
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), before);
    }

    #[test]
    fn deliberate_claim_release_and_timestamp_repair_are_supported() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("task.md");
        let before = "---\nid: 1\nclaimed_by: role-a\ncreated: old-format\n---\nbody\n";
        let after = "---\nid: 1\ncreated: repaired-format\n---\nbody\n";
        fs::write(&path, before).unwrap();
        write_task_file(&path, before, after).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), after);
    }

    #[cfg(unix)]
    #[test]
    fn readonly_structured_write_preserves_exact_mode_and_inode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let board = tmp.path().join("board");
        fs::create_dir_all(board.join("tasks")).unwrap();
        let path = board.join("tasks/001-task.md");
        let before = "---\nid: 1\nclaimed_by: role-a\n---\nbody\n";
        let after = "---\nid: 1\nclaimed_by: role-a\nartifacts: [a, b]\n---\nbody\n";
        fs::write(&path, before).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o440)).unwrap();
        let original = fs::metadata(&path).unwrap();
        write_task_file(&path, before, after).unwrap();
        let result = fs::metadata(&path).unwrap();
        assert_eq!(result.permissions().mode(), original.permissions().mode());
        assert_eq!(result.ino(), original.ino());
        assert_eq!(fs::read_to_string(path).unwrap(), after);
    }

    #[cfg(unix)]
    #[test]
    fn task_and_lock_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("task.md");
        let before = "---\nid: 1\n---\nbody\n";
        fs::write(&path, before).unwrap();
        let link = tmp.path().join("link.md");
        symlink(&path, &link).unwrap();
        assert!(write_task_file(&link, before, before).is_err());
        symlink(&path, tmp.path().join(".lock")).unwrap();
        assert!(write_task_file(&path, before, before).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), before);
    }

    #[test]
    fn writer_waits_for_backend_flock_and_rechecks_snapshot() {
        use std::sync::mpsc;
        use std::time::Duration;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("task.md");
        let before = "---\nid: 1\n---\nbody\n";
        fs::write(&path, before).unwrap();
        let held = BoardLock::acquire(tmp.path()).unwrap();
        let (tx, rx) = mpsc::channel();
        let file = path.clone();
        let writer = std::thread::spawn(move || {
            tx.send(write_task_file(
                &file,
                before,
                &before.replace("body", "new"),
            ))
            .unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        // Simulate another cooperative board writer finishing while holding lock.
        fs::write(&path, before.replace("body", "other")).unwrap();
        held.release().unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        writer.join().unwrap();
        assert!(fs::read_to_string(path).unwrap().contains("other"));
    }
    #[cfg(unix)]
    #[test]
    fn partial_write_failure_rolls_back_and_restores_exact_readonly_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("task.md");
        let before = "---\nid: 1\nclaimed_by: role-a\n---\nbody\n";
        let after = before.replace("body", "new body");
        fs::write(&path, before).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o440)).unwrap();
        let lock = BoardLock::acquire(tmp.path()).unwrap();
        let result = write_locked_with(&path, before, &after, |writer, _| {
            writer.write_all(b"partial overwrite")?;
            bail!("injected write failure")
        });
        lock.release().unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("injected write failure")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o440
        );
    }
    #[test]
    fn explicit_owner_assignment_uses_original_claim_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("task.md");
        let before = "---\nid: 1\nclaimed_by: role-a\n---\nbody\n";
        let after = before.replace("role-a", "role-b");
        fs::write(&path, before).unwrap();
        write_task_file(&path, before, &after).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), after);
    }
}
