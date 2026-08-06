mod app;
pub mod app_server;
mod chat;
mod git_workspace;
mod registry;
mod repository;
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
    /// List repositories registered with wyard
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => ui::run(App::load()?).await?,
        Some(Command::Repo { command }) => run_repo_command(command)?,
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
