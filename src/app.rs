use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{Receiver, TryRecvError},
    },
    time::Instant,
};

const THREAD_NAME_MODEL: &str = "gpt-5.6-luna";
const MAX_ARCHIVE_UNDOS: usize = 20;
static NEXT_DRAFT_THREAD_ID: AtomicU64 = AtomicU64::new(1);

use anyhow::{Context, Result};
use directories::BaseDirs;

use crate::{
    app_server::{AppServerEvent, AppServerRequest, ModelMetadata},
    chat::{ChatState, CommandPalette, fuzzy_score},
    codex_workspace,
    git_workspace::{self, Workspace},
    keybindings::KeyBindings,
    onboarding::{self, OnboardingStore},
    paths,
    performance::PerformanceSession,
    registry::{
        AttentionRegistry, PersistentAttentionKind, Registry, SideChatRecord, SideChatRegistry,
        ThreadKind, ThreadRecord, ThreadScope, ThreadTitleCache,
    },
    repository::{self, Repository, RepositoryStore, ScanEvent, ScanScope, start_scan},
    settings::{ExecutionMode, SettingsStore},
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
    ConfirmDeleteThread,
    DeletingThread,
    Chat,
    ChooseModel,
    ChooseReasoningEffort,
    ChoosePermissions,
    ConfirmDangerous,
    ChooseSideChat,
    ChooseThread,
    ChooseRenameAction,
    RenameThread,
    BulkRenameThreads,
    Attention,
    ConfirmQuit,
    Approval,
    Help,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadDeletionPhase {
    CheckingWorktree,
    DeletingHistory,
    RemovingWorktree,
}

#[derive(Clone, Debug)]
pub struct ThreadDeletionState {
    pub title: String,
    pub phase: ThreadDeletionPhase,
    pub started_at: Instant,
}

pub struct ThreadDeletionRequest {
    pub record: ThreadRecord,
    pub side_chat_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct DraftWorkspaceCleanup {
    pub scope: ThreadScope,
    pub repository_path: PathBuf,
    pub workspace_path: PathBuf,
}

#[derive(Clone, Debug)]
struct DraftThread {
    scope: ThreadScope,
    kind: ThreadKind,
    repository_path: PathBuf,
    workspace_path: PathBuf,
    cleanup_workspace_on_cancel: bool,
}

#[derive(Clone, Debug)]
pub struct PendingOnboarding {
    pub draft_id: String,
    pub locale: String,
    pub imported_repository_count: usize,
}

#[derive(Clone, Debug)]
pub struct ThreadItem {
    pub record: ThreadRecord,
    pub location_name: String,
    pub is_primary: bool,
}

pub struct PreparedThreadWorkspace {
    pub repository: Option<Repository>,
    pub workspace: Workspace,
    pub scope: ThreadScope,
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
pub enum BulkRenamePhase {
    Select,
    Generating { return_to_review: bool },
    Review,
    Editing,
    ConfirmApply,
    Applying,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BulkRenameProgress {
    Reading { completed: usize, total: usize },
    WaitingForCodex,
    Applying { completed: usize, total: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenameAction {
    RenameThread,
    SuggestThread,
    SuggestRepository,
    SuggestAll,
}

impl RenameAction {
    pub const ALL: [Self; 4] = [
        Self::RenameThread,
        Self::SuggestThread,
        Self::SuggestRepository,
        Self::SuggestAll,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::RenameThread => "Rename selected thread",
            Self::SuggestThread => "Suggest name for selected thread",
            Self::SuggestRepository => "Suggest names in this repository",
            Self::SuggestAll => "Suggest names for all threads",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RenameActionState {
    pub index: usize,
    pub return_mode: Mode,
    pub thread_id: Option<String>,
    pub repository_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BulkRenameCandidate {
    pub thread_id: String,
    pub repository_name: String,
    pub current_name: String,
    pub proposed_name: String,
    pub selected: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct BulkRenameState {
    pub scope_name: String,
    pub repository_path: PathBuf,
    pub show_repository_names: bool,
    pub requires_apply_confirmation: bool,
    pub return_mode: Mode,
    pub candidates: Vec<BulkRenameCandidate>,
    pub index: usize,
    pub phase: BulkRenamePhase,
    pub progress: Option<BulkRenameProgress>,
    pub progress_started_at: Instant,
    pub edit_input: String,
    generating_ids: HashSet<String>,
}

#[derive(Clone, Debug)]
pub struct ThreadNameGenerationRequest {
    pub repository_path: PathBuf,
    pub threads: Vec<(String, String)>,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ThreadNameApplyRequest {
    pub names: Vec<(String, String)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeRow {
    General,
    GeneralThread {
        thread_index: usize,
    },
    Repository {
        repository_index: usize,
    },
    Thread {
        repository_index: usize,
        thread_index: usize,
    },
}

#[derive(PartialEq)]
enum TreeSelection {
    General,
    Repository(PathBuf),
    Thread(String),
}

#[derive(Clone)]
struct ArchivedThreadUndo {
    id: String,
    title: String,
}

pub struct App {
    pub performance: Arc<PerformanceSession>,
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
    pub command_palette: Option<CommandPalette>,
    pub thread_deletion: Option<ThreadDeletionState>,
    pub scanning: bool,
    pub show_archived: bool,
    archived_thread_undos: VecDeque<ArchivedThreadUndo>,
    pub message: Option<String>,
    pub should_quit: bool,
    pub chats: HashMap<String, ChatState>,
    draft_threads: HashMap<String, DraftThread>,
    pending_onboarding: Option<PendingOnboarding>,
    preview_cache_order: VecDeque<String>,
    pub visible_chat_id: Option<String>,
    previous_visible_chat_id: Option<String>,
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
    pub thread_picker_original_focus: Focus,
    pub rename_thread_id: Option<String>,
    pub rename_input: String,
    pub rename_return_mode: Mode,
    pub rename_actions: Option<RenameActionState>,
    pub bulk_rename: Option<BulkRenameState>,
    pub active_chat_pane: ChatPane,
    pub resumed_threads: HashSet<String>,
    opened_threads: HashSet<String>,
    pub read_only_threads: HashSet<String>,
    owned_turns: HashMap<String, String>,
    pub pending_approvals: VecDeque<AppServerRequest>,
    pub approval_index: usize,
    pub attention_items: VecDeque<AttentionItem>,
    pub attention_index: usize,
    pub models: Vec<ModelMetadata>,
    pub model_index: usize,
    pub reasoning_effort_index: usize,
    pub reasoning_effort_returns_to_model: bool,
    pub execution_mode: ExecutionMode,
    pub keybindings: KeyBindings,
    pub permission_index: usize,
    thread_names: HashMap<String, Option<String>>,
    // Cached titles are displayed immediately but do not become authoritative until observed live.
    confirmed_thread_names: HashSet<String>,
    thread_title_cache: ThreadTitleCache,
    thread_registry: Registry,
    side_chat_registry: SideChatRegistry,
    attention_registry: AttentionRegistry,
    repository_store: RepositoryStore,
    settings_store: SettingsStore,
    workspaces_by_repository: HashMap<PathBuf, Vec<Workspace>>,
    scan_receiver: Option<Receiver<ScanEvent>>,
    scan_label: Option<&'static str>,
    scan_metric_scope: Option<&'static str>,
    scan_animation_tick: usize,
    scan_started_at: Option<Instant>,
    scan_first_result_recorded: bool,
    scan_found_count: usize,
    initial_home_scan_in_progress: bool,
}

impl App {
    pub fn load(performance: Arc<PerformanceSession>) -> Result<Self> {
        let repository_store = RepositoryStore::discover()?;
        let settings_store = SettingsStore::discover()?;
        let onboarding_store = OnboardingStore::discover()?;
        let (execution_mode, settings_error) = match settings_store.load() {
            Ok(mode) => (mode, None),
            Err(error) => (
                ExecutionMode::Auto,
                Some(format!("Could not load settings; using Auto: {error}")),
            ),
        };
        let (keybindings, keybindings_error) = match KeyBindings::load_or_create() {
            Ok(keybindings) => (keybindings, None),
            Err(error) => (
                KeyBindings::defaults(),
                Some(format!(
                    "Could not load keybindings; using defaults: {error}"
                )),
            ),
        };
        let thread_registry = Registry::discover()?;
        let thread_title_cache = ThreadTitleCache::discover()?;
        let (mut thread_names, thread_title_cache_error) = match thread_title_cache.load() {
            Ok(titles) => (
                titles
                    .into_iter()
                    .map(|(thread_id, title)| (thread_id, Some(title)))
                    .collect(),
                None,
            ),
            Err(error) => (
                HashMap::new(),
                Some(format!("Could not load cached thread titles: {error}")),
            ),
        };
        let has_existing_threads = match thread_registry.load() {
            Ok(records) => {
                let has_existing_threads = !records.is_empty();
                let registered = records
                    .into_iter()
                    .map(|thread| thread.id)
                    .collect::<HashSet<_>>();
                thread_names.retain(|thread_id, _| registered.contains(thread_id));
                has_existing_threads
            }
            Err(_) => true,
        };
        let mut repositories = repository_store.load_registered()?;
        let initial_home_scan_is_pending = repository_store.initial_home_scan_is_pending();
        let should_show_onboarding = should_show_onboarding(
            onboarding_store.is_pending(),
            initial_home_scan_is_pending,
            repositories.is_empty(),
            has_existing_threads,
        );
        let (imported_repository_count, imported_startup_repository) = if should_show_onboarding {
            import_codex_workspaces(&repository_store, &mut repositories)
        } else {
            (0, None)
        };
        let candidates = repository_store.load_candidates().unwrap_or_default();
        let browse_path = BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("/"));
        let registered_paths = repositories
            .iter()
            .map(|repository| repository.path.clone())
            .collect::<HashSet<_>>();
        let (expanded_repositories, startup_thread_id, startup_repository, ui_state_error) =
            match repository_store.load_ui_state() {
                Ok(Some(state)) => {
                    let mut expanded = state
                        .expanded_repositories
                        .into_iter()
                        .collect::<HashSet<_>>();
                    expanded.retain(|path| registered_paths.contains(path));
                    let selected_repository = state
                        .selected_repository
                        .filter(|path| registered_paths.contains(path));
                    (
                        expanded,
                        state.selected_thread_id,
                        selected_repository,
                        None,
                    )
                }
                Ok(None) => {
                    let repository = imported_startup_repository.clone().or_else(|| {
                        repositories
                            .first()
                            .map(|repository| repository.path.clone())
                    });
                    (
                        repository.iter().cloned().collect::<HashSet<_>>(),
                        None,
                        imported_startup_repository.clone(),
                        None,
                    )
                }
                Err(error) => (
                    repositories
                        .first()
                        .map(|repository| HashSet::from([repository.path.clone()]))
                        .unwrap_or_default(),
                    None,
                    None,
                    Some(format!("Could not load repository view state: {error}")),
                ),
            };
        let startup_message = [
            ui_state_error,
            settings_error,
            keybindings_error,
            thread_title_cache_error,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("; ");
        let mut app = Self {
            performance,
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
            mode: Mode::Normal,
            command_palette: None,
            thread_deletion: None,
            scanning: false,
            show_archived: false,
            archived_thread_undos: VecDeque::new(),
            message: (!startup_message.is_empty()).then_some(startup_message),
            should_quit: false,
            chats: HashMap::new(),
            draft_threads: HashMap::new(),
            pending_onboarding: None,
            preview_cache_order: VecDeque::new(),
            visible_chat_id: None,
            previous_visible_chat_id: None,
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
            thread_picker_original_focus: Focus::Navigation,
            rename_thread_id: None,
            rename_input: String::new(),
            rename_return_mode: Mode::Normal,
            rename_actions: None,
            bulk_rename: None,
            active_chat_pane: ChatPane::Main,
            resumed_threads: HashSet::new(),
            opened_threads: HashSet::new(),
            read_only_threads: HashSet::new(),
            owned_turns: HashMap::new(),
            pending_approvals: VecDeque::new(),
            approval_index: 0,
            attention_items: VecDeque::new(),
            attention_index: 0,
            models: Vec::new(),
            model_index: 0,
            reasoning_effort_index: 0,
            reasoning_effort_returns_to_model: false,
            execution_mode,
            keybindings,
            permission_index: usize::from(execution_mode == ExecutionMode::Dangerous),
            thread_names,
            confirmed_thread_names: HashSet::new(),
            thread_title_cache,
            thread_registry,
            side_chat_registry: SideChatRegistry::discover()?,
            attention_registry: AttentionRegistry::discover()?,
            repository_store,
            settings_store,
            workspaces_by_repository: HashMap::new(),
            scan_receiver: None,
            scan_label: None,
            scan_metric_scope: None,
            scan_animation_tick: 0,
            scan_started_at: None,
            scan_first_result_recorded: false,
            scan_found_count: 0,
            initial_home_scan_in_progress: false,
        };
        app.refresh_current();
        if let Some(thread_id) = startup_thread_id {
            app.restore_tree_selection(TreeSelection::Thread(thread_id.clone()));
            let restored = match app.selected_tree_row() {
                Some(TreeRow::GeneralThread { thread_index })
                | Some(TreeRow::Thread { thread_index, .. }) => app
                    .threads
                    .get(thread_index)
                    .is_some_and(|thread| thread.record.id == thread_id),
                _ => false,
            };
            if !restored && let Some(repository) = startup_repository {
                app.restore_tree_selection(TreeSelection::Repository(repository));
            }
            app.sync_selection_from_tree();
        } else if let Some(repository) = startup_repository {
            app.restore_tree_selection(TreeSelection::Repository(repository));
            app.sync_selection_from_tree();
        }
        let onboarding_opened = if should_show_onboarding {
            match app.create_general_workspace().and_then(|workspace| {
                let draft_id = app.begin_shikigami_help_draft_thread(&workspace)?;
                let mut chat =
                    ChatState::new(draft_id.clone(), workspace.path, "Shikigami Help".into());
                if let Some((model, display_name, effort)) = app.default_model_settings() {
                    chat.set_model(model, display_name, effort);
                }
                app.show_chat(chat);
                app.focus = Focus::Navigation;
                app.mode = Mode::Normal;
                app.pending_onboarding = Some(PendingOnboarding {
                    draft_id,
                    locale: onboarding::preferred_locale(),
                    imported_repository_count,
                });
                Ok(())
            }) {
                Ok(()) => true,
                Err(error) => {
                    app.message = Some(format!("Could not open welcome chat: {error}"));
                    false
                }
            }
        } else {
            false
        };
        if app.repositories.is_empty() {
            if !onboarding_opened {
                app.open_repository_add();
            }
            if initial_home_scan_is_pending {
                if app.start_root_scan().is_err() {
                    app.start_home_scan();
                }
                app.initial_home_scan_in_progress = true;
            }
        }
        if let Err(error) = app.restore_attention() {
            app.message = Some(format!("Could not restore attention list: {error}"));
        }
        Ok(app)
    }

    pub fn take_pending_onboarding(&mut self) -> Option<PendingOnboarding> {
        self.pending_onboarding.take()
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
        self.preview_cache_order
            .retain(|cached| cached != &thread_id);
        self.chats.insert(thread_id.clone(), chat);
        self.show_main_chat_id(thread_id);
    }

    pub fn add_background_chat(&mut self, chat: ChatState) {
        let thread_id = chat.thread_id.clone();
        self.preview_cache_order
            .retain(|cached| cached != &thread_id);
        self.chats.insert(thread_id, chat);
    }

    pub fn cache_chat_preview(&mut self, chat: ChatState, capacity: usize, show: bool) {
        let thread_id = chat.thread_id.clone();
        self.chats.insert(thread_id.clone(), chat);
        for evicted in touch_preview_cache(&mut self.preview_cache_order, &thread_id, capacity) {
            if self
                .chats
                .get(&evicted)
                .is_some_and(|chat| !chat.history_is_complete())
            {
                self.chats.remove(&evicted);
            }
        }
        if show {
            self.show_main_chat_id(thread_id);
        }
    }

    pub fn show_cached_chat(&mut self, thread_id: &str) -> bool {
        if self.chats.contains_key(thread_id) {
            if self
                .chats
                .get(thread_id)
                .is_some_and(|chat| !chat.history_is_complete())
            {
                let _ = touch_preview_cache(&mut self.preview_cache_order, thread_id, usize::MAX);
            }
            self.show_main_chat_id(thread_id.to_owned());
            true
        } else {
            false
        }
    }

    pub fn mark_chat_history_complete(&mut self, thread_id: &str) {
        self.preview_cache_order
            .retain(|cached| cached != thread_id);
        if let Some(chat) = self.chats.get_mut(thread_id) {
            chat.mark_history_complete();
        }
    }

    pub fn show_side_chat(&mut self, parent_thread_id: String, chat: ChatState) -> Result<()> {
        let thread_id = chat.thread_id.clone();
        self.side_chat_registry.register(
            thread_id.clone(),
            parent_thread_id.clone(),
            chat.title.clone(),
            chat.model.clone(),
            chat.model_display_name.clone(),
            chat.reasoning_effort.clone(),
            chat.side_chat_has_activity,
        )?;
        self.attach_side_chat(parent_thread_id, chat, true);
        Ok(())
    }

    pub fn restore_side_chats_for_parent(
        &mut self,
        parent_thread_id: &str,
        cwd: &Path,
    ) -> Result<()> {
        if self.side_chats_by_parent.contains_key(parent_thread_id) {
            return Ok(());
        }
        let records = self
            .side_chat_registry
            .load()?
            .into_iter()
            .filter(|record| {
                record.parent_thread_id == parent_thread_id && !record.pending_deletion
            })
            .collect::<Vec<_>>();
        for (index, record) in records.into_iter().enumerate() {
            let mut chat = ChatState::new(
                record.id,
                cwd.to_path_buf(),
                record
                    .title
                    .unwrap_or_else(|| format!("Sidechat {}", index + 1)),
            );
            if let (Some(model), Some(display_name)) = (record.model, record.model_display_name) {
                chat.set_model(model, display_name, record.reasoning_effort);
            } else if let Some((model, display_name, effort)) = self.default_model_settings() {
                chat.set_model(model, display_name, effort);
            }
            chat.mark_as_side_chat();
            chat.side_chat_has_activity = record.has_activity;
            chat.mark_history_partial();
            self.attach_side_chat(parent_thread_id.to_owned(), chat, false);
        }
        Ok(())
    }

    fn attach_side_chat(&mut self, parent_thread_id: String, chat: ChatState, show: bool) {
        let thread_id = chat.thread_id.clone();
        self.chats.insert(thread_id.clone(), chat);
        let side_chats = self
            .side_chats_by_parent
            .entry(parent_thread_id.clone())
            .or_default();
        if side_chats.contains(&thread_id) {
            return;
        }
        side_chats.push(thread_id.clone());
        let index = side_chats.len() - 1;
        self.opened_threads.insert(thread_id.clone());
        let selected = self
            .selected_side_chat_by_parent
            .entry(parent_thread_id.clone())
            .or_insert(0);
        if show {
            *selected = index;
        }
        if self.visible_chat_id.as_deref() == Some(&parent_thread_id) {
            let selected_id = side_chats.get(*selected).cloned();
            self.side_chat_id = selected_id;
            self.side_chat_parent_id = self.side_chat_id.as_ref().map(|_| parent_thread_id);
        }
        if show {
            self.active_chat_pane = ChatPane::Side;
        }
    }

    pub fn persist_side_chat_metadata(&self, thread_id: &str) -> Result<()> {
        let chat = self
            .chats
            .get(thread_id)
            .filter(|chat| chat.is_side_chat)
            .context("side chat is not loaded")?;
        self.side_chat_registry.update_metadata(
            thread_id,
            chat.title.clone(),
            chat.model.clone(),
            chat.model_display_name.clone(),
            chat.reasoning_effort.clone(),
            chat.side_chat_has_activity,
        )
    }

    pub fn saved_side_chats_for_parent(
        &self,
        parent_thread_id: &str,
    ) -> Result<Vec<SideChatRecord>> {
        Ok(self
            .side_chat_registry
            .load()?
            .into_iter()
            .filter(|record| record.parent_thread_id == parent_thread_id)
            .collect())
    }

    pub fn forget_missing_side_chat(&mut self, thread_id: &str) -> Result<()> {
        self.side_chat_registry.remove(thread_id)?;
        self.remove_side_chat_from_session(thread_id);
        Ok(())
    }

    pub fn begin_side_chat_deletion(&mut self) -> Result<Option<(String, Option<String>)>> {
        let Some(chat) = self.side_chat() else {
            return Ok(None);
        };
        let thread_id = chat.thread_id.clone();
        let turn_id = chat.active_turn_id.clone();
        self.side_chat_registry.mark_for_deletion(&thread_id)?;
        self.remove_side_chat_from_session(&thread_id);
        Ok(Some((thread_id, turn_id)))
    }

    fn remove_side_chat_from_session(&mut self, thread_id: &str) {
        let Some(parent_thread_id) = self.side_chat_parent(thread_id) else {
            return;
        };
        let removed_was_visible = self.side_chat_id.as_deref() == Some(thread_id);
        self.chats.remove(thread_id);
        self.remove_thread_name(thread_id);
        self.attention_items
            .retain(|item| item.thread_id != thread_id);
        self.resumed_threads.remove(thread_id);
        self.opened_threads.remove(thread_id);
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
        let scope = parent.record.scope;
        let repository_path = parent.record.repository_path.clone();
        let (cwd, title) = self
            .chats
            .get(&thread_id)
            .map(|chat| (chat.cwd.clone(), chat.title.clone()))
            .context("side chat is not loaded")?;

        match scope {
            ThreadScope::Repository => {
                self.thread_registry
                    .register_thread(thread_id.clone(), &repository_path, &cwd)?
            }
            ThreadScope::General => self
                .thread_registry
                .register_general_thread(thread_id.clone(), &cwd)?,
        }
        self.thread_names
            .insert(thread_id.clone(), Some(title.clone()));
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

    pub fn forget_temporary_side_chat(&self, thread_id: &str) -> Result<()> {
        self.side_chat_registry.remove(thread_id)
    }

    pub fn pending_side_chat_deletion_ids(&self) -> Result<Vec<String>> {
        self.side_chat_registry.pending_deletion_ids()
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
        self.thread_picker_original_focus = self.focus;
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

    pub fn selected_thread_picker(&self) -> Option<&ThreadItem> {
        self.thread_picker_matches
            .get(self.thread_picker_index)
            .and_then(|index| self.threads.get(*index))
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
        self.focus = self.thread_picker_original_focus;
        self.mode = thread_picker_return_mode(self.thread_picker_original_focus);
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
        if thread.record.scope == ThreadScope::General {
            return "General";
        }
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

    pub fn cycle_chat_pane(&mut self, forward: bool) {
        let Some(parent_thread_id) = self.visible_chat_id.clone() else {
            return;
        };
        let Some(side_chats) = self
            .side_chats_by_parent
            .get(&parent_thread_id)
            .filter(|side_chats| !side_chats.is_empty())
        else {
            return;
        };
        let current_side_chat = self.side_chat_id.as_deref().and_then(|current_thread_id| {
            side_chats
                .iter()
                .position(|thread_id| thread_id == current_thread_id)
        });
        let (pane, side_chat_index) = adjacent_chat_position(
            self.active_chat_pane,
            current_side_chat,
            side_chats.len(),
            forward,
        );
        if pane == ChatPane::Main {
            self.active_chat_pane = ChatPane::Main;
            return;
        }
        let Some(side_chat_index) = side_chat_index else {
            return;
        };
        let Some(thread_id) = side_chats.get(side_chat_index).cloned() else {
            return;
        };
        self.selected_side_chat_by_parent
            .insert(parent_thread_id.clone(), side_chat_index);
        self.side_chat_id = Some(thread_id);
        self.side_chat_parent_id = Some(parent_thread_id);
        self.active_chat_pane = ChatPane::Side;
        self.mark_thread_seen();
    }

    pub fn select_adjacent_thread(&mut self, forward: bool) -> bool {
        let current_thread_id = if self.focus == Focus::Navigation && self.selected_tree_is_thread()
        {
            self.selected_thread()
                .map(|thread| thread.record.id.as_str())
        } else {
            self.visible_chat_id.as_deref()
        };
        let Some(thread_id) = current_thread_id.and_then(|current_thread_id| {
            adjacent_thread_id(
                &self.threads,
                &self.repositories,
                current_thread_id,
                forward,
            )
        }) else {
            return false;
        };
        self.restore_tree_selection(TreeSelection::Thread(thread_id.to_owned()));
        self.sync_selection_from_tree();
        true
    }

    pub fn select_previous_thread(&mut self) -> bool {
        let Some(thread_id) = self.previous_visible_chat_id.clone() else {
            return false;
        };
        if self.visible_chat_id.as_deref() == Some(&thread_id)
            || !self
                .threads
                .iter()
                .any(|thread| thread.record.id == thread_id)
        {
            return false;
        }
        self.restore_tree_selection(TreeSelection::Thread(thread_id));
        self.sync_selection_from_tree();
        true
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

    pub fn record_owned_turn(&mut self, thread_id: String, turn_id: String) {
        self.owned_turns.insert(thread_id, turn_id);
    }

    pub fn owned_turn_count(&self) -> usize {
        self.owned_turns.len()
    }

    pub fn owned_turn_targets(&self) -> Vec<(String, String)> {
        self.owned_turns
            .iter()
            .map(|(thread_id, turn_id)| (thread_id.clone(), turn_id.clone()))
            .collect()
    }

    pub fn forget_owned_turn(&mut self, thread_id: &str, turn_id: &str) {
        if self.owned_turns.get(thread_id).map(String::as_str) == Some(turn_id) {
            self.owned_turns.remove(thread_id);
        }
    }

    pub fn reconcile_owned_turn_after_resync(&mut self, thread_id: &str) {
        let active_turn_id = self
            .chats
            .get(thread_id)
            .and_then(|chat| chat.active_turn_id.as_deref());
        reconcile_owned_turn(&mut self.owned_turns, thread_id, active_turn_id);
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

    pub fn mark_thread_opened(&mut self, thread_id: String) {
        self.opened_threads.insert(thread_id.clone());
        self.resumed_threads.insert(thread_id);
    }

    pub fn mark_thread_unsubscribed(&mut self, thread_id: &str) {
        self.resumed_threads.remove(thread_id);
        if !self.chats.contains_key(thread_id) {
            self.opened_threads.remove(thread_id);
        }
    }

    pub fn forget_thread_subscription(&mut self, thread_id: &str) {
        self.resumed_threads.remove(thread_id);
        self.opened_threads.remove(thread_id);
    }

    pub fn thread_subscription_targets(&self) -> HashSet<String> {
        thread_subscription_targets(
            self.visible_chat_id.as_deref(),
            self.side_chat_id.as_deref(),
            &self.opened_threads,
            &self.chats,
            &self.owned_turns,
        )
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

    pub fn open_permissions_picker(&mut self) {
        self.permission_index = usize::from(self.execution_mode == ExecutionMode::Dangerous);
        self.mode = Mode::ChoosePermissions;
    }

    pub fn move_permission_up(&mut self) {
        self.permission_index = self.permission_index.saturating_sub(1);
    }

    pub fn move_permission_down(&mut self) {
        self.permission_index = (self.permission_index + 1).min(1);
    }

    pub fn choose_permission(&mut self) -> Result<()> {
        let selected = if self.permission_index == 0 {
            ExecutionMode::Auto
        } else {
            ExecutionMode::Dangerous
        };
        if selected == ExecutionMode::Dangerous && self.execution_mode != selected {
            self.mode = Mode::ConfirmDangerous;
            return Ok(());
        }
        self.set_execution_mode(selected)
    }

    pub fn confirm_dangerous(&mut self) -> Result<()> {
        self.set_execution_mode(ExecutionMode::Dangerous)
    }

    pub fn set_execution_mode(&mut self, mode: ExecutionMode) -> Result<()> {
        self.settings_store.save(mode)?;
        self.execution_mode = mode;
        self.permission_index = usize::from(mode == ExecutionMode::Dangerous);
        self.mode = Mode::Chat;
        self.message = Some(format!(
            "Execution mode set to {}; applies to subsequent turns",
            mode.label()
        ));
        Ok(())
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

    /// Match the Codex TUI behavior: use live catalog metadata when present and avoid blocking
    /// input if a transient catalog failure left the selected model unknown.
    pub fn active_model_supports_images(&self) -> bool {
        let model = self.chat().and_then(|chat| chat.model.as_deref());
        model
            .and_then(|model| {
                self.models
                    .iter()
                    .find(|candidate| candidate.model == model)
            })
            .map(ModelMetadata::supports_images)
            .unwrap_or(true)
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
        if let Some(thread_id) = self
            .chat()
            .filter(|chat| chat.is_side_chat)
            .map(|chat| chat.thread_id.clone())
            && let Err(error) = self.persist_side_chat_metadata(&thread_id)
        {
            self.message = Some(format!("Could not save side chat settings: {error}"));
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
        if let Some((thread_id, name)) = thread_name_update(event) {
            self.apply_thread_name(thread_id, name);
            return;
        }
        let thread_id = event.thread_id.clone();
        if event.method == "turn/completed"
            && let Some(thread_id) = thread_id.as_deref()
        {
            let completed_turn_id = event
                .params
                .pointer("/turn/id")
                .and_then(serde_json::Value::as_str);
            if completed_turn_id.is_none()
                || self.owned_turns.get(thread_id).map(String::as_str) == completed_turn_id
            {
                self.owned_turns.remove(thread_id);
            }
        }
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

    pub fn pending_approval_for_thread(&self, thread_id: &str) -> Option<&AppServerRequest> {
        self.pending_approvals
            .iter()
            .find(|request| request.thread_id.as_deref() == Some(thread_id))
    }

    pub fn active_chat_has_pending_approval(&self) -> bool {
        self.active_chat_id()
            .is_some_and(|thread_id| self.pending_approval_for_thread(thread_id).is_some())
    }

    pub fn take_active_chat_approval(&mut self) -> Option<AppServerRequest> {
        let thread_id = self.active_chat_id()?.to_owned();
        take_pending_approval(&mut self.pending_approvals, Some(&thread_id))
    }

    pub fn unscoped_pending_approval(&self) -> Option<&AppServerRequest> {
        self.pending_approvals
            .iter()
            .find(|request| request.thread_id.is_none())
    }

    pub fn take_unscoped_pending_approval(&mut self) -> Option<AppServerRequest> {
        take_pending_approval(&mut self.pending_approvals, None)
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
            .filter(|thread| thread.record.scope == ThreadScope::Repository)
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
            if !self.reveal_chat(&item.thread_id) {
                self.message = Some("The selected thread is no longer available".into());
                self.close_attention();
            }
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
        self.owned_turns.remove(thread_id);
        self.attention_items
            .retain(|item| item.thread_id != thread_id);
        self.persist_attention();
        self.read_only_threads.remove(thread_id);
        if self.visible_chat_id.as_deref() == Some(thread_id) {
            self.visible_chat_id = None;
        }
    }

    fn show_main_chat_id(&mut self, thread_id: String) {
        if self.focus == Focus::Chat
            && self.visible_chat_id.as_deref() != Some(&thread_id)
            && let Some(previous) = self.visible_chat_id.as_ref()
            && self
                .threads
                .iter()
                .any(|thread| thread.record.id == *previous)
        {
            self.previous_visible_chat_id = Some(previous.clone());
        }
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
            let selection = TreeSelection::Thread(thread.record.id.clone());
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

    pub fn selected_tree_is_general(&self) -> bool {
        matches!(
            self.selected_tree_row(),
            Some(TreeRow::General | TreeRow::GeneralThread { .. })
        )
    }

    pub fn selected_tree_is_thread(&self) -> bool {
        matches!(
            self.selected_tree_row(),
            Some(TreeRow::Thread { .. } | TreeRow::GeneralThread { .. })
        )
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

    pub fn expand_all_repositories(&mut self) {
        self.expanded_repositories = self
            .repositories
            .iter()
            .map(|repository| repository.path.clone())
            .collect();
        self.persist_expanded_repositories();
    }

    pub fn collapse_all_repositories(&mut self) {
        self.expanded_repositories.clear();
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

    pub fn select_parent_group(&mut self) {
        if self
            .selected_thread()
            .is_some_and(|thread| thread.record.scope == ThreadScope::General)
        {
            self.select_general_row();
        } else {
            self.select_repository_row(self.repository_index);
        }
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

    pub fn prepare_thread_workspace(
        &mut self,
        source_thread_id: &str,
        new_worktree: bool,
    ) -> Result<PreparedThreadWorkspace> {
        let registered_thread_id = if self.thread_is_registered(source_thread_id) {
            source_thread_id
        } else {
            self.side_chats_by_parent
                .iter()
                .find_map(|(parent, side_chats)| {
                    side_chats
                        .iter()
                        .any(|thread_id| thread_id == source_thread_id)
                        .then_some(parent.as_str())
                })
                .context("source thread is not registered with Shikigami")?
        };
        let record = self
            .threads
            .iter()
            .find(|thread| thread.record.id == registered_thread_id)
            .map(|thread| thread.record.clone())
            .context("source thread is not registered with Shikigami")?;
        if record.scope == ThreadScope::General {
            anyhow::ensure!(!new_worktree, "General chats do not have a Git worktree");
            let cwd = self
                .chats
                .get(source_thread_id)
                .map(|chat| chat.cwd.clone())
                .unwrap_or(record.cwd);
            return Ok(PreparedThreadWorkspace {
                repository: None,
                workspace: Workspace {
                    name: cwd
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "general".into()),
                    is_primary: false,
                    path: cwd,
                },
                scope: ThreadScope::General,
            });
        }
        let repository = self
            .repositories
            .iter()
            .find(|repository| repository.path == record.repository_path)
            .cloned()
            .context("source repository is not registered with Shikigami")?;
        if new_worktree {
            let workspace =
                git_workspace::create_generated_workspace(&repository.path, &repository.name)?;
            self.refresh_current();
            return Ok(PreparedThreadWorkspace {
                repository: Some(repository),
                workspace,
                scope: ThreadScope::Repository,
            });
        }

        let cwd = self
            .chats
            .get(source_thread_id)
            .map(|chat| chat.cwd.clone())
            .unwrap_or(record.cwd);
        let workspace = self
            .workspaces_by_repository
            .get(&repository.path)
            .and_then(|workspaces| {
                workspaces
                    .iter()
                    .find(|workspace| workspace.path == cwd)
                    .cloned()
            })
            .unwrap_or_else(|| Workspace {
                name: cwd
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| cwd.display().to_string()),
                is_primary: cwd == repository.path,
                path: cwd,
            });
        Ok(PreparedThreadWorkspace {
            repository: Some(repository),
            workspace,
            scope: ThreadScope::Repository,
        })
    }

    pub fn selected_thread_has_active_turn(&self) -> Result<bool> {
        let thread_id = &self
            .selected_thread()
            .context("no thread selected")?
            .record
            .id;
        Ok(thread_group_has_active_turn(
            thread_id,
            &self.chats,
            &self.side_chats_by_parent,
            &self.owned_turns,
        ))
    }

    pub fn archive_selected_thread(&mut self) -> Result<()> {
        let record = self
            .selected_thread()
            .map(|thread| thread.record.clone())
            .context("no thread selected")?;
        let thread_position = self
            .selected_thread_position_in_repository()
            .context("no thread selected")?;
        anyhow::ensure!(
            !thread_group_has_active_turn(
                &record.id,
                &self.chats,
                &self.side_chats_by_parent,
                &self.owned_turns,
            ),
            "response is running; stop it before archiving"
        );
        self.thread_registry.set_archived(&record.id, true)?;
        remember_archive_undo(
            &mut self.archived_thread_undos,
            ArchivedThreadUndo {
                id: record.id.clone(),
                title: record.title.clone(),
            },
        );
        self.discard_chat(&record.id);
        self.refresh_current();
        self.select_nearby_thread(&record, thread_position);
        Ok(())
    }

    pub fn unarchive_selected_thread(&mut self) -> Result<()> {
        let record = self
            .selected_thread()
            .map(|thread| thread.record.clone())
            .context("no thread selected")?;
        let thread_position = self
            .selected_thread_position_in_repository()
            .context("no thread selected")?;
        self.thread_registry.set_archived(&record.id, false)?;
        forget_archive_undo(&mut self.archived_thread_undos, &record.id);
        self.refresh_current();
        self.select_nearby_thread(&record, thread_position);
        Ok(())
    }

    pub fn undo_last_archive(&mut self) -> Result<String> {
        let records = self.thread_registry.load()?;
        let undo = latest_valid_archive_undo(&mut self.archived_thread_undos, &records)
            .context("nothing to undo")?;
        self.thread_registry.set_archived(&undo.id, false)?;
        self.archived_thread_undos.pop_back();
        self.show_archived = false;
        self.refresh_current();
        self.restore_tree_selection(TreeSelection::Thread(undo.id));
        self.sync_selection_from_tree();
        Ok(undo.title)
    }

    pub fn selected_thread_delete_target(&self) -> Result<ThreadRecord> {
        let record = self.selected_thread_delete_candidate()?;
        if record.managed_worktree
            && record.cwd.is_dir()
            && !git_workspace::workspace_is_clean(&record.cwd)?
        {
            anyhow::bail!("worktree has changes; restore the thread and clean it before deleting");
        }
        Ok(record)
    }

    fn selected_thread_delete_candidate(&self) -> Result<ThreadRecord> {
        let record = self
            .selected_thread()
            .map(|thread| thread.record.clone())
            .context("no thread selected")?;
        ensure_thread_deletion_context(self.show_archived, &record)?;
        anyhow::ensure!(
            !thread_group_has_active_turn(
                &record.id,
                &self.chats,
                &self.side_chats_by_parent,
                &self.owned_turns,
            ),
            "response is running; stop it before deleting"
        );
        Ok(record)
    }

    pub fn begin_thread_deletion(&mut self) -> Result<ThreadDeletionRequest> {
        let record = self.selected_thread_delete_candidate()?;
        let side_chat_ids = self
            .saved_side_chats_for_parent(&record.id)?
            .into_iter()
            .map(|side_chat| side_chat.id)
            .collect();
        self.thread_deletion = Some(ThreadDeletionState {
            title: record.title.clone(),
            phase: if record.managed_worktree && record.cwd.is_dir() {
                ThreadDeletionPhase::CheckingWorktree
            } else {
                ThreadDeletionPhase::DeletingHistory
            },
            started_at: Instant::now(),
        });
        self.mode = Mode::DeletingThread;
        Ok(ThreadDeletionRequest {
            record,
            side_chat_ids,
        })
    }

    pub fn set_thread_deletion_phase(&mut self, phase: ThreadDeletionPhase) {
        if let Some(deletion) = self.thread_deletion.as_mut() {
            deletion.phase = phase;
        }
    }

    pub fn end_thread_deletion(&mut self) {
        self.thread_deletion = None;
        self.mode = if self.unscoped_pending_approval().is_some() {
            Mode::Approval
        } else if self.chat().is_some() && self.focus == Focus::Chat {
            Mode::Chat
        } else {
            Mode::Normal
        };
    }

    pub fn complete_thread_deletion(&mut self, thread_id: &str) -> Result<()> {
        let record = self
            .threads
            .iter()
            .find(|thread| thread.record.id == thread_id)
            .map(|thread| thread.record.clone())
            .context("thread is no longer selected")?;
        ensure_thread_deletion_context(self.show_archived, &record)?;
        self.thread_registry.remove(thread_id)?;
        self.side_chat_registry.remove_for_parent(thread_id)?;
        forget_archive_undo(&mut self.archived_thread_undos, thread_id);
        self.remove_thread_name(thread_id);
        self.discard_chat(thread_id);
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

    pub fn move_tree_to_top(&mut self) {
        self.tree_index = 0;
        self.sync_selection_from_tree();
    }

    pub fn move_tree_to_bottom(&mut self) {
        self.tree_index = self.tree_rows().len().saturating_sub(1);
        self.sync_selection_from_tree();
    }

    pub fn open_repository_add(&mut self) {
        self.mode = Mode::AddRepositories;
        self.candidate_index = 0;
        self.repository_query.clear();
        self.selected_candidates.clear();
    }

    pub fn start_root_scan(&mut self) -> Result<()> {
        if self.scanning {
            return Ok(());
        }
        let mut roots = self.repository_store.load_search_roots()?;
        roots.extend(repository::detected_search_roots());
        roots.sort();
        roots.dedup();
        if roots.is_empty() {
            anyhow::bail!("choose a projects folder first");
        }
        self.start_scan(ScanScope::Roots(roots));
        Ok(())
    }

    pub fn start_home_scan(&mut self) {
        if !self.scanning {
            self.start_scan(ScanScope::Home);
        }
    }

    pub fn scan_label(&self) -> Option<&'static str> {
        self.scan_label
    }

    pub fn scan_spinner(&self) -> &'static str {
        const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        FRAMES[self.scan_animation_tick % FRAMES.len()]
    }

    pub fn poll_scan(&mut self) {
        let mut finished = false;
        let mut completed = false;
        let mut worker_duration = None;
        let Some(receiver) = &self.scan_receiver else {
            return;
        };
        self.scan_animation_tick = self.scan_animation_tick.wrapping_add(1);
        loop {
            match receiver.try_recv() {
                Ok(ScanEvent::Found(repository)) => {
                    if !self
                        .candidates
                        .iter()
                        .any(|candidate| candidate.path == repository.path)
                    {
                        if !self.scan_first_result_recorded {
                            if let (Some(started), Some(scope)) =
                                (self.scan_started_at, self.scan_metric_scope)
                            {
                                self.performance.record_duration(
                                    "repository_scan.first_result",
                                    Some(started),
                                    "success",
                                    &[("scope", scope)],
                                );
                            }
                            self.scan_first_result_recorded = true;
                        }
                        self.candidates.push(repository);
                        self.scan_found_count += 1;
                        self.candidates
                            .sort_by(|left, right| left.name.cmp(&right.name));
                    }
                }
                Ok(ScanEvent::Finished {
                    worker_duration: duration,
                }) => {
                    finished = true;
                    completed = true;
                    worker_duration = Some(duration);
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if finished {
            let scan_started_at = self.scan_started_at.take();
            let scan_scope = self.scan_metric_scope.take();
            let scan_found_count = self.scan_found_count;
            self.scanning = false;
            self.scan_receiver = None;
            self.scan_label = None;
            let mut errors = Vec::new();
            if let Err(error) = self.repository_store.save_candidates(&self.candidates) {
                errors.push(error.to_string());
            }
            if self.initial_home_scan_in_progress
                && completed
                && let Err(error) = self.repository_store.mark_initial_home_scan_complete()
            {
                errors.push(error.to_string());
            }
            self.initial_home_scan_in_progress = false;
            if !errors.is_empty() {
                self.message = Some(errors.join("; "));
            }
            self.candidate_index = self
                .candidate_index
                .min(self.visible_candidates().len().saturating_sub(1));
            if let Some(scope) = scan_scope {
                let found = scan_found_count.to_string();
                if let Some(duration) = worker_duration {
                    self.performance.record_value(
                        "repository_scan.worker_total",
                        duration,
                        "success",
                        &[("scope", scope), ("found", &found)],
                    );
                }
                if let Some(started) = scan_started_at {
                    self.performance.record_duration(
                        "repository_scan.visible_total",
                        Some(started),
                        if completed { "success" } else { "error" },
                        &[("scope", scope), ("found", &found)],
                    );
                }
            }
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
        let Some(path) = selected_browse_directory(&self.browse_directories, self.browse_index)
            .map(Path::to_path_buf)
        else {
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
        let path = selected_browse_directory(&self.browse_directories, self.browse_index)
            .context("select a repository folder first")?;
        let repository = repository::repository_at(path)?;
        self.repository_store.register(&[repository])?;
        self.refresh_repositories()?;
        self.mode = Mode::Normal;
        Ok(())
    }

    pub fn register_repository_from_thread(
        &mut self,
        source_thread_id: &str,
        path: Option<&Path>,
    ) -> Result<(Repository, bool)> {
        let source_cwd = self
            .chats
            .get(source_thread_id)
            .map(|chat| chat.cwd.as_path());
        let path = repository_registration_path(path, source_cwd)?;
        let repository = repository::repository_at(&path)?;
        let added = !self
            .repositories
            .iter()
            .any(|registered| registered.path == repository.path);
        if added {
            self.repository_store
                .register(std::slice::from_ref(&repository))?;
            self.refresh_repositories()?;
            self.reveal_chat(source_thread_id);
        }
        Ok((repository, added))
    }

    pub fn scan_browse_path(&mut self) -> Result<PathBuf> {
        let root = self.repository_store.add_search_root(&self.browse_path)?;
        self.mode = Mode::AddRepositories;
        self.start_root_scan()?;
        Ok(root)
    }

    pub fn refresh_current(&mut self) {
        let selection = match self.selected_tree_row() {
            Some(TreeRow::General) => Some(TreeSelection::General),
            Some(TreeRow::GeneralThread { thread_index }) => self
                .threads
                .get(thread_index)
                .map(|thread| TreeSelection::Thread(thread.record.id.clone())),
            Some(TreeRow::Repository { repository_index }) => self
                .repositories
                .get(repository_index)
                .map(|repository| TreeSelection::Repository(repository.path.clone())),
            Some(TreeRow::Thread { thread_index, .. }) => self
                .threads
                .get(thread_index)
                .map(|thread| TreeSelection::Thread(thread.record.id.clone())),
            None => Some(TreeSelection::General),
        };
        self.locations.clear();
        self.threads.clear();
        self.workspaces_by_repository.clear();
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
                    .filter(|record| {
                        record.scope == ThreadScope::General
                            || registered_paths.contains(&record.repository_path)
                    })
                    .filter(|record| record.archived_at.is_some() == self.show_archived)
                    .map(|mut record| {
                        record.title = display_thread_name(
                            self.thread_names
                                .get(&record.id)
                                .and_then(|name| name.as_deref()),
                        );
                        let location = (record.scope == ThreadScope::Repository)
                            .then(|| self.workspaces_by_repository.get(&record.repository_path))
                            .flatten()
                            .into_iter()
                            .flatten()
                            .filter(|location| record.cwd.starts_with(&location.path))
                            .max_by_key(|location| location.path.components().count());
                        if record.scope == ThreadScope::Repository {
                            if record.worktree_branch.is_none() {
                                record.worktree_branch =
                                    location.map(|location| location.name.clone());
                            }
                            record.managed_worktree |= git_workspace::is_managed_workspace(
                                &record.cwd,
                                record.worktree_branch.as_deref(),
                            );
                        }
                        ThreadItem {
                            location_name: location
                                .map(|location| location.name.clone())
                                .or_else(|| record.worktree_branch.clone())
                                .unwrap_or_else(|| {
                                    if record.scope == ThreadScope::General {
                                        record
                                            .cwd
                                            .file_name()
                                            .map(|name| name.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| "scratch".into())
                                    } else {
                                        "removed location".into()
                                    }
                                }),
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
        self.select_repository_row(self.repository_index);
        self.save_repository_ui_state()?;
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

    pub fn begin_draft_thread(
        &mut self,
        workspace: &Workspace,
        scope: ThreadScope,
        cleanup_workspace_on_cancel: bool,
    ) -> Result<String> {
        self.begin_draft_thread_with_kind(
            workspace,
            scope,
            ThreadKind::Regular,
            cleanup_workspace_on_cancel,
        )
    }

    pub fn begin_shikigami_help_draft_thread(&mut self, workspace: &Workspace) -> Result<String> {
        self.begin_draft_thread_with_kind(
            workspace,
            ThreadScope::General,
            ThreadKind::ShikigamiHelp,
            true,
        )
    }

    fn begin_draft_thread_with_kind(
        &mut self,
        workspace: &Workspace,
        scope: ThreadScope,
        kind: ThreadKind,
        cleanup_workspace_on_cancel: bool,
    ) -> Result<String> {
        let repository_path = match scope {
            ThreadScope::Repository => self
                .selected_repository()
                .map(|repository| repository.path.clone())
                .context("no repository selected")?,
            ThreadScope::General => workspace.path.clone(),
        };
        let id = format!(
            "draft-{}-{}",
            std::process::id(),
            NEXT_DRAFT_THREAD_ID.fetch_add(1, Ordering::Relaxed)
        );
        self.draft_threads.insert(
            id.clone(),
            DraftThread {
                scope,
                kind,
                repository_path,
                workspace_path: workspace.path.clone(),
                cleanup_workspace_on_cancel,
            },
        );
        Ok(id)
    }

    pub fn is_draft_thread(&self, thread_id: &str) -> bool {
        self.draft_threads.contains_key(thread_id)
    }

    pub fn materialize_draft_thread(&mut self, draft_id: &str, thread_id: String) -> Result<()> {
        let draft = self
            .draft_threads
            .get(draft_id)
            .cloned()
            .context("draft thread not found")?;
        anyhow::ensure!(self.chats.contains_key(draft_id), "draft chat not found");
        match draft.scope {
            ThreadScope::Repository => self.thread_registry.register_thread(
                thread_id.clone(),
                &draft.repository_path,
                &draft.workspace_path,
            )?,
            ThreadScope::General => self.thread_registry.register_general_thread_with_kind(
                thread_id.clone(),
                &draft.workspace_path,
                draft.kind,
            )?,
        }

        self.draft_threads.remove(draft_id);
        let mut chat = self
            .chats
            .remove(draft_id)
            .expect("draft chat was checked above");
        chat.thread_id.clone_from(&thread_id);
        self.chats.insert(thread_id.clone(), chat);
        if self.visible_chat_id.as_deref() == Some(draft_id) {
            self.visible_chat_id = Some(thread_id.clone());
        }
        self.confirmed_thread_names.insert(thread_id.clone());
        self.thread_names.insert(thread_id, None);
        self.refresh_current();
        Ok(())
    }

    pub fn reveal_shikigami_help_thread(&mut self) -> Result<bool> {
        let Some(thread_id) = self
            .thread_registry
            .load()?
            .into_iter()
            .filter(|record| {
                record.kind == ThreadKind::ShikigamiHelp && record.archived_at.is_none()
            })
            .max_by_key(|record| record.updated_at)
            .map(|record| record.id)
        else {
            return Ok(false);
        };
        self.show_archived = false;
        self.refresh_current();
        Ok(self.reveal_chat(&thread_id))
    }

    pub fn cancel_visible_draft_thread(&mut self) -> Option<DraftWorkspaceCleanup> {
        let draft_id = self.visible_chat_id.as_deref()?;
        let draft = self.draft_threads.remove(draft_id)?;
        let draft_id = draft_id.to_owned();
        self.discard_chat(&draft_id);
        draft
            .cleanup_workspace_on_cancel
            .then_some(DraftWorkspaceCleanup {
                scope: draft.scope,
                repository_path: draft.repository_path,
                workspace_path: draft.workspace_path,
            })
    }

    pub fn forget_workspace(&mut self, workspace_path: &Path) {
        self.locations
            .retain(|workspace| workspace.path != workspace_path);
        for workspaces in self.workspaces_by_repository.values_mut() {
            workspaces.retain(|workspace| workspace.path != workspace_path);
        }
    }

    pub fn unused_main_chat_cleanup_target(&self) -> Result<Option<String>> {
        let Some(thread_id) = self.visible_chat_id.as_deref() else {
            return Ok(None);
        };
        let Some(chat) = self.chats.get(thread_id) else {
            return Ok(None);
        };
        let Some(thread) = self
            .threads
            .iter()
            .find(|thread| thread.record.id == thread_id)
        else {
            return Ok(None);
        };
        if !chat.is_unused_main_thread() || thread.record.title != "Untitled thread" {
            return Ok(None);
        }
        if thread.record.managed_worktree
            && thread.record.cwd.is_dir()
            && !git_workspace::workspace_is_clean(&thread.record.cwd)?
        {
            return Ok(None);
        }
        Ok(Some(thread_id.to_owned()))
    }

    pub fn remove_unused_main_chat(&mut self, thread_id: &str) -> Result<()> {
        anyhow::ensure!(
            self.unused_main_chat_cleanup_target()?.as_deref() == Some(thread_id),
            "chat is no longer safe to remove"
        );
        let record = self
            .threads
            .iter()
            .find(|thread| thread.record.id == thread_id)
            .map(|thread| thread.record.clone())
            .context("thread not found")?;
        if record.managed_worktree && record.cwd.is_dir() {
            git_workspace::remove_managed_workspace(
                &record.repository_path,
                &record.cwd,
                record.worktree_branch.as_deref(),
            )?;
        }
        self.thread_registry.remove(thread_id)?;
        self.remove_thread_name(thread_id);
        self.discard_chat(thread_id);
        self.refresh_current();
        Ok(())
    }

    pub fn register_app_server_thread_in_repository(
        &mut self,
        thread_id: String,
        repository_path: PathBuf,
        cwd: PathBuf,
    ) -> Result<()> {
        self.thread_registry
            .register_thread(thread_id.clone(), &repository_path, &cwd)?;
        self.confirmed_thread_names.insert(thread_id.clone());
        self.thread_names.insert(thread_id, None);
        self.refresh_current();
        Ok(())
    }

    pub fn create_general_workspace(&self) -> Result<Workspace> {
        let path = paths::create_general_workspace()?;
        Ok(Workspace {
            name: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "new-chat".into()),
            is_primary: false,
            path,
        })
    }

    pub fn register_app_server_general_thread(
        &mut self,
        thread_id: String,
        cwd: PathBuf,
    ) -> Result<()> {
        self.thread_registry
            .register_general_thread(thread_id.clone(), &cwd)?;
        self.confirmed_thread_names.insert(thread_id.clone());
        self.thread_names.insert(thread_id, None);
        self.refresh_current();
        Ok(())
    }

    pub fn rollback_unstarted_thread(
        &mut self,
        thread_id: &str,
        remove_workspace: bool,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.owned_turns.contains_key(thread_id)
                && self
                    .chats
                    .get(thread_id)
                    .is_none_or(ChatState::is_unused_main_thread),
            "thread has already started"
        );
        let record = self
            .threads
            .iter()
            .find(|thread| thread.record.id == thread_id)
            .map(|thread| thread.record.clone())
            .context("thread not found")?;
        if remove_workspace && record.cwd.is_dir() {
            git_workspace::remove_managed_workspace(
                &record.repository_path,
                &record.cwd,
                record.worktree_branch.as_deref(),
            )?;
        }
        self.thread_registry.remove(thread_id)?;
        self.remove_thread_name(thread_id);
        self.discard_chat(thread_id);
        self.forget_thread_subscription(thread_id);
        self.refresh_current();
        Ok(())
    }

    pub fn replace_empty_thread_id(
        &mut self,
        old_thread_id: &str,
        new_thread_id: String,
    ) -> Result<()> {
        let thread = self
            .threads
            .iter()
            .find(|thread| thread.record.id == old_thread_id)
            .context("thread not found")?;
        anyhow::ensure!(
            thread.record.title == "Untitled thread",
            "only an empty thread can be reconnected"
        );
        self.thread_registry
            .replace_thread_id(old_thread_id, new_thread_id.clone())?;
        let name = self.thread_names.remove(old_thread_id).unwrap_or(None);
        self.thread_names.insert(new_thread_id.clone(), name);
        if self.confirmed_thread_names.remove(old_thread_id) {
            self.confirmed_thread_names.insert(new_thread_id.clone());
        }
        self.sync_thread_title_cache();

        let thread = self
            .threads
            .iter_mut()
            .find(|thread| thread.record.id == old_thread_id)
            .context("thread disappeared while reconnecting")?;
        thread.record.id.clone_from(&new_thread_id);
        if let Some(mut chat) = self.chats.remove(old_thread_id) {
            chat.thread_id.clone_from(&new_thread_id);
            self.chats.insert(new_thread_id.clone(), chat);
        }
        if self.visible_chat_id.as_deref() == Some(old_thread_id) {
            self.visible_chat_id = Some(new_thread_id.clone());
        }
        self.resumed_threads.remove(old_thread_id);
        self.opened_threads.remove(old_thread_id);
        self.read_only_threads.remove(old_thread_id);
        Ok(())
    }

    pub fn thread_has_name(&self, thread_id: &str) -> bool {
        self.thread_names
            .get(thread_id)
            .is_some_and(Option::is_some)
    }

    pub fn apply_thread_name(&mut self, thread_id: &str, name: Option<String>) {
        let changed = self.apply_thread_name_in_memory(thread_id, name);
        self.confirmed_thread_names.insert(thread_id.to_owned());
        if changed {
            self.sync_thread_title_cache();
        }
    }

    pub fn apply_thread_names(
        &mut self,
        names: Vec<(String, Option<String>)>,
        overwrite_existing: bool,
    ) {
        for (thread_id, name) in names {
            if should_apply_refreshed_thread_name(
                overwrite_existing,
                &self.confirmed_thread_names,
                &thread_id,
            ) {
                self.apply_thread_name_in_memory(&thread_id, name);
                self.confirmed_thread_names.insert(thread_id);
            }
        }
        self.sync_thread_title_cache();
    }

    fn apply_thread_name_in_memory(&mut self, thread_id: &str, name: Option<String>) -> bool {
        let changed = self.thread_names.get(thread_id) != Some(&name);
        let title = display_thread_name(name.as_deref());
        self.thread_names.insert(thread_id.to_owned(), name);
        apply_thread_name_to(&mut self.threads, &mut self.chats, thread_id, &title);
        if self.mode == Mode::ChooseThread {
            self.refresh_thread_picker_matches();
        }
        changed
    }

    fn sync_thread_title_cache(&mut self) {
        if let Err(error) = self.thread_title_cache.sync(&self.thread_names) {
            self.message = Some(format!("Could not cache thread titles: {error}"));
        }
    }

    fn remove_thread_name(&mut self, thread_id: &str) {
        self.confirmed_thread_names.remove(thread_id);
        if self.thread_names.remove(thread_id).is_some() {
            self.sync_thread_title_cache();
        }
    }

    pub fn registered_thread_ids(&self) -> Result<HashSet<String>> {
        Ok(self
            .thread_registry
            .load()?
            .into_iter()
            .map(|thread| thread.id)
            .collect())
    }

    pub fn open_rename_actions(&mut self, from_picker: bool) {
        let thread = if from_picker {
            self.selected_thread_picker()
        } else if self.selected_tree_is_thread() {
            self.selected_thread()
        } else {
            None
        };
        let thread_id = thread.map(|thread| thread.record.id.clone());
        let repository_path = thread
            .filter(|thread| thread.record.scope == ThreadScope::Repository)
            .map(|thread| thread.record.repository_path.clone())
            .or_else(|| {
                (thread.is_none() && !self.selected_tree_is_general())
                    .then(|| {
                        self.selected_repository()
                            .map(|repository| repository.path.clone())
                    })
                    .flatten()
            });
        let return_mode = if from_picker {
            Mode::ChooseThread
        } else {
            Mode::Normal
        };
        let mut state = RenameActionState {
            index: 0,
            return_mode,
            thread_id,
            repository_path,
        };
        state.index = RenameAction::ALL
            .iter()
            .position(|action| self.rename_action_is_available_for(&state, *action))
            .unwrap_or(0);
        self.rename_actions = Some(state);
        self.mode = Mode::ChooseRenameAction;
        self.message = None;
    }

    pub fn close_rename_actions(&mut self) {
        if let Some(state) = self.rename_actions.take() {
            self.mode = state.return_mode;
        } else {
            self.mode = Mode::Normal;
        }
    }

    pub fn rename_action_is_available(&self, action: RenameAction) -> bool {
        self.rename_actions
            .as_ref()
            .is_some_and(|state| self.rename_action_is_available_for(state, action))
    }

    fn rename_action_is_available_for(
        &self,
        state: &RenameActionState,
        action: RenameAction,
    ) -> bool {
        if self.show_archived {
            return false;
        }
        match action {
            RenameAction::RenameThread => state.thread_id.is_some(),
            RenameAction::SuggestThread => state.thread_id.is_some(),
            RenameAction::SuggestRepository => state.repository_path.as_ref().is_some_and(|path| {
                self.threads
                    .iter()
                    .any(|thread| &thread.record.repository_path == path)
            }),
            RenameAction::SuggestAll => !self.threads.is_empty(),
        }
    }

    pub fn selected_rename_action(&self) -> Option<RenameAction> {
        let state = self.rename_actions.as_ref()?;
        let action = *RenameAction::ALL.get(state.index)?;
        self.rename_action_is_available(action).then_some(action)
    }

    pub fn move_rename_action(&mut self, forward: bool) {
        let Some(state) = self.rename_actions.as_ref() else {
            return;
        };
        let available = RenameAction::ALL.map(|action| self.rename_action_is_available(action));
        let current = state.index;
        if let Some(next) = next_available_index(current, forward, &available)
            && let Some(state) = self.rename_actions.as_mut()
        {
            state.index = next;
        }
    }

    pub fn open_thread_rename_from_action(&mut self) {
        let Some(state) = self.rename_actions.take() else {
            return;
        };
        let Some(thread_id) = state.thread_id else {
            self.rename_actions = Some(state);
            self.message = Some("No thread selected".into());
            return;
        };
        self.rename_thread_id = Some(thread_id.clone());
        self.rename_input = self
            .thread_names
            .get(&thread_id)
            .and_then(|name| name.clone())
            .unwrap_or_default();
        self.rename_return_mode = state.return_mode;
        self.mode = Mode::RenameThread;
        self.message = None;
    }

    pub fn open_selected_thread_suggestion_from_action(
        &mut self,
    ) -> Result<ThreadNameGenerationRequest> {
        let action_state = self
            .rename_actions
            .as_ref()
            .context("rename actions are not open")?;
        anyhow::ensure!(
            self.rename_action_is_available_for(action_state, RenameAction::SuggestThread),
            "No active thread is selected"
        );
        let action_state = action_state.clone();
        let thread_id = action_state
            .thread_id
            .as_ref()
            .context("No thread selected")?;
        let thread = self
            .threads
            .iter()
            .find(|thread| &thread.record.id == thread_id)
            .cloned()
            .context("Selected thread is no longer available")?;
        let repository_name = self.repository_name_for_thread(&thread).to_owned();
        let workspace_path = thread.record.cwd.clone();
        let candidate = BulkRenameCandidate {
            thread_id: thread.record.id,
            repository_name,
            current_name: thread.record.title.clone(),
            proposed_name: thread.record.title,
            selected: true,
            error: None,
        };

        self.rename_actions = None;
        self.bulk_rename = Some(BulkRenameState {
            scope_name: "Selected thread".into(),
            repository_path: workspace_path,
            show_repository_names: false,
            requires_apply_confirmation: false,
            return_mode: action_state.return_mode,
            candidates: vec![candidate],
            index: 0,
            phase: BulkRenamePhase::Select,
            progress: None,
            progress_started_at: Instant::now(),
            edit_input: String::new(),
            generating_ids: HashSet::new(),
        });
        self.mode = Mode::BulkRenameThreads;
        self.message = None;
        self.begin_bulk_name_generation(false)
    }

    pub fn close_thread_rename(&mut self) {
        self.mode = self.rename_return_mode.clone();
        self.rename_thread_id = None;
        self.rename_input.clear();
        if self.mode == Mode::ChooseThread {
            self.refresh_thread_picker_matches();
        }
    }

    pub fn open_bulk_thread_rename_from_action(&mut self, all_repositories: bool) {
        let Some(action_state) = self.rename_actions.as_ref() else {
            return;
        };
        let action = if all_repositories {
            RenameAction::SuggestAll
        } else {
            RenameAction::SuggestRepository
        };
        if !self.rename_action_is_available_for(action_state, action) {
            self.message = Some("No active threads are available for that scope".into());
            return;
        }
        let action_state = action_state.clone();
        let workspace_path = if all_repositories {
            action_state
                .thread_id
                .as_ref()
                .and_then(|id| self.threads.iter().find(|thread| &thread.record.id == id))
                .or_else(|| self.threads.first())
                .map(|thread| thread.record.cwd.clone())
        } else {
            action_state.repository_path.clone()
        };
        let Some(workspace_path) = workspace_path else {
            self.message = Some("No active threads are available for that scope".into());
            return;
        };
        let scope_name = if all_repositories {
            "All threads".into()
        } else {
            self.repositories
                .iter()
                .find(|repository| repository.path == workspace_path)
                .map(|repository| repository.name.clone())
                .unwrap_or_else(|| "Selected repository".into())
        };
        let candidates = self
            .threads
            .iter()
            .filter(|thread| {
                all_repositories
                    || (thread.record.scope == ThreadScope::Repository
                        && thread.record.repository_path == workspace_path)
            })
            .map(|thread| BulkRenameCandidate {
                thread_id: thread.record.id.clone(),
                repository_name: self.repository_name_for_thread(thread).to_owned(),
                current_name: thread.record.title.clone(),
                proposed_name: thread.record.title.clone(),
                selected: true,
                error: None,
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            self.message = Some("No active threads are available for that scope".into());
            return;
        }
        self.rename_actions = None;
        self.bulk_rename = Some(BulkRenameState {
            scope_name,
            repository_path: workspace_path,
            show_repository_names: all_repositories,
            requires_apply_confirmation: true,
            return_mode: action_state.return_mode,
            candidates,
            index: 0,
            phase: BulkRenamePhase::Select,
            progress: None,
            progress_started_at: Instant::now(),
            edit_input: String::new(),
            generating_ids: HashSet::new(),
        });
        self.mode = Mode::BulkRenameThreads;
        self.message = None;
    }

    pub fn close_bulk_thread_rename(&mut self) {
        if let Some(state) = self.bulk_rename.take() {
            self.mode = state.return_mode;
            if self.mode == Mode::ChooseThread {
                self.refresh_thread_picker_matches();
            }
        } else {
            self.mode = Mode::Normal;
        }
    }

    pub fn move_bulk_rename_up(&mut self) {
        if let Some(state) = self.bulk_rename.as_mut() {
            state.index = state.index.saturating_sub(1);
        }
    }

    pub fn move_bulk_rename_down(&mut self) {
        if let Some(state) = self.bulk_rename.as_mut() {
            state.index = state
                .index
                .saturating_add(1)
                .min(state.candidates.len().saturating_sub(1));
        }
    }

    pub fn toggle_bulk_rename_candidate(&mut self) {
        let Some(state) = self.bulk_rename.as_mut() else {
            return;
        };
        if let Some(candidate) = state.candidates.get_mut(state.index) {
            candidate.selected = !candidate.selected;
            candidate.error = None;
        }
    }

    pub fn toggle_all_bulk_rename_candidates(&mut self) {
        let Some(state) = self.bulk_rename.as_mut() else {
            return;
        };
        let select = state.candidates.iter().any(|candidate| !candidate.selected);
        for candidate in &mut state.candidates {
            candidate.selected = select;
            candidate.error = None;
        }
    }

    pub fn begin_bulk_name_generation(
        &mut self,
        selected_only: bool,
    ) -> Result<ThreadNameGenerationRequest> {
        let model_settings = thread_name_model_settings(&self.models);
        let state = self
            .bulk_rename
            .as_mut()
            .context("bulk rename is not open")?;
        let return_to_review = state.phase != BulkRenamePhase::Select;
        let threads = if selected_only {
            state
                .candidates
                .get(state.index)
                .map(|candidate| {
                    vec![(candidate.thread_id.clone(), candidate.current_name.clone())]
                })
                .unwrap_or_default()
        } else {
            state
                .candidates
                .iter()
                .filter(|candidate| candidate.selected)
                .map(|candidate| (candidate.thread_id.clone(), candidate.current_name.clone()))
                .collect()
        };
        anyhow::ensure!(!threads.is_empty(), "Select at least one thread");
        state.generating_ids = threads.iter().map(|(id, _)| id.clone()).collect();
        state.phase = BulkRenamePhase::Generating { return_to_review };
        state.progress = Some(BulkRenameProgress::Reading {
            completed: 0,
            total: threads.len(),
        });
        state.progress_started_at = Instant::now();
        let (model, effort) = model_settings
            .map(|(model, effort)| (Some(model), effort))
            .unwrap_or_default();
        Ok(ThreadNameGenerationRequest {
            repository_path: state.repository_path.clone(),
            threads,
            model,
            effort,
        })
    }

    pub fn complete_bulk_name_generation(
        &mut self,
        result: std::result::Result<Vec<(String, String)>, String>,
    ) {
        let Some(state) = self.bulk_rename.as_mut() else {
            return;
        };
        let return_to_review = matches!(
            state.phase,
            BulkRenamePhase::Generating {
                return_to_review: true
            }
        );
        match result {
            Ok(suggestions) => {
                let suggestions = suggestions.into_iter().collect::<HashMap<_, _>>();
                for candidate in &mut state.candidates {
                    if !state.generating_ids.contains(&candidate.thread_id) {
                        continue;
                    }
                    candidate.error = None;
                    if let Some(name) = suggestions.get(&candidate.thread_id) {
                        candidate.proposed_name.clone_from(name);
                        candidate.selected = name.trim() != candidate.current_name.trim();
                    } else {
                        candidate.error = Some("Codex did not return a suggestion".into());
                        candidate.selected = false;
                    }
                }
                state.phase = BulkRenamePhase::Review;
                self.message = None;
            }
            Err(error) => {
                state.phase = if return_to_review {
                    BulkRenamePhase::Review
                } else {
                    BulkRenamePhase::Select
                };
                self.message = Some(format!("Could not suggest thread names: {error}"));
            }
        }
        state.generating_ids.clear();
        state.progress = None;
    }

    pub fn update_bulk_rename_progress(&mut self, progress: BulkRenameProgress) {
        let Some(state) = self.bulk_rename.as_mut() else {
            return;
        };
        let stage_changed = state.progress.is_none_or(|current| {
            std::mem::discriminant(&current) != std::mem::discriminant(&progress)
        });
        state.progress = Some(progress);
        if stage_changed {
            state.progress_started_at = Instant::now();
        }
    }

    pub fn bulk_rename_is_busy(&self) -> bool {
        self.bulk_rename.as_ref().is_some_and(|state| {
            matches!(
                state.phase,
                BulkRenamePhase::Generating { .. } | BulkRenamePhase::Applying
            )
        })
    }

    pub fn begin_bulk_rename_edit(&mut self) {
        let Some(state) = self.bulk_rename.as_mut() else {
            return;
        };
        let Some(candidate) = state.candidates.get(state.index) else {
            return;
        };
        state.edit_input.clone_from(&candidate.proposed_name);
        state.phase = BulkRenamePhase::Editing;
        self.message = None;
    }

    pub fn cancel_bulk_rename_edit(&mut self) {
        if let Some(state) = self.bulk_rename.as_mut() {
            state.edit_input.clear();
            state.phase = BulkRenamePhase::Review;
        }
        self.message = None;
    }

    pub fn save_bulk_rename_edit(&mut self, name: String) {
        let Some(state) = self.bulk_rename.as_mut() else {
            return;
        };
        if let Some(candidate) = state.candidates.get_mut(state.index) {
            candidate.proposed_name = name;
            candidate.selected = candidate.proposed_name.trim() != candidate.current_name.trim();
            candidate.error = None;
        }
        state.edit_input.clear();
        state.phase = BulkRenamePhase::Review;
        self.message = None;
    }

    pub fn confirm_bulk_thread_rename(&mut self) -> Result<()> {
        let state = self
            .bulk_rename
            .as_mut()
            .context("bulk rename is not open")?;
        anyhow::ensure!(
            state.candidates.iter().any(|candidate| {
                candidate.selected
                    && candidate.proposed_name.trim() != candidate.current_name.trim()
            }),
            "Select at least one changed name"
        );
        state.phase = BulkRenamePhase::ConfirmApply;
        Ok(())
    }

    pub fn submit_bulk_thread_rename(&mut self) -> Result<Option<ThreadNameApplyRequest>> {
        let requires_confirmation = self
            .bulk_rename
            .as_ref()
            .context("bulk rename is not open")?
            .requires_apply_confirmation;
        if requires_confirmation {
            self.confirm_bulk_thread_rename()?;
            Ok(None)
        } else {
            self.begin_bulk_thread_rename_apply().map(Some)
        }
    }

    pub fn begin_bulk_thread_rename_apply(&mut self) -> Result<ThreadNameApplyRequest> {
        let state = self
            .bulk_rename
            .as_mut()
            .context("bulk rename is not open")?;
        let names = state
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.selected
                    && candidate.proposed_name.trim() != candidate.current_name.trim()
            })
            .map(|candidate| (candidate.thread_id.clone(), candidate.proposed_name.clone()))
            .collect::<Vec<_>>();
        anyhow::ensure!(!names.is_empty(), "Select at least one changed name");
        state.phase = BulkRenamePhase::Applying;
        state.progress = Some(BulkRenameProgress::Applying {
            completed: 0,
            total: names.len(),
        });
        state.progress_started_at = Instant::now();
        Ok(ThreadNameApplyRequest { names })
    }

    pub fn complete_bulk_thread_rename_apply(
        &mut self,
        successes: Vec<(String, String)>,
        failures: Vec<(String, String)>,
    ) {
        for (thread_id, name) in &successes {
            self.apply_thread_name(thread_id, Some(name.clone()));
        }
        if failures.is_empty() {
            let count = successes.len();
            self.close_bulk_thread_rename();
            self.message = Some(format!(
                "Renamed {count} thread{}",
                if count == 1 { "" } else { "s" }
            ));
            return;
        }
        let failures = failures.into_iter().collect::<HashMap<_, _>>();
        if let Some(state) = self.bulk_rename.as_mut() {
            for candidate in &mut state.candidates {
                if let Some(error) = failures.get(&candidate.thread_id) {
                    candidate.error = Some(error.clone());
                    candidate.selected = true;
                } else if successes.iter().any(|(id, _)| id == &candidate.thread_id) {
                    candidate.current_name.clone_from(&candidate.proposed_name);
                    candidate.selected = false;
                    candidate.error = None;
                }
            }
            state.phase = BulkRenamePhase::Review;
            state.progress = None;
        }
        self.message = Some(format!(
            "{} thread rename{} failed; review the highlighted rows",
            failures.len(),
            if failures.len() == 1 { "" } else { "s" }
        ));
    }

    fn start_scan(&mut self, scope: ScanScope) {
        self.scan_label = Some(match &scope {
            ScanScope::Roots(_) => "projects folders",
            ScanScope::Home => "home directory",
        });
        self.scan_metric_scope = Some(match &scope {
            ScanScope::Roots(_) => "roots",
            ScanScope::Home => "home",
        });
        self.scan_started_at = self.performance.start_timer();
        self.scan_receiver = Some(start_scan(scope));
        self.scan_animation_tick = 0;
        self.scan_first_result_recorded = false;
        self.scan_found_count = 0;
        self.scanning = true;
    }

    pub fn persist_repository_ui_state(&mut self) {
        if let Err(error) = self.save_repository_ui_state() {
            self.message = Some(format!("Could not save repository view state: {error}"));
        }
    }

    fn save_repository_ui_state(&self) -> Result<()> {
        let (selected_repository, selected_thread_id) = match self.selected_tree_row() {
            Some(TreeRow::Repository { repository_index }) => (
                self.repositories
                    .get(repository_index)
                    .map(|repository| repository.path.as_path()),
                None,
            ),
            Some(TreeRow::Thread {
                repository_index,
                thread_index,
            }) => (
                self.repositories
                    .get(repository_index)
                    .map(|repository| repository.path.as_path()),
                self.threads
                    .get(thread_index)
                    .map(|thread| thread.record.id.as_str()),
            ),
            Some(TreeRow::GeneralThread { thread_index }) => (
                None,
                self.threads
                    .get(thread_index)
                    .map(|thread| thread.record.id.as_str()),
            ),
            Some(TreeRow::General) | None => (None, None),
        };
        self.repository_store.save_ui_state(
            &self.expanded_repositories,
            selected_repository,
            selected_thread_id,
        )
    }

    fn persist_expanded_repositories(&mut self) {
        self.persist_repository_ui_state();
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
            Some(TreeRow::General) => {}
            Some(TreeRow::GeneralThread { thread_index }) => {
                self.thread_index = thread_index;
            }
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
        self.locations = if matches!(
            self.selected_tree_row(),
            Some(TreeRow::General | TreeRow::GeneralThread { .. })
        ) {
            Vec::new()
        } else {
            self.selected_repository()
                .and_then(|repository| self.workspaces_by_repository.get(&repository.path))
                .cloned()
                .unwrap_or_default()
        };
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

    fn select_general_row(&mut self) {
        self.tree_index = 0;
        self.sync_selection_from_tree();
    }

    fn selected_thread_position_in_repository(&self) -> Option<usize> {
        let rows = self.tree_rows();
        let selected = rows.get(self.tree_index)?;
        Some(
            rows[..self.tree_index]
                .iter()
                .filter(|row| same_thread_group(row, selected))
                .count(),
        )
    }

    fn select_nearby_thread(&mut self, record: &ThreadRecord, preferred_position: usize) {
        let rows = self.tree_rows();
        let indexes = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row_belongs_to_record_group(row, record, &self.repositories))
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if let Some(tree_index) = indexes
            .get(preferred_position)
            .or_else(|| indexes.last())
            .copied()
        {
            self.tree_index = tree_index;
            self.sync_selection_from_tree();
        } else if record.scope == ThreadScope::General {
            self.select_general_row();
        } else if let Some(repository_index) = self
            .repositories
            .iter()
            .position(|repository| repository.path == record.repository_path)
        {
            self.select_repository_row(repository_index);
        }
    }

    fn restore_tree_selection(&mut self, selection: TreeSelection) {
        if selection == TreeSelection::General {
            self.select_general_row();
            return;
        }
        let (repository_path, thread_id) = match selection {
            TreeSelection::General => unreachable!(),
            TreeSelection::Repository(path) => (path, None),
            TreeSelection::Thread(id) => {
                let Some(thread) = self.threads.iter().find(|thread| thread.record.id == id) else {
                    return;
                };
                if thread.record.scope == ThreadScope::General {
                    if let Some(index) = self.tree_rows().iter().position(|row| {
                        matches!(row, TreeRow::GeneralThread { thread_index } if self.threads[*thread_index].record.id == id)
                    }) {
                        self.tree_index = index;
                    }
                    return;
                }
                (thread.record.repository_path.clone(), Some(id))
            }
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

fn display_thread_name(name: Option<&str>) -> String {
    name.filter(|name| !name.trim().is_empty())
        .unwrap_or("Untitled thread")
        .to_owned()
}

fn should_apply_refreshed_thread_name(
    overwrite_existing: bool,
    confirmed_thread_names: &HashSet<String>,
    thread_id: &str,
) -> bool {
    overwrite_existing || !confirmed_thread_names.contains(thread_id)
}

fn thread_name_update(event: &AppServerEvent) -> Option<(&str, Option<String>)> {
    (event.method == "thread/name/updated").then_some(())?;
    let thread_id = event.thread_id.as_deref()?;
    let name = event
        .params
        .get("threadName")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Some((thread_id, name))
}

fn apply_thread_name_to(
    threads: &mut [ThreadItem],
    chats: &mut HashMap<String, ChatState>,
    thread_id: &str,
    title: &str,
) {
    if let Some(thread) = threads
        .iter_mut()
        .find(|thread| thread.record.id == thread_id)
    {
        thread.record.title = title.to_owned();
    }
    if let Some(chat) = chats.get_mut(thread_id) {
        chat.title = title.to_owned();
    }
}

fn should_show_onboarding(
    marker_is_pending: bool,
    initial_scan_is_pending: bool,
    repositories_are_empty: bool,
    has_existing_threads: bool,
) -> bool {
    marker_is_pending && initial_scan_is_pending && repositories_are_empty && !has_existing_threads
}

fn repository_registration_path(
    requested_path: Option<&Path>,
    source_cwd: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(path) = requested_path {
        anyhow::ensure!(path.is_absolute(), "repository path must be absolute");
        return Ok(path.to_path_buf());
    }
    source_cwd
        .map(Path::to_path_buf)
        .context("source thread working directory is unavailable")
}

fn import_codex_workspaces(
    repository_store: &RepositoryStore,
    repositories: &mut Vec<Repository>,
) -> (usize, Option<PathBuf>) {
    let Ok(Some(state)) = codex_workspace::discover() else {
        return (0, None);
    };
    let (imported, active_repository) = resolve_codex_workspaces(state);
    if imported.is_empty() || repository_store.register(&imported).is_err() {
        return (0, None);
    }
    let Ok(registered) = repository_store.load_registered() else {
        return (0, None);
    };
    let imported_count = imported.len();
    *repositories = registered;
    (imported_count, active_repository)
}

fn resolve_codex_workspaces(
    state: codex_workspace::CodexWorkspaceState,
) -> (Vec<Repository>, Option<PathBuf>) {
    let active_roots = state.active_roots.into_iter().collect::<HashSet<_>>();
    let mut imported = Vec::new();
    let mut active_repository = None;
    for root in state.roots {
        let Ok(repository) = repository::repository_at(&root) else {
            continue;
        };
        if active_roots.contains(&root) {
            active_repository = Some(repository.path.clone());
        }
        if !imported
            .iter()
            .any(|candidate: &Repository| candidate.path == repository.path)
        {
            imported.push(repository);
        }
    }
    let startup_repository =
        active_repository.or_else(|| imported.first().map(|repository| repository.path.clone()));
    (imported, startup_repository)
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

fn ensure_thread_deletion_context(show_archived: bool, record: &ThreadRecord) -> Result<()> {
    anyhow::ensure!(
        show_archived && record.archived_at.is_some(),
        "permanent deletion is only available for archived threads"
    );
    Ok(())
}

fn take_pending_approval(
    approvals: &mut VecDeque<AppServerRequest>,
    thread_id: Option<&str>,
) -> Option<AppServerRequest> {
    let index = approvals
        .iter()
        .position(|request| request.thread_id.as_deref() == thread_id)?;
    approvals.remove(index)
}

fn thread_group_has_active_turn(
    thread_id: &str,
    chats: &HashMap<String, ChatState>,
    side_chats_by_parent: &HashMap<String, Vec<String>>,
    owned_turns: &HashMap<String, String>,
) -> bool {
    std::iter::once(thread_id)
        .chain(
            side_chats_by_parent
                .get(thread_id)
                .into_iter()
                .flatten()
                .map(String::as_str),
        )
        .any(|thread_id| {
            owned_turns.contains_key(thread_id)
                || chats
                    .get(thread_id)
                    .is_some_and(|chat| chat.active_turn_id.is_some())
        })
}

fn thread_picker_return_mode(focus: Focus) -> Mode {
    match focus {
        Focus::Navigation => Mode::Normal,
        Focus::Chat => Mode::Chat,
    }
}

fn thread_subscription_targets(
    visible_chat_id: Option<&str>,
    side_chat_id: Option<&str>,
    opened_threads: &HashSet<String>,
    chats: &HashMap<String, ChatState>,
    owned_turns: &HashMap<String, String>,
) -> HashSet<String> {
    opened_threads
        .iter()
        .filter(|thread_id| {
            visible_chat_id == Some(thread_id.as_str())
                || side_chat_id == Some(thread_id.as_str())
                || chats
                    .get(thread_id.as_str())
                    .is_some_and(|chat| chat.active_turn_id.is_some())
                || owned_turns.contains_key(thread_id.as_str())
        })
        .cloned()
        .collect()
}

fn reconcile_owned_turn(
    owned_turns: &mut HashMap<String, String>,
    thread_id: &str,
    active_turn_id: Option<&str>,
) {
    if owned_turns.get(thread_id).map(String::as_str) != active_turn_id {
        owned_turns.remove(thread_id);
    }
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

fn thread_name_model_settings(models: &[ModelMetadata]) -> Option<(String, Option<String>)> {
    let model = models
        .iter()
        .find(|model| model.model == THREAD_NAME_MODEL)
        .or_else(|| models.iter().find(|model| model.is_default))
        .or_else(|| models.first())?;
    let effort = model
        .supported_reasoning_efforts
        .iter()
        .find(|effort| effort.reasoning_effort == "low")
        .map(|effort| effort.reasoning_effort.clone())
        .or_else(|| preferred_reasoning_effort(model).map(str::to_owned));
    Some((model.model.clone(), effort))
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

fn adjacent_chat_position(
    active_pane: ChatPane,
    current_side_chat: Option<usize>,
    side_chat_count: usize,
    forward: bool,
) -> (ChatPane, Option<usize>) {
    if side_chat_count == 0 {
        return (ChatPane::Main, None);
    }
    let current = match active_pane {
        ChatPane::Main => 0,
        ChatPane::Side => current_side_chat.map(|index| index + 1).unwrap_or(0),
    };
    let next = cycle_index(current, side_chat_count + 1, forward);
    if next == 0 {
        (ChatPane::Main, None)
    } else {
        (ChatPane::Side, Some(next - 1))
    }
}

fn next_available_index(current: usize, forward: bool, available: &[bool]) -> Option<usize> {
    if forward {
        ((current + 1)..available.len()).find(|index| available[*index])
    } else {
        (0..current).rev().find(|index| available[*index])
    }
}

fn touch_preview_cache(
    order: &mut VecDeque<String>,
    thread_id: &str,
    capacity: usize,
) -> Vec<String> {
    order.retain(|cached| cached != thread_id);
    order.push_back(thread_id.to_owned());
    let mut evicted = Vec::new();
    while order.len() > capacity {
        if let Some(thread_id) = order.pop_front() {
            evicted.push(thread_id);
        }
    }
    evicted
}

fn tree_rows_for(
    repositories: &[Repository],
    threads: &[ThreadItem],
    expanded_repositories: &HashSet<PathBuf>,
) -> Vec<TreeRow> {
    let mut rows = Vec::with_capacity(1 + repositories.len() + threads.len());
    rows.push(TreeRow::General);
    rows.extend(
        threads
            .iter()
            .enumerate()
            .filter(|(_, thread)| thread.record.scope == ThreadScope::General)
            .map(|(thread_index, _)| TreeRow::GeneralThread { thread_index }),
    );
    for (repository_index, repository) in repositories.iter().enumerate() {
        rows.push(TreeRow::Repository { repository_index });
        if expanded_repositories.contains(&repository.path) {
            rows.extend(
                threads
                    .iter()
                    .enumerate()
                    .filter(|(_, thread)| {
                        thread.record.scope == ThreadScope::Repository
                            && thread.record.repository_path == repository.path
                    })
                    .map(|(thread_index, _)| TreeRow::Thread {
                        repository_index,
                        thread_index,
                    }),
            );
        }
    }
    rows
}

fn thread_navigation_order<'a>(
    threads: &'a [ThreadItem],
    repositories: &[Repository],
) -> Vec<&'a str> {
    let mut thread_ids = threads
        .iter()
        .filter(|thread| thread.record.scope == ThreadScope::General)
        .map(|thread| thread.record.id.as_str())
        .collect::<Vec<_>>();
    for repository in repositories {
        thread_ids.extend(
            threads
                .iter()
                .filter(|thread| {
                    thread.record.scope == ThreadScope::Repository
                        && thread.record.repository_path == repository.path
                })
                .map(|thread| thread.record.id.as_str()),
        );
    }
    thread_ids
}

fn adjacent_thread_id<'a>(
    threads: &'a [ThreadItem],
    repositories: &[Repository],
    current_thread_id: &str,
    forward: bool,
) -> Option<&'a str> {
    let thread_ids = thread_navigation_order(threads, repositories);
    if thread_ids.len() < 2 {
        return None;
    }
    let current = thread_ids
        .iter()
        .position(|thread_id| *thread_id == current_thread_id)?;
    Some(thread_ids[cycle_index(current, thread_ids.len(), forward)])
}

fn same_thread_group(left: &TreeRow, right: &TreeRow) -> bool {
    match (left, right) {
        (TreeRow::GeneralThread { .. }, TreeRow::GeneralThread { .. }) => true,
        (
            TreeRow::Thread {
                repository_index: left,
                ..
            },
            TreeRow::Thread {
                repository_index: right,
                ..
            },
        ) => left == right,
        _ => false,
    }
}

fn row_belongs_to_record_group(
    row: &TreeRow,
    record: &ThreadRecord,
    repositories: &[Repository],
) -> bool {
    match row {
        TreeRow::GeneralThread { .. } => record.scope == ThreadScope::General,
        TreeRow::Thread {
            repository_index, ..
        } => {
            record.scope == ThreadScope::Repository
                && repositories
                    .get(*repository_index)
                    .is_some_and(|repository| repository.path == record.repository_path)
        }
        _ => false,
    }
}

fn remember_archive_undo(history: &mut VecDeque<ArchivedThreadUndo>, undo: ArchivedThreadUndo) {
    forget_archive_undo(history, &undo.id);
    if history.len() == MAX_ARCHIVE_UNDOS {
        history.pop_front();
    }
    history.push_back(undo);
}

fn forget_archive_undo(history: &mut VecDeque<ArchivedThreadUndo>, thread_id: &str) {
    history.retain(|undo| undo.id != thread_id);
}

fn latest_valid_archive_undo(
    history: &mut VecDeque<ArchivedThreadUndo>,
    records: &[ThreadRecord],
) -> Option<ArchivedThreadUndo> {
    while let Some(undo) = history.back() {
        let is_still_archived = records
            .iter()
            .any(|record| record.id == undo.id && record.archived_at.is_some());
        if is_still_archived {
            return Some(undo.clone());
        }
        history.pop_back();
    }
    None
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

fn selected_browse_directory(directories: &[PathBuf], index: usize) -> Option<&Path> {
    directories.get(index).map(PathBuf::as_path)
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
    use tempfile::tempdir;

    use super::*;

    fn thread(id: &str, repository: &str) -> ThreadItem {
        ThreadItem {
            record: ThreadRecord {
                id: id.into(),
                scope: ThreadScope::Repository,
                kind: ThreadKind::Regular,
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

    fn general_thread(id: &str) -> ThreadItem {
        let mut thread = thread(id, "/general");
        thread.record.scope = ThreadScope::General;
        thread.is_primary = false;
        thread
    }

    fn model(default: &str, efforts: &[&str]) -> ModelMetadata {
        named_model("test", true, default, efforts)
    }

    fn named_model(name: &str, is_default: bool, default: &str, efforts: &[&str]) -> ModelMetadata {
        ModelMetadata {
            id: name.into(),
            model: name.into(),
            display_name: name.into(),
            description: String::new(),
            default_reasoning_effort: default.into(),
            supported_reasoning_efforts: efforts
                .iter()
                .map(|effort| crate::app_server::ReasoningEffortMetadata {
                    reasoning_effort: (*effort).into(),
                    description: String::new(),
                })
                .collect(),
            input_modalities: vec!["text".into(), "image".into()],
            is_default,
        }
    }

    fn approval(id: i64, thread_id: Option<&str>) -> AppServerRequest {
        AppServerRequest {
            id: json!(id),
            method: "item/permissions/requestApproval".into(),
            params: json!({"threadId": thread_id}),
            thread_id: thread_id.map(str::to_owned),
            turn_id: None,
        }
    }

    #[test]
    fn repository_registration_uses_an_exact_path_or_the_source_cwd() {
        let root = tempdir().unwrap();
        let explicit = root.path().join("repository");
        let source_cwd = root.path().join("current-worktree");

        assert_eq!(
            repository_registration_path(Some(&explicit), Some(&source_cwd)).unwrap(),
            explicit
        );
        assert_eq!(
            repository_registration_path(None, Some(&source_cwd)).unwrap(),
            source_cwd
        );
        assert!(repository_registration_path(Some(Path::new("relative")), None).is_err());
        assert!(repository_registration_path(None, None).is_err());
    }

    fn archive_undo(id: &str) -> ArchivedThreadUndo {
        ArchivedThreadUndo {
            id: id.into(),
            title: id.into(),
        }
    }

    #[test]
    fn archive_undo_history_is_bounded_and_deduplicated() {
        let mut history = VecDeque::new();
        for index in 0..=MAX_ARCHIVE_UNDOS {
            remember_archive_undo(&mut history, archive_undo(&index.to_string()));
        }

        assert_eq!(history.len(), MAX_ARCHIVE_UNDOS);
        assert_eq!(history.front().map(|undo| undo.id.as_str()), Some("1"));
        assert_eq!(history.back().map(|undo| undo.id.as_str()), Some("20"));

        remember_archive_undo(&mut history, archive_undo("1"));
        assert_eq!(history.len(), MAX_ARCHIVE_UNDOS);
        assert_eq!(history.back().map(|undo| undo.id.as_str()), Some("1"));
    }

    #[test]
    fn browse_selection_uses_the_highlighted_directory() {
        let directories = vec![PathBuf::from("first"), PathBuf::from("second")];

        assert_eq!(
            selected_browse_directory(&directories, 1),
            Some(Path::new("second"))
        );
        assert_eq!(selected_browse_directory(&directories, 2), None);
    }

    #[test]
    fn archive_undo_skips_deleted_and_restored_threads() {
        let mut archived = thread("archived", "/repo").record;
        archived.archived_at = Some(1);
        let restored = thread("restored", "/repo").record;
        let records = vec![archived, restored];
        let mut history = VecDeque::from([
            archive_undo("archived"),
            archive_undo("restored"),
            archive_undo("deleted"),
        ]);

        let undo = latest_valid_archive_undo(&mut history, &records).unwrap();

        assert_eq!(undo.id, "archived");
        assert_eq!(history.len(), 1);
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
    fn luna_with_low_effort_is_used_for_thread_names() {
        let models = vec![
            named_model("gpt-5.6-sol", true, "medium", &["low", "medium"]),
            named_model(THREAD_NAME_MODEL, false, "medium", &["low", "medium"]),
        ];

        assert_eq!(
            thread_name_model_settings(&models),
            Some((THREAD_NAME_MODEL.into(), Some("low".into())))
        );
    }

    #[test]
    fn thread_names_fall_back_to_the_default_model() {
        let models = vec![
            named_model("other", false, "medium", &["medium"]),
            named_model("default", true, "medium", &["medium"]),
        ];

        assert_eq!(
            thread_name_model_settings(&models),
            Some(("default".into(), Some("medium".into())))
        );
    }

    #[test]
    fn rename_actions_offer_a_selected_thread_suggestion_before_bulk_scopes() {
        assert_eq!(
            RenameAction::ALL,
            [
                RenameAction::RenameThread,
                RenameAction::SuggestThread,
                RenameAction::SuggestRepository,
                RenameAction::SuggestAll,
            ]
        );
        assert_eq!(
            RenameAction::SuggestThread.label(),
            "Suggest name for selected thread"
        );
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
    fn preview_cache_evicts_the_least_recently_used_thread() {
        let mut order = VecDeque::new();
        assert!(touch_preview_cache(&mut order, "one", 2).is_empty());
        assert!(touch_preview_cache(&mut order, "two", 2).is_empty());
        assert!(touch_preview_cache(&mut order, "one", 2).is_empty());

        assert_eq!(touch_preview_cache(&mut order, "three", 2), ["two"]);
        assert_eq!(order, ["one", "three"]);
    }

    #[test]
    fn subscriptions_keep_visible_and_running_opened_threads_only() {
        let mut chats = HashMap::from([
            (
                "visible".into(),
                ChatState::new("visible".into(), "/repo".into(), "visible".into()),
            ),
            (
                "running".into(),
                ChatState::new("running".into(), "/repo".into(), "running".into()),
            ),
            (
                "idle".into(),
                ChatState::new("idle".into(), "/repo".into(), "idle".into()),
            ),
            (
                "preview".into(),
                ChatState::new("preview".into(), "/repo".into(), "preview".into()),
            ),
        ]);
        chats
            .get_mut("running")
            .expect("running chat")
            .active_turn_id = Some("turn-1".into());
        let opened = HashSet::from(["visible".into(), "running".into(), "idle".into()]);

        assert_eq!(
            thread_subscription_targets(Some("visible"), None, &opened, &chats, &HashMap::new()),
            HashSet::from(["visible".into(), "running".into()])
        );
    }

    #[test]
    fn owned_background_turn_remains_a_subscription_target() {
        let chats = HashMap::from([(
            "background".into(),
            ChatState::new("background".into(), "/repo".into(), "background".into()),
        )]);
        let opened = HashSet::from(["background".into()]);
        let owned = HashMap::from([("background".into(), "turn-1".into())]);

        assert_eq!(
            thread_subscription_targets(None, None, &opened, &chats, &owned),
            HashSet::from(["background".into()])
        );
    }

    #[test]
    fn resync_forgets_an_owned_turn_that_is_no_longer_active() {
        let mut owned = HashMap::from([
            ("completed".into(), "turn-1".into()),
            ("running".into(), "turn-2".into()),
        ]);

        reconcile_owned_turn(&mut owned, "completed", None);
        reconcile_owned_turn(&mut owned, "running", Some("turn-2"));

        assert!(!owned.contains_key("completed"));
        assert_eq!(owned.get("running").map(String::as_str), Some("turn-2"));
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
                TreeRow::General,
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
    fn tree_groups_general_threads_before_repositories() {
        let repositories = vec![Repository {
            name: "repo".into(),
            path: "/repo".into(),
        }];
        let threads = vec![thread("repo-thread", "/repo"), general_thread("quick")];

        assert_eq!(
            tree_rows_for(&repositories, &threads, &HashSet::new()),
            vec![
                TreeRow::General,
                TreeRow::GeneralThread { thread_index: 1 },
                TreeRow::Repository {
                    repository_index: 0,
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
                TreeRow::General,
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
    fn thread_shortcuts_follow_the_full_sidebar_order() {
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
        let threads = vec![
            thread("two-new", "/two"),
            general_thread("general"),
            thread("one", "/one"),
            thread("two-old", "/two"),
        ];

        assert_eq!(
            thread_navigation_order(&threads, &repositories),
            vec!["general", "one", "two-new", "two-old"]
        );
        assert_eq!(
            adjacent_thread_id(&threads, &repositories, "one", true),
            Some("two-new")
        );
        assert_eq!(
            adjacent_thread_id(&threads, &repositories, "one", false),
            Some("general")
        );
    }

    #[test]
    fn horizontal_chat_navigation_cycles_main_and_side_chats() {
        assert_eq!(
            adjacent_chat_position(ChatPane::Main, Some(0), 2, true),
            (ChatPane::Side, Some(0))
        );
        assert_eq!(
            adjacent_chat_position(ChatPane::Side, Some(0), 2, true),
            (ChatPane::Side, Some(1))
        );
        assert_eq!(
            adjacent_chat_position(ChatPane::Side, Some(1), 2, true),
            (ChatPane::Main, None)
        );
        assert_eq!(
            adjacent_chat_position(ChatPane::Main, Some(0), 2, false),
            (ChatPane::Side, Some(1))
        );
    }

    #[test]
    fn thread_group_matching_distinguishes_general_and_repositories() {
        let rows = [
            TreeRow::GeneralThread { thread_index: 0 },
            TreeRow::Repository {
                repository_index: 0,
            },
            TreeRow::Thread {
                repository_index: 0,
                thread_index: 0,
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
                thread_index: 2,
            },
        ];

        assert!(same_thread_group(&rows[2], &rows[3]));
        assert!(!same_thread_group(&rows[0], &rows[2]));
        assert!(!same_thread_group(&rows[2], &rows[5]));
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
    fn name_update_changes_tree_chat_and_search_title_together() {
        let mut threads = vec![thread("thread-1", "/one")];
        let mut chats = HashMap::from([(
            "thread-1".into(),
            ChatState::new("thread-1".into(), "/one".into(), "Old name".into()),
        )]);

        apply_thread_name_to(&mut threads, &mut chats, "thread-1", "New name");

        assert_eq!(threads[0].record.title, "New name");
        assert_eq!(chats["thread-1"].title, "New name");
        assert_eq!(thread_picker_matches(&threads, &[], "New"), vec![0]);
        assert!(thread_picker_matches(&threads, &[], "Old").is_empty());
    }

    #[test]
    fn external_name_notification_accepts_set_and_cleared_names() {
        let named = AppServerEvent {
            method: "thread/name/updated".into(),
            params: json!({"threadId":"thread-1", "threadName":"External name"}),
            thread_id: Some("thread-1".into()),
            turn_id: None,
        };
        let cleared = AppServerEvent {
            method: "thread/name/updated".into(),
            params: json!({"threadId":"thread-1"}),
            thread_id: Some("thread-1".into()),
            turn_id: None,
        };

        assert_eq!(
            thread_name_update(&named),
            Some(("thread-1", Some("External name".into())))
        );
        assert_eq!(thread_name_update(&cleared), Some(("thread-1", None)));
        assert_eq!(display_thread_name(None), "Untitled thread");
    }

    #[test]
    fn thread_picker_returns_to_its_opening_pane_when_cancelled() {
        assert_eq!(thread_picker_return_mode(Focus::Navigation), Mode::Normal);
        assert_eq!(thread_picker_return_mode(Focus::Chat), Mode::Chat);
    }

    #[test]
    fn permanent_deletion_requires_an_archived_thread_in_the_archived_view() {
        let mut item = thread("thread-1", "/one");

        assert!(ensure_thread_deletion_context(false, &item.record).is_err());
        assert!(ensure_thread_deletion_context(true, &item.record).is_err());

        item.record.archived_at = Some(1);
        assert!(ensure_thread_deletion_context(false, &item.record).is_err());
        assert!(ensure_thread_deletion_context(true, &item.record).is_ok());
    }

    #[test]
    fn rename_action_navigation_skips_unavailable_entries() {
        let available = [false, true, true];

        assert_eq!(next_available_index(0, true, &available), Some(1));
        assert_eq!(next_available_index(1, true, &available), Some(2));
        assert_eq!(next_available_index(2, false, &available), Some(1));
        assert_eq!(next_available_index(1, false, &available), None);
    }

    #[test]
    fn pending_approval_is_taken_only_from_its_thread() {
        let mut approvals = VecDeque::from([
            approval(1, Some("one")),
            approval(2, Some("two")),
            approval(3, Some("one")),
            approval(4, None),
        ]);

        assert_eq!(
            take_pending_approval(&mut approvals, Some("two")).map(|request| request.id),
            Some(json!(2))
        );
        assert_eq!(
            take_pending_approval(&mut approvals, Some("one")).map(|request| request.id),
            Some(json!(1))
        );
        assert_eq!(
            take_pending_approval(&mut approvals, None).map(|request| request.id),
            Some(json!(4))
        );
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].id, json!(3));
    }

    #[test]
    fn active_parent_or_side_chat_prevents_archiving() {
        let mut chats = HashMap::from([
            (
                "parent".into(),
                ChatState::new("parent".into(), "/one".into(), "parent".into()),
            ),
            (
                "side".into(),
                ChatState::new("side".into(), "/one".into(), "side".into()),
            ),
        ]);
        let side_chats = HashMap::from([("parent".into(), vec!["side".into()])]);
        let mut owned_turns = HashMap::new();

        assert!(!thread_group_has_active_turn(
            "parent",
            &chats,
            &side_chats,
            &owned_turns
        ));

        chats.get_mut("parent").unwrap().active_turn_id = Some("turn-parent".into());
        assert!(thread_group_has_active_turn(
            "parent",
            &chats,
            &side_chats,
            &owned_turns
        ));

        chats.get_mut("parent").unwrap().active_turn_id = None;
        owned_turns.insert("side".into(), "turn-side".into());
        assert!(thread_group_has_active_turn(
            "parent",
            &chats,
            &side_chats,
            &owned_turns
        ));
        assert!(!thread_group_has_active_turn(
            "unrelated",
            &chats,
            &side_chats,
            &owned_turns
        ));
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
    fn live_name_updates_win_over_delayed_background_refreshes() {
        let confirmed = HashSet::from(["live".into()]);

        assert!(should_apply_refreshed_thread_name(
            false, &confirmed, "cached"
        ));
        assert!(!should_apply_refreshed_thread_name(
            false, &confirmed, "live"
        ));
        assert!(should_apply_refreshed_thread_name(true, &confirmed, "live"));
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

    #[test]
    fn onboarding_is_limited_to_a_genuinely_fresh_installation() {
        assert!(should_show_onboarding(true, true, true, false));
        assert!(!should_show_onboarding(false, true, true, false));
        assert!(!should_show_onboarding(true, false, true, false));
        assert!(!should_show_onboarding(true, true, false, false));
        assert!(!should_show_onboarding(true, true, true, true));
    }

    #[test]
    fn resolves_valid_codex_workspaces_and_selects_the_active_repository() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("first");
        let active = temp.path().join("active");
        let invalid = temp.path().join("invalid");
        for path in [&first, &active] {
            fs::create_dir_all(path.join(".git")).unwrap();
        }
        fs::create_dir(&invalid).unwrap();

        let (repositories, selected) =
            resolve_codex_workspaces(codex_workspace::CodexWorkspaceState {
                roots: vec![first.clone(), active.clone(), first, invalid.clone()],
                active_roots: vec![active.clone(), invalid],
            });

        assert_eq!(repositories.len(), 2);
        assert_eq!(selected, Some(active.canonicalize().unwrap()));
    }

    #[test]
    fn selects_the_first_imported_repository_without_an_active_workspace() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("first");
        fs::create_dir_all(first.join(".git")).unwrap();

        let (_, selected) = resolve_codex_workspaces(codex_workspace::CodexWorkspaceState {
            roots: vec![first.clone()],
            active_roots: Vec::new(),
        });

        assert_eq!(selected, Some(first.canonicalize().unwrap()));
    }
}
