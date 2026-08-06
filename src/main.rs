mod app;
mod codex;
mod config;
mod jj;
mod ui;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::{app::App, config::ConfigStore};

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
    /// Register a JJ repository
    Add {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    /// List registered repositories
    List,
    /// Remove a repository from wyard (files are untouched)
    Remove { name: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let store = ConfigStore::discover()?;

    match cli.command {
        None => ui::run(App::load(store)?)?,
        Some(Command::Repo { command }) => run_repo_command(store, command)?,
    }

    Ok(())
}

fn run_repo_command(store: ConfigStore, command: RepoCommand) -> Result<()> {
    let mut config = store.load()?;

    match command {
        RepoCommand::Add { path, name } => {
            let repository = config::Repository::from_path(path, name)?;
            jj::list_workspaces(&repository.path)?;
            config.add_repository(repository.clone())?;
            store.save(&config)?;
            println!(
                "registered {} ({})",
                repository.name,
                repository.path.display()
            );
        }
        RepoCommand::List => {
            for repository in &config.repositories {
                println!("{}\t{}", repository.name, repository.path.display());
            }
        }
        RepoCommand::Remove { name } => {
            config.remove_repository(&name)?;
            store.save(&config)?;
            println!("removed {name} from wyard");
        }
    }

    Ok(())
}
