//! Shared, validated snapshot of the parsed board for the daemon poll loop.
//!
//! Parsing every task file is expensive and many automation steps need the
//! same data in the same tick. `board_tasks` reuses one parse until something
//! changes: either a board write bumped the directory's write generation
//! (`note_board_write`), or the tasks directory fingerprint (names, sizes,
//! mtimes) differs. When in doubt it reloads.
//!
//! The snapshot itself is owned by the daemon. Board writers that run outside
//! the daemon (`board_write`, kanban-md wrappers) only know the tasks
//! directory, so they bump a process-wide per-directory generation counter
//! that the daemon compares against the generation recorded at load time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use anyhow::Result;

use super::TeamDaemon;
use crate::task::Task;

/// Board write generations keyed by (canonical, when possible) tasks dir.
fn generations() -> std::sync::MutexGuard<'static, HashMap<PathBuf, u64>> {
    static GENERATIONS: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    GENERATIONS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn generation_key(tasks_dir: &Path) -> PathBuf {
    std::fs::canonicalize(tasks_dir).unwrap_or_else(|_| tasks_dir.to_path_buf())
}

fn write_generation(tasks_dir: &Path) -> u64 {
    generations()
        .get(&generation_key(tasks_dir))
        .copied()
        .unwrap_or(0)
}

#[cfg(test)]
thread_local! {
    /// Snapshot reloads on this thread (tests assert on per-tick counts).
    pub(super) static SNAPSHOT_LOADS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Record that a board write happened under `tasks_dir` (or a subprocess
/// that may have changed it ran): every daemon snapshot of that board is
/// treated as stale on its next use.
pub(crate) fn note_board_write(tasks_dir: &Path) {
    let key = generation_key(tasks_dir);
    let mut generations = generations();
    let generation = generations.entry(key).or_insert(0);
    *generation = generation.wrapping_add(1);
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
    tasks_dir: PathBuf,
    tasks: Arc<Vec<Task>>,
    fingerprint: Option<Fingerprint>,
    /// `write_generation(tasks_dir)` observed before the parse started.
    generation: u64,
    #[allow(dead_code)]
    loaded_at: Instant,
}

/// The daemon's cached board parse; empty until the first `board_tasks` call.
#[derive(Default)]
pub(in crate::team) struct BoardSnapshotCache {
    snapshot: Option<BoardSnapshot>,
}

impl BoardSnapshotCache {
    /// Parsed tasks under `tasks_dir`, reusing the cached parse while the
    /// directory fingerprint and write generation are unchanged.
    fn tasks(&mut self, tasks_dir: PathBuf) -> Result<Arc<Vec<Task>>> {
        let current = fingerprint(&tasks_dir);
        let generation = write_generation(&tasks_dir);
        if let Some(snapshot) = &self.snapshot
            && snapshot.tasks_dir == tasks_dir
            && snapshot.generation == generation
            && current.is_some()
            && snapshot.fingerprint == current
        {
            return Ok(Arc::clone(&snapshot.tasks));
        }
        self.snapshot = None;
        #[cfg(test)]
        SNAPSHOT_LOADS.with(|count| count.set(count.get() + 1));
        // Frontmatter repair runs once per real reload, not once per consumer.
        if let Some(board_dir) = tasks_dir.parent() {
            crate::team::status::repair_board_frontmatter_logged(board_dir, "board_snapshot")?;
        }
        // Repairs are board writes of our own; do not count them as staleness.
        let generation = write_generation(&tasks_dir);
        let tasks = Arc::new(crate::task::load_tasks_from_dir(&tasks_dir)?);
        // Fingerprint after loading so frontmatter repairs made during the
        // parse do not invalidate the snapshot on the next call.
        self.snapshot = Some(BoardSnapshot {
            fingerprint: fingerprint(&tasks_dir),
            tasks_dir,
            tasks: Arc::clone(&tasks),
            generation,
            loaded_at: Instant::now(),
        });
        Ok(tasks)
    }

    /// Drop the cached snapshot; the next `tasks` call reparses.
    fn invalidate(&mut self) {
        self.snapshot = None;
    }
}

impl TeamDaemon {
    /// Parsed board tasks, reusing the cached snapshot while it is valid.
    pub(super) fn board_tasks(&mut self) -> Result<Arc<Vec<Task>>> {
        let tasks_dir = self.board_dir().join("tasks");
        self.board_snapshot.tasks(tasks_dir)
    }

    /// Drop the cached snapshot; the next `board_tasks` call reparses.
    pub(super) fn invalidate_board_snapshot(&mut self) {
        self.board_snapshot.invalidate();
    }
}
