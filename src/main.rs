mod app;
mod codex;
mod ghq;
mod git_workspace;
mod ui;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::app::App;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage registered repositories
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// List Git repositories discovered under ghq root
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => ui::run(App::load()?)?,
        Some(Command::Repo { command }) => run_repo_command(command)?,
    }

    Ok(())
}

fn run_repo_command(command: RepoCommand) -> Result<()> {
    match command {
        RepoCommand::List => {
            for repository in ghq::discover_repositories()? {
                println!("{}\t{}", repository.name, repository.path.display());
            }
        }
    }

    Ok(())
}
