//! Shared, validated snapshot of the parsed board for the daemon poll loop.
//!
//! Parsing every task file is expensive and many automation steps need the
//! same data in the same tick. `board_tasks` reuses one parse until something
//! changes: either a daemon board write dropped it (`note_board_write`), or the
//! tasks directory fingerprint (names, sizes, mtimes) differs. When in doubt
//! it reloads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use anyhow::Result;

use super::TeamDaemon;
use crate::task::Task;

/// Snapshots keyed by tasks dir, so `TeamDaemon` needs no extra field and
/// concurrent daemons (tests) never share state.
fn snapshots() -> std::sync::MutexGuard<'static, HashMap<PathBuf, BoardSnapshot>> {
    static SNAPSHOTS: OnceLock<Mutex<HashMap<PathBuf, BoardSnapshot>>> = OnceLock::new();
    SNAPSHOTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
thread_local! {
    /// Snapshot reloads on this thread (tests assert on per-tick counts).
    pub(super) static SNAPSHOT_LOADS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Record that a board write happened under `tasks_dir` (or a subprocess
/// that may have changed it ran): drops every cached snapshot for that board.
pub(crate) fn note_board_write(tasks_dir: &Path) {
    let canonical = std::fs::canonicalize(tasks_dir).ok();
    snapshots().retain(|key, _| {
        key != tasks_dir
            && !canonical.as_ref().is_some_and(|canonical| {
                std::fs::canonicalize(key).ok().as_ref() == Some(canonical)
            })
    });
}

type Fingerprint = (
    Option<SystemTime>,
    Vec<(std::ffi::OsString, u64, Option<SystemTime>)>,
);

fn fingerprint(dir: &Path) -> Option<Fingerprint> {
    let dir_mtime = std::fs::metadata(dir).and_then(|m| m.modified()).ok();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        let meta = entry.metadata().ok()?;
        files.push((entry.file_name(), meta.len(), meta.modified().ok()));
    }
    files.sort();
    Some((dir_mtime, files))
}

struct BoardSnapshot {
    tasks: Arc<Vec<Task>>,
    fingerprint: Option<Fingerprint>,
    #[allow(dead_code)]
    loaded_at: Instant,
}

impl TeamDaemon {
    /// Parsed board tasks, reusing the cached snapshot while it is valid.
    pub(super) fn board_tasks(&mut self) -> Result<Arc<Vec<Task>>> {
        let tasks_dir = self.board_dir().join("tasks");
        let current = fingerprint(&tasks_dir);
        if let Some(snapshot) = snapshots().get(&tasks_dir)
            && current.is_some()
            && snapshot.fingerprint == current
        {
            return Ok(Arc::clone(&snapshot.tasks));
        }
        snapshots().remove(&tasks_dir);
        #[cfg(test)]
        SNAPSHOT_LOADS.with(|count| count.set(count.get() + 1));
        let tasks = Arc::new(crate::task::load_tasks_from_dir(&tasks_dir)?);
        // Fingerprint after loading so frontmatter repairs made during the
        // parse do not invalidate the snapshot on the next call.
        let snapshot = BoardSnapshot {
            tasks: Arc::clone(&tasks),
            fingerprint: fingerprint(&tasks_dir),
            loaded_at: Instant::now(),
        };
        snapshots().insert(tasks_dir, snapshot);
        Ok(tasks)
    }

    /// Drop the cached snapshot; the next `board_tasks` call reparses.
    pub(super) fn invalidate_board_snapshot(&self) {
        let tasks_dir = self.board_dir().join("tasks");
        snapshots().remove(&tasks_dir);
    }
}
