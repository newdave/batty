//! Pure helpers for bench recovery: persisted per-task recovery state, the
//! effort ladder, and the shared "moderate bar" checks.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_yaml::Value;

use super::daemon::completion_checks::VerificationRunResult;
use super::task_cmd::{update_task_frontmatter, yaml_key};

/// Effort assumed for a member that has no explicit override.
const BASE_EFFORT: &str = "medium";

/// Recovery state stored in task frontmatter so it survives daemon restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct BenchRecoveryFrontmatter {
    #[serde(default)]
    pub bench_reset_count: u32,
    #[serde(default)]
    pub avoid_engineers: Vec<String>,
    #[serde(default)]
    pub effort_override: Option<String>,
    #[serde(default)]
    pub bench_reset_capped: bool,
}

/// Read the recovery state of a task file. Unreadable or malformed files
/// yield the default (no resets recorded).
pub fn read_bench_recovery(task_path: &Path) -> BenchRecoveryFrontmatter {
    let Ok(content) = std::fs::read_to_string(task_path) else {
        return BenchRecoveryFrontmatter::default();
    };
    let Ok((frontmatter, _)) = crate::task::frontmatter::split(&content) else {
        return BenchRecoveryFrontmatter::default();
    };
    serde_yaml::from_str(frontmatter).unwrap_or_default()
}

/// Write the recovery state, leaving every unrelated frontmatter key intact.
pub fn write_bench_recovery(task_path: &Path, state: &BenchRecoveryFrontmatter) -> Result<()> {
    update_task_frontmatter(task_path, |mapping| {
        mapping.insert(
            yaml_key("bench_reset_count"),
            Value::Number(state.bench_reset_count.into()),
        );
        if state.avoid_engineers.is_empty() {
            mapping.remove(yaml_key("avoid_engineers"));
        } else {
            mapping.insert(
                yaml_key("avoid_engineers"),
                Value::Sequence(
                    state
                        .avoid_engineers
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        match &state.effort_override {
            Some(effort) => {
                mapping.insert(yaml_key("effort_override"), Value::String(effort.clone()));
            }
            None => {
                mapping.remove(yaml_key("effort_override"));
            }
        }
        if state.bench_reset_capped {
            mapping.insert(yaml_key("bench_reset_capped"), Value::Bool(true));
        } else {
            mapping.remove(yaml_key("bench_reset_capped"));
        }
    })
}

/// One step up the ladder from `current` (or from "medium" when the member has
/// none). Returns `None` when the ladder is empty or `current` is not on it,
/// and stays at the top rung.
pub fn next_effort(ladder: &[String], current: Option<&str>) -> Option<String> {
    let current = current.unwrap_or(BASE_EFFORT);
    let index = ladder.iter().position(|step| step == current)?;
    let next = (index + 1).min(ladder.len() - 1);
    Some(ladder[next].clone())
}

/// Git evidence for the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarEvidence {
    pub commits_ahead_of_trunk: u32,
    pub attributed_commits: u32,
}

/// The commit half of the bar: ahead of trunk and at least one attributed commit.
pub fn bar_commits_met(evidence: &BarEvidence) -> bool {
    evidence.commits_ahead_of_trunk > 0 && evidence.attributed_commits > 0
}

/// Count commits in `base..task_branch` whose subject lacks the daemon's
/// `[batty]` auto-save marker.
pub fn attributed_commit_count(worktree: &Path, base: &str, task_branch: &str) -> Result<u32> {
    let range = format!("{base}..{task_branch}");
    let output = crate::git::cmd(worktree)
        .args(["log", "--format=%s", &range])
        .run()
        .with_context(|| format!("failed to list commits in {range}"))?;
    let count = output
        .stdout_str()
        .lines()
        .filter(|subject| !subject.contains("[batty]"))
        .count();
    Ok(count as u32)
}

/// Commits an engineer made outside the task branch that may belong on it.
///
/// Candidates are non-merge commits on the engineer's own branches (its base
/// branch and other `<engineer>/...` branches, never another engineer's) that
/// are not on `task_branch` or trunk, were committed no earlier than the point
/// where the task branch forked from the base branch, and are not daemon
/// `[batty]` auto-saves. Returned oldest first. Batty records no assignment
/// time, so the fork point stands in for it.
pub fn find_rescuable_commits(
    repo: &Path,
    engineer: &str,
    task_branch: &str,
    base_branch: &str,
    trunk: &str,
) -> Result<Vec<String>> {
    let fork_point = crate::git::cmd(repo)
        .args(["merge-base", task_branch, base_branch])
        .run()
        .with_context(|| format!("no fork point for {task_branch} and {base_branch}"))?
        .stdout_trimmed();
    let fork_time: i64 = crate::git::cmd(repo)
        .args(["log", "-1", "--format=%ct", &fork_point])
        .run()?
        .stdout_trimmed()
        .parse()
        .context("unreadable fork point commit time")?;

    let trunk_exists = crate::git::cmd(repo)
        .args(["rev-parse", "--verify", "--quiet", trunk])
        .allow_fail()
        .run()
        .is_ok_and(|output| output.success());

    let branches = crate::git::cmd(repo)
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .run()?
        .stdout_str();
    let mut found: Vec<(i64, String)> = Vec::new();
    for branch in branches.lines().map(str::trim) {
        if branch == task_branch
            || !crate::worktree::ops::branch_belongs_to_engineer(branch, engineer)
        {
            continue;
        }
        let range = format!("{task_branch}..{branch}");
        let mut log = crate::git::cmd(repo).args([
            "log",
            "--reverse",
            "--no-merges",
            "--format=%H\t%ct\t%s",
            &range,
        ]);
        if trunk_exists {
            log = log.args(["--not", trunk]);
        }
        let output = log.run()?.stdout_str();
        for line in output.lines() {
            let mut fields = line.splitn(3, '\t');
            let (Some(hash), Some(time), Some(subject)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let Ok(time) = time.parse::<i64>() else {
                continue;
            };
            if time < fork_time
                || subject.contains("[batty]")
                || found.iter().any(|(_, seen)| seen == hash)
            {
                continue;
            }
            found.push((time, hash.to_string()));
        }
    }
    // Stable: commits from one branch keep their order when timestamps tie.
    found.sort_by_key(|(time, _)| *time);
    Ok(found.into_iter().map(|(_, hash)| hash).collect())
}

/// Cherry-pick `commits` (oldest first) onto the branch checked out in
/// `worktree`. `-x` records the source commit, so each pick is a new commit
/// even when it would otherwise be identical to a commit already on the
/// engineer's base branch. On failure the caller must abort the operation.
pub fn cherry_pick_commits(worktree: &Path, commits: &[String]) -> Result<()> {
    if commits.is_empty() {
        return Ok(());
    }
    crate::git::cmd(worktree)
        .args(["cherry-pick", "-x"])
        .args(commits)
        .run()
        .with_context(|| format!("cherry-pick of {} commit(s) failed", commits.len()))?;
    Ok(())
}

/// The verification half of the bar. Only a real passing run counts: a run
/// skipped by `auto_run_tests=false` reports `passed: true` but proves
/// nothing, and a timed-out run is recorded as failed.
pub fn verification_meets_bar(run: &VerificationRunResult, auto_run_tests: bool) -> bool {
    auto_run_tests && run.passed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::{git_ok, init_git_repo};
    use std::path::PathBuf;

    fn ladder() -> Vec<String> {
        ["low", "medium", "high", "xhigh", "max"]
            .map(String::from)
            .to_vec()
    }

    fn run_result(passed: bool) -> VerificationRunResult {
        VerificationRunResult {
            passed,
            output: String::new(),
            results: crate::team::test_results::TestResults {
                framework: "cargo".to_string(),
                total: None,
                passed: 0,
                failed: 0,
                ignored: 0,
                failures: Vec::new(),
                summary: None,
            },
            failures: Vec::new(),
            file_paths: Vec::new(),
            recovery: None,
        }
    }

    #[test]
    fn next_effort_steps_up_one_rung() {
        assert_eq!(
            next_effort(&ladder(), Some("medium")).as_deref(),
            Some("high")
        );
        assert_eq!(
            next_effort(&ladder(), Some("low")).as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn next_effort_none_starts_from_medium() {
        assert_eq!(next_effort(&ladder(), None).as_deref(), Some("high"));
    }

    #[test]
    fn next_effort_caps_at_top() {
        assert_eq!(next_effort(&ladder(), Some("max")).as_deref(), Some("max"));
    }

    #[test]
    fn next_effort_unknown_or_empty_ladder_is_none() {
        assert_eq!(next_effort(&ladder(), Some("bogus")), None);
        assert_eq!(next_effort(&[], Some("low")), None);
        assert_eq!(next_effort(&[], None), None);
    }

    #[test]
    fn next_effort_none_with_ladder_lacking_medium_is_none() {
        let ladder = vec!["low".to_string(), "high".to_string()];
        assert_eq!(next_effort(&ladder, None), None);
    }

    #[test]
    fn bar_commits_truth_table() {
        let met = |ahead, attributed| {
            bar_commits_met(&BarEvidence {
                commits_ahead_of_trunk: ahead,
                attributed_commits: attributed,
            })
        };
        assert!(!met(0, 0));
        assert!(!met(1, 0));
        assert!(!met(0, 1));
        assert!(met(1, 1));
    }

    #[test]
    fn attributed_commits_exclude_batty_auto_saves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "bench-recovery-attr");
        git_ok(&repo, &["branch", "eng-main/eng-1"]);
        git_ok(&repo, &["checkout", "-b", "eng-1/42"]);
        std::fs::write(repo.join("real.txt"), "real\n").unwrap();
        git_ok(&repo, &["add", "real.txt"]);
        git_ok(&repo, &["commit", "-m", "implement the thing"]);
        std::fs::write(repo.join("wip.txt"), "wip\n").unwrap();
        git_ok(&repo, &["add", "wip.txt"]);
        git_ok(
            &repo,
            &["commit", "-m", "wip: auto-save before restart [batty]"],
        );

        let count = attributed_commit_count(&repo, "eng-main/eng-1", "eng-1/42").unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn attributed_commits_zero_when_only_auto_saves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "bench-recovery-zero");
        git_ok(&repo, &["branch", "eng-main/eng-1"]);
        git_ok(&repo, &["checkout", "-b", "eng-1/43"]);
        std::fs::write(repo.join("wip.txt"), "wip\n").unwrap();
        git_ok(&repo, &["add", "wip.txt"]);
        git_ok(&repo, &["commit", "-m", "wip: auto-save [batty]"]);

        assert_eq!(
            attributed_commit_count(&repo, "eng-main/eng-1", "eng-1/43").unwrap(),
            0
        );
    }

    #[test]
    fn attributed_commits_errors_on_unknown_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = init_git_repo(&tmp, "bench-recovery-err");
        assert!(attributed_commit_count(&repo, "no-such-base", "no-such-branch").is_err());
    }

    #[test]
    fn verification_bar_requires_real_passing_run() {
        assert!(verification_meets_bar(&run_result(true), true));
        assert!(!verification_meets_bar(&run_result(false), true));
        // Skipped by policy: reports passed=true but must not count.
        assert!(!verification_meets_bar(&run_result(true), false));
        // Timed out runs are recorded as failed.
        // Shape mirrors verify_job::timed_out_verification_run.
        let mut timed_out = run_result(false);
        timed_out.output = "verification timed out after 30s".to_string();
        timed_out.results.framework = "timeout".to_string();
        timed_out.results.failed = 1;
        timed_out.failures = vec![timed_out.output.clone()];
        assert!(!verification_meets_bar(&timed_out, true));
        assert!(!verification_meets_bar(&timed_out, false));
    }

    #[test]
    fn frontmatter_round_trip_keeps_unrelated_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("001-task.md");
        std::fs::write(
            &path,
            "---\nid: 1\ntitle: task\nstatus: todo\npriority: high\n---\n\nBody text.\n",
        )
        .unwrap();
        assert_eq!(
            read_bench_recovery(&path),
            BenchRecoveryFrontmatter::default()
        );

        let state = BenchRecoveryFrontmatter {
            bench_reset_count: 1,
            avoid_engineers: vec!["eng-1".to_string()],
            effort_override: Some("high".to_string()),
            bench_reset_capped: false,
        };
        write_bench_recovery(&path, &state).unwrap();

        assert_eq!(read_bench_recovery(&path), state);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("priority: high"), "{content}");
        assert!(content.contains("title: task"), "{content}");
        assert!(content.contains("Body text."), "{content}");
        assert!(!content.contains("bench_reset_capped"), "{content}");
    }

    fn commit_file(repo: &Path, file: &str, message: &str) {
        std::fs::write(repo.join(file), format!("{file}\n")).unwrap();
        git_ok(repo, &["add", file]);
        git_ok(repo, &["commit", "-m", message]);
    }

    /// eng-main/eng-1 and task branch eng-1/42 at one commit, task branch checked out.
    fn rescue_fixture(tmp: &tempfile::TempDir) -> PathBuf {
        let repo = init_git_repo(tmp, "bench-recovery-rescue");
        git_ok(&repo, &["branch", "eng-main/eng-1"]);
        git_ok(&repo, &["checkout", "-b", "eng-1/42"]);
        repo
    }

    fn rescuable(repo: &Path) -> Vec<String> {
        find_rescuable_commits(repo, "eng-1", "eng-1/42", "eng-main/eng-1", "main").unwrap()
    }

    #[test]
    fn rescuable_commits_found_on_engineer_base_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = rescue_fixture(&tmp);
        git_ok(&repo, &["checkout", "eng-main/eng-1"]);
        commit_file(&repo, "a.txt", "feat: a");
        commit_file(&repo, "b.txt", "feat: b");
        git_ok(&repo, &["checkout", "eng-1/42"]);

        let commits = rescuable(&repo);
        assert_eq!(commits.len(), 2);

        cherry_pick_commits(&repo, &commits).unwrap();
        assert_eq!(
            attributed_commit_count(&repo, "eng-main/eng-1", "eng-1/42").unwrap(),
            2
        );
    }

    #[test]
    fn rescuable_commits_skip_other_engineers_and_auto_saves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = rescue_fixture(&tmp);
        git_ok(&repo, &["checkout", "-b", "eng-main/eng-2", "main"]);
        commit_file(&repo, "other.txt", "feat: other engineer");
        git_ok(&repo, &["checkout", "-b", "eng-2/7", "main"]);
        commit_file(&repo, "other2.txt", "feat: other task");
        git_ok(&repo, &["checkout", "eng-main/eng-1"]);
        commit_file(&repo, "wip.txt", "wip: auto-save [batty]");
        git_ok(&repo, &["checkout", "eng-1/42"]);

        assert!(rescuable(&repo).is_empty());
    }

    #[test]
    fn rescuable_commits_skip_commits_older_than_the_fork_point() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = rescue_fixture(&tmp);
        git_ok(&repo, &["checkout", "-b", "eng-1/old", "main"]);
        std::fs::write(repo.join("old.txt"), "old\n").unwrap();
        git_ok(&repo, &["add", "old.txt"]);
        crate::git::cmd(&repo)
            .args(["commit", "-m", "feat: old work"])
            .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00Z")
            .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00Z")
            .run()
            .unwrap();
        git_ok(&repo, &["checkout", "eng-1/42"]);

        assert!(rescuable(&repo).is_empty());
    }

    #[test]
    fn rescuable_commits_include_engineers_stray_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = rescue_fixture(&tmp);
        git_ok(&repo, &["checkout", "-b", "eng-1/scratch"]);
        commit_file(&repo, "s.txt", "feat: stray");
        git_ok(&repo, &["checkout", "eng-1/42"]);

        assert_eq!(rescuable(&repo).len(), 1);
    }

    #[test]
    fn cherry_pick_commits_conflict_errors_and_can_be_aborted() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = rescue_fixture(&tmp);
        commit_file(&repo, "clash.txt", "feat: task side");
        git_ok(&repo, &["checkout", "eng-main/eng-1"]);
        std::fs::write(repo.join("clash.txt"), "base side\n").unwrap();
        git_ok(&repo, &["add", "clash.txt"]);
        git_ok(&repo, &["commit", "-m", "feat: base side"]);
        git_ok(&repo, &["checkout", "eng-1/42"]);

        let commits = rescuable(&repo);
        assert_eq!(commits.len(), 1);
        assert!(cherry_pick_commits(&repo, &commits).is_err());
        crate::worktree::ops::abort_in_progress_git_operations(&repo);
        assert!(!repo.join(".git").join("CHERRY_PICK_HEAD").exists());
    }

    #[test]
    fn cherry_pick_commits_empty_list_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = rescue_fixture(&tmp);
        cherry_pick_commits(&repo, &[]).unwrap();
    }

    #[test]
    fn read_bench_recovery_missing_file_is_default() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            read_bench_recovery(&tmp.path().join("missing.md")),
            BenchRecoveryFrontmatter::default()
        );
    }
}
