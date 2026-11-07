//! Advisory review rubric: sent to the manager after a task enters review,
//! never part of the merge route.

use super::*;
use crate::team::config::{DecisionPath, DecisionProvider, DecisionsConfig};
use crate::team::decide::test_server::serve_once;
use crate::team::test_support::EnvVarGuard;

const KEY: &str = "gateway-key-for-rubric-tests-0123456789";

fn review_daemon(repo: &Path, endpoint: &str, key_env: &str) -> crate::team::daemon::TeamDaemon {
    let policy = AutoMergePolicy {
        enabled: true,
        ..AutoMergePolicy::default()
    };
    let mut daemon = auto_merge_daemon(repo, policy);
    daemon.merge.set_auto_merge_override(42, false);
    daemon.config.team_config.workflow_policy.decisions = DecisionsConfig {
        provider: DecisionProvider::Jev,
        endpoint: endpoint.to_string(),
        api_key_env: key_env.to_string(),
        paths: vec![DecisionPath::Rubric],
        ..DecisionsConfig::default()
    };
    daemon
}

fn drain_decisions(daemon: &mut crate::team::daemon::TeamDaemon) {
    for _ in 0..500 {
        daemon.poll_decisions().unwrap();
        if daemon.decision_jobs_in_flight() == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("rubric job did not finish");
}

fn task_status_of(repo: &Path) -> String {
    let tasks = crate::task::load_tasks_from_dir(
        &repo
            .join(".batty")
            .join("team_config")
            .join("board")
            .join("tasks"),
    )
    .unwrap();
    tasks.into_iter().find(|t| t.id == 42).unwrap().status
}

#[test]
fn review_task_gets_a_follow_up_rubric_message() {
    let _key = EnvVarGuard::set("BATTY_TEST_RUBRIC_ROUTE", KEY);
    let reply = serde_json::json!({
        "answers": [
            {"type": "score", "name": "criterion_1", "score": 2.0, "confidence": 0.9},
            {"type": "score", "name": "criterion_2", "score": 0.9, "confidence": 0.8},
            {"type": "predicate", "name": "criteria_met", "probability": 0.2}
        ],
        "provider_metadata": {"gateway": {"cost": "0.0001"}}
    })
    .to_string();
    let (endpoint, rx, server) = serve_once("HTTP/1.1 200 OK", reply, Duration::ZERO);
    let (_tmp, repo, _worktree) = setup_auto_merge_repo("eng-1");
    write_task_file_with_body(
        &repo,
        42,
        "auto-merge-task",
        "Add a note.\n\n## Acceptance Criteria\n- adds a note file\n- keeps tests green\n",
    );
    let mut daemon = review_daemon(&repo, &endpoint, "BATTY_TEST_RUBRIC_ROUTE");

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    // The route is unchanged: review, with today's manager message.
    assert_eq!(task_status_of(&repo), "review");
    let root = inbox::inboxes_root(&repo);
    let before = inbox::pending_messages(&root, "manager").unwrap();
    assert_eq!(before.len(), 1);
    assert!(before[0].body.contains("Auto-merge disabled by override"));

    drain_decisions(&mut daemon);
    server.join().unwrap();

    let after = inbox::pending_messages(&root, "manager").unwrap();
    let rubric: Vec<_> = after
        .iter()
        .filter(|m| m.body.starts_with("[rubric]"))
        .collect();
    assert_eq!(rubric.len(), 1);
    assert_eq!(
        rubric[0].body,
        "[rubric] Task #42: criteria met: no\n- adds a note file: met\n- keeps tests green: partly met"
    );
    assert_eq!(task_status_of(&repo), "review");

    let events =
        read_events(&repo.join(".batty").join("team_config").join("events.jsonl")).unwrap();
    let rubric_events: Vec<_> = events
        .iter()
        .filter(|e| e.event == "review_rubric")
        .collect();
    assert_eq!(rubric_events.len(), 1);
    assert_eq!(rubric_events[0].task.as_deref(), Some("42"));
    assert_eq!(rubric_events[0].success, Some(false));
    assert!(rx.recv().unwrap().contains("adds a note file"));
}

#[test]
fn task_without_criteria_starts_no_rubric() {
    let _key = EnvVarGuard::set("BATTY_TEST_RUBRIC_NONE", KEY);
    let (_tmp, repo, _worktree) = setup_auto_merge_repo("eng-1");
    let mut daemon = review_daemon(
        &repo,
        "http://127.0.0.1:1/v1/decisions",
        "BATTY_TEST_RUBRIC_NONE",
    );
    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    assert_eq!(task_status_of(&repo), "review");
    assert_eq!(daemon.decision_jobs_in_flight(), 0);
}

#[test]
fn rubric_failure_sends_no_message() {
    let _key = EnvVarGuard::set("BATTY_TEST_RUBRIC_FAIL", KEY);
    let (endpoint, _rx, server) =
        serve_once("HTTP/1.1 502 Bad Gateway", "{}".to_string(), Duration::ZERO);
    let (_tmp, repo, _worktree) = setup_auto_merge_repo("eng-1");
    write_task_file_with_body(
        &repo,
        42,
        "auto-merge-task",
        "## Acceptance Criteria\n- adds a note file\n",
    );
    let mut daemon = review_daemon(&repo, &endpoint, "BATTY_TEST_RUBRIC_FAIL");
    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();
    drain_decisions(&mut daemon);
    server.join().unwrap();

    let messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "manager").unwrap();
    assert!(messages.iter().all(|m| !m.body.starts_with("[rubric]")));
    let events =
        read_events(&repo.join(".batty").join("team_config").join("events.jsonl")).unwrap();
    let fallbacks: Vec<_> = events
        .iter()
        .filter(|e| e.event == "decision_fallback")
        .collect();
    assert_eq!(fallbacks.len(), 1);
    assert_eq!(fallbacks[0].reason.as_deref(), Some("http_502"));
    assert!(events.iter().all(|e| e.event != "review_rubric"));
}
