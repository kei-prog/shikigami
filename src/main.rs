mod app;
pub mod app_server;
mod chat;
mod clipboard;
mod git_workspace;
mod keybindings;
mod paths;
mod registry;
mod repository;
mod settings;
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
    /// Inspect user-editable configuration
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCommand {
    /// List repositories registered with Shikigami
    List,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the keybindings configuration path
    Path,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => {
            let _instance_lock = app_server::InstanceLock::acquire()?;
            ui::run(App::load()?).await?
        }
        Some(Command::Repo { command }) => run_repo_command(command)?,
        Some(Command::Config { command }) => run_config_command(command)?,
    }

    Ok(())
}

fn run_config_command(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Path => println!("{}", keybindings::KeyBindings::config_path()?.display()),
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
