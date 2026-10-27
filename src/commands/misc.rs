//! Release, verification, research and shell completions.

use crate::{
    cli::{self, ResearchCommand, ResearchFormatArg, ResearchKeepPolicyArg},
    release, team,
};
use anyhow::Result;
use std::path::Path;

pub(super) fn run_completions(shell: cli::CompletionShell) -> Result<()> {
    use clap::CommandFactory;
    let shell = match shell {
        cli::CompletionShell::Bash => clap_complete::Shell::Bash,
        cli::CompletionShell::Zsh => clap_complete::Shell::Zsh,
        cli::CompletionShell::Fish => clap_complete::Shell::Fish,
    };
    clap_complete::generate(
        shell,
        &mut cli::Cli::command(),
        "batty",
        &mut std::io::stdout(),
    );
    Ok(())
}

pub(super) fn run_verify(root: &Path) -> Result<()> {
    team::verification::cmd_verify(root)?;
    Ok(())
}

pub(super) fn run_release(root: &Path, tag: Option<String>, readiness: bool) -> Result<()> {
    if readiness {
        release::cmd_release_readiness(root, tag.as_deref())?;
    } else {
        release::cmd_release(root, tag.as_deref())?;
    }
    Ok(())
}

pub(super) fn run_research(root: &Path, command: ResearchCommand) -> Result<()> {
    match command {
        ResearchCommand::Start {
            hypothesis,
            evaluator,
            format,
            keep_policy,
            max_iterations,
            worktree,
        } => {
            let worktree_dir = if worktree.is_absolute() {
                worktree
            } else {
                std::env::current_dir()?.join(worktree)
            };
            let mission = team::autoresearch::start_research(
                root,
                team::autoresearch::StartResearchOptions {
                    hypothesis,
                    evaluator_command: evaluator,
                    evaluator_format: match format {
                        ResearchFormatArg::Json => team::autoresearch::EvaluatorFormat::Json,
                        ResearchFormatArg::ExitCode => {
                            team::autoresearch::EvaluatorFormat::ExitCode
                        }
                    },
                    keep_policy: match keep_policy {
                        ResearchKeepPolicyArg::PassOnly => team::autoresearch::KeepPolicy::PassOnly,
                        ResearchKeepPolicyArg::ScoreImprovement => {
                            team::autoresearch::KeepPolicy::ScoreImprovement
                        }
                        ResearchKeepPolicyArg::ParityImprovement => {
                            team::autoresearch::KeepPolicy::ParityImprovement
                        }
                    },
                    max_iterations,
                    worktree_dir,
                },
            )?;
            println!("Started research mission {}", mission.id);
            println!("Hypothesis: {}", mission.hypothesis);
            println!("Worktree: {}", mission.worktree_dir.display());
        }
        ResearchCommand::Status => {
            team::autoresearch::print_status(root)?;
        }
        ResearchCommand::Ledger => {
            team::autoresearch::print_ledger(root)?;
        }
        ResearchCommand::Stop => match team::autoresearch::stop_current_research(root)? {
            Some(mission) => println!("Stopped research mission {}", mission.id),
            None => println!("No active research mission."),
        },
    }
    Ok(())
}
