use std::{path::Path, process::Command};

use anyhow::{Context, Result, bail};

use crate::registry::ThreadRecord;

pub fn run_new(cwd: &Path, repository_path: &Path) -> Result<()> {
    run(cwd, repository_path, None)
}

pub fn resume(thread: &ThreadRecord) -> Result<()> {
    run(&thread.cwd, &thread.repository_path, Some(&thread.id))
}

fn run(cwd: &Path, repository_path: &Path, thread_id: Option<&str>) -> Result<()> {
    if !cwd.is_dir() {
        bail!("thread directory no longer exists: {}", cwd.display());
    }
    let executable = std::env::current_exe().context("locate wyard executable")?;
    let notifier = serde_json::to_string(&[
        executable.to_string_lossy().as_ref(),
        "capture-thread",
        repository_path.to_string_lossy().as_ref(),
    ])?;
    let mut command = Command::new("codex");
    command
        .current_dir(cwd)
        .args(["-c", &format!("notify={notifier}")]);
    if let Some(thread_id) = thread_id {
        command.args(["resume", thread_id]);
    }
    let status = command
        .status()
        .with_context(|| format!("start Codex in {}", cwd.display()))?;
    if !status.success() {
        bail!("Codex exited with {status}");
    }
    Ok(())
}
