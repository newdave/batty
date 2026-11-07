use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::config::RoleType;
use super::hierarchy::resolve_hierarchy;
use super::{team_config_dir, team_config_path};

const BENCH_FILE_NAME: &str = "bench.yaml";
const BENCH_LOCK_STALE_SECS: u64 = 30;
const BENCH_LOCK_TIMEOUT_MS: u64 = 2_000;
const BENCH_LOCK_RETRY_MS: u64 = 10;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchState {
    #[serde(default)]
    pub benched: BTreeMap<String, BenchEntry>,
}

/// Who benched an engineer. Automation must never touch `Manual` entries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchSource {
    #[default]
    Manual,
    AutoPark,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchEntry {
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub source: BenchSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<u32>,
}

pub fn bench_file_path(project_root: &Path) -> PathBuf {
    team_config_dir(project_root).join(BENCH_FILE_NAME)
}

pub fn load_bench_state(project_root: &Path) -> Result<BenchState> {
    load_bench_state_from_path(&bench_file_path(project_root))
}

pub fn benched_engineer_names(project_root: &Path) -> Result<BTreeSet<String>> {
    Ok(load_bench_state(project_root)?
        .benched
        .keys()
        .cloned()
        .collect())
}

/// Load every bench entry, including provenance.
pub fn bench_entries(project_root: &Path) -> Result<BTreeMap<String, BenchEntry>> {
    Ok(load_bench_state(project_root)?.benched)
}

pub fn bench_engineer(
    project_root: &Path,
    engineer: &str,
    reason: Option<&str>,
) -> Result<BenchEntry> {
    bench_engineer_with(project_root, engineer, reason, BenchSource::Manual, None)
}

pub fn bench_engineer_with(
    project_root: &Path,
    engineer: &str,
    reason: Option<&str>,
    source: BenchSource,
    task_id: Option<u32>,
) -> Result<BenchEntry> {
    validate_engineer(project_root, engineer)?;
    with_bench_lock(project_root, || {
        let path = bench_file_path(project_root);
        let mut state = load_bench_state_from_path(&path)?;
        let entry = BenchEntry {
            timestamp: Utc::now().to_rfc3339(),
            reason: normalize_reason(reason),
            source,
            task_id,
        };
        state.benched.insert(engineer.to_string(), entry.clone());
        write_bench_state(&path, &state)?;
        Ok(entry)
    })
}

pub fn unbench_engineer(project_root: &Path, engineer: &str) -> Result<bool> {
    validate_engineer(project_root, engineer)?;
    with_bench_lock(project_root, || {
        let path = bench_file_path(project_root);
        let mut state = load_bench_state_from_path(&path)?;
        let removed = state.benched.remove(engineer).is_some();
        write_bench_state(&path, &state)?;
        Ok(removed)
    })
}

pub fn format_benched_engineers_section(state: &BenchState) -> Option<String> {
    if state.benched.is_empty() {
        return None;
    }

    let mut lines = vec![
        "Benched Engineers".to_string(),
        format!("{:<20} {:<26} {}", "ENGINEER", "SINCE", "REASON"),
    ];
    for (engineer, entry) in &state.benched {
        let reason = entry.reason.as_deref().unwrap_or("-");
        let reason = match (entry.source, entry.task_id) {
            (BenchSource::AutoPark, Some(task_id)) => {
                format!("{reason} (auto, task #{task_id})")
            }
            (BenchSource::AutoPark, None) => format!("{reason} (auto)"),
            (BenchSource::Manual, _) => reason.to_string(),
        };
        lines.push(format!(
            "{:<20} {:<26} {}",
            engineer, entry.timestamp, reason,
        ));
    }

    Some(lines.join("\n"))
}

fn normalize_reason(reason: Option<&str>) -> Option<String> {
    reason
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .map(str::to_string)
}

fn validate_engineer(project_root: &Path, engineer: &str) -> Result<()> {
    let config = super::config::TeamConfig::load(&team_config_path(project_root))?;
    let members = resolve_hierarchy(&config)?;
    if members
        .iter()
        .any(|member| member.name == engineer && member.role_type == RoleType::Engineer)
    {
        Ok(())
    } else {
        bail!("unknown engineer '{engineer}'");
    }
}

fn load_bench_state_from_path(path: &Path) -> Result<BenchState> {
    if !path.exists() {
        return Ok(BenchState::default());
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(BenchState::default());
    }
    serde_yaml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_bench_state(path: &Path, state: &BenchState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let yaml = serde_yaml::to_string(state).context("failed to serialize bench state")?;
    let temp_path = path.with_extension(format!(
        "yaml.tmp-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    std::fs::write(&temp_path, yaml)
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    std::fs::rename(&temp_path, path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn with_bench_lock<T>(project_root: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _guard = BenchLockGuard::acquire(project_root)?;
    operation()
}

struct BenchLockGuard {
    path: PathBuf,
}

impl BenchLockGuard {
    fn acquire(project_root: &Path) -> Result<Self> {
        let path = bench_file_path(project_root).with_extension("yaml.lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let started = Instant::now();
        loop {
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path) {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if started.elapsed() >= Duration::from_millis(BENCH_LOCK_TIMEOUT_MS) {
                        bail!("timed out waiting for bench state lock");
                    }
                    thread::sleep(Duration::from_millis(BENCH_LOCK_RETRY_MS));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to acquire {}", path.display()));
                }
            }
        }
    }
}

impl Drop for BenchLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn lock_is_stale(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age.as_secs() >= BENCH_LOCK_STALE_SECS
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    fn write_team_config(root: &Path) {
        let team_dir = root.join(".batty").join("team_config");
        std::fs::create_dir_all(&team_dir).unwrap();
        std::fs::write(
            team_dir.join("team.yaml"),
            r#"
name: test
agent: codex
roles:
  - name: architect
    role_type: architect
  - name: manager
    role_type: manager
  - name: engineer
    role_type: engineer
    instances: 2
"#,
        )
        .unwrap();
    }

    #[test]
    fn bench_engineer_persists_reason_and_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(tmp.path());

        let entry = bench_engineer(tmp.path(), "eng-1-1", Some("session end")).unwrap();
        let state = load_bench_state(tmp.path()).unwrap();

        assert_eq!(state.benched.get("eng-1-1"), Some(&entry));
        assert_eq!(entry.reason.as_deref(), Some("session end"));
        assert!(!entry.timestamp.is_empty());
    }

    #[test]
    fn unbench_engineer_removes_entry() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(tmp.path());

        bench_engineer(tmp.path(), "eng-1-1", Some("pause")).unwrap();
        assert!(unbench_engineer(tmp.path(), "eng-1-1").unwrap());
        assert!(
            !load_bench_state(tmp.path())
                .unwrap()
                .benched
                .contains_key("eng-1-1")
        );
    }

    #[test]
    fn bench_engineer_rejects_unknown_engineer() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(tmp.path());

        let error = bench_engineer(tmp.path(), "eng-9", Some("pause")).unwrap_err();
        assert!(error.to_string().contains("unknown engineer 'eng-9'"));
    }

    #[test]
    fn format_section_includes_timestamp_and_reason() {
        let mut state = BenchState::default();
        state.benched.insert(
            "eng-1-1".to_string(),
            BenchEntry {
                timestamp: "2026-04-10T10:00:00Z".to_string(),
                reason: Some("session end".to_string()),
                source: BenchSource::Manual,
                task_id: None,
            },
        );

        let formatted = format_benched_engineers_section(&state).unwrap();
        assert!(formatted.contains("Benched Engineers"));
        assert!(formatted.contains("eng-1-1"));
        assert!(formatted.contains("2026-04-10T10:00:00Z"));
        assert!(formatted.contains("session end"));
    }

    #[test]
    fn legacy_entry_without_source_loads_as_manual() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(tmp.path());
        std::fs::write(
            bench_file_path(tmp.path()),
            "benched:\n  eng-1-1:\n    timestamp: \"2026-04-10T10:00:00Z\"\n    reason: old\n",
        )
        .unwrap();

        let entries = bench_entries(tmp.path()).unwrap();
        let entry = entries.get("eng-1-1").unwrap();
        assert_eq!(entry.source, BenchSource::Manual);
        assert_eq!(entry.task_id, None);
    }

    #[test]
    fn bench_engineer_with_persists_source_and_task_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(tmp.path());

        let entry = bench_engineer_with(
            tmp.path(),
            "eng-1-1",
            Some("zero diff"),
            BenchSource::AutoPark,
            Some(42),
        )
        .unwrap();
        assert_eq!(entry.source, BenchSource::AutoPark);

        let raw = std::fs::read_to_string(bench_file_path(tmp.path())).unwrap();
        assert!(raw.contains("source: auto_park"), "{raw}");
        assert!(raw.contains("task_id: 42"), "{raw}");

        let state = load_bench_state(tmp.path()).unwrap();
        let formatted = format_benched_engineers_section(&state).unwrap();
        assert!(formatted.contains("(auto, task #42)"), "{formatted}");
    }

    #[test]
    fn bench_engineer_wrapper_writes_manual_without_task_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(tmp.path());

        let entry = bench_engineer(tmp.path(), "eng-1-1", None).unwrap();
        assert_eq!(entry.source, BenchSource::Manual);
        assert_eq!(entry.task_id, None);

        let raw = std::fs::read_to_string(bench_file_path(tmp.path())).unwrap();
        assert!(raw.contains("source: manual"), "{raw}");
        assert!(!raw.contains("task_id"), "{raw}");
        let formatted =
            format_benched_engineers_section(&load_bench_state(tmp.path()).unwrap()).unwrap();
        assert!(!formatted.contains("(auto"));
    }

    #[test]
    fn concurrent_bench_and_unbench_preserve_both_updates() {
        let tmp = tempfile::tempdir().unwrap();
        write_team_config(tmp.path());
        bench_engineer(tmp.path(), "eng-1-1", Some("existing")).unwrap();

        let barrier = Arc::new(Barrier::new(3));
        let root_a = tmp.path().to_path_buf();
        let barrier_a = Arc::clone(&barrier);
        let add = std::thread::spawn(move || {
            barrier_a.wait();
            for _ in 0..50 {
                bench_engineer(&root_a, "eng-1-2", Some("new")).unwrap();
            }
        });
        let root_b = tmp.path().to_path_buf();
        let barrier_b = Arc::clone(&barrier);
        let remove = std::thread::spawn(move || {
            barrier_b.wait();
            for _ in 0..50 {
                unbench_engineer(&root_b, "eng-1-1").unwrap();
            }
        });

        barrier.wait();
        add.join().unwrap();
        remove.join().unwrap();
        let state = load_bench_state(tmp.path()).unwrap();
        assert!(!state.benched.contains_key("eng-1-1"));
        assert_eq!(
            state
                .benched
                .get("eng-1-2")
                .and_then(|entry| entry.reason.as_deref()),
            Some("new")
        );
    }
}
