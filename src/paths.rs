use std::{fs, io::ErrorKind, path::PathBuf};

use anyhow::{Context, Result};
use directories::{ProjectDirs, UserDirs};

const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "kei-prog";

pub fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, "shikigami")
        .context("cannot determine Shikigami data directory")
}

pub fn create_general_workspace() -> Result<PathBuf> {
    let user_dirs = UserDirs::new().context("cannot determine user directories")?;
    let root = user_dirs
        .document_dir()
        .unwrap_or_else(|| user_dirs.home_dir())
        .join("Shikigami");
    create_general_workspace_in(&root)
}

fn create_general_workspace_in(root: &std::path::Path) -> Result<PathBuf> {
    fs::create_dir_all(root)
        .with_context(|| format!("create General workspace root {}", root.display()))?;

    for suffix in 1.. {
        let workspace = root.join(general_workspace_name(suffix));
        match fs::create_dir(&workspace) {
            Ok(()) => return Ok(workspace),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create General workspace {}", workspace.display()));
            }
        }
    }
    unreachable!("workspace suffix range is unbounded")
}

fn general_workspace_name(suffix: usize) -> String {
    if suffix == 1 {
        "new-chat".to_owned()
    } else {
        format!("new-chat-{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{create_general_workspace_in, general_workspace_name};

    #[test]
    fn general_workspace_names_are_stable() {
        assert_eq!(
            (1..=3).map(general_workspace_name).collect::<Vec<_>>(),
            ["new-chat", "new-chat-2", "new-chat-3"]
        );
    }

    #[test]
    fn general_workspaces_do_not_reuse_an_existing_chat_directory() {
        let temp = tempdir().unwrap();

        let first = create_general_workspace_in(temp.path()).unwrap();
        let second = create_general_workspace_in(temp.path()).unwrap();

        assert_eq!(first.file_name().unwrap(), "new-chat");
        assert_eq!(second.file_name().unwrap(), "new-chat-2");
        assert!(first.is_dir());
        assert!(second.is_dir());
    }
}
