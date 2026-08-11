mod app;
pub mod app_server;
mod chat;
mod clipboard;
mod git_workspace;
mod keybindings;
mod onboarding;
mod paths;
mod performance;
mod registry;
mod repository;
mod settings;
mod ui;

use std::{
    env,
    ffi::OsString,
    io::{self, Write},
    path::Path,
    process::Command as ProcessCommand,
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use crate::app::App;

#[derive(Debug, Parser)]
#[command(version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    /// Create the keybindings config if needed and open it in an editor
    #[arg(long, conflicts_with_all = ["config_path", "reset_config"])]
    config: bool,
    /// Print the keybindings configuration path
    #[arg(long, conflicts_with_all = ["config", "reset_config"])]
    config_path: bool,
    /// Back up the current keybindings config and restore all defaults
    #[arg(long, conflicts_with_all = ["config", "config_path"])]
    reset_config: bool,
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
    /// Show locally recorded startup and interaction performance
    Perf,
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

fn main() -> Result<()> {
    let performance = performance::PerformanceSession::start();
    let result = run(Arc::clone(&performance));
    let _ = performance.save();
    result
}

fn run(performance: Arc<performance::PerformanceSession>) -> Result<()> {
    let cli_started = performance.start_timer();
    let cli = Cli::parse();
    performance.record_duration("startup.cli_parse", cli_started, "success", &[]);
    match (cli.config, cli.config_path, cli.reset_config, cli.command) {
        (true, false, false, None) => open_config()?,
        (false, true, false, None) => print_config_path()?,
        (false, false, true, None) => reset_config()?,
        (false, false, false, None) => {
            performance.mark_interactive();
            let lock_started = performance.start_timer();
            let instance_lock = app_server::InstanceLock::acquire();
            performance.record_duration(
                "startup.instance_lock",
                lock_started,
                if instance_lock.is_ok() {
                    "success"
                } else {
                    "error"
                },
                &[],
            );
            let _instance_lock = instance_lock?;
            let runtime_started = performance.start_timer();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("build Tokio runtime")?;
            performance.record_duration("startup.runtime", runtime_started, "success", &[]);
            let app_started = performance.start_timer();
            let app = App::load(Arc::clone(&performance));
            performance.record_duration(
                "startup.app_load",
                app_started,
                if app.is_ok() { "success" } else { "error" },
                &[],
            );
            runtime.block_on(ui::run(app?))?
        }
        (false, false, false, Some(Command::Repo { command })) => run_repo_command(command)?,
        (false, false, false, Some(Command::Config { command })) => run_config_command(command)?,
        (false, false, false, Some(Command::Perf)) => performance::print_report()?,
        _ => unreachable!("clap rejects conflicting config options"),
    }

    Ok(())
}

fn run_config_command(command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Path => print_config_path()?,
    }
    Ok(())
}

fn print_config_path() -> Result<()> {
    println!("{}", keybindings::KeyBindings::config_path()?.display());
    Ok(())
}

fn open_config() -> Result<()> {
    let path = keybindings::KeyBindings::ensure_config_file()?;
    let visual = env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let editor = env::var("EDITOR")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let (program, args) = editor_invocation(visual.as_deref(), editor.as_deref(), &path)?;
    let status = ProcessCommand::new(&program)
        .args(&args)
        .status()
        .with_context(|| format!("open config {} with {program}", path.display()))?;
    if !status.success() {
        bail!("config editor `{program}` exited with {status}");
    }
    Ok(())
}

fn reset_config() -> Result<()> {
    let path = keybindings::KeyBindings::config_path()?;
    if path.exists() {
        print!("Reset keybindings to defaults? [y/N] ");
        io::stdout().flush().context("show reset confirmation")?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .context("read reset confirmation")?;
        if !reset_confirmed(&answer) {
            println!("Config was not changed");
            return Ok(());
        }
    }

    let (path, backup) = keybindings::KeyBindings::reset_config_file()?;
    println!("Reset config to defaults: {}", path.display());
    if let Some(backup) = backup {
        println!("Previous config backup: {}", backup.display());
    }
    Ok(())
}

fn reset_confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn editor_invocation(
    visual: Option<&str>,
    editor: Option<&str>,
    path: &Path,
) -> Result<(String, Vec<OsString>)> {
    if let Some(specification) = visual
        .filter(|value| !value.trim().is_empty())
        .or_else(|| editor.filter(|value| !value.trim().is_empty()))
    {
        let mut parts = shlex::split(specification)
            .with_context(|| format!("parse editor command `{specification}`"))?;
        if parts.is_empty() {
            bail!("editor command is empty");
        }
        let program = parts.remove(0);
        let mut args = parts.into_iter().map(OsString::from).collect::<Vec<_>>();
        args.push(path.as_os_str().to_owned());
        return Ok((program, args));
    }

    #[cfg(target_os = "macos")]
    return Ok((
        "open".into(),
        vec![OsString::from("-e"), path.as_os_str().to_owned()],
    ));

    #[cfg(not(target_os = "macos"))]
    Ok(("xdg-open".into(), vec![path.as_os_str().to_owned()]))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_options_are_top_level_flags() {
        let edit = Cli::try_parse_from(["shi", "--config"]).unwrap();
        let path = Cli::try_parse_from(["shi", "--config-path"]).unwrap();
        let reset = Cli::try_parse_from(["shi", "--reset-config"]).unwrap();

        assert!(edit.config);
        assert!(path.config_path);
        assert!(reset.reset_config);
    }

    #[test]
    fn performance_report_is_a_subcommand() {
        let cli = Cli::try_parse_from(["shi", "perf"]).unwrap();

        assert!(matches!(cli.command, Some(Command::Perf)));
    }

    #[test]
    fn config_flags_conflict_with_subcommands() {
        assert!(Cli::try_parse_from(["shi", "--config", "repo", "list"]).is_err());
        assert!(Cli::try_parse_from(["shi", "--config", "--config-path"]).is_err());
        assert!(Cli::try_parse_from(["shi", "--config", "--reset-config"]).is_err());
    }

    #[test]
    fn visual_precedes_editor_and_preserves_the_config_path() {
        let path = Path::new("/tmp/config with spaces.json");

        let (program, args) = editor_invocation(Some("code --wait"), Some("vim"), path).unwrap();

        assert_eq!(program, "code");
        assert_eq!(
            args,
            [OsString::from("--wait"), path.as_os_str().to_owned()]
        );
    }

    #[test]
    fn an_empty_visual_setting_falls_through_to_editor() {
        let (program, args) =
            editor_invocation(Some(""), Some("vim"), Path::new("config.json")).unwrap();

        assert_eq!(program, "vim");
        assert_eq!(args, [OsString::from("config.json")]);
    }

    #[test]
    fn reset_confirmation_accepts_only_explicit_yes() {
        assert!(reset_confirmed("y\n"));
        assert!(reset_confirmed("YES"));
        assert!(!reset_confirmed(""));
        assert!(!reset_confirmed("n"));
    }
}
