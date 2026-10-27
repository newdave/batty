//! Binary hot-reload: fingerprint tracking, marker files, and exec-based restart.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

#[cfg(not(unix))]
use anyhow::bail;
use anyhow::{Context, Result};
use tracing::warn;

use super::{TeamDaemon, now_unix};

pub(super) const HOT_RELOAD_CHECK_INTERVAL: Duration = Duration::from_secs(30);
pub(super) const HOT_RELOAD_MIN_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BinaryFingerprint {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub len: u64,
    #[cfg(unix)]
    pub inode: u64,
}

impl BinaryFingerprint {
    /// Stable identity of this exact binary build, for warning dedupe.
    pub fn dedupe_key(&self) -> String {
        #[cfg(unix)]
        let inode = self.inode;
        #[cfg(not(unix))]
        let inode = 0;
        format!(
            "{}:{:?}:{}:{}",
            self.path.display(),
            self.modified,
            self.len,
            inode
        )
    }

    pub fn capture(path: &Path) -> Result<Self> {
        let metadata =
            fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
        let modified = metadata
            .modified()
            .with_context(|| format!("failed to read mtime for {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            modified,
            len: metadata.len(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(&metadata),
        })
    }

    pub fn changed_from(&self, previous: &Self) -> bool {
        self.modified != previous.modified || self.len != previous.len || {
            #[cfg(unix)]
            {
                self.inode != previous.inode
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct HotReloadMonitor {
    binary: BinaryFingerprint,
    last_checked: Instant,
    last_reload_attempt: Option<Instant>,
}

impl HotReloadMonitor {
    pub fn new(binary: BinaryFingerprint) -> Self {
        Self {
            binary,
            last_checked: Instant::now(),
            last_reload_attempt: None,
        }
    }

    pub fn for_current_exe() -> Result<Self> {
        let path = std::env::current_exe().context("failed to resolve current executable")?;
        Ok(Self::new(BinaryFingerprint::capture(&path)?))
    }

    pub fn should_check(&self) -> bool {
        self.last_checked.elapsed() >= HOT_RELOAD_CHECK_INTERVAL
    }

    pub fn changed_binary(&mut self) -> Result<Option<BinaryFingerprint>> {
        self.last_checked = Instant::now();
        let current = BinaryFingerprint::capture(&self.binary.path)?;
        Ok(current.changed_from(&self.binary).then_some(current))
    }

    pub fn can_attempt_reload(&self) -> bool {
        self.last_reload_attempt
            .map(|instant| instant.elapsed() >= HOT_RELOAD_MIN_INTERVAL)
            .unwrap_or(true)
    }

    pub fn mark_reload_attempt(&mut self) {
        self.last_reload_attempt = Some(Instant::now());
    }
}

impl TeamDaemon {
    /// True the first time a warning of `kind` is raised for this binary
    /// fingerprint, so a binary that stays unreloadable does not warn on
    /// every check.
    fn warn_hot_reload_once(&mut self, kind: &str, binary: &BinaryFingerprint) -> bool {
        self.deferred
            .hot_reload_warned
            .insert(format!("{kind}:{}", binary.dedupe_key()))
    }

    pub(super) fn maybe_hot_reload_binary(
        &mut self,
        monitor: Option<&mut HotReloadMonitor>,
    ) -> Result<()> {
        let Some(monitor) = monitor else {
            return Ok(());
        };
        if !monitor.should_check() {
            return Ok(());
        }

        let Some(updated_binary) = monitor.changed_binary()? else {
            return Ok(());
        };

        if !monitor.can_attempt_reload() {
            if self.warn_hot_reload_once("rate-limited", &updated_binary) {
                warn!(
                    path = %updated_binary.path.display(),
                    "binary changed again but reload attempt is rate-limited"
                );
            }
            return Ok(());
        }

        if !binary_is_reloadable(&updated_binary.path) {
            if self.warn_hot_reload_once("not-reloadable", &updated_binary) {
                warn!(
                    path = %updated_binary.path.display(),
                    "binary changed but is not safe to hot-reload yet"
                );
            }
            return Ok(());
        }

        monitor.mark_reload_attempt();
        // Persist state and the reload marker before touching anything
        // irreversible: if either fails the reload aborts with running tests
        // intact.
        self.persist_runtime_state(false)?;
        self.record_daemon_reloading();
        self.record_orchestrator_action(format!(
            "runtime: daemon reloading after binary change ({})",
            updated_binary.path.display()
        ));
        write_hot_reload_marker(&self.config.project_root)?;

        // exec would orphan running test commands; kill them and persist the
        // interrupted verifications so the new binary re-runs them.
        self.stop_background_tests();
        if let Err(error) = self.persist_runtime_state(false) {
            warn!(
                error = %error,
                "failed to persist interrupted verifications before hot reload"
            );
        }

        if let Err(error) = exec_reloaded_daemon(&updated_binary.path, &self.config.project_root) {
            let _ = clear_hot_reload_marker(&self.config.project_root);
            warn!(
                path = %updated_binary.path.display(),
                error = %error,
                "failed to exec updated daemon binary; continuing on existing process"
            );
            self.record_orchestrator_action(format!("runtime: daemon reload failed ({error})"));
        }

        Ok(())
    }
}

pub(super) fn hot_reload_marker_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("reload")
}

pub(super) fn write_hot_reload_marker(project_root: &Path) -> Result<()> {
    let path = hot_reload_marker_path(project_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&path, now_unix().to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub(super) fn clear_hot_reload_marker(project_root: &Path) -> Result<()> {
    let path = hot_reload_marker_path(project_root);
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(&path).with_context(|| format!("failed to remove {}", path.display()))?;
    Ok(())
}

pub(super) fn consume_hot_reload_marker(project_root: &Path) -> bool {
    let path = hot_reload_marker_path(project_root);
    if !path.exists() {
        return false;
    }
    clear_hot_reload_marker(project_root).is_ok()
}

pub(super) fn hot_reload_daemon_args(project_root: &Path) -> Vec<String> {
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .to_string();
    vec![
        "-v".to_string(),
        "daemon".to_string(),
        "--project-root".to_string(),
        root,
        "--resume".to_string(),
    ]
}

pub(super) fn binary_is_reloadable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return false;
        }
    }

    #[cfg(target_os = "macos")]
    {
        // On macOS, `cp` preserves the embedded codesign bytes (so
        // `codesign --verify` passes), but the kernel's amfi cache keys on
        // inode identity — a replaced binary at the same path is SIGKILLed
        // on exec even though the signature is valid. Refreshing the adhoc
        // signature with `codesign --force --sign -` invalidates the cache
        // and restores exec trust. Do it once per reload attempt before
        // the verify check so a freshly-copied binary won't crash the
        // daemon post-exec.
        let resigned = std::process::Command::new("codesign")
            .args(["--force", "--sign", "-", path.to_string_lossy().as_ref()])
            .status();
        if !matches!(resigned, Ok(status) if status.success()) {
            warn!(
                path = %path.display(),
                "codesign adhoc re-sign failed before hot-reload verify; exec may SIGKILL"
            );
        }

        let Ok(status) = std::process::Command::new("codesign")
            .args(["--verify", path.to_string_lossy().as_ref()])
            .status()
        else {
            return false;
        };
        if !status.success() {
            return false;
        }
    }

    true
}

#[cfg(unix)]
pub(super) fn exec_reloaded_daemon(executable: &Path, project_root: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let error = std::process::Command::new(executable)
        .args(hot_reload_daemon_args(project_root))
        .exec();
    Err(anyhow::Error::new(error).context(format!("failed to exec {}", executable.display())))
}

#[cfg(not(unix))]
pub(super) fn exec_reloaded_daemon(_executable: &Path, _project_root: &Path) -> Result<()> {
    bail!("daemon hot reload via exec is only supported on unix")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::test_support::TestDaemonBuilder;

    #[test]
    fn warn_hot_reload_once_dedupes_per_kind_and_fingerprint() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("batty");
        fs::write(&binary, "build-a").unwrap();
        let fingerprint = BinaryFingerprint::capture(&binary).unwrap();
        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();

        assert!(daemon.warn_hot_reload_once("rate-limited", &fingerprint));
        assert!(!daemon.warn_hot_reload_once("rate-limited", &fingerprint));
        assert!(daemon.warn_hot_reload_once("not-reloadable", &fingerprint));

        fs::write(&binary, "build-b-longer").unwrap();
        let rebuilt = BinaryFingerprint::capture(&binary).unwrap();
        assert!(daemon.warn_hot_reload_once("rate-limited", &rebuilt));
    }

    #[cfg(unix)]
    #[test]
    fn unreloadable_binary_warns_once_across_checks() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let binary = tmp.path().join("batty");
        fs::write(&binary, "old").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o644)).unwrap();
        let mut monitor = HotReloadMonitor::new(BinaryFingerprint::capture(&binary).unwrap());
        // Replace the binary (new inode/length) without exec permission.
        fs::remove_file(&binary).unwrap();
        fs::write(&binary, "new-and-longer").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o644)).unwrap();

        let mut daemon = TestDaemonBuilder::new(tmp.path()).build();
        for _ in 0..3 {
            monitor.last_checked = Instant::now() - HOT_RELOAD_CHECK_INTERVAL;
            daemon.maybe_hot_reload_binary(Some(&mut monitor)).unwrap();
        }
        assert_eq!(daemon.deferred.hot_reload_warned.len(), 1);
        assert!(!hot_reload_marker_path(tmp.path()).exists());
    }
}
