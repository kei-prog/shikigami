use std::{path::Path, process::Command};

use anyhow::{Context, Result, bail};

pub fn run(workspace_path: &Path) -> Result<()> {
    let status = Command::new("codex")
        .current_dir(workspace_path)
        .status()
        .with_context(|| format!("start Codex in {}", workspace_path.display()))?;
    if !status.success() {
        bail!("Codex exited with {status}");
    }
    Ok(())
}
