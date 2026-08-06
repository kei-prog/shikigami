use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Workspace {
    pub name: String,
    pub path: PathBuf,
    pub is_primary: bool,
}

pub fn list_workspaces(repository_path: &Path) -> Result<Vec<Workspace>> {
    let output = git(repository_path)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()
        .context("run git worktree list")?;
    ensure_success("git worktree list", &output)?;
    let primary_path = repository_path
        .canonicalize()
        .with_context(|| format!("resolve {}", repository_path.display()))?;
    parse_workspaces(
        &String::from_utf8(output.stdout).context("git returned non-UTF-8 output")?,
        &primary_path,
    )
}

fn parse_workspaces(output: &str, primary_path: &Path) -> Result<Vec<Workspace>> {
    let mut workspaces = Vec::new();
    for record in output.split("\0\0").filter(|record| !record.is_empty()) {
        let mut path = None;
        let mut branch = None;
        for field in record.split('\0') {
            if let Some(value) = field.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            } else if let Some(value) = field.strip_prefix("branch refs/heads/") {
                branch = Some(value.to_owned());
            }
        }
        let path = path.context("git worktree record has no path")?;
        let name = branch.unwrap_or_else(|| {
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("detached")
                .to_owned()
        });
        workspaces.push(Workspace {
            is_primary: path == primary_path,
            name,
            path,
        });
    }
    Ok(workspaces)
}

fn git(path: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(path);
    command
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
    use super::*;

    #[test]
    fn parses_porcelain_worktree_output() {
        let output = concat!(
            "worktree /tmp/repo\0HEAD abc\0branch refs/heads/main\0\0",
            "worktree /tmp/feature\0HEAD def\0branch refs/heads/feature\0\0",
        );
        let workspaces = parse_workspaces(output, Path::new("/tmp/repo")).unwrap();
        assert_eq!(
            workspaces,
            vec![
                Workspace {
                    name: "main".into(),
                    path: "/tmp/repo".into(),
                    is_primary: true,
                },
                Workspace {
                    name: "feature".into(),
                    path: "/tmp/feature".into(),
                    is_primary: false,
                },
            ]
        );
    }
}
