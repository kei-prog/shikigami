use std::{
    collections::HashSet,
    fs,
    path::PathBuf,
    sync::mpsc::{Receiver, TryRecvError},
};

use anyhow::{Context, Result};
use directories::BaseDirs;

use crate::{
    git_workspace::{self, Workspace},
    registry::{Registry, ThreadRecord},
    repository::{self, Repository, RepositoryStore, ScanEvent, ScanScope, start_scan},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    Repositories,
    Threads,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    Normal,
    AddRepositories,
    FilterRepositories,
    BrowseDirectory,
    ChooseLocation,
    ConfirmRemoveRepository,
    ConfirmRemoveThread,
    Help,
}

#[derive(Clone, Debug)]
pub struct ThreadItem {
    pub record: ThreadRecord,
    pub location_name: String,
    pub is_primary: bool,
}

pub struct App {
    pub repositories: Vec<Repository>,
    pub threads: Vec<ThreadItem>,
    pub locations: Vec<Workspace>,
    pub candidates: Vec<Repository>,
    pub selected_candidates: HashSet<PathBuf>,
    pub repository_query: String,
    pub browse_path: PathBuf,
    pub browse_directories: Vec<PathBuf>,
    pub repository_index: usize,
    pub thread_index: usize,
    pub location_index: usize,
    pub candidate_index: usize,
    pub browse_index: usize,
    pub focus: Focus,
    pub mode: Mode,
    pub scanning: bool,
    pub message: Option<String>,
    pub should_quit: bool,
    thread_registry: Registry,
    repository_store: RepositoryStore,
    scan_receiver: Option<Receiver<ScanEvent>>,
}

impl App {
    pub fn load() -> Result<Self> {
        let repository_store = RepositoryStore::discover()?;
        let repositories = repository_store.load_registered()?;
        let candidates = repository_store.load_candidates().unwrap_or_default();
        let browse_path = BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"));
        let first_run = repositories.is_empty();
        let mut app = Self {
            repositories,
            threads: Vec::new(),
            locations: Vec::new(),
            candidates,
            selected_candidates: HashSet::new(),
            repository_query: String::new(),
            browse_path,
            browse_directories: Vec::new(),
            repository_index: 0,
            thread_index: 0,
            location_index: 0,
            candidate_index: 0,
            browse_index: 0,
            focus: Focus::Repositories,
            mode: if first_run {
                Mode::AddRepositories
            } else {
                Mode::Normal
            },
            scanning: false,
            message: None,
            should_quit: false,
            thread_registry: Registry::discover()?,
            repository_store,
            scan_receiver: None,
        };
        app.refresh_current();
        if first_run {
            app.start_scan(ScanScope::Quick);
        }
        Ok(app)
    }

    pub fn selected_repository(&self) -> Option<&Repository> {
        self.repositories.get(self.repository_index)
    }

    pub fn selected_thread(&self) -> Option<&ThreadItem> {
        self.threads.get(self.thread_index)
    }

    pub fn selected_location(&self) -> Option<&Workspace> {
        self.locations.get(self.location_index)
    }

    pub fn visible_candidates(&self) -> Vec<&Repository> {
        let query = self.repository_query.to_ascii_lowercase();
        self.candidates
            .iter()
            .filter(|candidate| {
                !self
                    .repositories
                    .iter()
                    .any(|repository| repository.path == candidate.path)
            })
            .filter(|candidate| {
                query.is_empty()
                    || candidate.name.to_ascii_lowercase().contains(&query)
                    || candidate
                        .path
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains(&query)
            })
            .collect()
    }

    pub fn selected_candidate(&self) -> Option<&Repository> {
        self.visible_candidates().get(self.candidate_index).copied()
    }

    pub fn move_up(&mut self) {
        match self.mode {
            Mode::AddRepositories => self.candidate_index = self.candidate_index.saturating_sub(1),
            Mode::BrowseDirectory => self.browse_index = self.browse_index.saturating_sub(1),
            Mode::ChooseLocation => self.location_index = self.location_index.saturating_sub(1),
            _ => match self.focus {
                Focus::Repositories => {
                    self.repository_index = self.repository_index.saturating_sub(1);
                    self.thread_index = 0;
                    self.refresh_current();
                }
                Focus::Threads => self.thread_index = self.thread_index.saturating_sub(1),
            },
        }
    }

    pub fn move_down(&mut self) {
        match self.mode {
            Mode::AddRepositories => {
                let count = self.visible_candidates().len();
                if self.candidate_index + 1 < count {
                    self.candidate_index += 1;
                }
            }
            Mode::BrowseDirectory => {
                if self.browse_index + 1 < self.browse_directories.len() {
                    self.browse_index += 1;
                }
            }
            Mode::ChooseLocation => {
                if self.location_index + 1 < self.locations.len() {
                    self.location_index += 1;
                }
            }
            _ => match self.focus {
                Focus::Repositories => {
                    if self.repository_index + 1 < self.repositories.len() {
                        self.repository_index += 1;
                        self.thread_index = 0;
                        self.refresh_current();
                    }
                }
                Focus::Threads => {
                    if self.thread_index + 1 < self.threads.len() {
                        self.thread_index += 1;
                    }
                }
            },
        }
    }

    pub fn open_repository_add(&mut self) {
        self.mode = Mode::AddRepositories;
        self.candidate_index = 0;
        self.repository_query.clear();
        self.selected_candidates.clear();
        if !self.scanning {
            self.start_scan(ScanScope::Quick);
        }
    }

    pub fn start_home_scan(&mut self) {
        if !self.scanning {
            self.start_scan(ScanScope::Home);
        }
    }

    pub fn poll_scan(&mut self) {
        let mut finished = false;
        let Some(receiver) = &self.scan_receiver else {
            return;
        };
        loop {
            match receiver.try_recv() {
                Ok(ScanEvent::Found(repository)) => {
                    if !self
                        .candidates
                        .iter()
                        .any(|candidate| candidate.path == repository.path)
                    {
                        self.candidates.push(repository);
                        self.candidates
                            .sort_by(|left, right| left.name.cmp(&right.name));
                    }
                }
                Ok(ScanEvent::Finished) | Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if finished {
            self.scanning = false;
            self.scan_receiver = None;
            if let Err(error) = self.repository_store.save_candidates(&self.candidates) {
                self.message = Some(error.to_string());
            }
            self.candidate_index = self
                .candidate_index
                .min(self.visible_candidates().len().saturating_sub(1));
        }
    }

    pub fn toggle_selected_candidate(&mut self) {
        let Some(path) = self
            .selected_candidate()
            .map(|candidate| candidate.path.clone())
        else {
            return;
        };
        if !self.selected_candidates.remove(&path) {
            self.selected_candidates.insert(path);
        }
    }

    pub fn register_candidates(&mut self) -> Result<()> {
        let mut selected = self
            .candidates
            .iter()
            .filter(|candidate| self.selected_candidates.contains(&candidate.path))
            .cloned()
            .collect::<Vec<_>>();
        if selected.is_empty()
            && let Some(candidate) = self.selected_candidate().cloned()
        {
            selected.push(candidate);
        }
        if selected.is_empty() {
            anyhow::bail!("select a repository first");
        }
        self.repository_store.register(&selected)?;
        self.refresh_repositories()?;
        self.mode = Mode::Normal;
        self.selected_candidates.clear();
        Ok(())
    }

    pub fn open_browser(&mut self) {
        self.mode = Mode::BrowseDirectory;
        self.refresh_browser();
    }

    pub fn browse_into_selected(&mut self) {
        let Some(path) = self.browse_directories.get(self.browse_index).cloned() else {
            return;
        };
        self.browse_path = path;
        self.refresh_browser();
    }

    pub fn browse_parent(&mut self) {
        if let Some(parent) = self.browse_path.parent() {
            self.browse_path = parent.to_path_buf();
            self.refresh_browser();
        }
    }

    pub fn register_browse_path(&mut self) -> Result<()> {
        let repository = repository::repository_at(&self.browse_path)?;
        self.repository_store.register(&[repository])?;
        self.refresh_repositories()?;
        self.mode = Mode::Normal;
        Ok(())
    }

    pub fn refresh_current(&mut self) {
        self.locations.clear();
        self.threads.clear();
        let Some(repository) = self.selected_repository().cloned() else {
            return;
        };

        match git_workspace::list_workspaces(&repository.path) {
            Ok(locations) => self.locations = locations,
            Err(error) => self.message = Some(error.to_string()),
        }
        match self.thread_registry.load() {
            Ok(records) => {
                self.threads = records
                    .into_iter()
                    .filter(|record| record.repository_path == repository.path)
                    .map(|record| {
                        let location = self
                            .locations
                            .iter()
                            .filter(|location| record.cwd.starts_with(&location.path))
                            .max_by_key(|location| location.path.components().count());
                        ThreadItem {
                            location_name: location
                                .map(|location| location.name.clone())
                                .unwrap_or_else(|| "unknown".into()),
                            is_primary: location.is_some_and(|location| location.is_primary),
                            record,
                        }
                    })
                    .collect();
                self.threads.sort_by(|left, right| {
                    right
                        .record
                        .updated_at
                        .cmp(&left.record.updated_at)
                        .then_with(|| left.record.title.cmp(&right.record.title))
                });
            }
            Err(error) => self.message = Some(error.to_string()),
        }
        self.thread_index = self.thread_index.min(self.threads.len().saturating_sub(1));
        self.location_index = self
            .location_index
            .min(self.locations.len().saturating_sub(1));
    }

    pub fn refresh_repositories(&mut self) -> Result<()> {
        let selected_path = self
            .selected_repository()
            .map(|repository| repository.path.clone());
        self.repositories = self.repository_store.load_registered()?;
        self.repository_index = selected_path
            .and_then(|path| {
                self.repositories
                    .iter()
                    .position(|repository| repository.path == path)
            })
            .unwrap_or(0)
            .min(self.repositories.len().saturating_sub(1));
        self.thread_index = 0;
        self.location_index = 0;
        self.refresh_current();
        Ok(())
    }

    pub fn unregister_selected_repository(&mut self) -> Result<()> {
        let path = self
            .selected_repository()
            .map(|repository| repository.path.clone())
            .context("no repository selected")?;
        self.repository_store.unregister(&path)?;
        self.refresh_repositories()?;
        if self.repositories.is_empty() {
            self.open_repository_add();
        }
        Ok(())
    }

    pub fn remove_selected_thread(&mut self) -> Result<()> {
        let thread_id = self
            .selected_thread()
            .map(|thread| thread.record.id.clone())
            .context("no thread selected")?;
        self.thread_registry.remove(&thread_id)?;
        self.refresh_current();
        Ok(())
    }

    fn start_scan(&mut self, scope: ScanScope) {
        self.scan_receiver = Some(start_scan(scope));
        self.scanning = true;
    }

    fn refresh_browser(&mut self) {
        self.browse_directories = fs::read_dir(&self.browse_path)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|entry| {
                        if entry.file_name().to_string_lossy().starts_with('.') {
                            return None;
                        }
                        entry
                            .file_type()
                            .ok()
                            .filter(|kind| kind.is_dir() && !kind.is_symlink())
                            .map(|_| entry.path())
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.browse_directories.sort();
        self.browse_index = 0;
    }
}
