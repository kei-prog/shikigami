use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
};

use anyhow::{Context, Result, bail};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct Repository {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Default, Deserialize, Serialize)]
struct RepositoryData {
    repositories: Vec<Repository>,
}

#[derive(Default, Deserialize, Serialize)]
struct RepositoryUiData {
    expanded_repositories: Vec<PathBuf>,
}

#[derive(Default, Deserialize, Serialize)]
struct RepositoryRootsData {
    roots: Vec<PathBuf>,
}

pub struct RepositoryStore {
    registered_path: PathBuf,
    candidates_path: PathBuf,
    ui_state_path: PathBuf,
    roots_path: PathBuf,
    initial_home_scan_path: PathBuf,
}

#[derive(Clone, Debug)]
pub enum ScanScope {
    Roots(Vec<PathBuf>),
    Home,
}

pub enum ScanEvent {
    Found(Repository),
    Finished,
}

impl RepositoryStore {
    pub fn discover() -> Result<Self> {
        let dirs = paths::project_dirs()?;
        let registered_path = dirs.data_local_dir().join("repositories.json");
        let candidates_path = dirs.cache_dir().join("repository-candidates.json");
        let ui_state_path = dirs.data_local_dir().join("repository-ui.json");
        let roots_path = dirs.data_local_dir().join("repository-roots.json");
        let initial_home_scan_path = dirs.data_local_dir().join("initial-home-scan-complete");
        Ok(Self {
            registered_path,
            candidates_path,
            ui_state_path,
            roots_path,
            initial_home_scan_path,
        })
    }

    #[cfg(test)]
    fn at(root: &Path) -> Self {
        Self {
            registered_path: root.join("repositories.json"),
            candidates_path: root.join("candidates.json"),
            ui_state_path: root.join("repository-ui.json"),
            roots_path: root.join("roots.json"),
            initial_home_scan_path: root.join("initial-home-scan-complete"),
        }
    }

    pub fn initial_home_scan_is_pending(&self) -> bool {
        !self.initial_home_scan_path.exists()
    }

    pub fn mark_initial_home_scan_complete(&self) -> Result<()> {
        let parent = self
            .initial_home_scan_path
            .parent()
            .context("initial home scan path has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        fs::write(&self.initial_home_scan_path, b"")
            .with_context(|| format!("write {}", self.initial_home_scan_path.display()))
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

    pub fn load_expanded_repositories(&self) -> Result<Option<HashSet<PathBuf>>> {
        if !self.ui_state_path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&self.ui_state_path)
            .with_context(|| format!("read {}", self.ui_state_path.display()))?;
        let data: RepositoryUiData = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", self.ui_state_path.display()))?;
        Ok(Some(data.expanded_repositories.into_iter().collect()))
    }

    pub fn save_expanded_repositories(&self, repositories: &HashSet<PathBuf>) -> Result<()> {
        let parent = self
            .ui_state_path
            .parent()
            .context("repository UI state path has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let mut expanded_repositories = repositories.iter().cloned().collect::<Vec<_>>();
        expanded_repositories.sort();
        let temporary = self.ui_state_path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(&RepositoryUiData {
                expanded_repositories,
            })?,
        )
        .with_context(|| format!("write {}", temporary.display()))?;
        fs::rename(&temporary, &self.ui_state_path)
            .with_context(|| format!("replace {}", self.ui_state_path.display()))?;
        Ok(())
    }

    pub fn load_search_roots(&self) -> Result<Vec<PathBuf>> {
        if !self.roots_path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&self.roots_path)
            .with_context(|| format!("read {}", self.roots_path.display()))?;
        let mut data: RepositoryRootsData = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", self.roots_path.display()))?;
        data.roots.retain(|root| root.is_dir());
        data.roots.sort();
        data.roots.dedup();
        Ok(data.roots)
    }

    pub fn add_search_root(&self, root: &Path) -> Result<PathBuf> {
        let root = root
            .canonicalize()
            .with_context(|| format!("resolve projects folder {}", root.display()))?;
        if !root.is_dir() {
            bail!("projects folder is not a directory: {}", root.display());
        }
        let mut roots = self.load_search_roots()?;
        if !roots.contains(&root) {
            roots.push(root.clone());
            roots.sort();
            self.save_search_roots(&roots)?;
        }
        Ok(root)
    }

    fn save_search_roots(&self, roots: &[PathBuf]) -> Result<()> {
        let parent = self
            .roots_path
            .parent()
            .context("repository roots path has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let temporary = self.roots_path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(&RepositoryRootsData {
                roots: roots.to_vec(),
            })?,
        )
        .with_context(|| format!("write {}", temporary.display()))?;
        fs::rename(&temporary, &self.roots_path)
            .with_context(|| format!("replace {}", self.roots_path.display()))?;
        Ok(())
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
        let max_depth = max_depth(&scope);
        for root in scan_roots(scope) {
            walk(&root, 0, max_depth, &sender, &mut seen);
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

pub fn detected_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(output) = Command::new("ghq").arg("root").output()
        && output.status.success()
        && let Ok(root) = String::from_utf8(output.stdout)
    {
        let root = PathBuf::from(root.trim());
        if root.is_dir() {
            roots.push(root);
        }
    }
    roots
}

fn scan_roots(scope: ScanScope) -> Vec<PathBuf> {
    if let ScanScope::Roots(roots) = scope {
        return roots;
    }
    let Some(base_dirs) = BaseDirs::new() else {
        return Vec::new();
    };
    vec![base_dirs.home_dir().to_path_buf()]
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

fn max_depth(scope: &ScanScope) -> usize {
    match scope {
        ScanScope::Roots(_) => 6,
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
    fn tracks_initial_home_scan_without_repeating_after_completion() {
        let temp = tempdir().unwrap();
        let store = RepositoryStore::at(temp.path());

        assert!(store.initial_home_scan_is_pending());
        store.mark_initial_home_scan_complete().unwrap();
        assert!(!store.initial_home_scan_is_pending());
    }

    #[test]
    fn empty_repository_data_does_not_skip_initial_home_scan() {
        let temp = tempdir().unwrap();
        let store = RepositoryStore::at(temp.path());
        store.save_candidates(&[]).unwrap();
        assert!(store.initial_home_scan_is_pending());

        save(&store.registered_path, &[]).unwrap();
        assert!(store.initial_home_scan_is_pending());
    }

    #[test]
    fn stores_repository_expansion_state_including_all_collapsed() {
        let temp = tempdir().unwrap();
        let store = RepositoryStore::at(temp.path());
        assert_eq!(store.load_expanded_repositories().unwrap(), None);

        let expanded = HashSet::from([PathBuf::from("/one"), PathBuf::from("/two")]);
        store.save_expanded_repositories(&expanded).unwrap();
        assert_eq!(store.load_expanded_repositories().unwrap(), Some(expanded));

        store.save_expanded_repositories(&HashSet::new()).unwrap();
        assert_eq!(
            store.load_expanded_repositories().unwrap(),
            Some(HashSet::new())
        );
    }

    #[test]
    fn stores_search_roots_without_duplicates() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();
        let store = RepositoryStore::at(temp.path());

        assert!(store.load_search_roots().unwrap().is_empty());
        assert_eq!(store.add_search_root(&second).unwrap(), second);
        assert_eq!(store.add_search_root(&first).unwrap(), first);
        store.add_search_root(&second).unwrap();

        assert_eq!(store.load_search_roots().unwrap(), vec![first, second]);
    }

    #[test]
    fn forgets_search_roots_that_no_longer_exist() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("projects");
        fs::create_dir(&root).unwrap();
        let store = RepositoryStore::at(temp.path());
        store.add_search_root(&root).unwrap();
        fs::remove_dir(&root).unwrap();

        assert!(store.load_search_roots().unwrap().is_empty());
    }

    #[test]
    fn skips_expensive_directories() {
        assert!(should_skip(Path::new("/tmp/project/node_modules")));
        assert!(should_skip(Path::new("/Users/someone/Library")));
        assert!(!should_skip(Path::new("/Users/someone/Projects")));
    }
}
