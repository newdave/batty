use anyhow::Result;
use batty_cli::{cli::Cli, env_file};
use clap::Parser;
use std::path::PathBuf;
use tracing::debug;

/// Resolve the project root directory.
///
/// If running inside a git worktree, resolves to the main repository root
/// so that all `.batty/` operations (inboxes, team config, kanban board,
/// daemon PID, events) use the shared project directory.
fn project_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Use git to find the main repo root (handles worktrees)
    if let Ok(output) = batty_cli::git::cmd(&cwd)
        .args(["rev-parse", "--git-common-dir"])
        .allow_fail()
        .run()
    {
        if output.status.success() {
            let git_common = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let git_path = if std::path::Path::new(&git_common).is_absolute() {
                PathBuf::from(&git_common)
            } else {
                cwd.join(&git_common)
            };
            // .git dir's parent is the repo root
            if let Some(repo_root) = git_path.parent() {
                if let Ok(canonical) = repo_root.canonicalize() {
                    return canonical;
                }
            }
        }
    }

    cwd
}

fn setup_tracing(verbose: u8) {
    let filter = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    // Prefer `BATTY_LOG` (batty-native, discoverable), fall back to
    // `RUST_LOG` (standard rust convention), else CLI verbosity.
    let env_filter = match std::env::var("BATTY_LOG") {
        Ok(value) if !value.trim().is_empty() => tracing_subscriber::EnvFilter::new(value),
        _ => tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(filter)),
    };

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    setup_tracing(cli.verbose);

    let root = project_root();
    env_file::load_project_env(&root)?;
    debug!(root = %root.display(), "project root");

    batty_cli::commands::dispatch(&root, cli.command)
}
