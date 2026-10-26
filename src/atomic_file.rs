//! Crash-safe file replacement.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Replace `path` with `contents` atomically: write a temp file in the same
/// directory, fsync it, then rename it over the target. Readers see either the
/// old or the new file, never a truncated one, and a crash mid-write leaves
/// the original intact.
///
/// The temp file is created with mode 0600, so newly written files are
/// private to the user. If `path` is a symlink, its target is replaced.
pub fn write(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    let target = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::canonicalize(path)
            .with_context(|| format!("failed to resolve symlink {}", path.display()))?,
        _ => path.to_path_buf(),
    };
    let parent = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create {}", parent.display()))?;

    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create temp file in {}", parent.display()))?;
    temp.write_all(contents.as_ref())
        .and_then(|()| temp.as_file().sync_all())
        .with_context(|| format!("failed to write temp file for {}", target.display()))?;
    temp.persist(&target)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to write {}", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_and_replaces_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("state.json");
        write(&path, "first").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");
        write(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        let leftovers = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
        assert_eq!(leftovers, 1, "temp files must not be left behind");
    }

    #[cfg(unix)]
    #[test]
    fn new_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".env");
        write(&path, "TOKEN=secret\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn write_through_symlink_replaces_target() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real.env");
        let link = tmp.path().join(".env");
        std::fs::write(&real, "old").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write(&link, "new").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
    }
}
