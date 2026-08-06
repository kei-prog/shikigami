use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub path: PathBuf,
}

pub fn list_workspaces(repository_path: &Path) -> Result<Vec<Workspace>> {
    let output = Command::new("jj")
        .arg("-R")
        .arg(repository_path)
        .args([
            "--ignore-working-copy",
            "workspace",
            "list",
            "--template",
            "name ++ \"\\t\" ++ root ++ \"\\n\"",
        ])
        .output()
        .context("run jj workspace list")?;
    ensure_success("jj workspace list", &output)?;
    parse_workspaces(&String::from_utf8(output.stdout).context("jj returned non-UTF-8 output")?)
}

pub fn add_workspace(repository_path: &Path, name: &str, destination: &Path) -> Result<()> {
    if destination.exists() {
        bail!(
            "workspace destination already exists: {}",
            destination.display()
        );
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create workspace parent {}", parent.display()))?;
    }
    let output = Command::new("jj")
        .arg("-R")
        .arg(repository_path)
        .args(["workspace", "add"])
        .arg(destination)
        .args(["--name", name])
        .output()
        .context("run jj workspace add")?;
    ensure_success("jj workspace add", &output)
}

pub fn forget_workspace(repository_path: &Path, name: &str) -> Result<()> {
    let output = Command::new("jj")
        .arg("-R")
        .arg(repository_path)
        .args(["workspace", "forget", name])
        .output()
        .context("run jj workspace forget")?;
    ensure_success("jj workspace forget", &output)
}

pub fn workspace_status(workspace_path: &Path) -> Result<String> {
    let output = Command::new("jj")
        .arg("-R")
        .arg(workspace_path)
        .arg("status")
        .output()
        .context("run jj status")?;
    ensure_success("jj status", &output)?;
    Ok(String::from_utf8(output.stdout)
        .context("jj returned non-UTF-8 output")?
        .trim()
        .to_owned())
}

fn parse_workspaces(output: &str) -> Result<Vec<Workspace>> {
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, path) = line
                .split_once('\t')
                .with_context(|| format!("unexpected jj workspace output: {line}"))?;
            Ok(Workspace {
                name: name.to_owned(),
                path: PathBuf::from(path),
            })
        })
        .collect()
}

fn ensure_success(action: &str, output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    bail!("{action} failed: {stderr}")
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn parses_workspace_template_output() {
        let workspaces = parse_workspaces("default\t/tmp/repo\nfeature\t/tmp/feature\n").unwrap();
        assert_eq!(
            workspaces,
            vec![
                Workspace {
                    name: "default".into(),
                    path: "/tmp/repo".into()
                },
                Workspace {
                    name: "feature".into(),
                    path: "/tmp/feature".into()
                },
            ]
        );
    }

    #[test]
    fn manages_real_jj_workspaces() {
        let temp = tempdir().unwrap();
        let repository = temp.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        let status = Command::new("jj")
            .args(["git", "init", "--colocate"])
            .arg(&repository)
            .status()
            .unwrap();
        assert!(status.success());

        let destination = temp.path().join("feature");
        add_workspace(&repository, "feature", &destination).unwrap();
        let canonical_destination = destination.canonicalize().unwrap();
        let workspaces = list_workspaces(&repository).unwrap();
        assert!(workspaces.iter().any(
            |workspace| workspace.name == "feature" && workspace.path == canonical_destination
        ));

        forget_workspace(&repository, "feature").unwrap();
        assert!(
            list_workspaces(&repository)
                .unwrap()
                .iter()
                .all(|workspace| workspace.name != "feature")
        );
        assert!(destination.exists(), "forget must preserve workspace files");
    }
}
