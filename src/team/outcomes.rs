//! Explicit acceptance evidence for code and non-code deliverables, separate from merging.
use super::{config::TeamConfig, team_config_path};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;

fn database(root: &Path) -> Result<Connection> {
    std::fs::create_dir_all(root.join(".batty"))?;
    let connection = Connection::open(root.join(".batty/outcomes.db"))?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS accepted_outcomes (
        id INTEGER PRIMARY KEY, task_id INTEGER NOT NULL, accepted_at INTEGER NOT NULL,
        evidence_sha256 TEXT NOT NULL, commit_sha TEXT NOT NULL, session_id TEXT,
        defects INTEGER NOT NULL, UNIQUE(task_id,evidence_sha256,commit_sha)
    );",
    )?;
    Ok(connection)
}

pub fn record(
    root: &Path,
    task_id: u32,
    evidence: &Path,
    commit: Option<&str>,
    session: Option<&str>,
    defects: u32,
) -> Result<()> {
    let tasks = crate::task::load_tasks_from_dir(&root.join(".batty/team_config/board/tasks"))?;
    if !tasks.iter().any(|t| t.id == task_id) {
        bail!("unknown task #{task_id}");
    }
    let evidence_sha256 = format!(
        "{:x}",
        Sha256::digest(
            std::fs::read(evidence).context("acceptance evidence must be a readable file")?
        )
    );
    let mut commit_sha = String::new();
    if let Some(commit) = commit {
        let config = TeamConfig::load(&team_config_path(root))?;
        let output = crate::git::cmd(root)
            .args([
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{commit}^{{commit}}"),
            ])
            .allow_fail()
            .run()?;
        if !output.status.success() {
            bail!("commit evidence is not a valid commit");
        }
        commit_sha = String::from_utf8(output.stdout)?.trim().into();
        if !crate::git::cmd(root)
            .args([
                "merge-base",
                "--is-ancestor",
                &commit_sha,
                config.trunk_branch(),
            ])
            .allow_fail()
            .run()
            .map(|output| output.status)?
            .success()
        {
            bail!("commit evidence is not integrated into the configured trunk");
        }
    }
    let connection = database(root)?;
    connection.execute("INSERT OR IGNORE INTO accepted_outcomes(task_id,accepted_at,evidence_sha256,commit_sha,session_id,defects) VALUES(?1,?2,?3,?4,?5,?6)",
        params![task_id, chrono::Utc::now().timestamp(), evidence_sha256, commit_sha, session, defects])?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub since: i64,
    pub until: i64,
    pub accepted_tasks: u64,
    pub reported_defects: u64,
    pub linked_sessions: u64,
    pub tokens_per_accepted_task: Option<f64>,
    pub usage_note: &'static str,
}

pub fn report(root: &Path, since: i64, until: i64) -> Result<Summary> {
    if since > until {
        bail!("since must be no later than until");
    }
    let connection = database(root)?;
    let (accepted_tasks, reported_defects, linked_sessions): (i64, i64, i64) = connection.query_row(
        "SELECT count(*),coalesce(sum(defects),0),count(DISTINCT session_id) FROM accepted_outcomes WHERE id IN
        (SELECT max(id) FROM accepted_outcomes WHERE accepted_at BETWEEN ?1 AND ?2 GROUP BY task_id)", params![since,until],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    Ok(Summary {
        since,
        until,
        accepted_tasks: accepted_tasks.try_into()?,
        reported_defects: reported_defects.try_into()?,
        linked_sessions: linked_sessions.try_into()?,
        tokens_per_accepted_task: None,
        usage_note: "Acceptance is explicit; an exact run/session usage window is required before reporting tokens per accepted task.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn non_code_acceptance_needs_no_commit_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".batty/team_config/board/tasks")).unwrap();
        std::fs::write(
            root.join(".batty/team_config/board/tasks/001-review.md"),
            "---\nid: 1\ntitle: Review findings\nstatus: review\npriority: medium\n---\n",
        )
        .unwrap();
        let evidence = root.join("findings.txt");
        std::fs::write(&evidence, "Verified external evidence").unwrap();
        record(root, 1, &evidence, None, Some("session-a"), 2).unwrap();
        record(root, 1, &evidence, None, Some("session-a"), 2).unwrap();
        let report = report(root, 0, chrono::Utc::now().timestamp()).unwrap();
        assert_eq!(report.accepted_tasks, 1);
        assert_eq!(report.reported_defects, 2);
        assert_eq!(report.linked_sessions, 1);
        assert!(report.tokens_per_accepted_task.is_none());
        assert!(super::report(root, 10, 0).is_err());
        assert!(record(root, 2, &evidence, None, None, 0).is_err());
    }
}
