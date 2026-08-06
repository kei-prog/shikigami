use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::PathBuf,
    sync::mpsc::{Receiver, TryRecvError},
};

use anyhow::{Context, Result};
use directories::BaseDirs;

use crate::{
    app_server::{AppServerEvent, AppServerRequest},
    chat::ChatState,
    git_workspace::{self, Workspace},
    registry::{Registry, ThreadRecord},
    repository::{self, Repository, RepositoryStore, ScanEvent, ScanScope, start_scan},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    Navigation,
    Chat,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    Normal,
    AddRepositories,
    FilterRepositories,
    BrowseDirectory,
    ChooseThreadTarget,
    ChooseExistingWorktree,
    ConfirmRemoveRepository,
    ConfirmRemoveThread,
    ConfirmArchiveCleanup,
    Chat,
    Approval,
    Help,
}

#[derive(Clone, Debug)]
pub struct ThreadItem {
    pub record: ThreadRecord,
    pub location_name: String,
    pub is_primary: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeRow {
    Repository {
        repository_index: usize,
    },
    Thread {
        repository_index: usize,
        thread_index: usize,
    },
}

enum TreeSelection {
    Repository(PathBuf),
    Thread { id: String, repository: PathBuf },
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
    pub tree_index: usize,
    pub expanded_repositories: HashSet<PathBuf>,
    pub location_index: usize,
    pub thread_target_index: usize,
    pub candidate_index: usize,
    pub browse_index: usize,
    pub focus: Focus,
    pub mode: Mode,
    pub scanning: bool,
    pub show_archived: bool,
    pub message: Option<String>,
    pub should_quit: bool,
    pub chats: HashMap<String, ChatState>,
    pub visible_chat_id: Option<String>,
    pub resumed_threads: HashSet<String>,
    pub pending_approvals: VecDeque<AppServerRequest>,
    thread_registry: Registry,
    repository_store: RepositoryStore,
    workspaces_by_repository: HashMap<PathBuf, Vec<Workspace>>,
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
        let expanded_repositories = repositories
            .first()
            .map(|repository| HashSet::from([repository.path.clone()]))
            .unwrap_or_default();
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
            tree_index: 0,
            expanded_repositories,
            location_index: 0,
            thread_target_index: 0,
            candidate_index: 0,
            browse_index: 0,
            focus: Focus::Navigation,
            mode: if first_run {
                Mode::AddRepositories
            } else {
                Mode::Normal
            },
            scanning: false,
            show_archived: false,
            message: None,
            should_quit: false,
            chats: HashMap::new(),
            visible_chat_id: None,
            resumed_threads: HashSet::new(),
            pending_approvals: VecDeque::new(),
            thread_registry: Registry::discover()?,
            repository_store,
            workspaces_by_repository: HashMap::new(),
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

    pub fn chat(&self) -> Option<&ChatState> {
        self.visible_chat_id
            .as_ref()
            .and_then(|thread_id| self.chats.get(thread_id))
    }

    pub fn chat_mut(&mut self) -> Option<&mut ChatState> {
        let thread_id = self.visible_chat_id.clone()?;
        self.chats.get_mut(&thread_id)
    }

    pub fn show_chat(&mut self, chat: ChatState) {
        let thread_id = chat.thread_id.clone();
        self.chats.insert(thread_id.clone(), chat);
        self.visible_chat_id = Some(thread_id);
    }

    pub fn show_cached_chat(&mut self, thread_id: &str) -> bool {
        if self.chats.contains_key(thread_id) {
            self.visible_chat_id = Some(thread_id.to_owned());
            true
        } else {
            false
        }
    }

    pub fn apply_chat_event(&mut self, event: &AppServerEvent) {
        apply_chat_event_to(&mut self.chats, event);
    }

    pub fn discard_chat(&mut self, thread_id: &str) {
        self.chats.remove(thread_id);
        self.resumed_threads.remove(thread_id);
        if self.visible_chat_id.as_deref() == Some(thread_id) {
            self.visible_chat_id = None;
        }
    }

    pub fn tree_rows(&self) -> Vec<TreeRow> {
        tree_rows_for(
            &self.repositories,
            &self.threads,
            &self.expanded_repositories,
        )
    }

    pub fn selected_tree_row(&self) -> Option<TreeRow> {
        self.tree_rows().get(self.tree_index).copied()
    }

    pub fn selected_tree_is_repository(&self) -> bool {
        matches!(self.selected_tree_row(), Some(TreeRow::Repository { .. }))
    }

    pub fn selected_tree_is_thread(&self) -> bool {
        matches!(self.selected_tree_row(), Some(TreeRow::Thread { .. }))
    }

    pub fn repository_is_expanded(&self, repository_index: usize) -> bool {
        self.repositories
            .get(repository_index)
            .is_some_and(|repository| self.expanded_repositories.contains(&repository.path))
    }

    pub fn expand_selected_repository(&mut self) {
        let Some(repository) = self.selected_repository().cloned() else {
            return;
        };
        self.expanded_repositories.insert(repository.path);
    }

    pub fn collapse_selected_repository(&mut self) {
        let Some(repository) = self.selected_repository().cloned() else {
            return;
        };
        self.expanded_repositories.remove(&repository.path);
        self.select_repository_row(self.repository_index);
    }

    pub fn toggle_selected_repository(&mut self) {
        if self.repository_is_expanded(self.repository_index) {
            self.collapse_selected_repository();
        } else {
            self.expand_selected_repository();
        }
    }

    pub fn select_parent_repository(&mut self) {
        self.select_repository_row(self.repository_index);
    }

    pub fn primary_location(&self) -> Option<&Workspace> {
        self.locations.iter().find(|location| location.is_primary)
    }

    pub fn existing_worktrees(&self) -> Vec<&Workspace> {
        self.locations
            .iter()
            .filter(|location| !location.is_primary)
            .collect()
    }

    pub fn selected_existing_worktree(&self) -> Option<&Workspace> {
        self.existing_worktrees().get(self.location_index).copied()
    }

    pub fn create_generated_worktree(&mut self) -> Result<Workspace> {
        let repository = self
            .selected_repository()
            .cloned()
            .context("no repository selected")?;
        let workspace =
            git_workspace::create_generated_workspace(&repository.path, &repository.name)?;
        self.refresh_current();
        Ok(workspace)
    }

    pub fn selected_thread_has_clean_managed_worktree(&self) -> Result<bool> {
        let thread = self.selected_thread().context("no thread selected")?;
        if !thread.record.managed_worktree || !thread.record.cwd.is_dir() {
            return Ok(false);
        }
        git_workspace::workspace_is_clean(&thread.record.cwd)
    }

    pub fn archive_selected_thread(&mut self, remove_worktree: bool) -> Result<()> {
        let record = self
            .selected_thread()
            .map(|thread| thread.record.clone())
            .context("no thread selected")?;
        self.thread_registry.set_archived(&record.id, true)?;
        if remove_worktree
            && let Err(error) = git_workspace::remove_managed_workspace(
                &record.repository_path,
                &record.cwd,
                record.worktree_branch.as_deref(),
            )
        {
            self.refresh_current();
            return Err(error);
        }
        self.discard_chat(&record.id);
        self.refresh_current();
        Ok(())
    }

    pub fn unarchive_selected_thread(&mut self) -> Result<()> {
        let record = self
            .selected_thread()
            .map(|thread| thread.record.clone())
            .context("no thread selected")?;
        if record.managed_worktree && !record.cwd.exists() {
            git_workspace::restore_managed_workspace(
                &record.repository_path,
                &record.cwd,
                record.worktree_branch.as_deref(),
            )?;
        }
        self.thread_registry.set_archived(&record.id, false)?;
        self.refresh_current();
        Ok(())
    }

    pub fn toggle_archive_view(&mut self) {
        self.show_archived = !self.show_archived;
        self.thread_index = 0;
        self.refresh_current();
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
            Mode::ChooseThreadTarget => {
                self.thread_target_index = self.thread_target_index.saturating_sub(1)
            }
            Mode::ChooseExistingWorktree => {
                self.location_index = self.location_index.saturating_sub(1)
            }
            _ => {
                self.tree_index = self.tree_index.saturating_sub(1);
                self.sync_selection_from_tree();
            }
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
            Mode::ChooseThreadTarget => {
                if self.thread_target_index < 2 {
                    self.thread_target_index += 1;
                }
            }
            Mode::ChooseExistingWorktree => {
                if self.location_index + 1 < self.existing_worktrees().len() {
                    self.location_index += 1;
                }
            }
            _ => {
                let count = self.tree_rows().len();
                if self.tree_index + 1 < count {
                    self.tree_index += 1;
                    self.sync_selection_from_tree();
                }
            }
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
        let selection = match self.selected_tree_row() {
            Some(TreeRow::Repository { repository_index }) => self
                .repositories
                .get(repository_index)
                .map(|repository| TreeSelection::Repository(repository.path.clone())),
            Some(TreeRow::Thread { thread_index, .. }) => {
                self.threads
                    .get(thread_index)
                    .map(|thread| TreeSelection::Thread {
                        id: thread.record.id.clone(),
                        repository: thread.record.repository_path.clone(),
                    })
            }
            None => self
                .selected_repository()
                .map(|repository| TreeSelection::Repository(repository.path.clone())),
        };
        self.locations.clear();
        self.threads.clear();
        self.workspaces_by_repository.clear();
        if self.repositories.is_empty() {
            return;
        }

        for repository in &self.repositories {
            match git_workspace::list_workspaces(&repository.path) {
                Ok(locations) => {
                    self.workspaces_by_repository
                        .insert(repository.path.clone(), locations);
                }
                Err(error) => self.message = Some(error.to_string()),
            }
        }
        let registered_paths = self
            .repositories
            .iter()
            .map(|repository| repository.path.clone())
            .collect::<HashSet<_>>();
        match self.thread_registry.load() {
            Ok(records) => {
                self.threads = records
                    .into_iter()
                    .filter(|record| registered_paths.contains(&record.repository_path))
                    .filter(|record| record.archived_at.is_some() == self.show_archived)
                    .map(|mut record| {
                        let location = self
                            .workspaces_by_repository
                            .get(&record.repository_path)
                            .into_iter()
                            .flatten()
                            .filter(|location| record.cwd.starts_with(&location.path))
                            .max_by_key(|location| location.path.components().count());
                        if record.worktree_branch.is_none() {
                            record.worktree_branch = location.map(|location| location.name.clone());
                        }
                        record.managed_worktree |= git_workspace::is_managed_workspace(
                            &record.cwd,
                            record.worktree_branch.as_deref(),
                        );
                        ThreadItem {
                            location_name: location
                                .map(|location| location.name.clone())
                                .or_else(|| record.worktree_branch.clone())
                                .unwrap_or_else(|| "removed location".into()),
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
        self.tree_index = self
            .tree_index
            .min(self.tree_rows().len().saturating_sub(1));
        if let Some(selection) = selection {
            self.restore_tree_selection(selection);
        }
        self.sync_selection_from_tree();
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
        self.expanded_repositories.retain(|path| {
            self.repositories
                .iter()
                .any(|repository| repository.path == *path)
        });
        if self.expanded_repositories.is_empty()
            && let Some(repository) = self.repositories.get(self.repository_index)
        {
            self.expanded_repositories.insert(repository.path.clone());
        }
        self.select_repository_row(self.repository_index);
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
        let removed_thread_ids = self
            .threads
            .iter()
            .filter(|thread| thread.record.repository_path == path)
            .map(|thread| thread.record.id.clone())
            .collect::<Vec<_>>();
        for thread_id in removed_thread_ids {
            self.discard_chat(&thread_id);
        }
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
        self.discard_chat(&thread_id);
        self.refresh_current();
        Ok(())
    }

    pub fn register_app_server_thread(&mut self, thread_id: String, cwd: PathBuf) -> Result<()> {
        let repository = self
            .selected_repository()
            .cloned()
            .context("no repository selected")?;
        self.thread_registry
            .register_thread(thread_id, &repository.path, &cwd)?;
        self.refresh_current();
        Ok(())
    }

    pub fn update_thread_title(&mut self, thread_id: &str, prompt: &str) -> Result<()> {
        self.thread_registry.set_title(thread_id, prompt)?;
        if let Some(thread) = self
            .threads
            .iter_mut()
            .find(|thread| thread.record.id == thread_id)
            && thread.record.title == "Untitled thread"
        {
            thread.record.title = prompt
                .lines()
                .next()
                .unwrap_or(prompt)
                .chars()
                .take(80)
                .collect();
        }
        if let Some(chat) = self.chats.get_mut(thread_id)
            && chat.title == "Untitled thread"
        {
            chat.title = prompt
                .lines()
                .next()
                .unwrap_or(prompt)
                .chars()
                .take(80)
                .collect();
        }
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

    fn sync_selection_from_tree(&mut self) {
        match self.selected_tree_row() {
            Some(TreeRow::Repository { repository_index }) => {
                self.repository_index = repository_index;
            }
            Some(TreeRow::Thread {
                repository_index,
                thread_index,
            }) => {
                self.repository_index = repository_index;
                self.thread_index = thread_index;
            }
            None => {
                self.repository_index = 0;
                self.thread_index = 0;
            }
        }
        self.locations = self
            .selected_repository()
            .and_then(|repository| self.workspaces_by_repository.get(&repository.path))
            .cloned()
            .unwrap_or_default();
        self.location_index = self
            .location_index
            .min(self.existing_worktrees().len().saturating_sub(1));
    }

    fn select_repository_row(&mut self, repository_index: usize) {
        if let Some(index) = self.tree_rows().iter().position(
            |row| matches!(row, TreeRow::Repository { repository_index: index } if *index == repository_index),
        ) {
            self.tree_index = index;
            self.sync_selection_from_tree();
        }
    }

    fn restore_tree_selection(&mut self, selection: TreeSelection) {
        let (repository_path, thread_id) = match selection {
            TreeSelection::Repository(path) => (path, None),
            TreeSelection::Thread { id, repository } => (repository, Some(id)),
        };
        let Some(repository_index) = self
            .repositories
            .iter()
            .position(|repository| repository.path == repository_path)
        else {
            return;
        };
        if let Some(id) = thread_id
            && let Some(thread_index) = self
                .threads
                .iter()
                .position(|thread| thread.record.id == id)
        {
            self.expanded_repositories.insert(repository_path.clone());
            if let Some(index) = self.tree_rows().iter().position(|row| {
                matches!(
                    row,
                    TreeRow::Thread {
                        thread_index: index,
                        ..
                    } if *index == thread_index
                )
            }) {
                self.tree_index = index;
                return;
            }
        }
        self.select_repository_row(repository_index);
    }
}

fn tree_rows_for(
    repositories: &[Repository],
    threads: &[ThreadItem],
    expanded_repositories: &HashSet<PathBuf>,
) -> Vec<TreeRow> {
    let mut rows = Vec::with_capacity(repositories.len() + threads.len());
    for (repository_index, repository) in repositories.iter().enumerate() {
        rows.push(TreeRow::Repository { repository_index });
        if expanded_repositories.contains(&repository.path) {
            rows.extend(
                threads
                    .iter()
                    .enumerate()
                    .filter(|(_, thread)| thread.record.repository_path == repository.path)
                    .map(|(thread_index, _)| TreeRow::Thread {
                        repository_index,
                        thread_index,
                    }),
            );
        }
    }
    rows
}

fn apply_chat_event_to(chats: &mut HashMap<String, ChatState>, event: &AppServerEvent) {
    if event.method == "skills/changed" {
        for chat in chats.values_mut() {
            chat.skills_stale = true;
        }
    } else if let Some(thread_id) = &event.thread_id
        && let Some(chat) = chats.get_mut(thread_id)
    {
        chat.apply(event);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn thread(id: &str, repository: &str) -> ThreadItem {
        ThreadItem {
            record: ThreadRecord {
                id: id.into(),
                repository_path: repository.into(),
                cwd: format!("{repository}/{id}").into(),
                title: id.into(),
                created_at: 0,
                updated_at: 0,
                archived_at: None,
                managed_worktree: false,
                worktree_branch: None,
            },
            location_name: "primary".into(),
            is_primary: true,
        }
    }

    #[test]
    fn tree_groups_threads_below_each_expanded_repository() {
        let repositories = vec![
            Repository {
                name: "one".into(),
                path: "/one".into(),
            },
            Repository {
                name: "two".into(),
                path: "/two".into(),
            },
        ];
        let threads = vec![thread("two-thread", "/two"), thread("one-thread", "/one")];
        let expanded = HashSet::from([PathBuf::from("/one"), PathBuf::from("/two")]);

        assert_eq!(
            tree_rows_for(&repositories, &threads, &expanded),
            vec![
                TreeRow::Repository {
                    repository_index: 0,
                },
                TreeRow::Thread {
                    repository_index: 0,
                    thread_index: 1,
                },
                TreeRow::Repository {
                    repository_index: 1,
                },
                TreeRow::Thread {
                    repository_index: 1,
                    thread_index: 0,
                },
            ]
        );
    }

    #[test]
    fn collapsed_repository_hides_only_its_threads() {
        let repositories = vec![
            Repository {
                name: "one".into(),
                path: "/one".into(),
            },
            Repository {
                name: "two".into(),
                path: "/two".into(),
            },
        ];
        let threads = vec![thread("one-thread", "/one"), thread("two-thread", "/two")];
        let expanded = HashSet::from([PathBuf::from("/two")]);

        assert_eq!(
            tree_rows_for(&repositories, &threads, &expanded),
            vec![
                TreeRow::Repository {
                    repository_index: 0,
                },
                TreeRow::Repository {
                    repository_index: 1,
                },
                TreeRow::Thread {
                    repository_index: 1,
                    thread_index: 1,
                },
            ]
        );
    }

    #[test]
    fn app_server_events_update_only_their_thread_chat() {
        let mut chats = HashMap::from([
            (
                "one".into(),
                ChatState::new("one".into(), "/one".into(), "one".into()),
            ),
            (
                "two".into(),
                ChatState::new("two".into(), "/two".into(), "two".into()),
            ),
        ]);
        apply_chat_event_to(
            &mut chats,
            &AppServerEvent {
                method: "item/agentMessage/delta".into(),
                params: json!({"threadId":"two", "delta":"hello"}),
                thread_id: Some("two".into()),
                turn_id: Some("turn-two".into()),
            },
        );

        assert!(chats["one"].messages.is_empty());
        assert_eq!(chats["two"].messages.len(), 1);
        assert_eq!(chats["two"].messages[0].content, "hello");
    }

    #[test]
    fn skill_changes_invalidate_every_cached_chat() {
        let mut chats = HashMap::from([
            (
                "one".into(),
                ChatState::new("one".into(), "/one".into(), "one".into()),
            ),
            (
                "two".into(),
                ChatState::new("two".into(), "/two".into(), "two".into()),
            ),
        ]);
        apply_chat_event_to(
            &mut chats,
            &AppServerEvent {
                method: "skills/changed".into(),
                params: Value::Null,
                thread_id: None,
                turn_id: None,
            },
        );

        assert!(chats.values().all(|chat| chat.skills_stale));
    }
}
