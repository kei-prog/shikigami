use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Repository {
    pub name: String,
    pub path: PathBuf,
}

pub fn discover_repositories() -> Result<Vec<Repository>> {
    let output = Command::new("ghq")
        .arg("root")
        .output()
        .context("run ghq root; install ghq and ensure it is in PATH")?;
    if !output.status.success() {
        bail!(
            "ghq root failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let root = String::from_utf8(output.stdout).context("ghq returned non-UTF-8 output")?;
    discover_in_root(Path::new(root.trim()))
}

pub fn workspace_path(repository: &Repository, workspace_name: &str) -> Result<PathBuf> {
    validate_name(workspace_name)?;
    let dirs = ProjectDirs::from("dev", "kei-prog", "wyard")
        .context("cannot determine wyard data directory")?;
    let repository_key = repository
        .path
        .components()
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<PathBuf>();
    Ok(dirs
        .data_local_dir()
        .join("workspaces")
        .join(repository_key)
        .join(workspace_name))
}

fn discover_in_root(root: &Path) -> Result<Vec<Repository>> {
    let mut repositories = Vec::new();
    for host in child_directories(root)? {
        for owner in child_directories(&host)? {
            for repository_path in child_directories(&owner)? {
                if !repository_path.join(".git").exists() {
                    continue;
                }
                let owner_name = owner.file_name().and_then(|name| name.to_str());
                let repository_name = repository_path.file_name().and_then(|name| name.to_str());
                let (Some(owner_name), Some(repository_name)) = (owner_name, repository_name)
                else {
                    continue;
                };
                repositories.push(Repository {
                    name: format!("{owner_name}/{repository_name}"),
                    path: repository_path,
                });
            }
        }
    }
    repositories.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(repositories)
}

fn child_directories(path: &Path) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            directories.push(path);
        }
    }
    Ok(directories)
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        bail!("name must contain only ASCII letters, numbers, '.', '_' or '-'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn discovers_git_repositories() {
        let temp = tempdir().unwrap();
        let git_repository = temp.path().join("github.com/kei-prog/legacy");
        fs::create_dir_all(git_repository.join(".git")).unwrap();

        let repositories = discover_in_root(temp.path()).unwrap();

        assert_eq!(
            repositories,
            vec![Repository {
                name: "kei-prog/legacy".into(),
                path: git_repository,
            }]
        );
    }

    #[test]
    fn rejects_unsafe_workspace_names() {
        assert!(validate_name("feature-one").is_ok());
        assert!(validate_name("../outside").is_err());
        assert!(validate_name("two words").is_err());
    }
}
