use std::path::Path;
use std::sync::Arc;

use super::board_snapshot::note_board_write;
use crate::team::test_support::TestDaemonBuilder;

fn parses() -> u64 {
    crate::task::BOARD_PARSE_COUNT.with(|count| count.get())
}

fn snapshot_loads() -> u64 {
    super::board_snapshot::SNAPSHOT_LOADS.with(|count| count.get())
}

fn write_task(dir: &Path, id: u32, status: &str) {
    std::fs::write(
        dir.join(format!("{id:03}-task.md")),
        format!(
            "---\nid: {id}\ntitle: task {id}\nstatus: {status}\npriority: medium\nclass: standard\n---\n\nBody.\n"
        ),
    )
    .unwrap();
}

fn board(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let tasks_dir = tmp.path().join(".batty/team_config/board/tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    tasks_dir
}

#[test]
fn snapshot_reused_until_board_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = board(&tmp);
    write_task(&tasks_dir, 1, "todo");
    let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

    let before = parses();
    let first = daemon.board_tasks().unwrap();
    let second = daemon.board_tasks().unwrap();
    assert_eq!(parses() - before, 1, "second call must reuse the snapshot");
    assert!(Arc::ptr_eq(&first, &second));

    // A board write through the write helpers is visible to later steps at once.
    write_task(&tasks_dir, 2, "todo");
    note_board_write(&tasks_dir);
    let third = daemon.board_tasks().unwrap();
    assert_eq!(third.len(), 2);
    assert_eq!(parses() - before, 2);

    // An external change is caught by the directory fingerprint.
    write_task(&tasks_dir, 3, "todo");
    assert_eq!(daemon.board_tasks().unwrap().len(), 3);

    daemon.invalidate_board_snapshot();
    let reloaded = parses();
    daemon.board_tasks().unwrap();
    assert_eq!(parses() - reloaded, 1);
}

#[test]
fn board_write_within_tick_is_visible_to_later_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = board(&tmp);
    write_task(&tasks_dir, 1, "todo");
    let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

    let tasks = daemon.board_tasks().unwrap();
    assert_eq!(tasks[0].status, "todo");

    let path = tasks_dir.join("001-task.md");
    let original = std::fs::read_to_string(&path).unwrap();
    let updated = original.replace("status: todo", "status: done");
    crate::team::board_write::write_task_file(&path, &original, &updated).unwrap();

    let tasks = daemon.board_tasks().unwrap();
    assert_eq!(tasks[0].status, "done");
}

#[test]
fn tick_parses_board_at_most_once_when_nothing_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = board(&tmp);
    write_task(&tasks_dir, 1, "todo");
    write_task(&tasks_dir, 2, "in-progress");
    let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

    daemon.tick();
    let before = snapshot_loads();
    daemon.tick();
    let per_tick = snapshot_loads() - before;
    assert!(
        per_tick <= 1,
        "expected at most 1 snapshot reload per tick, got {per_tick}"
    );
}

#[test]
fn board_write_invalidates_every_daemon_snapshot_of_that_board_only() {
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = board(&tmp);
    write_task(&tasks_dir, 1, "todo");
    let other_tmp = tempfile::tempdir().unwrap();
    let other_dir = board(&other_tmp);
    write_task(&other_dir, 1, "todo");
    let mut first = TestDaemonBuilder::new(tmp.path()).build();
    let mut second = TestDaemonBuilder::new(tmp.path()).build();
    let mut other = TestDaemonBuilder::new(other_tmp.path()).build();
    first.board_tasks().unwrap();
    second.board_tasks().unwrap();
    other.board_tasks().unwrap();

    // Nothing changed on disk, so only the write generation can invalidate.
    note_board_write(&tasks_dir);
    let before = parses();
    first.board_tasks().unwrap();
    second.board_tasks().unwrap();
    assert_eq!(parses() - before, 2, "both daemons on the board reload");

    let before = parses();
    other.board_tasks().unwrap();
    assert_eq!(parses() - before, 0, "a write to another board is ignored");
}

#[test]
fn tick_with_dispatch_and_pane_labels_parses_board_at_most_once() {
    use crate::team::test_support::{engineer_member, manager_member};

    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = board(&tmp);
    write_task(&tasks_dir, 1, "todo");
    write_task(&tasks_dir, 2, "todo");
    write_task(&tasks_dir, 3, "in-progress");
    let members = vec![
        manager_member("manager", None),
        engineer_member("eng-1", Some("manager"), false),
    ];
    let pane_map = [("eng-1".to_string(), "%999".to_string())].into();
    let mut daemon = TestDaemonBuilder::new(tmp.path())
        .members(members)
        .pane_map(pane_map)
        .build();

    // Warm up: the first ticks may assign, enqueue, and write the board.
    daemon.tick();
    daemon.tick();
    let before = parses();
    let loads = snapshot_loads();
    // Exercise the converted paths directly as well as through the tick.
    daemon.enqueue_dispatch_candidates().unwrap();
    daemon.tick();
    let per_run = parses() - before;
    assert!(
        per_run <= 1,
        "dispatch + pane labels must share one board parse when nothing changes, got {per_run} \
         parses ({} snapshot loads)",
        snapshot_loads() - loads
    );
}
