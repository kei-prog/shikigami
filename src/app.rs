use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::PathBuf,
    sync::mpsc::{Receiver, TryRecvError},
};

use anyhow::{Context, Result};
use directories::BaseDirs;

use crate::{
    app_server::{AppServerEvent, AppServerRequest, ModelMetadata},
    chat::{ChatState, fuzzy_score},
    git_workspace::{self, Workspace},
    registry::{
        AttentionRegistry, PersistentAttentionKind, Registry, SideChatRegistry, ThreadRecord,
    },
    repository::{self, Repository, RepositoryStore, ScanEvent, ScanScope, start_scan},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    Navigation,
    Chat,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatPane {
    Main,
    Side,
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
    ChooseModel,
    ChooseReasoningEffort,
    ChooseSideChat,
    ChooseThread,
    Attention,
    ConfirmQuitSideChats,
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
pub enum AttentionKind {
    Completed,
    Failed,
    Approval,
}

impl AttentionKind {
    fn priority(self) -> u8 {
        match self {
            Self::Completed => 0,
            Self::Failed => 1,
            Self::Approval => 2,
        }
    }

    fn persistent(self) -> Option<PersistentAttentionKind> {
        match self {
            Self::Completed => Some(PersistentAttentionKind::Completed),
            Self::Failed => Some(PersistentAttentionKind::Failed),
            Self::Approval => None,
        }
    }

    fn from_persistent(kind: PersistentAttentionKind) -> Self {
        match kind {
            PersistentAttentionKind::Completed => Self::Completed,
            PersistentAttentionKind::Failed => Self::Failed,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionItem {
    pub thread_id: String,
    pub kind: AttentionKind,
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
    pub side_chat_id: Option<String>,
    pub side_chat_parent_id: Option<String>,
    pub side_chats_by_parent: HashMap<String, Vec<String>>,
    pub selected_side_chat_by_parent: HashMap<String, usize>,
    pub side_chat_picker_index: usize,
    pub side_chat_picker_original_index: usize,
    pub side_chat_picker_original_pane: ChatPane,
    pub thread_picker_query: String,
    pub thread_picker_index: usize,
    pub thread_picker_matches: Vec<usize>,
    pub active_chat_pane: ChatPane,
    pub resumed_threads: HashSet<String>,
    pub read_only_threads: HashSet<String>,
    pub pending_approvals: VecDeque<AppServerRequest>,
    pub attention_items: VecDeque<AttentionItem>,
    pub attention_index: usize,
    pub models: Vec<ModelMetadata>,
    pub model_index: usize,
    pub reasoning_effort_index: usize,
    pub reasoning_effort_returns_to_model: bool,
    thread_registry: Registry,
    side_chat_registry: SideChatRegistry,
    attention_registry: AttentionRegistry,
    repository_store: RepositoryStore,
    workspaces_by_repository: HashMap<PathBuf, Vec<Workspace>>,
    scan_receiver: Option<Receiver<ScanEvent>>,
}

impl App {
    pub fn load() -> Result<Self> {
        let repository_store = RepositoryStore::discover()?;
        let thread_registry = Registry::discover()?;
        let repositories = repository_store.load_registered()?;
        let candidates = repository_store.load_candidates().unwrap_or_default();
        let browse_path = BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"));
        let first_run = repositories.is_empty();
        let registered_paths = repositories
            .iter()
            .map(|repository| repository.path.clone())
            .collect::<HashSet<_>>();
        let (expanded_repositories, ui_state_error) =
            match repository_store.load_expanded_repositories() {
                Ok(Some(mut expanded)) => {
                    expanded.retain(|path| registered_paths.contains(path));
                    (expanded, None)
                }
                Ok(None) => (
                    repositories
                        .first()
                        .map(|repository| HashSet::from([repository.path.clone()]))
                        .unwrap_or_default(),
                    None,
                ),
                Err(error) => (
                    repositories
                        .first()
                        .map(|repository| HashSet::from([repository.path.clone()]))
                        .unwrap_or_default(),
                    Some(format!("Could not load repository view state: {error}")),
                ),
            };
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
            message: ui_state_error,
            should_quit: false,
            chats: HashMap::new(),
            visible_chat_id: None,
            side_chat_id: None,
            side_chat_parent_id: None,
            side_chats_by_parent: HashMap::new(),
            selected_side_chat_by_parent: HashMap::new(),
            side_chat_picker_index: 0,
            side_chat_picker_original_index: 0,
            side_chat_picker_original_pane: ChatPane::Main,
            thread_picker_query: String::new(),
            thread_picker_index: 0,
            thread_picker_matches: Vec::new(),
            active_chat_pane: ChatPane::Main,
            resumed_threads: HashSet::new(),
            read_only_threads: HashSet::new(),
            pending_approvals: VecDeque::new(),
            attention_items: VecDeque::new(),
            attention_index: 0,
            models: Vec::new(),
            model_index: 0,
            reasoning_effort_index: 0,
            reasoning_effort_returns_to_model: false,
            thread_registry,
            side_chat_registry: SideChatRegistry::discover()?,
            attention_registry: AttentionRegistry::discover()?,
            repository_store,
            workspaces_by_repository: HashMap::new(),
            scan_receiver: None,
        };
        app.refresh_current();
        if let Err(error) = app.restore_attention() {
            app.message = Some(format!("Could not restore attention list: {error}"));
        }
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
        self.active_chat_id()
            .and_then(|thread_id| self.chats.get(thread_id))
    }

    pub fn chat_mut(&mut self) -> Option<&mut ChatState> {
        let thread_id = self.active_chat_id()?.to_owned();
        self.chats.get_mut(&thread_id)
    }

    pub fn main_chat(&self) -> Option<&ChatState> {
        self.visible_chat_id
            .as_ref()
            .and_then(|thread_id| self.chats.get(thread_id))
    }

    pub fn side_chat(&self) -> Option<&ChatState> {
        self.side_chat_id
            .as_ref()
            .and_then(|thread_id| self.chats.get(thread_id))
    }

    pub fn has_side_chat(&self) -> bool {
        self.side_chat().is_some()
    }

    fn active_chat_id(&self) -> Option<&str> {
        match self.active_chat_pane {
            ChatPane::Main => self.visible_chat_id.as_deref(),
            ChatPane::Side => self
                .side_chat_id
                .as_deref()
                .or(self.visible_chat_id.as_deref()),
        }
    }

    pub fn show_chat(&mut self, chat: ChatState) {
        let thread_id = chat.thread_id.clone();
        self.chats.insert(thread_id.clone(), chat);
        self.show_main_chat_id(thread_id);
    }

    pub fn show_cached_chat(&mut self, thread_id: &str) -> bool {
        if self.chats.contains_key(thread_id) {
            self.show_main_chat_id(thread_id.to_owned());
            true
        } else {
            false
        }
    }

    pub fn show_side_chat(&mut self, parent_thread_id: String, chat: ChatState) -> Result<()> {
        let thread_id = chat.thread_id.clone();
        self.side_chat_registry
            .register(thread_id.clone(), parent_thread_id.clone())?;
        self.chats.insert(thread_id.clone(), chat);
        let side_chats = self
            .side_chats_by_parent
            .entry(parent_thread_id.clone())
            .or_default();
        side_chats.push(thread_id.clone());
        let index = side_chats.len() - 1;
        self.selected_side_chat_by_parent
            .insert(parent_thread_id.clone(), index);
        self.side_chat_id = Some(thread_id);
        self.side_chat_parent_id = Some(parent_thread_id);
        self.active_chat_pane = ChatPane::Side;
        Ok(())
    }

    pub fn complete_side_chat_deletion(&mut self, thread_id: &str) -> Result<()> {
        self.side_chat_registry.remove(thread_id)?;
        let Some(parent_thread_id) = self.side_chat_parent(thread_id) else {
            return Ok(());
        };
        let removed_was_visible = self.side_chat_id.as_deref() == Some(thread_id);
        self.chats.remove(thread_id);
        self.attention_items
            .retain(|item| item.thread_id != thread_id);
        self.resumed_threads.remove(thread_id);
        let mut remove_parent = false;
        if let Some(side_chats) = self.side_chats_by_parent.get_mut(&parent_thread_id) {
            let removed_index = side_chats.iter().position(|id| id == thread_id);
            side_chats.retain(|id| id != thread_id);
            if side_chats.is_empty() {
                remove_parent = true;
            } else {
                let previous_index = self
                    .selected_side_chat_by_parent
                    .get(&parent_thread_id)
                    .copied()
                    .unwrap_or(0);
                let index = if removed_index.is_some_and(|removed| removed < previous_index) {
                    previous_index - 1
                } else {
                    previous_index.min(side_chats.len() - 1)
                };
                self.selected_side_chat_by_parent
                    .insert(parent_thread_id.clone(), index);
                if removed_was_visible {
                    self.side_chat_id = side_chats.get(index).cloned();
                }
            }
        }
        if remove_parent {
            self.side_chats_by_parent.remove(&parent_thread_id);
            self.selected_side_chat_by_parent.remove(&parent_thread_id);
            if removed_was_visible {
                self.side_chat_id = None;
                self.side_chat_parent_id = None;
            }
        } else if removed_was_visible {
            self.side_chat_parent_id = Some(parent_thread_id);
        }
        if removed_was_visible {
            self.active_chat_pane = ChatPane::Main;
        }
        Ok(())
    }

    pub fn promote_side_chat(&mut self) -> Result<(String, Option<String>)> {
        if self.active_chat_pane != ChatPane::Side {
            anyhow::bail!("focus a side chat before promoting it");
        }
        let thread_id = self.side_chat_id.clone().context("no side chat selected")?;
        let parent_thread_id = self
            .side_chat_parent_id
            .clone()
            .context("side chat has no parent thread")?;
        let parent = self
            .threads
            .iter()
            .find(|thread| thread.record.id == parent_thread_id)
            .context("parent thread is not registered")?;
        let repository_path = parent.record.repository_path.clone();
        let (cwd, title) = self
            .chats
            .get(&thread_id)
            .map(|chat| (chat.cwd.clone(), chat.title.clone()))
            .context("side chat is not loaded")?;

        self.thread_registry.register_thread_named(
            thread_id.clone(),
            &repository_path,
            &cwd,
            &title,
        )?;
        let cleanup_warning = self
            .side_chat_registry
            .remove(&thread_id)
            .err()
            .map(|error| error.to_string());
        if let Some(chat) = self.chats.get_mut(&thread_id) {
            chat.mark_as_main_chat();
        }
        let mut remove_parent = false;
        if let Some(side_chats) = self.side_chats_by_parent.get_mut(&parent_thread_id) {
            side_chats.retain(|id| id != &thread_id);
            if side_chats.is_empty() {
                remove_parent = true;
            } else {
                let index = self
                    .selected_side_chat_by_parent
                    .get(&parent_thread_id)
                    .copied()
                    .unwrap_or(0)
                    .min(side_chats.len() - 1);
                self.selected_side_chat_by_parent
                    .insert(parent_thread_id.clone(), index);
            }
        }
        if remove_parent {
            self.side_chats_by_parent.remove(&parent_thread_id);
            self.selected_side_chat_by_parent.remove(&parent_thread_id);
        }
        self.refresh_current();
        if !self.reveal_chat(&thread_id) {
            anyhow::bail!("promoted thread is not available");
        }
        Ok((title, cleanup_warning))
    }

    pub fn abandoned_side_chat_ids(&self) -> Result<Vec<String>> {
        let registered = self
            .thread_registry
            .load()?
            .into_iter()
            .map(|thread| thread.id)
            .collect::<HashSet<_>>();
        self.side_chat_registry.reconcile(&registered)
    }

    pub fn forget_temporary_side_chat(&self, thread_id: &str) -> Result<()> {
        self.side_chat_registry.remove(thread_id)
    }

    pub fn side_chat_cleanup_targets(&self) -> Vec<(String, Option<String>)> {
        self.side_chats_by_parent
            .values()
            .flatten()
            .map(|thread_id| {
                let turn_id = self
                    .chats
                    .get(thread_id)
                    .and_then(|chat| chat.active_turn_id.clone());
                (thread_id.clone(), turn_id)
            })
            .collect()
    }

    pub fn current_side_chats(&self) -> Vec<&ChatState> {
        let Some(parent_thread_id) = self.visible_chat_id.as_deref() else {
            return Vec::new();
        };
        self.side_chats_by_parent
            .get(parent_thread_id)
            .into_iter()
            .flatten()
            .filter_map(|thread_id| self.chats.get(thread_id))
            .collect()
    }

    pub fn open_side_chat_picker(&mut self) {
        let Some(parent_thread_id) = self.visible_chat_id.as_deref() else {
            return;
        };
        let count = self
            .side_chats_by_parent
            .get(parent_thread_id)
            .map(Vec::len)
            .unwrap_or(0);
        if count == 0 {
            self.message = Some("No side chats for this thread".into());
            return;
        }
        self.side_chat_picker_index = self
            .selected_side_chat_by_parent
            .get(parent_thread_id)
            .copied()
            .unwrap_or(0)
            .min(count - 1);
        self.side_chat_picker_original_index = self.side_chat_picker_index;
        self.side_chat_picker_original_pane = self.active_chat_pane;
        self.preview_side_chat_picker();
        self.active_chat_pane = ChatPane::Side;
        self.mode = Mode::ChooseSideChat;
    }

    pub fn move_side_chat_picker_up(&mut self) {
        self.side_chat_picker_index = self.side_chat_picker_index.saturating_sub(1);
        self.preview_side_chat_picker();
    }

    pub fn move_side_chat_picker_down(&mut self) {
        let count = self.current_side_chats().len();
        if self.side_chat_picker_index + 1 < count {
            self.side_chat_picker_index += 1;
            self.preview_side_chat_picker();
        }
    }

    pub fn cancel_side_chat_picker(&mut self) {
        self.side_chat_picker_index = self.side_chat_picker_original_index;
        self.preview_side_chat_picker();
        self.active_chat_pane = self.side_chat_picker_original_pane;
        self.mode = Mode::Chat;
    }

    pub fn select_side_chat_from_picker(&mut self) {
        let Some(parent_thread_id) = self.visible_chat_id.clone() else {
            self.mode = Mode::Chat;
            return;
        };
        let Some(thread_id) = self
            .side_chats_by_parent
            .get(&parent_thread_id)
            .and_then(|side_chats| side_chats.get(self.side_chat_picker_index))
            .cloned()
        else {
            self.mode = Mode::Chat;
            return;
        };
        self.selected_side_chat_by_parent
            .insert(parent_thread_id.clone(), self.side_chat_picker_index);
        self.side_chat_id = Some(thread_id);
        self.side_chat_parent_id = Some(parent_thread_id);
        self.active_chat_pane = ChatPane::Side;
        self.focus = Focus::Chat;
        self.mode = Mode::Chat;
        self.mark_thread_seen();
    }

    pub fn open_thread_picker(&mut self) {
        self.thread_picker_query.clear();
        self.refresh_thread_picker_matches();
        self.thread_picker_index = self
            .visible_chat_id
            .as_deref()
            .and_then(|visible_id| {
                self.thread_picker_matches
                    .iter()
                    .position(|thread_index| self.threads[*thread_index].record.id == visible_id)
            })
            .unwrap_or(0);
        self.mode = Mode::ChooseThread;
    }

    pub fn thread_picker_threads(&self) -> impl Iterator<Item = &ThreadItem> {
        self.thread_picker_matches
            .iter()
            .filter_map(|index| self.threads.get(*index))
    }

    pub fn move_thread_picker_up(&mut self) {
        self.thread_picker_index = self.thread_picker_index.saturating_sub(1);
    }

    pub fn move_thread_picker_down(&mut self) {
        if self.thread_picker_index + 1 < self.thread_picker_matches.len() {
            self.thread_picker_index += 1;
        }
    }

    pub fn push_thread_picker_query(&mut self, character: char) {
        self.thread_picker_query.push(character);
        self.refresh_thread_picker_matches();
    }

    pub fn pop_thread_picker_query(&mut self) {
        self.thread_picker_query.pop();
        self.refresh_thread_picker_matches();
    }

    pub fn cancel_thread_picker(&mut self) {
        self.mode = Mode::Chat;
        self.focus = Focus::Chat;
    }

    pub fn activate_selected_thread_picker(&mut self) -> bool {
        let Some(thread_id) = self
            .thread_picker_matches
            .get(self.thread_picker_index)
            .and_then(|index| self.threads.get(*index))
            .map(|thread| thread.record.id.clone())
        else {
            return false;
        };
        self.reveal_chat(&thread_id)
    }

    pub fn repository_name_for_thread(&self, thread: &ThreadItem) -> &str {
        self.repositories
            .iter()
            .find(|repository| repository.path == thread.record.repository_path)
            .map(|repository| repository.name.as_str())
            .unwrap_or("unknown repository")
    }

    pub fn thread_picker_status(&self, thread_id: &str) -> &'static str {
        if self
            .chats
            .get(thread_id)
            .is_some_and(|chat| chat.active_turn_id.is_some())
        {
            "working"
        } else if self.read_only_threads.contains(thread_id) {
            "read-only"
        } else if self
            .attention_items
            .iter()
            .any(|item| item.thread_id == thread_id)
        {
            "attention"
        } else if self.visible_chat_id.as_deref() == Some(thread_id) {
            "current"
        } else {
            "ready"
        }
    }

    pub fn cycle_side_chat(&mut self, forward: bool) {
        if self.active_chat_pane != ChatPane::Side {
            return;
        }
        let Some(parent_thread_id) = self.visible_chat_id.clone() else {
            return;
        };
        let Some(side_chats) = self.side_chats_by_parent.get(&parent_thread_id) else {
            return;
        };
        let Some(current_thread_id) = self.side_chat_id.as_deref() else {
            return;
        };
        let current = side_chats
            .iter()
            .position(|thread_id| thread_id == current_thread_id)
            .unwrap_or(0);
        let next = cycle_index(current, side_chats.len(), forward);
        let Some(thread_id) = side_chats.get(next).cloned() else {
            return;
        };
        self.selected_side_chat_by_parent
            .insert(parent_thread_id.clone(), next);
        self.side_chat_id = Some(thread_id);
        self.side_chat_parent_id = Some(parent_thread_id);
        self.mark_thread_seen();
    }

    pub fn current_side_chat_position(&self) -> Option<(usize, usize)> {
        let parent_thread_id = self.visible_chat_id.as_deref()?;
        let side_chats = self.side_chats_by_parent.get(parent_thread_id)?;
        let current_thread_id = self.side_chat_id.as_deref()?;
        let index = side_chats
            .iter()
            .position(|thread_id| thread_id == current_thread_id)?;
        Some((index + 1, side_chats.len()))
    }

    fn preview_side_chat_picker(&mut self) {
        let Some(parent_thread_id) = self.visible_chat_id.clone() else {
            return;
        };
        let Some(thread_id) = self
            .side_chats_by_parent
            .get(&parent_thread_id)
            .and_then(|side_chats| side_chats.get(self.side_chat_picker_index))
            .cloned()
        else {
            return;
        };
        self.side_chat_id = Some(thread_id);
        self.side_chat_parent_id = Some(parent_thread_id);
    }

    pub fn side_chat_count(&self) -> usize {
        self.side_chats_by_parent.values().map(Vec::len).sum()
    }

    pub fn toggle_chat_pane(&mut self) {
        if self.has_side_chat() {
            self.active_chat_pane = match self.active_chat_pane {
                ChatPane::Main => ChatPane::Side,
                ChatPane::Side => ChatPane::Main,
            };
        }
    }

    pub fn visible_chat_has_active_turn(&self) -> bool {
        self.main_chat()
            .is_some_and(|chat| chat.active_turn_id.is_some())
            || self
                .side_chat()
                .is_some_and(|chat| chat.active_turn_id.is_some())
    }

    pub fn has_active_turn(&self) -> bool {
        self.chats
            .values()
            .any(|chat| chat.active_turn_id.is_some())
    }

    pub fn thread_is_registered(&self, thread_id: &str) -> bool {
        self.threads
            .iter()
            .any(|thread| thread.record.id == thread_id)
    }

    pub fn active_chat_is_read_only(&self) -> bool {
        self.active_chat_id()
            .is_some_and(|thread_id| self.read_only_threads.contains(thread_id))
    }

    pub fn set_thread_read_only(&mut self, thread_id: &str, read_only: bool) {
        if read_only {
            self.read_only_threads.insert(thread_id.to_owned());
        } else {
            self.read_only_threads.remove(thread_id);
        }
    }

    pub fn set_models(&mut self, models: Vec<ModelMetadata>) {
        self.model_index = models
            .iter()
            .position(|model| model.is_default)
            .unwrap_or(0);
        self.models = models;
        self.reasoning_effort_index = self
            .selected_model()
            .and_then(|model| {
                let default = preferred_reasoning_effort(model)?;
                model
                    .supported_reasoning_efforts
                    .iter()
                    .position(|effort| effort.reasoning_effort == default)
            })
            .unwrap_or(0);
    }

    pub fn default_model_settings(&self) -> Option<(String, String, Option<String>)> {
        let model = self
            .models
            .iter()
            .find(|model| model.is_default)
            .or_else(|| self.models.first())?;
        Some((
            model.model.clone(),
            model.display_name.clone(),
            preferred_reasoning_effort(model).map(str::to_owned),
        ))
    }

    pub fn open_model_picker(&mut self) {
        let current_model = self.chat().and_then(|chat| chat.model.clone());
        self.model_index = current_model
            .as_deref()
            .and_then(|current| self.models.iter().position(|model| model.model == current))
            .or_else(|| self.models.iter().position(|model| model.is_default))
            .unwrap_or(0);
        self.sync_reasoning_effort_index();
        self.mode = Mode::ChooseModel;
    }

    pub fn open_current_reasoning_effort_picker(&mut self) {
        let current_model = self.chat().and_then(|chat| chat.model.clone());
        self.model_index = current_model
            .as_deref()
            .and_then(|current| self.models.iter().position(|model| model.model == current))
            .or_else(|| self.models.iter().position(|model| model.is_default))
            .unwrap_or(0);
        self.sync_reasoning_effort_index();
        self.reasoning_effort_returns_to_model = false;
        self.mode = Mode::ChooseReasoningEffort;
    }

    pub fn open_selected_reasoning_effort_picker(&mut self) {
        self.sync_reasoning_effort_index();
        self.reasoning_effort_returns_to_model = true;
        self.mode = Mode::ChooseReasoningEffort;
    }

    pub fn cancel_reasoning_effort_picker(&mut self) {
        self.mode = if self.reasoning_effort_returns_to_model {
            Mode::ChooseModel
        } else {
            Mode::Chat
        };
        self.reasoning_effort_returns_to_model = false;
    }

    pub fn selected_model(&self) -> Option<&ModelMetadata> {
        self.models.get(self.model_index)
    }

    pub fn move_model_up(&mut self) {
        self.model_index = self.model_index.saturating_sub(1);
        self.sync_reasoning_effort_index();
    }

    pub fn move_model_down(&mut self) {
        if self.model_index + 1 < self.models.len() {
            self.model_index += 1;
            self.sync_reasoning_effort_index();
        }
    }

    pub fn move_reasoning_effort_up(&mut self) {
        self.reasoning_effort_index = self.reasoning_effort_index.saturating_sub(1);
    }

    pub fn move_reasoning_effort_down(&mut self) {
        let count = self
            .selected_model()
            .map(|model| model.supported_reasoning_efforts.len())
            .unwrap_or(0);
        if self.reasoning_effort_index + 1 < count {
            self.reasoning_effort_index += 1;
        }
    }

    pub fn apply_selected_model(&mut self) {
        let Some(model) = self.selected_model().cloned() else {
            self.mode = Mode::Chat;
            return;
        };
        let effort = model
            .supported_reasoning_efforts
            .get(self.reasoning_effort_index)
            .map(|effort| effort.reasoning_effort.clone())
            .or_else(|| preferred_reasoning_effort(&model).map(str::to_owned));
        if let Some(chat) = self.chat_mut() {
            chat.set_model(model.model, model.display_name, effort);
        }
        self.reasoning_effort_returns_to_model = false;
        self.mode = Mode::Chat;
    }

    fn sync_reasoning_effort_index(&mut self) {
        let current_effort = self.chat().and_then(|chat| chat.reasoning_effort.clone());
        self.reasoning_effort_index = self
            .selected_model()
            .and_then(|model| {
                current_effort
                    .as_deref()
                    .and_then(|current| {
                        model
                            .supported_reasoning_efforts
                            .iter()
                            .position(|effort| effort.reasoning_effort == current)
                    })
                    .or_else(|| {
                        let default = preferred_reasoning_effort(model)?;
                        model
                            .supported_reasoning_efforts
                            .iter()
                            .position(|effort| effort.reasoning_effort == default)
                    })
            })
            .unwrap_or(0);
    }

    pub fn apply_chat_event(&mut self, event: &AppServerEvent) {
        let thread_id = event.thread_id.clone();
        let was_visible = thread_id
            .as_deref()
            .is_some_and(|thread_id| self.thread_is_visible(thread_id));
        apply_chat_event_to(&mut self.chats, event);
        let attention_changed = if !was_visible
            && let (Some(thread_id), Some(kind)) = (thread_id, attention_kind_for_event(event))
            && self.chats.contains_key(&thread_id)
        {
            upsert_attention(&mut self.attention_items, thread_id, kind)
        } else {
            false
        };
        self.clamp_attention_index();
        if attention_changed {
            self.persist_attention();
        }
    }

    pub fn enqueue_approval(&mut self, request: AppServerRequest) {
        if let Some(thread_id) = request.thread_id.clone() {
            upsert_attention(
                &mut self.attention_items,
                thread_id,
                AttentionKind::Approval,
            );
        }
        self.pending_approvals.push_back(request);
        self.clamp_attention_index();
        self.persist_attention();
    }

    pub fn approval_resolved(&mut self, thread_id: Option<&str>) {
        let Some(thread_id) = thread_id else {
            return;
        };
        let still_pending = self
            .pending_approvals
            .iter()
            .any(|request| request.thread_id.as_deref() == Some(thread_id));
        if !still_pending {
            self.attention_items
                .retain(|item| item.thread_id != thread_id || item.kind != AttentionKind::Approval);
        }
        self.clamp_attention_index();
        self.persist_attention();
    }

    pub fn attention_count(&self) -> usize {
        self.attention_items.len()
    }

    pub fn repository_attention_counts(&self) -> HashMap<PathBuf, usize> {
        let mut repositories_by_thread = self
            .threads
            .iter()
            .map(|thread| {
                (
                    thread.record.id.as_str(),
                    thread.record.repository_path.clone(),
                )
            })
            .collect::<HashMap<_, _>>();
        for (parent_id, side_chats) in &self.side_chats_by_parent {
            let Some(repository_path) = repositories_by_thread.get(parent_id.as_str()).cloned()
            else {
                continue;
            };
            for thread_id in side_chats {
                repositories_by_thread.insert(thread_id, repository_path.clone());
            }
        }
        let mut counts = HashMap::new();
        for item in &self.attention_items {
            if let Some(repository_path) = repositories_by_thread.get(item.thread_id.as_str()) {
                *counts.entry(repository_path.clone()).or_insert(0) += 1;
            }
        }
        counts
    }

    pub fn open_attention(&mut self) {
        if self.attention_items.is_empty() {
            self.message = Some("Nothing needs attention".into());
            return;
        }
        self.attention_index = self
            .attention_index
            .min(self.attention_items.len().saturating_sub(1));
        self.mode = Mode::Attention;
    }

    pub fn close_attention(&mut self) {
        self.mode = if self.chat().is_some() && self.focus == Focus::Chat {
            Mode::Chat
        } else {
            Mode::Normal
        };
    }

    pub fn move_attention_up(&mut self) {
        self.attention_index = self.attention_index.saturating_sub(1);
    }

    pub fn move_attention_down(&mut self) {
        if self.attention_index + 1 < self.attention_items.len() {
            self.attention_index += 1;
        }
    }

    pub fn dismiss_selected_attention(&mut self) {
        let Some(item) = self.attention_items.get(self.attention_index) else {
            self.close_attention();
            return;
        };
        if item.kind == AttentionKind::Approval {
            self.message = Some("Accept or decline the pending approval".into());
            return;
        }
        self.attention_items.remove(self.attention_index);
        self.clamp_attention_index();
        self.persist_attention();
        if self.attention_items.is_empty() {
            self.close_attention();
        }
    }

    pub fn activate_selected_attention(&mut self) {
        let Some(item) = self.attention_items.get(self.attention_index).cloned() else {
            self.close_attention();
            return;
        };
        if item.kind == AttentionKind::Approval {
            if let Some(index) = self
                .pending_approvals
                .iter()
                .position(|request| request.thread_id.as_deref() == Some(item.thread_id.as_str()))
                && let Some(request) = self.pending_approvals.remove(index)
            {
                self.pending_approvals.push_front(request);
            }
            self.reveal_chat(&item.thread_id);
            self.mode = Mode::Approval;
            return;
        }
        self.attention_items.remove(self.attention_index);
        self.clamp_attention_index();
        self.persist_attention();
        if !self.reveal_chat(&item.thread_id) {
            self.message = Some("The selected thread is no longer available".into());
            self.close_attention();
        }
    }

    pub fn discard_chat(&mut self, thread_id: &str) {
        self.discard_side_chats_for_parent(thread_id);
        self.chats.remove(thread_id);
        self.attention_items
            .retain(|item| item.thread_id != thread_id);
        self.persist_attention();
        self.resumed_threads.remove(thread_id);
        self.read_only_threads.remove(thread_id);
        if self.visible_chat_id.as_deref() == Some(thread_id) {
            self.visible_chat_id = None;
        }
    }

    fn show_main_chat_id(&mut self, thread_id: String) {
        self.visible_chat_id = Some(thread_id.clone());
        let selected = self
            .selected_side_chat_by_parent
            .get(&thread_id)
            .copied()
            .unwrap_or(0);
        self.side_chat_id = self
            .side_chats_by_parent
            .get(&thread_id)
            .and_then(|side_chats| side_chats.get(selected))
            .cloned();
        self.side_chat_parent_id = self.side_chat_id.as_ref().map(|_| thread_id);
        self.active_chat_pane = ChatPane::Main;
        self.mark_thread_seen();
    }

    fn reveal_chat(&mut self, thread_id: &str) -> bool {
        if let Some(thread) = self
            .threads
            .iter()
            .find(|thread| thread.record.id == thread_id)
        {
            let selection = TreeSelection::Thread {
                id: thread.record.id.clone(),
                repository: thread.record.repository_path.clone(),
            };
            self.restore_tree_selection(selection);
            self.sync_selection_from_tree();
            self.show_main_chat_id(thread_id.to_owned());
            self.focus = Focus::Chat;
            self.mode = Mode::Chat;
            return true;
        }
        let Some(parent_thread_id) = self.side_chat_parent(thread_id) else {
            return false;
        };
        if !self.reveal_chat(&parent_thread_id) {
            return false;
        }
        let Some(index) = self
            .side_chats_by_parent
            .get(&parent_thread_id)
            .and_then(|side_chats| side_chats.iter().position(|id| id == thread_id))
        else {
            return false;
        };
        self.selected_side_chat_by_parent
            .insert(parent_thread_id.clone(), index);
        self.side_chat_id = Some(thread_id.to_owned());
        self.side_chat_parent_id = Some(parent_thread_id);
        self.active_chat_pane = ChatPane::Side;
        self.mark_thread_seen();
        true
    }

    fn side_chat_parent(&self, thread_id: &str) -> Option<String> {
        self.side_chats_by_parent
            .iter()
            .find(|(_, side_chats)| side_chats.iter().any(|id| id == thread_id))
            .map(|(parent_id, _)| parent_id.clone())
    }

    fn thread_is_visible(&self, thread_id: &str) -> bool {
        self.visible_chat_id.as_deref() == Some(thread_id)
            || self.side_chat_id.as_deref() == Some(thread_id)
    }

    fn mark_thread_seen(&mut self) {
        let previous_len = self.attention_items.len();
        let visible = [
            self.visible_chat_id.as_deref(),
            self.side_chat_id.as_deref(),
        ];
        self.attention_items.retain(|item| {
            item.kind == AttentionKind::Approval
                || !visible.into_iter().flatten().any(|id| id == item.thread_id)
        });
        self.clamp_attention_index();
        if self.attention_items.len() != previous_len {
            self.persist_attention();
        }
    }

    fn restore_attention(&mut self) -> Result<()> {
        let registered = self
            .threads
            .iter()
            .map(|thread| thread.record.id.clone())
            .collect::<HashSet<_>>();
        self.attention_items = self
            .attention_registry
            .reconcile(&registered)?
            .into_iter()
            .map(|item| AttentionItem {
                thread_id: item.thread_id,
                kind: AttentionKind::from_persistent(item.kind),
            })
            .collect();
        self.clamp_attention_index();
        Ok(())
    }

    fn persist_attention(&mut self) {
        let desired = self
            .attention_items
            .iter()
            .filter(|item| self.thread_is_registered(&item.thread_id))
            .filter_map(|item| Some((item.thread_id.clone(), item.kind.persistent()?)))
            .collect::<Vec<_>>();
        if let Err(error) = self.attention_registry.sync(&desired) {
            self.message = Some(format!("Could not save attention list: {error}"));
        }
    }

    fn clamp_attention_index(&mut self) {
        self.attention_index = self
            .attention_index
            .min(self.attention_items.len().saturating_sub(1));
    }

    fn discard_side_chats_for_parent(&mut self, parent_thread_id: &str) {
        if let Some(side_chats) = self.side_chats_by_parent.remove(parent_thread_id) {
            self.attention_items
                .retain(|item| !side_chats.contains(&item.thread_id));
            for thread_id in side_chats {
                self.chats.remove(&thread_id);
                self.resumed_threads.remove(&thread_id);
            }
            self.clamp_attention_index();
        }
        self.selected_side_chat_by_parent.remove(parent_thread_id);
        if self.side_chat_parent_id.as_deref() == Some(parent_thread_id) {
            self.side_chat_id = None;
            self.side_chat_parent_id = None;
            self.active_chat_pane = ChatPane::Main;
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
        self.persist_expanded_repositories();
    }

    pub fn collapse_selected_repository(&mut self) {
        let Some(repository) = self.selected_repository().cloned() else {
            return;
        };
        self.expanded_repositories.remove(&repository.path);
        self.select_repository_row(self.repository_index);
        self.persist_expanded_repositories();
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
        let had_repositories = !self.repositories.is_empty();
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
        if !had_repositories
            && self.expanded_repositories.is_empty()
            && let Some(repository) = self.repositories.get(self.repository_index)
        {
            self.expanded_repositories.insert(repository.path.clone());
        }
        self.repository_store
            .save_expanded_repositories(&self.expanded_repositories)?;
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

    fn persist_expanded_repositories(&mut self) {
        if let Err(error) = self
            .repository_store
            .save_expanded_repositories(&self.expanded_repositories)
        {
            self.message = Some(format!("Could not save repository view state: {error}"));
        }
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

    fn refresh_thread_picker_matches(&mut self) {
        self.thread_picker_matches =
            thread_picker_matches(&self.threads, &self.repositories, &self.thread_picker_query);
        self.thread_picker_index = 0;
    }
}

fn thread_picker_matches(
    threads: &[ThreadItem],
    repositories: &[Repository],
    query: &str,
) -> Vec<usize> {
    let mut matches = threads
        .iter()
        .enumerate()
        .filter_map(|(index, thread)| {
            let repository_name = repositories
                .iter()
                .find(|repository| repository.path == thread.record.repository_path)
                .map(|repository| repository.name.as_str())
                .unwrap_or_default();
            let score = [
                fuzzy_score(&thread.record.title, query),
                fuzzy_score(repository_name, query).map(|score| score - 1_000),
                fuzzy_score(&thread.location_name, query).map(|score| score - 1_500),
                fuzzy_score(&thread.record.cwd.to_string_lossy(), query).map(|score| score - 2_000),
            ]
            .into_iter()
            .flatten()
            .max()?;
            Some((index, score))
        })
        .collect::<Vec<_>>();
    if !query.is_empty() {
        matches.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    }
    matches.into_iter().map(|(index, _)| index).collect()
}

fn preferred_reasoning_effort(model: &ModelMetadata) -> Option<&str> {
    model
        .supported_reasoning_efforts
        .iter()
        .find(|effort| effort.reasoning_effort == "medium")
        .or_else(|| {
            model
                .supported_reasoning_efforts
                .iter()
                .find(|effort| effort.reasoning_effort == model.default_reasoning_effort)
        })
        .or_else(|| model.supported_reasoning_efforts.first())
        .map(|effort| effort.reasoning_effort.as_str())
}

fn cycle_index(current: usize, count: usize, forward: bool) -> usize {
    if count == 0 {
        return 0;
    }
    if forward {
        (current + 1) % count
    } else {
        current.checked_sub(1).unwrap_or(count - 1)
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

fn attention_kind_for_event(event: &AppServerEvent) -> Option<AttentionKind> {
    match event.method.as_str() {
        "error" => Some(AttentionKind::Failed),
        "turn/completed" => {
            let status = event
                .params
                .pointer("/turn/status")
                .or_else(|| event.params.get("status"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("completed");
            if matches!(status, "failed" | "error")
                || event
                    .params
                    .pointer("/turn/error")
                    .is_some_and(|error| !error.is_null())
            {
                Some(AttentionKind::Failed)
            } else {
                Some(AttentionKind::Completed)
            }
        }
        _ => None,
    }
}

fn upsert_attention(
    items: &mut VecDeque<AttentionItem>,
    thread_id: String,
    kind: AttentionKind,
) -> bool {
    if let Some(item) = items.iter_mut().find(|item| item.thread_id == thread_id) {
        if kind.priority() > item.kind.priority() {
            item.kind = kind;
            return true;
        }
        return false;
    }
    items.push_back(AttentionItem { thread_id, kind });
    true
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

    fn model(default: &str, efforts: &[&str]) -> ModelMetadata {
        ModelMetadata {
            id: "test".into(),
            model: "test".into(),
            display_name: "Test".into(),
            description: String::new(),
            default_reasoning_effort: default.into(),
            supported_reasoning_efforts: efforts
                .iter()
                .map(|effort| crate::app_server::ReasoningEffortMetadata {
                    reasoning_effort: (*effort).into(),
                    description: String::new(),
                })
                .collect(),
            is_default: true,
        }
    }

    #[test]
    fn medium_is_the_preferred_reasoning_effort() {
        let model = model("low", &["low", "medium", "high"]);
        assert_eq!(preferred_reasoning_effort(&model), Some("medium"));
    }

    #[test]
    fn model_default_is_used_when_medium_is_unavailable() {
        let model = model("low", &["low", "high"]);
        assert_eq!(preferred_reasoning_effort(&model), Some("low"));
    }

    #[test]
    fn side_chat_cycle_wraps_in_both_directions() {
        assert_eq!(cycle_index(0, 3, true), 1);
        assert_eq!(cycle_index(2, 3, true), 0);
        assert_eq!(cycle_index(2, 3, false), 1);
        assert_eq!(cycle_index(0, 3, false), 2);
        assert_eq!(cycle_index(0, 0, true), 0);
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
    fn thread_picker_fuzzy_search_prioritizes_title_matches() {
        let repositories = vec![
            Repository {
                name: "frontend".into(),
                path: "/frontend".into(),
            },
            Repository {
                name: "backend".into(),
                path: "/backend".into(),
            },
        ];
        let threads = vec![
            thread("code-review", "/frontend"),
            thread("review", "/backend"),
        ];

        assert_eq!(
            thread_picker_matches(&threads, &repositories, "rev"),
            vec![1, 0]
        );
        assert_eq!(
            thread_picker_matches(&threads, &repositories, "backend"),
            vec![1]
        );
    }

    #[test]
    fn thread_picker_keeps_recent_order_without_a_query() {
        let threads = vec![thread("newest", "/one"), thread("older", "/one")];

        assert_eq!(thread_picker_matches(&threads, &[], ""), vec![0, 1]);
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

    #[test]
    fn attention_queue_deduplicates_and_keeps_the_more_urgent_state() {
        let mut items = VecDeque::new();
        upsert_attention(&mut items, "one".into(), AttentionKind::Completed);
        upsert_attention(&mut items, "one".into(), AttentionKind::Failed);
        upsert_attention(&mut items, "one".into(), AttentionKind::Completed);

        assert_eq!(
            items,
            VecDeque::from([AttentionItem {
                thread_id: "one".into(),
                kind: AttentionKind::Failed,
            }])
        );
    }

    #[test]
    fn failed_turns_are_classified_for_attention() {
        let event = AppServerEvent {
            method: "turn/completed".into(),
            params: json!({"turn":{"status":"failed","error":{"message":"boom"}}}),
            thread_id: Some("one".into()),
            turn_id: Some("turn-one".into()),
        };

        assert_eq!(
            attention_kind_for_event(&event),
            Some(AttentionKind::Failed)
        );
    }
}
