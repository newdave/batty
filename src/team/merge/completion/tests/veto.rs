//! Jev completion veto: only a completion that passed every hard gate can be
//! vetoed, and any fallback leaves today's outcome.

use super::*;
use crate::team::config::{DecisionPath, DecisionProvider, DecisionTimeouts, DecisionsConfig};
use crate::team::decide::paths::CompletionOutcome;
use crate::team::decide::test_server::serve_once;
use crate::team::test_support::EnvVarGuard;

const KEY: &str = "gateway-key-for-veto-tests-0123456789";
const UNUSED: &str = "http://127.0.0.1:1/v1/decisions";

/// A repo with task 42 and an engineer worktree holding one real code commit.
fn passing_completion(
    tmp: &tempfile::TempDir,
    endpoint: &str,
    key_env: &str,
) -> (PathBuf, crate::team::daemon::TeamDaemon) {
    let repo = init_git_repo(tmp, "batty-merge-test");
    write_task_file(&repo, 42, "veto-task");
    let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
    std::fs::write(
        worktree_dir.join("src").join("lib.rs"),
        "pub fn smoke() -> bool { true }\n",
    )
    .unwrap();
    git_ok(&worktree_dir, &["add", "src/lib.rs"]);
    git_ok(&worktree_dir, &["commit", "-m", "real change"]);
    seed_completion_packet_metadata(&repo, 42, "veto-task", &worktree_dir);

    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    let policy = &mut daemon.config.team_config.workflow_policy;
    policy.verification.auto_run_tests = false;
    policy.decisions = DecisionsConfig {
        provider: DecisionProvider::Jev,
        endpoint: endpoint.to_string(),
        api_key_env: key_env.to_string(),
        paths: vec![DecisionPath::Completion],
        ..DecisionsConfig::default()
    };
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);
    (repo, daemon)
}

fn events_named(repo: &Path, name: &str) -> Vec<crate::team::events::TeamEvent> {
    read_events(&repo.join(".batty").join("team_config").join("events.jsonl"))
        .unwrap()
        .into_iter()
        .filter(|event| event.event == name)
        .collect()
}

fn completion_reply(done: f64, narration_only: f64) -> String {
    serde_json::json!({
        "answers": [
            {"type": "predicate", "name": "done", "probability": done},
            {"type": "predicate", "name": "narration_only", "probability": narration_only}
        ],
        "provider_metadata": {"gateway": {"cost": "0.00002"}}
    })
    .to_string()
}

#[test]
fn cached_narration_veto_rejects_a_passing_completion() {
    let _key = EnvVarGuard::unset("BATTY_TEST_VETO_SEEDED_A");
    let tmp = tempfile::tempdir().unwrap();
    let (repo, mut daemon) = passing_completion(&tmp, UNUSED, "BATTY_TEST_VETO_SEEDED_A");
    daemon.seed_completion_outcome(
        42,
        CompletionOutcome {
            done: true,
            narration_only: true,
        },
    );

    complete_task_and_process_queue(&mut daemon, "eng-1");

    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    let vetoed = events_named(&repo, "completion_vetoed");
    assert_eq!(vetoed.len(), 1);
    assert_eq!(vetoed[0].task.as_deref(), Some("42"));
    assert_eq!(vetoed[0].reason.as_deref(), Some("narration_only"));
    assert_eq!(events_named(&repo, "narration_rejection").len(), 1);
    let messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert_eq!(messages.len(), 1);
}

#[test]
fn cached_not_done_veto_rejects_as_insufficient_evidence() {
    let _key = EnvVarGuard::unset("BATTY_TEST_VETO_SEEDED_B");
    let tmp = tempfile::tempdir().unwrap();
    let (repo, mut daemon) = passing_completion(&tmp, UNUSED, "BATTY_TEST_VETO_SEEDED_B");
    daemon.seed_completion_outcome(
        42,
        CompletionOutcome {
            done: false,
            narration_only: false,
        },
    );

    complete_task_and_process_queue(&mut daemon, "eng-1");

    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    let vetoed = events_named(&repo, "completion_vetoed");
    assert_eq!(vetoed[0].reason.as_deref(), Some("insufficient_evidence"));
    assert!(events_named(&repo, "narration_rejection").is_empty());
    let messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert!(messages[0].body.contains("no sufficient task evidence"));
}

#[test]
fn approving_outcome_accepts_the_completion() {
    let _key = EnvVarGuard::unset("BATTY_TEST_VETO_SEEDED_C");
    let tmp = tempfile::tempdir().unwrap();
    let (repo, mut daemon) = passing_completion(&tmp, UNUSED, "BATTY_TEST_VETO_SEEDED_C");
    daemon.seed_completion_outcome(
        42,
        CompletionOutcome {
            done: true,
            narration_only: false,
        },
    );
    complete_task_and_process_queue(&mut daemon, "eng-1");
    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert!(events_named(&repo, "completion_vetoed").is_empty());
}

#[test]
fn veto_never_fires_on_a_no_commits_rejection() {
    let _key = EnvVarGuard::unset("BATTY_TEST_VETO_NOCOMMITS");
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(&tmp, "batty-merge-test");
    write_task_file(&repo, 42, "no-commit-task");
    let (worktree_dir, team_config_dir) = engineer_worktree_paths(&repo, "eng-1");
    setup_engineer_worktree(&repo, &worktree_dir, "eng-1", &team_config_dir).unwrap();
    let mut daemon = setup_completion_daemon(&repo, "eng-1");
    let policy = &mut daemon.config.team_config.workflow_policy;
    policy.verification.auto_run_tests = false;
    policy.decisions.provider = DecisionProvider::Jev;
    policy.decisions.api_key_env = "BATTY_TEST_VETO_NOCOMMITS".to_string();
    daemon.seed_completion_outcome(
        42,
        CompletionOutcome {
            done: false,
            narration_only: true,
        },
    );
    daemon.set_active_task_for_test("eng-1", 42);
    daemon.set_member_state_for_test("eng-1", MemberState::Working);

    complete_task_and_process_queue(&mut daemon, "eng-1");

    assert!(events_named(&repo, "completion_vetoed").is_empty());
    let messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert!(messages[0].body.contains("no commits ahead of main"));
}

#[test]
fn veto_never_fires_on_failing_tests() {
    let _key = EnvVarGuard::unset("BATTY_TEST_VETO_TESTSFAIL");
    let tmp = tempfile::tempdir().unwrap();
    let (repo, mut daemon) = passing_completion(&tmp, UNUSED, "BATTY_TEST_VETO_TESTSFAIL");
    let policy = &mut daemon.config.team_config.workflow_policy;
    policy.verification.auto_run_tests = true;
    policy.verification.test_command = Some("false".to_string());
    daemon.seed_completion_outcome(
        42,
        CompletionOutcome {
            done: false,
            narration_only: true,
        },
    );

    handle_engineer_completion_and_wait(&mut daemon, "eng-1").unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    assert!(events_named(&repo, "completion_vetoed").is_empty());
    let messages = inbox::pending_messages(&inbox::inboxes_root(&repo), "eng-1").unwrap();
    assert!(messages[0].body.contains("test command did not pass"));
}

#[test]
fn test_skipped_completion_waits_for_the_jev_answer_then_vetoes() {
    let _key = EnvVarGuard::set("BATTY_TEST_VETO_LIVE", KEY);
    let (endpoint, rx, server) = serve_once(
        "HTTP/1.1 200 OK",
        completion_reply(0.1, 0.95),
        Duration::from_millis(100),
    );
    let tmp = tempfile::tempdir().unwrap();
    let (repo, mut daemon) = passing_completion(&tmp, &endpoint, "BATTY_TEST_VETO_LIVE");
    let planted = format!("{}{}", "sk-", "ant-api03-PlantedVetoSecret0123456789");
    daemon.stash_completion_response("eng-1", &format!("All done! I will now write {planted}"));

    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    // Parked: verification is done, the decision is still running.
    assert_eq!(daemon.verification.background_tests.awaiting_veto.len(), 1);
    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    // A repeated completion while parked is ignored.
    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    assert_eq!(daemon.decision_jobs_in_flight(), 1);

    wait_for_verification_jobs(&mut daemon).unwrap();
    server.join().unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), Some(42));
    let vetoed = events_named(&repo, "completion_vetoed");
    assert_eq!(vetoed.len(), 1);
    assert_eq!(vetoed[0].reason.as_deref(), Some("narration_only"));
    let request = rx.recv().unwrap();
    assert!(request.contains("All done! I will now write"));
    assert!(request.contains("veto-task"));
    assert!(!request.contains("PlantedVetoSecret"));
}

#[test]
fn jev_fallback_leaves_the_completion_unchanged() {
    let _key = EnvVarGuard::set("BATTY_TEST_VETO_FALLBACK", KEY);
    let (endpoint, _rx, server) = serve_once(
        "HTTP/1.1 500 Internal Server Error",
        "{}".to_string(),
        Duration::ZERO,
    );
    let tmp = tempfile::tempdir().unwrap();
    let (repo, mut daemon) = passing_completion(&tmp, &endpoint, "BATTY_TEST_VETO_FALLBACK");

    complete_task_and_process_queue(&mut daemon, "eng-1");
    server.join().unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert!(events_named(&repo, "completion_vetoed").is_empty());
    let fallbacks = events_named(&repo, "decision_fallback");
    assert_eq!(fallbacks.len(), 1);
    assert_eq!(fallbacks[0].reason.as_deref(), Some("http_500"));
}

#[test]
fn parked_completion_proceeds_once_the_completion_timeout_passes() {
    let _key = EnvVarGuard::set("BATTY_TEST_VETO_TIMEOUT", KEY);
    let (endpoint, _rx, server) = serve_once(
        "HTTP/1.1 200 OK",
        completion_reply(0.1, 0.95),
        Duration::from_millis(1500),
    );
    let tmp = tempfile::tempdir().unwrap();
    let (repo, mut daemon) = passing_completion(&tmp, &endpoint, "BATTY_TEST_VETO_TIMEOUT");
    daemon
        .config
        .team_config
        .workflow_policy
        .decisions
        .timeouts_ms = DecisionTimeouts {
        completion: 150,
        ..DecisionTimeouts::default()
    };

    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    poll_verification_jobs(&mut daemon).unwrap();
    assert_eq!(daemon.verification.background_tests.awaiting_veto.len(), 1);
    std::thread::sleep(Duration::from_millis(200));
    // Decisions are not polled: the parked deadline alone releases it.
    poll_verification_jobs(&mut daemon).unwrap();
    assert!(
        daemon
            .verification
            .background_tests
            .awaiting_veto
            .is_empty()
    );
    daemon.process_merge_queue_for_test().unwrap();

    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert!(events_named(&repo, "completion_vetoed").is_empty());
    server.join().unwrap();
}

#[test]
fn heuristic_provider_never_parks_or_vetoes() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, mut daemon) = passing_completion(&tmp, UNUSED, "BATTY_TEST_VETO_HEURISTIC");
    daemon.config.team_config.workflow_policy.decisions.provider = DecisionProvider::Heuristic;
    daemon.seed_completion_outcome(
        42,
        CompletionOutcome {
            done: false,
            narration_only: true,
        },
    );
    daemon.stash_completion_response("eng-1", "anything");

    handle_engineer_completion(&mut daemon, "eng-1").unwrap();
    assert!(
        daemon
            .verification
            .background_tests
            .awaiting_veto
            .is_empty()
    );
    assert_eq!(daemon.decision_jobs_in_flight(), 0);
    daemon.process_merge_queue_for_test().unwrap();
    assert_eq!(daemon.active_task_id("eng-1"), None);
    assert!(events_named(&repo, "completion_vetoed").is_empty());
}
