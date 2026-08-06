use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use anyhow::{Context, Result, bail};
use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct Repository {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Default, Deserialize, Serialize)]
struct RepositoryData {
    repositories: Vec<Repository>,
}

pub struct RepositoryStore {
    registered_path: PathBuf,
    candidates_path: PathBuf,
}

#[derive(Clone, Copy)]
pub enum ScanScope {
    Quick,
    Home,
}

pub enum ScanEvent {
    Found(Repository),
    Finished,
}

impl RepositoryStore {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "kei-prog", "wyard")
            .context("cannot determine wyard data directory")?;
        Ok(Self {
            registered_path: dirs.data_local_dir().join("repositories.json"),
            candidates_path: dirs.cache_dir().join("repository-candidates.json"),
        })
    }

    #[cfg(test)]
    fn at(root: &Path) -> Self {
        Self {
            registered_path: root.join("repositories.json"),
            candidates_path: root.join("candidates.json"),
        }
    }

    pub fn load_registered(&self) -> Result<Vec<Repository>> {
        load(&self.registered_path)
    }

    pub fn load_candidates(&self) -> Result<Vec<Repository>> {
        load(&self.candidates_path)
    }

    pub fn save_candidates(&self, repositories: &[Repository]) -> Result<()> {
        save(&self.candidates_path, repositories)
    }

    pub fn register(&self, repositories: &[Repository]) -> Result<()> {
        let mut registered = self.load_registered()?;
        for repository in repositories {
            if !registered.iter().any(|item| item.path == repository.path) {
                registered.push(repository.clone());
            }
        }
        sort_repositories(&mut registered);
        save(&self.registered_path, &registered)
    }

    pub fn unregister(&self, path: &Path) -> Result<()> {
        let mut registered = self.load_registered()?;
        registered.retain(|repository| repository.path != path);
        save(&self.registered_path, &registered)
    }
}

pub fn start_scan(scope: ScanScope) -> Receiver<ScanEvent> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut seen = HashSet::new();
        for root in scan_roots(scope) {
            walk(&root, 0, max_depth(scope), &sender, &mut seen);
        }
        let _ = sender.send(ScanEvent::Finished);
    });
    receiver
}

pub fn repository_at(path: &Path) -> Result<Repository> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .with_context(|| format!("inspect Git repository at {}", path.display()))?;
    if !output.status.success() {
        bail!("not a Git repository: {}", path.display());
    }
    let stdout = String::from_utf8(output.stdout).context("git returned non-UTF-8 output")?;
    let primary = stdout
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .context("git worktree list returned no repository path")?
        .canonicalize()
        .context("resolve repository path")?;
    Ok(Repository {
        name: display_name(&primary),
        path: primary,
    })
}

fn scan_roots(scope: ScanScope) -> Vec<PathBuf> {
    let Some(base_dirs) = BaseDirs::new() else {
        return Vec::new();
    };
    let home = base_dirs.home_dir();
    if matches!(scope, ScanScope::Home) {
        return vec![home.to_path_buf()];
    }

    let mut roots = [
        "Developer",
        "Develop",
        "Projects",
        "Code",
        "Source",
        "src",
        "dev",
    ]
    .into_iter()
    .map(|name| home.join(name))
    .filter(|path| path.is_dir())
    .collect::<Vec<_>>();
    if let Ok(output) = Command::new("ghq").arg("root").output()
        && output.status.success()
        && let Ok(root) = String::from_utf8(output.stdout)
    {
        let root = PathBuf::from(root.trim());
        if root.is_dir() && !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

fn walk(
    path: &Path,
    depth: usize,
    max_depth: usize,
    sender: &Sender<ScanEvent>,
    seen: &mut HashSet<PathBuf>,
) {
    if depth > max_depth || should_skip(path) {
        return;
    }
    if path.join(".git").exists() {
        if let Ok(repository) = repository_at(path)
            && seen.insert(repository.path.clone())
        {
            let _ = sender.send(ScanEvent::Found(repository));
        }
        return;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && !file_type.is_symlink() {
            walk(&entry.path(), depth + 1, max_depth, sender, seen);
        }
    }
}

fn should_skip(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with('.')
        || matches!(
            name,
            ".Trash"
                | ".cache"
                | ".cargo"
                | ".npm"
                | ".pnpm-store"
                | ".rustup"
                | ".venv"
                | "Library"
                | "Applications"
                | "Movies"
                | "Music"
                | "Pictures"
                | "node_modules"
                | "target"
                | "vendor"
        )
}

fn max_depth(scope: ScanScope) -> usize {
    match scope {
        ScanScope::Quick => 6,
        ScanScope::Home => 12,
    }
}

fn display_name(path: &Path) -> String {
    let parts = path
        .components()
        .rev()
        .take(2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<PathBuf>();
    parts.to_string_lossy().into_owned()
}

fn load(path: &Path) -> Result<Vec<Repository>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut data: RepositoryData =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    data.repositories
        .retain(|repository| repository.path.is_dir());
    sort_repositories(&mut data.repositories);
    Ok(data.repositories)
}

fn save(path: &Path, repositories: &[Repository]) -> Result<()> {
    let parent = path
        .parent()
        .context("repository data path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(&RepositoryData {
            repositories: repositories.to_vec(),
        })?,
    )
    .with_context(|| format!("write {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn sort_repositories(repositories: &mut [Repository]) {
    repositories.sort_by(|left, right| left.name.cmp(&right.name));
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn stores_only_registered_repositories() {
        let temp = tempdir().unwrap();
        let repository_path = temp.path().join("repo");
        fs::create_dir(&repository_path).unwrap();
        let store = RepositoryStore::at(temp.path());
        let repository = Repository {
            name: "owner/repo".into(),
            path: repository_path.clone(),
        };

        store.register(std::slice::from_ref(&repository)).unwrap();
        store.register(std::slice::from_ref(&repository)).unwrap();
        assert_eq!(store.load_registered().unwrap(), vec![repository]);

        store.unregister(&repository_path).unwrap();
        assert!(store.load_registered().unwrap().is_empty());
    }

    #[test]
    fn skips_expensive_directories() {
        assert!(should_skip(Path::new("/tmp/project/node_modules")));
        assert!(should_skip(Path::new("/Users/someone/Library")));
        assert!(!should_skip(Path::new("/Users/someone/Projects")));
    }
}
