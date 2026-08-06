mod app;
mod codex;
mod git_workspace;
mod registry;
mod repository;
mod ui;

use std::path::PathBuf;

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
    #[command(hide = true)]
    CaptureThread {
        repository: PathBuf,
        payload: String,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// List repositories registered with wyard
    List,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => ui::run(App::load()?)?,
        Some(Command::Repo { command }) => run_repo_command(command)?,
        Some(Command::CaptureThread {
            repository,
            payload,
        }) => registry::capture_notification(&repository, &payload)?,
    }

    Ok(())
}

fn run_repo_command(command: RepoCommand) -> Result<()> {
    match command {
        RepoCommand::List => {
            for repository in repository::RepositoryStore::discover()?.load_registered()? {
                println!("{}\t{}", repository.name, repository.path.display());
            }
        }
    }

    Ok(())
}
