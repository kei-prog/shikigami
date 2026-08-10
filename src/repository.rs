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
pub(crate) struct RepositoryUiData {
    pub(crate) expanded_repositories: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) selected_repository: Option<PathBuf>,
    #[serde(default)]
    pub(crate) selected_thread_id: Option<String>,
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

    pub(crate) fn load_ui_state(&self) -> Result<Option<RepositoryUiData>> {
        if !self.ui_state_path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&self.ui_state_path)
            .with_context(|| format!("read {}", self.ui_state_path.display()))?;
        let data: RepositoryUiData = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", self.ui_state_path.display()))?;
        Ok(Some(data))
    }

    pub(crate) fn save_ui_state(
        &self,
        repositories: &HashSet<PathBuf>,
        selected_repository: Option<&Path>,
        selected_thread_id: Option<&str>,
    ) -> Result<()> {
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
                selected_repository: selected_repository.map(Path::to_path_buf),
                selected_thread_id: selected_thread_id.map(str::to_owned),
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
        let ghq_root = ghq_root();
        for root in scan_roots(scope) {
            if ghq_root.as_ref().is_some_and(|ghq_root| ghq_root == &root)
                && scan_ghq(&sender, &mut seen)
            {
                continue;
            }
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
    ghq_root().into_iter().collect()
}

fn ghq_root() -> Option<PathBuf> {
    if let Ok(output) = Command::new("ghq").arg("root").output()
        && output.status.success()
        && let Ok(root) = String::from_utf8(output.stdout)
    {
        let root = PathBuf::from(root.trim());
        if root.is_dir() {
            return Some(root);
        }
    }
    None
}

fn scan_ghq(sender: &Sender<ScanEvent>, seen: &mut HashSet<PathBuf>) -> bool {
    let Ok(output) = Command::new("ghq").args(["list", "--full-path"]).output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let Ok(stdout) = String::from_utf8(output.stdout) else {
        return false;
    };
    let paths = stdout.lines().map(PathBuf::from).collect::<Vec<_>>();

    // Primary repositories require no Git subprocess and become visible first.
    for path in paths.iter().filter(|path| path.join(".git").is_dir()) {
        if let Ok(repository) = primary_repository(path) {
            send_repository(repository, sender, seen);
        }
    }
    // Resolve linked worktrees afterward; their primary repositories are usually already seen.
    for path in paths.iter().filter(|path| path.join(".git").is_file()) {
        if let Ok(repository) = repository_at(path) {
            send_repository(repository, sender, seen);
        }
    }
    true
}

fn primary_repository(path: &Path) -> Result<Repository> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolve repository path {}", path.display()))?;
    Ok(Repository {
        name: display_name(&path),
        path,
    })
}

fn send_repository(
    repository: Repository,
    sender: &Sender<ScanEvent>,
    seen: &mut HashSet<PathBuf>,
) {
    if seen.insert(repository.path.clone()) {
        let _ = sender.send(ScanEvent::Found(repository));
    }
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
        if let Ok(repository) = repository_at(path) {
            send_repository(repository, sender, seen);
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
        ScanScope::Home => 6,
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
        assert!(store.load_ui_state().unwrap().is_none());

        let expanded = HashSet::from([PathBuf::from("/one"), PathBuf::from("/two")]);
        store.save_ui_state(&expanded, None, None).unwrap();
        assert_eq!(
            store
                .load_ui_state()
                .unwrap()
                .unwrap()
                .expanded_repositories
                .into_iter()
                .collect::<HashSet<_>>(),
            expanded
        );

        store.save_ui_state(&HashSet::new(), None, None).unwrap();
        assert!(
            store
                .load_ui_state()
                .unwrap()
                .unwrap()
                .expanded_repositories
                .is_empty()
        );
    }

    #[test]
    fn stores_selected_thread_and_repository_in_ui_state() {
        let temp = tempdir().unwrap();
        let store = RepositoryStore::at(temp.path());
        let repository = Path::new("/owner/repo");

        store
            .save_ui_state(&HashSet::new(), Some(repository), Some("thread-123"))
            .unwrap();

        let state = store.load_ui_state().unwrap().unwrap();
        assert_eq!(state.selected_repository.as_deref(), Some(repository));
        assert_eq!(state.selected_thread_id.as_deref(), Some("thread-123"));
    }

    #[test]
    fn loads_ui_state_saved_before_selection_was_persisted() {
        let temp = tempdir().unwrap();
        let store = RepositoryStore::at(temp.path());
        fs::write(
            &store.ui_state_path,
            br#"{"expanded_repositories":["/owner/repo"]}"#,
        )
        .unwrap();

        let state = store.load_ui_state().unwrap().unwrap();
        assert_eq!(
            state.expanded_repositories,
            vec![PathBuf::from("/owner/repo")]
        );
        assert!(state.selected_repository.is_none());
        assert!(state.selected_thread_id.is_none());
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

    #[test]
    fn limits_home_scan_depth() {
        assert_eq!(max_depth(&ScanScope::Home), 6);
    }

    #[test]
    fn recognizes_primary_ghq_repository_without_running_git() {
        let temp = tempdir().unwrap();
        let repository_path = temp.path().join("owner").join("repo");
        fs::create_dir_all(repository_path.join(".git")).unwrap();

        let repository = primary_repository(&repository_path).unwrap();

        assert_eq!(repository.name, "owner/repo");
        assert_eq!(repository.path, repository_path.canonicalize().unwrap());
    }
}
