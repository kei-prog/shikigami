use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

use crate::paths;

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

pub fn create_generated_workspace(
    repository_path: &Path,
    repository_name: &str,
) -> Result<Workspace> {
    let repository_key = repository_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let identifier = generated_identifier()?;
    let branch = format!("shi/{identifier}");
    let destination = managed_worktrees_root()?
        .join(repository_key)
        .join(&identifier);
    create_workspace_at(repository_path, &branch, &destination)
}

pub fn current_branch(workspace_path: &Path) -> Result<Option<String>> {
    let output = git(workspace_path)
        .args(["branch", "--show-current"])
        .output()
        .context("read worktree branch")?;
    ensure_success("git branch --show-current", &output)?;
    let branch = String::from_utf8(output.stdout)
        .context("git returned non-UTF-8 output")?
        .trim()
        .to_owned();
    Ok((!branch.is_empty()).then_some(branch))
}

pub fn is_managed_workspace(path: &Path, branch: Option<&str>) -> bool {
    match branch {
        Some(branch) if branch.starts_with("shi/") => {
            managed_worktrees_root().is_ok_and(|root| path_is_within(path, &root))
        }
        Some(branch) if branch.starts_with("wyard/") => {
            legacy_managed_worktrees_root().is_ok_and(|root| path_is_within(path, &root))
        }
        _ => false,
    }
}

pub fn workspace_is_clean(path: &Path) -> Result<bool> {
    let output = git(path)
        .args(["status", "--porcelain"])
        .output()
        .context("read worktree status")?;
    ensure_success("git status --porcelain", &output)?;
    Ok(output.stdout.is_empty())
}

pub fn remove_managed_workspace(
    repository_path: &Path,
    workspace_path: &Path,
    branch: Option<&str>,
) -> Result<()> {
    if !is_managed_workspace(workspace_path, branch) {
        bail!("refusing to remove a worktree not owned by Shikigami");
    }
    if !workspace_is_clean(workspace_path)? {
        bail!("worktree has changes and was not removed");
    }
    let output = git(repository_path)
        .args(["worktree", "remove"])
        .arg(workspace_path)
        .output()
        .context("remove managed worktree")?;
    ensure_success("git worktree remove", &output)
}

pub fn restore_managed_workspace(
    repository_path: &Path,
    workspace_path: &Path,
    branch: Option<&str>,
) -> Result<()> {
    let branch = branch.context("archived thread has no worktree branch")?;
    if !is_managed_workspace(workspace_path, Some(branch)) {
        bail!("refusing to restore a worktree not owned by Shikigami");
    }
    if workspace_path.exists() {
        return Ok(());
    }
    let parent = workspace_path.parent().context("worktree has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create worktree directory {}", parent.display()))?;
    let output = git(repository_path)
        .args(["worktree", "add"])
        .arg(workspace_path)
        .arg(branch)
        .output()
        .context("restore managed worktree")?;
    ensure_success("git worktree add", &output)
}

fn managed_worktrees_root() -> Result<PathBuf> {
    Ok(paths::project_dirs()?.data_local_dir().join("worktrees"))
}

fn legacy_managed_worktrees_root() -> Result<PathBuf> {
    Ok(paths::legacy_project_dirs()?
        .data_local_dir()
        .join("worktrees"))
}

fn path_is_within(path: &Path, root: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return path.starts_with(root);
    };
    if let Ok(path) = path.canonicalize() {
        return path.starts_with(root);
    }
    path.parent()
        .and_then(|parent| parent.canonicalize().ok())
        .is_some_and(|parent| parent.starts_with(root))
}

fn create_workspace_at(
    repository_path: &Path,
    branch: &str,
    destination: &Path,
) -> Result<Workspace> {
    if destination.exists() {
        bail!(
            "worktree destination already exists: {}",
            destination.display()
        );
    }
    let parent = destination.parent().context("worktree has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create worktree directory {}", parent.display()))?;
    let output = git(repository_path)
        .args(["worktree", "add", "-b", branch])
        .arg(destination)
        .arg("HEAD")
        .output()
        .context("run git worktree add")?;
    ensure_success("git worktree add", &output)?;
    Ok(Workspace {
        name: branch.to_owned(),
        path: destination
            .canonicalize()
            .with_context(|| format!("resolve worktree {}", destination.display()))?,
        is_primary: false,
    })
}

fn generated_identifier() -> Result<String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    Ok(format!(
        "{:010x}",
        (nanos ^ u128::from(std::process::id())) & 0xffffffffff
    ))
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
    fn creates_a_worktree_and_branch() {
        let temp = tempdir().unwrap();
        let repository = temp.path().join("repo");
        fs::create_dir(&repository).unwrap();
        run_git(&repository, &["init", "-b", "main"]);
        run_git(&repository, &["config", "user.name", "Shikigami test"]);
        run_git(
            &repository,
            &["config", "user.email", "shikigami@example.invalid"],
        );
        fs::write(repository.join("README.md"), "test\n").unwrap();
        run_git(&repository, &["add", "README.md"]);
        run_git(&repository, &["commit", "-m", "Initial"]);

        let destination = temp.path().join("worktree");
        let workspace = create_workspace_at(&repository, "shi/abc123", &destination).unwrap();
        assert_eq!(workspace.name, "shi/abc123");
        assert_eq!(workspace.path, destination.canonicalize().unwrap());
        assert!(!workspace.is_primary);
        assert!(workspace_is_clean(&workspace.path).unwrap());
        assert!(!is_managed_workspace(&workspace.path, Some("shi/abc123")));
    }

    #[test]
    fn recognizes_only_owned_paths_and_branches() {
        let root = managed_worktrees_root().unwrap();
        assert!(is_managed_workspace(
            &root.join("owner-repo/abc123"),
            Some("shi/abc123")
        ));
        assert!(!is_managed_workspace(
            &root.join("owner-repo/abc123"),
            Some("feature")
        ));
        assert!(!is_managed_workspace(
            Path::new("/tmp/abc123"),
            Some("shi/abc123")
        ));
        let legacy_root = legacy_managed_worktrees_root().unwrap();
        assert!(is_managed_workspace(
            &legacy_root.join("owner-repo/abc123"),
            Some("wyard/abc123")
        ));
        assert!(!is_managed_workspace(
            &root.join("owner-repo/abc123"),
            Some("wyard/abc123")
        ));
    }

    fn run_git(repository: &Path, args: &[&str]) {
        let status = git(repository).args(args).status().unwrap();
        assert!(status.success(), "git command failed: {args:?}");
    }
}
