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

    let branch_ref = format!("refs/heads/{name}");
    let branch_exists = git(repository_path)
        .args(["show-ref", "--verify", "--quiet", &branch_ref])
        .status()
        .context("check workspace branch")?;
    let mut command = git(repository_path);
    command.args(["worktree", "add"]);
    if branch_exists.success() {
        command.arg(destination).arg(name);
    } else if branch_exists.code() == Some(1) {
        command.args(["-b", name]).arg(destination).arg("HEAD");
    } else {
        bail!("failed to check whether branch '{name}' exists");
    }
    let output = command.output().context("run git worktree add")?;
    ensure_success("git worktree add", &output)
}

pub fn remove_workspace(repository_path: &Path, workspace_path: &Path) -> Result<()> {
    let output = git(repository_path)
        .args(["worktree", "remove"])
        .arg(workspace_path)
        .output()
        .context("run git worktree remove")?;
    ensure_success("git worktree remove", &output)
}

pub fn workspace_status(workspace_path: &Path) -> Result<String> {
    let output = git(workspace_path)
        .args(["status", "--short", "--branch"])
        .output()
        .context("run git status")?;
    ensure_success("git status", &output)?;
    Ok(String::from_utf8(output.stdout)
        .context("git returned non-UTF-8 output")?
        .trim()
        .to_owned())
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
    use tempfile::tempdir;

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

    #[test]
    fn manages_real_git_worktrees() {
        let temp = tempdir().unwrap();
        let repository = temp.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        run_git(&repository, &["init", "-b", "main"]);
        run_git(&repository, &["config", "user.name", "wyard test"]);
        run_git(
            &repository,
            &["config", "user.email", "wyard@example.invalid"],
        );
        std::fs::write(repository.join("README.md"), "test\n").unwrap();
        run_git(&repository, &["add", "README.md"]);
        run_git(&repository, &["commit", "-m", "Initial"]);

        let destination = temp.path().join("feature");
        add_workspace(&repository, "feature", &destination).unwrap();
        let canonical_destination = destination.canonicalize().unwrap();
        assert!(
            list_workspaces(&repository)
                .unwrap()
                .iter()
                .any(|workspace| {
                    workspace.name == "feature" && workspace.path == canonical_destination
                })
        );

        remove_workspace(&repository, &destination).unwrap();
        assert!(
            !destination.exists(),
            "git worktree remove deletes its directory"
        );
        assert!(
            list_workspaces(&repository)
                .unwrap()
                .iter()
                .all(|workspace| workspace.name != "feature")
        );
    }

    fn run_git(repository: &Path, args: &[&str]) {
        let status = git(repository).args(args).status().unwrap();
        assert!(status.success(), "git command failed: {args:?}");
    }
}
