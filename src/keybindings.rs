use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use serde::{Deserialize, Serialize};

use crate::paths;

const CONFIG_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum KeyContext {
    ApprovalChat,
    ChatPaletteEmpty,
    ChatPaletteQuery,
    ChatScroll,
    ChatInput,
    ChatInputEmpty,
    ChooseModel,
    ChooseReasoning,
    ChoosePermissions,
    ConfirmDangerous,
    ChooseSideChat,
    ChooseThreadEmpty,
    ChooseThreadQuery,
    ChooseRenameAction,
    RenameThread,
    BulkRenameSelect,
    BulkRenameReview,
    BulkRenameEdit,
    BulkRenameConfirm,
    Attention,
    ConfirmQuit,
    Approval,
    AddRepositories,
    FilterRepositories,
    BrowseDirectory,
    ChooseThreadTarget,
    ChooseExistingWorktree,
    ConfirmDeleteThread,
    ConfirmRemoveRepository,
    Help,
    Normal,
    NormalRepository,
    NormalThread,
    Inactive,
}

#[derive(Clone, Copy)]
struct ActionSpec {
    name: &'static str,
    context: KeyContext,
    canonical: &'static str,
    defaults: &'static [&'static str],
    fallbacks: &'static [&'static str],
}

macro_rules! action {
    ($name:literal, $context:ident, $canonical:literal, [$($default:literal),+ $(,)?]) => {
        ActionSpec {
            name: $name,
            context: KeyContext::$context,
            canonical: $canonical,
            defaults: &[$($default),+],
            fallbacks: &[],
        }
    };
    ($name:literal, $context:ident, $canonical:literal, [$($default:literal),+ $(,)?], fallback [$($fallback:literal),+ $(,)?]) => {
        ActionSpec {
            name: $name,
            context: KeyContext::$context,
            canonical: $canonical,
            defaults: &[$($default),+],
            fallbacks: &[$($fallback),+],
        }
    };
}

// Action names are the stable user-facing config contract. The canonical key is only used to
// feed the existing mode handlers after a configured key has matched.
static ACTIONS: &[ActionSpec] = &[
    action!("approval_chat.up", ApprovalChat, "up", ["up", "k"]),
    action!("approval_chat.down", ApprovalChat, "down", ["down", "j"]),
    action!("approval_chat.confirm", ApprovalChat, "enter", ["enter"]),
    action!("approval_chat.allow", ApprovalChat, "y", ["y", "Y"]),
    action!("approval_chat.deny", ApprovalChat, "n", ["n", "N"]),
    action!(
        "approval_chat.cancel",
        ApprovalChat,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!(
        "approval_chat.toggle_pane",
        ApprovalChat,
        "ctrl+g",
        ["ctrl+g"]
    ),
    action!(
        "palette.cancel",
        ChatPaletteEmpty,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("palette.up", ChatPaletteEmpty, "up", ["up", "k"]),
    action!("palette.down", ChatPaletteEmpty, "down", ["down", "j"]),
    action!(
        "palette.erase",
        ChatPaletteEmpty,
        "backspace",
        ["backspace"]
    ),
    action!("palette.select", ChatPaletteEmpty, "enter", ["enter"]),
    action!(
        "palette.cancel",
        ChatPaletteQuery,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("palette.query_up", ChatPaletteQuery, "up", ["up"]),
    action!("palette.query_down", ChatPaletteQuery, "down", ["down"]),
    action!(
        "palette.erase",
        ChatPaletteQuery,
        "backspace",
        ["backspace"]
    ),
    action!("palette.select", ChatPaletteQuery, "enter", ["enter"]),
    action!(
        "chat_scroll.focus_input",
        ChatScroll,
        "i",
        ["i", "enter", "tab"]
    ),
    action!(
        "chat_scroll.focus_tree",
        ChatScroll,
        "esc",
        ["esc", "h", "left"]
    ),
    action!(
        "chat_scroll.focus_next_pane",
        ChatScroll,
        "l",
        ["l", "right"]
    ),
    action!("chat_scroll.interrupt", ChatScroll, "ctrl+c", ["ctrl+c"]),
    action!("chat_scroll.palette", ChatScroll, "/", ["/"]),
    action!("chat_scroll.side_chat", ChatScroll, "ctrl+s", ["ctrl+s"]),
    action!("chat_scroll.toggle_pane", ChatScroll, "ctrl+g", ["ctrl+g"]),
    action!("chat_scroll.next_chat", ChatScroll, "ctrl+n", ["ctrl+n"]),
    action!(
        "chat_scroll.previous_chat",
        ChatScroll,
        "ctrl+p",
        ["ctrl+p"]
    ),
    action!(
        "chat_scroll.half_page_up",
        ChatScroll,
        "ctrl+u",
        ["ctrl+u", "u"]
    ),
    action!(
        "chat_scroll.half_page_down",
        ChatScroll,
        "ctrl+d",
        ["ctrl+d", "d"]
    ),
    action!("chat_scroll.previous_message", ChatScroll, "K", ["K"]),
    action!("chat_scroll.next_message", ChatScroll, "J", ["J"]),
    action!("chat_scroll.copy_message", ChatScroll, "y", ["y"]),
    action!("chat_scroll.copy_conversation", ChatScroll, "Y", ["Y"]),
    action!("chat_scroll.copy_editor_command", ChatScroll, "e", ["e"]),
    action!("chat_scroll.open_link_1", ChatScroll, "1", ["1"]),
    action!("chat_scroll.open_link_2", ChatScroll, "2", ["2"]),
    action!("chat_scroll.open_link_3", ChatScroll, "3", ["3"]),
    action!("chat_scroll.open_link_4", ChatScroll, "4", ["4"]),
    action!("chat_scroll.open_link_5", ChatScroll, "5", ["5"]),
    action!("chat_scroll.open_link_6", ChatScroll, "6", ["6"]),
    action!("chat_scroll.open_link_7", ChatScroll, "7", ["7"]),
    action!("chat_scroll.open_link_8", ChatScroll, "8", ["8"]),
    action!("chat_scroll.open_link_9", ChatScroll, "9", ["9"]),
    action!("chat_scroll.line_up", ChatScroll, "up", ["up", "k"]),
    action!("chat_scroll.line_down", ChatScroll, "down", ["down", "j"]),
    action!("chat_scroll.page_up", ChatScroll, "pageup", ["pageup"]),
    action!(
        "chat_scroll.page_down",
        ChatScroll,
        "pagedown",
        ["pagedown"]
    ),
    action!("chat_scroll.top", ChatScroll, "home", ["home", "g"]),
    action!("chat_scroll.bottom", ChatScroll, "end", ["end", "G"]),
    action!("chat_input.scroll", ChatInput, "tab", ["tab"]),
    action!("chat_input.focus_tree", ChatInput, "esc", ["esc"]),
    action!("chat_input.interrupt", ChatInput, "ctrl+c", ["ctrl+c"]),
    action!("chat_input.side_chat", ChatInput, "ctrl+s", ["ctrl+s"]),
    action!("chat_input.toggle_pane", ChatInput, "ctrl+g", ["ctrl+g"]),
    action!("chat_input.next_chat", ChatInput, "ctrl+n", ["ctrl+n"]),
    action!("chat_input.previous_chat", ChatInput, "ctrl+p", ["ctrl+p"]),
    action!(
        "chat_input.paste_image",
        ChatInput,
        "ctrl+v",
        ["ctrl+v", "alt+v"]
    ),
    action!("chat_input.remove_image", ChatInput, "ctrl+x", ["ctrl+x"]),
    action!("chat_input.clear", ChatInput, "ctrl+u", ["ctrl+u"]),
    action!(
        "chat_input.line_start",
        ChatInput,
        "ctrl+a",
        ["ctrl+a", "home"]
    ),
    action!(
        "chat_input.line_end",
        ChatInput,
        "ctrl+e",
        ["ctrl+e", "end"]
    ),
    action!("chat_input.reasoning", ChatInput, "ctrl+r", ["ctrl+r"]),
    action!(
        "chat_input.backspace",
        ChatInput,
        "backspace",
        ["backspace"]
    ),
    action!("chat_input.delete", ChatInput, "delete", ["delete"]),
    action!("chat_input.left", ChatInput, "left", ["left"]),
    action!("chat_input.right", ChatInput, "right", ["right"]),
    action!("chat_input.up", ChatInput, "up", ["up"]),
    action!("chat_input.down", ChatInput, "down", ["down"]),
    action!(
        "chat_input.newline",
        ChatInput,
        "shift+enter",
        ["shift+enter"]
    ),
    action!("chat_input.submit", ChatInput, "enter", ["enter"]),
    action!("chat_input.scroll", ChatInputEmpty, "tab", ["tab"]),
    action!("chat_input.focus_tree", ChatInputEmpty, "esc", ["esc"]),
    action!("chat_input.interrupt", ChatInputEmpty, "ctrl+c", ["ctrl+c"]),
    action!("chat_input.side_chat", ChatInputEmpty, "ctrl+s", ["ctrl+s"]),
    action!(
        "chat_input.toggle_pane",
        ChatInputEmpty,
        "ctrl+g",
        ["ctrl+g"]
    ),
    action!("chat_input.next_chat", ChatInputEmpty, "ctrl+n", ["ctrl+n"]),
    action!(
        "chat_input.previous_chat",
        ChatInputEmpty,
        "ctrl+p",
        ["ctrl+p"]
    ),
    action!(
        "chat_input.paste_image",
        ChatInputEmpty,
        "ctrl+v",
        ["ctrl+v", "alt+v"]
    ),
    action!(
        "chat_input.remove_image",
        ChatInputEmpty,
        "ctrl+x",
        ["ctrl+x"]
    ),
    action!("chat_input.clear", ChatInputEmpty, "ctrl+u", ["ctrl+u"]),
    action!(
        "chat_input.line_start",
        ChatInputEmpty,
        "ctrl+a",
        ["ctrl+a", "home"]
    ),
    action!(
        "chat_input.line_end",
        ChatInputEmpty,
        "ctrl+e",
        ["ctrl+e", "end"]
    ),
    action!("chat_input.reasoning", ChatInputEmpty, "ctrl+r", ["ctrl+r"]),
    action!(
        "chat_input.backspace",
        ChatInputEmpty,
        "backspace",
        ["backspace"]
    ),
    action!("chat_input.delete", ChatInputEmpty, "delete", ["delete"]),
    action!("chat_input.left", ChatInputEmpty, "left", ["left"]),
    action!("chat_input.right", ChatInputEmpty, "right", ["right"]),
    action!("chat_input.up", ChatInputEmpty, "up", ["up"]),
    action!("chat_input.down", ChatInputEmpty, "down", ["down"]),
    action!(
        "chat_input.newline",
        ChatInputEmpty,
        "shift+enter",
        ["shift+enter"]
    ),
    action!("chat_input.submit", ChatInputEmpty, "enter", ["enter"]),
    action!("chat_input.palette", ChatInputEmpty, "/", ["/"]),
    action!("model.cancel", ChooseModel, "esc", ["esc"], fallback["esc"]),
    action!("model.up", ChooseModel, "up", ["up", "k"]),
    action!("model.down", ChooseModel, "down", ["down", "j"]),
    action!("model.select", ChooseModel, "enter", ["enter"]),
    action!(
        "reasoning.cancel",
        ChooseReasoning,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("reasoning.up", ChooseReasoning, "up", ["up", "k"]),
    action!("reasoning.down", ChooseReasoning, "down", ["down", "j"]),
    action!("reasoning.select", ChooseReasoning, "enter", ["enter"]),
    action!(
        "permissions.cancel",
        ChoosePermissions,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("permissions.up", ChoosePermissions, "up", ["up", "k"]),
    action!("permissions.down", ChoosePermissions, "down", ["down", "j"]),
    action!("permissions.select", ChoosePermissions, "enter", ["enter"]),
    action!("dangerous.confirm", ConfirmDangerous, "y", ["y", "Y"]),
    action!(
        "dangerous.cancel",
        ConfirmDangerous,
        "esc",
        ["n", "N", "esc"],
        fallback["esc"]
    ),
    action!(
        "side_chat.cancel",
        ChooseSideChat,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("side_chat.up", ChooseSideChat, "up", ["up", "k"]),
    action!("side_chat.down", ChooseSideChat, "down", ["down", "j"]),
    action!("side_chat.select", ChooseSideChat, "enter", ["enter"]),
    action!(
        "thread_picker.cancel",
        ChooseThreadEmpty,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("thread_picker.up", ChooseThreadEmpty, "up", ["up", "k"]),
    action!(
        "thread_picker.down",
        ChooseThreadEmpty,
        "down",
        ["down", "j"]
    ),
    action!(
        "thread_picker.erase",
        ChooseThreadEmpty,
        "backspace",
        ["backspace"]
    ),
    action!(
        "thread_picker.select",
        ChooseThreadEmpty,
        "enter",
        ["enter"]
    ),
    action!("thread_picker.copy_id", ChooseThreadEmpty, "y", ["y"]),
    action!("thread_picker.copy_resume", ChooseThreadEmpty, "Y", ["Y"]),
    action!("thread_picker.rename", ChooseThreadEmpty, "R", ["R"]),
    action!(
        "thread_picker.cancel",
        ChooseThreadQuery,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("thread_picker.query_up", ChooseThreadQuery, "up", ["up"]),
    action!(
        "thread_picker.query_down",
        ChooseThreadQuery,
        "down",
        ["down"]
    ),
    action!(
        "thread_picker.erase",
        ChooseThreadQuery,
        "backspace",
        ["backspace"]
    ),
    action!(
        "thread_picker.select",
        ChooseThreadQuery,
        "enter",
        ["enter"]
    ),
    action!("thread_picker.rename", ChooseThreadQuery, "R", ["R"]),
    action!(
        "rename_action.cancel",
        ChooseRenameAction,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("rename_action.up", ChooseRenameAction, "up", ["up", "k"]),
    action!(
        "rename_action.down",
        ChooseRenameAction,
        "down",
        ["down", "j"]
    ),
    action!(
        "rename_action.select",
        ChooseRenameAction,
        "enter",
        ["enter"]
    ),
    action!(
        "rename.cancel",
        RenameThread,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("rename.erase", RenameThread, "backspace", ["backspace"]),
    action!("rename.save", RenameThread, "enter", ["enter"]),
    action!(
        "bulk_select.cancel",
        BulkRenameSelect,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("bulk_select.up", BulkRenameSelect, "up", ["up", "k"]),
    action!("bulk_select.down", BulkRenameSelect, "down", ["down", "j"]),
    action!("bulk_select.toggle", BulkRenameSelect, "space", ["space"]),
    action!("bulk_select.toggle_all", BulkRenameSelect, "a", ["a"]),
    action!("bulk_select.generate", BulkRenameSelect, "enter", ["enter"]),
    action!(
        "bulk_review.cancel",
        BulkRenameReview,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("bulk_review.up", BulkRenameReview, "up", ["up", "k"]),
    action!("bulk_review.down", BulkRenameReview, "down", ["down", "j"]),
    action!("bulk_review.toggle", BulkRenameReview, "space", ["space"]),
    action!("bulk_review.edit", BulkRenameReview, "e", ["e"]),
    action!("bulk_review.regenerate", BulkRenameReview, "r", ["r"]),
    action!("bulk_review.apply", BulkRenameReview, "enter", ["enter"]),
    action!(
        "bulk_edit.cancel",
        BulkRenameEdit,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!(
        "bulk_edit.erase",
        BulkRenameEdit,
        "backspace",
        ["backspace"]
    ),
    action!("bulk_edit.save", BulkRenameEdit, "enter", ["enter"]),
    action!(
        "bulk_confirm.apply",
        BulkRenameConfirm,
        "enter",
        ["y", "Y", "enter"]
    ),
    action!(
        "bulk_confirm.cancel",
        BulkRenameConfirm,
        "esc",
        ["n", "N", "esc"],
        fallback["esc"]
    ),
    action!(
        "attention.cancel",
        Attention,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("attention.up", Attention, "up", ["up", "k"]),
    action!("attention.down", Attention, "down", ["down", "j"]),
    action!("attention.dismiss", Attention, "d", ["d", "x"]),
    action!("attention.open", Attention, "enter", ["enter"]),
    action!("quit.confirm", ConfirmQuit, "y", ["y", "Y"]),
    action!(
        "quit.cancel",
        ConfirmQuit,
        "esc",
        ["n", "N", "esc"],
        fallback["esc"]
    ),
    action!("approval.up", Approval, "up", ["up", "k"]),
    action!("approval.down", Approval, "down", ["down", "j"]),
    action!("approval.confirm", Approval, "enter", ["enter"]),
    action!("approval.allow", Approval, "y", ["y", "Y"]),
    action!("approval.deny", Approval, "n", ["n", "N"]),
    action!("approval.cancel", Approval, "esc", ["esc"], fallback["esc"]),
    action!("repositories.quit", AddRepositories, "q", ["q"]),
    action!(
        "repositories.cancel",
        AddRepositories,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("repositories.up", AddRepositories, "up", ["up", "k"]),
    action!("repositories.down", AddRepositories, "down", ["down", "j"]),
    action!("repositories.toggle", AddRepositories, "space", ["space"]),
    action!("repositories.palette", AddRepositories, "/", ["/"]),
    action!("repositories.filter", AddRepositories, "f", ["f"]),
    action!("repositories.rescan", AddRepositories, "r", ["r"]),
    action!("repositories.scan_home", AddRepositories, "s", ["s"]),
    action!("repositories.browse", AddRepositories, "b", ["b"]),
    action!("repositories.add", AddRepositories, "enter", ["enter"]),
    action!(
        "repository_filter.close",
        FilterRepositories,
        "esc",
        ["esc", "enter"],
        fallback["esc"]
    ),
    action!(
        "repository_filter.erase",
        FilterRepositories,
        "backspace",
        ["backspace"]
    ),
    action!(
        "directory.cancel",
        BrowseDirectory,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!(
        "directory.parent",
        BrowseDirectory,
        "backspace",
        ["backspace", "h", "left"]
    ),
    action!("directory.up", BrowseDirectory, "up", ["up", "k"]),
    action!("directory.down", BrowseDirectory, "down", ["down", "j"]),
    action!(
        "directory.open",
        BrowseDirectory,
        "enter",
        ["enter", "l", "right"]
    ),
    action!("directory.scan", BrowseDirectory, "s", ["s"]),
    action!("directory.add", BrowseDirectory, "a", ["a"]),
    action!(
        "thread_target.cancel",
        ChooseThreadTarget,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("thread_target.up", ChooseThreadTarget, "up", ["up", "k"]),
    action!(
        "thread_target.down",
        ChooseThreadTarget,
        "down",
        ["down", "j"]
    ),
    action!(
        "thread_target.select",
        ChooseThreadTarget,
        "enter",
        ["enter"]
    ),
    action!(
        "worktree.cancel",
        ChooseExistingWorktree,
        "esc",
        ["esc"],
        fallback["esc"]
    ),
    action!("worktree.up", ChooseExistingWorktree, "up", ["up", "k"]),
    action!(
        "worktree.down",
        ChooseExistingWorktree,
        "down",
        ["down", "j"]
    ),
    action!(
        "worktree.select",
        ChooseExistingWorktree,
        "enter",
        ["enter"]
    ),
    action!(
        "delete_thread.confirm",
        ConfirmDeleteThread,
        "y",
        ["y", "Y"]
    ),
    action!(
        "delete_thread.cancel",
        ConfirmDeleteThread,
        "esc",
        ["n", "N", "esc"],
        fallback["esc"]
    ),
    action!(
        "remove_repository.confirm",
        ConfirmRemoveRepository,
        "y",
        ["y", "Y"]
    ),
    action!(
        "remove_repository.cancel",
        ConfirmRemoveRepository,
        "esc",
        ["n", "N", "esc"],
        fallback["esc"]
    ),
    action!("help.ask_shikigami", Help, "enter", ["enter"]),
    action!(
        "help.close",
        Help,
        "esc",
        ["esc", "q", "?"],
        fallback["esc"]
    ),
    action!("normal.quit", Normal, "q", ["q"]),
    action!("normal.help", Normal, "?", ["?"]),
    action!("normal.attention", Normal, "!", ["!"]),
    action!("normal.palette", Normal, "/", ["/"]),
    action!("normal.find_thread", Normal, "f", ["f"]),
    action!("normal.collapse_all", Normal, "H", ["H"]),
    action!("normal.expand_all", Normal, "L", ["L"]),
    action!("normal.focus_chat", Normal, "tab", ["tab"]),
    action!("normal.up", Normal, "up", ["up", "k"]),
    action!("normal.down", Normal, "down", ["down", "j"]),
    action!("normal.add_repository", Normal, "a", ["a"]),
    action!("normal.toggle_archived", Normal, "A", ["A"]),
    action!("normal.undo_archive", Normal, "u", ["u"]),
    action!("normal.new_thread", Normal, "n", ["n"]),
    action!("normal.refresh", Normal, "r", ["r"]),
    action!("normal.rename", Normal, "R", ["R"]),
    action!(
        "normal.repository.collapse",
        NormalRepository,
        "h",
        ["h", "left"]
    ),
    action!(
        "normal.repository.expand",
        NormalRepository,
        "l",
        ["l", "right"]
    ),
    action!("normal.repository.remove", NormalRepository, "d", ["d"]),
    action!(
        "normal.repository.toggle",
        NormalRepository,
        "enter",
        ["enter"]
    ),
    action!(
        "normal.thread.parent",
        NormalThread,
        "esc",
        ["esc", "h", "left"],
        fallback["esc"]
    ),
    action!(
        "normal.thread.open_scroll",
        NormalThread,
        "l",
        ["l", "right"]
    ),
    action!(
        "normal.thread.open_input",
        NormalThread,
        "i",
        ["i", "enter"]
    ),
    action!("normal.thread.copy_id", NormalThread, "y", ["y"]),
    action!("normal.thread.copy_resume", NormalThread, "Y", ["Y"]),
    action!("normal.thread.delete", NormalThread, "d", ["d"]),
    action!("normal.thread.archive_or_restore", NormalThread, "x", ["x"]),
];

#[derive(Deserialize, Serialize)]
struct ConfigFile {
    version: u8,
    keybindings: BTreeMap<String, Vec<String>>,
}

#[derive(Clone)]
struct ResolvedAction {
    name: &'static str,
    canonical: KeyPattern,
    configured: Vec<KeyPattern>,
    defaults: Vec<KeyPattern>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct KeyPattern {
    code: KeyCode,
    modifiers: KeyModifiers,
}

pub struct KeyBindings {
    path: PathBuf,
    by_context: HashMap<KeyContext, Vec<ResolvedAction>>,
    labels: HashMap<String, String>,
}

impl KeyBindings {
    pub fn load_or_create() -> Result<Self> {
        let path = Self::ensure_config_file()?;
        Self::load_or_create_at(path)
    }

    pub fn ensure_config_file() -> Result<PathBuf> {
        let path = Self::config_path()?;
        ensure_config_file_at(&path)?;
        Ok(path)
    }

    pub fn reset_config_file() -> Result<(PathBuf, Option<PathBuf>)> {
        let path = Self::config_path()?;
        let backup = reset_config_file_at(&path)?;
        Ok((path, backup))
    }

    pub fn defaults() -> Self {
        Self::from_config(Self::config_path().unwrap_or_default(), default_config())
            .expect("default keybindings are valid")
    }

    pub fn config_path() -> Result<PathBuf> {
        Ok(paths::project_dirs()?.config_dir().join("config.json"))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn label(&self, action: &str) -> &str {
        self.labels.get(action).map(String::as_str).unwrap_or("?")
    }

    pub fn resolve(&self, contexts: &[KeyContext], key: KeyEvent) -> KeyEvent {
        let pressed = KeyPattern::from_event(key);
        for context in contexts {
            if let Some(action) = self.by_context.get(context).and_then(|actions| {
                actions
                    .iter()
                    .find(|action| action.configured.iter().any(|binding| binding == &pressed))
            }) {
                return action.canonical.to_event();
            }
        }
        if contexts.iter().any(|context| {
            self.by_context.get(context).is_some_and(|actions| {
                actions.iter().any(|action| {
                    action.defaults.iter().any(|binding| {
                        binding == &pressed
                            || (binding.modifiers.is_empty() && binding.code == pressed.code)
                    })
                })
            })
        }) {
            return KeyEvent::new(KeyCode::Null, KeyModifiers::NONE);
        }
        key
    }

    fn load_or_create_at(path: PathBuf) -> Result<Self> {
        ensure_config_file_at(&path)?;
        let bytes = fs::read(&path)
            .with_context(|| format!("read keybindings config {}", path.display()))?;
        let config: ConfigFile = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse keybindings config {}", path.display()))?;
        Self::from_config(path, config)
    }

    fn from_config(path: PathBuf, config: ConfigFile) -> Result<Self> {
        if config.version != CONFIG_VERSION {
            bail!(
                "unsupported keybindings config version {}; expected {CONFIG_VERSION}",
                config.version
            );
        }
        for name in config.keybindings.keys() {
            if !ACTIONS.iter().any(|action| action.name == name) {
                bail!("unknown keybinding action `{name}`");
            }
        }

        let mut by_context: HashMap<KeyContext, Vec<ResolvedAction>> = HashMap::new();
        let mut labels = HashMap::new();
        for spec in ACTIONS {
            let configured = match config.keybindings.get(spec.name) {
                Some(bindings) => bindings
                    .iter()
                    .map(|binding| parse_binding(binding))
                    .collect::<Result<Vec<_>>>()?,
                None => spec
                    .defaults
                    .iter()
                    .map(|binding| parse_binding(binding))
                    .collect::<Result<Vec<_>>>()?,
            };
            let mut configured = configured;
            for fallback in spec.fallbacks {
                let fallback = parse_binding(fallback)?;
                if !configured.contains(&fallback) {
                    configured.push(fallback);
                }
            }
            labels.entry(spec.name.to_owned()).or_insert_with(|| {
                configured
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" / ")
            });
            let action = ResolvedAction {
                name: spec.name,
                canonical: parse_binding(spec.canonical)?,
                configured,
                defaults: spec
                    .defaults
                    .iter()
                    .map(|binding| parse_binding(binding))
                    .collect::<Result<_>>()?,
            };
            by_context.entry(spec.context).or_default().push(action);
        }
        for actions in by_context.values() {
            let mut seen = HashMap::<&KeyPattern, &str>::new();
            for action in actions {
                for binding in &action.configured {
                    if let Some(previous) = seen.insert(binding, action.name)
                        && previous != action.name
                    {
                        bail!(
                            "key `{binding}` is assigned to both `{previous}` and `{}` in one mode",
                            action.name
                        );
                    }
                }
            }
        }
        validate_combined_contexts(
            &by_context,
            KeyContext::NormalRepository,
            KeyContext::Normal,
        )?;
        validate_combined_contexts(&by_context, KeyContext::NormalThread, KeyContext::Normal)?;
        Ok(Self {
            path,
            by_context,
            labels,
        })
    }
}

fn ensure_config_file_at(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let parent = path
        .parent()
        .context("keybindings config path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create config directory {}", parent.display()))?;
    let data = serde_json::to_vec_pretty(&default_config())?;
    fs::write(path, data)
        .with_context(|| format!("create keybindings config {}", path.display()))?;
    Ok(())
}

fn reset_config_file_at(path: &Path) -> Result<Option<PathBuf>> {
    let backup = if path.exists() {
        let backup = available_backup_path(path);
        fs::copy(path, &backup).with_context(|| {
            format!(
                "back up keybindings config {} to {}",
                path.display(),
                backup.display()
            )
        })?;
        Some(backup)
    } else {
        None
    };

    let parent = path
        .parent()
        .context("keybindings config path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create config directory {}", parent.display()))?;
    let temporary = path.with_extension("json.tmp");
    let data = serde_json::to_vec_pretty(&default_config())?;
    fs::write(&temporary, data)
        .with_context(|| format!("write default config {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("replace keybindings config {}", path.display()))?;
    Ok(backup)
}

fn available_backup_path(path: &Path) -> PathBuf {
    let mut base = path.as_os_str().to_owned();
    base.push(".backup");
    let base = PathBuf::from(base);
    if !base.exists() {
        return base;
    }
    for suffix in 2.. {
        let mut candidate = base.as_os_str().to_owned();
        candidate.push(format!(".{suffix}"));
        let candidate = PathBuf::from(candidate);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("backup suffix range is unbounded")
}

fn validate_combined_contexts(
    by_context: &HashMap<KeyContext, Vec<ResolvedAction>>,
    first: KeyContext,
    second: KeyContext,
) -> Result<()> {
    let mut seen = HashMap::<&KeyPattern, &str>::new();
    for context in [first, second] {
        for action in by_context.get(&context).into_iter().flatten() {
            for binding in &action.configured {
                if let Some(previous) = seen.insert(binding, action.name)
                    && previous != action.name
                {
                    bail!(
                        "key `{binding}` is assigned to both `{previous}` and `{}` in one mode",
                        action.name
                    );
                }
            }
        }
    }
    Ok(())
}

fn default_config() -> ConfigFile {
    let mut keybindings = BTreeMap::new();
    for action in ACTIONS {
        keybindings
            .entry(action.name.to_owned())
            .or_insert_with(|| {
                action
                    .defaults
                    .iter()
                    .map(|key| (*key).to_owned())
                    .collect()
            });
    }
    ConfigFile {
        version: CONFIG_VERSION,
        keybindings,
    }
}

fn parse_binding(binding: &str) -> Result<KeyPattern> {
    let normalized = binding.trim();
    if normalized.is_empty() {
        bail!("keybinding cannot be empty");
    }
    let parts = normalized.split('+').collect::<Vec<_>>();
    let key_name = parts.last().copied().unwrap_or_default();
    let mut modifiers = KeyModifiers::NONE;
    for modifier in &parts[..parts.len().saturating_sub(1)] {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers.insert(KeyModifiers::CONTROL),
            "alt" | "option" => modifiers.insert(KeyModifiers::ALT),
            "shift" => modifiers.insert(KeyModifiers::SHIFT),
            _ => bail!("unknown modifier `{modifier}` in keybinding `{binding}`"),
        }
    }
    let lower = key_name.to_ascii_lowercase();
    let code = match lower.as_str() {
        "backspace" => KeyCode::Backspace,
        "enter" => KeyCode::Enter,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "page_up" => KeyCode::PageUp,
        "pagedown" | "page_down" => KeyCode::PageDown,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "plus" => KeyCode::Char('+'),
        _ if key_name.chars().count() == 1 => KeyCode::Char(key_name.chars().next().unwrap()),
        _ if lower.starts_with('f') => {
            let number = lower[1..]
                .parse::<u8>()
                .with_context(|| format!("invalid function key `{key_name}`"))?;
            KeyCode::F(number)
        }
        _ => bail!("unknown key `{key_name}` in keybinding `{binding}`"),
    };
    Ok(KeyPattern { code, modifiers }.normalized())
}

impl KeyPattern {
    fn from_event(event: KeyEvent) -> Self {
        Self {
            code: event.code,
            modifiers: event.modifiers,
        }
        .normalized()
    }

    fn normalized(mut self) -> Self {
        if let KeyCode::Char(character) = self.code {
            if self.modifiers.contains(KeyModifiers::SHIFT) && character.is_ascii_lowercase() {
                self.code = KeyCode::Char(character.to_ascii_uppercase());
            }
            self.modifiers.remove(KeyModifiers::SHIFT);
        }
        self
    }

    fn to_event(&self) -> KeyEvent {
        KeyEvent {
            code: self.code,
            modifiers: self.modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }
}

impl std::fmt::Display for KeyPattern {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            write!(formatter, "ctrl+")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            write!(formatter, "alt+")?;
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            write!(formatter, "shift+")?;
        }
        match self.code {
            KeyCode::Char(' ') => formatter.write_str("space"),
            KeyCode::Char(character) => write!(formatter, "{character}"),
            KeyCode::Backspace => formatter.write_str("backspace"),
            KeyCode::Enter => formatter.write_str("enter"),
            KeyCode::Left => formatter.write_str("left"),
            KeyCode::Right => formatter.write_str("right"),
            KeyCode::Up => formatter.write_str("up"),
            KeyCode::Down => formatter.write_str("down"),
            KeyCode::Home => formatter.write_str("home"),
            KeyCode::End => formatter.write_str("end"),
            KeyCode::PageUp => formatter.write_str("pageup"),
            KeyCode::PageDown => formatter.write_str("pagedown"),
            KeyCode::Tab => formatter.write_str("tab"),
            KeyCode::BackTab => formatter.write_str("backtab"),
            KeyCode::Delete => formatter.write_str("delete"),
            KeyCode::Insert => formatter.write_str("insert"),
            KeyCode::F(number) => write!(formatter, "f{number}"),
            KeyCode::Esc => formatter.write_str("esc"),
            _ => formatter.write_str("unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn creates_an_editable_default_config() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.json");

        let bindings = KeyBindings::load_or_create_at(path.clone()).unwrap();

        assert_eq!(bindings.path(), path);
        let config: ConfigFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.keybindings["normal.quit"], ["q"]);
        assert_eq!(config.keybindings["help.ask_shikigami"], ["enter"]);
        assert_eq!(config.keybindings["chat_input.side_chat"], ["ctrl+s"]);
        assert_eq!(config.keybindings["chat_scroll.side_chat"], ["ctrl+s"]);
    }

    #[test]
    fn loads_an_existing_config_without_rewriting_it() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.json");
        let data = br#"{
  "version": 1,
  "keybindings": {
    "normal.quit": ["ctrl+q"]
  }
}"#;
        fs::write(&path, data).unwrap();

        let bindings = KeyBindings::load_or_create_at(path.clone()).unwrap();

        assert_eq!(fs::read(path).unwrap(), data);
        assert_eq!(
            bindings
                .resolve(
                    &[KeyContext::Normal],
                    KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)
                )
                .code,
            KeyCode::Char('q')
        );
    }

    #[test]
    fn opening_an_invalid_existing_config_does_not_replace_it() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.json");
        fs::write(&path, "needs repair").unwrap();

        ensure_config_file_at(&path).unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "needs repair");
    }

    #[test]
    fn resetting_config_backs_up_the_existing_file_and_writes_defaults() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.json");
        fs::write(&path, "custom config").unwrap();

        let backup = reset_config_file_at(&path).unwrap().unwrap();

        assert_eq!(backup, temp.path().join("config.json.backup"));
        assert_eq!(fs::read_to_string(backup).unwrap(), "custom config");
        let reset: ConfigFile = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(reset.version, CONFIG_VERSION);
        assert_eq!(reset.keybindings["normal.quit"], ["q"]);
    }

    #[test]
    fn resetting_config_does_not_overwrite_an_older_backup() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("config.json");
        fs::write(&path, "current").unwrap();
        fs::write(temp.path().join("config.json.backup"), "older").unwrap();

        let backup = reset_config_file_at(&path).unwrap().unwrap();

        assert_eq!(backup, temp.path().join("config.json.backup.2"));
        assert_eq!(fs::read_to_string(backup).unwrap(), "current");
        assert_eq!(
            fs::read_to_string(temp.path().join("config.json.backup")).unwrap(),
            "older"
        );
    }

    #[test]
    fn resetting_a_missing_config_creates_defaults_without_a_backup() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("nested/config.json");

        let backup = reset_config_file_at(&path).unwrap();

        assert!(backup.is_none());
        assert!(path.is_file());
    }

    #[test]
    fn replaces_defaults_and_suppresses_the_old_key() {
        let mut config = default_config();
        config
            .keybindings
            .insert("normal.quit".into(), vec!["ctrl+q".into()]);
        let bindings = KeyBindings::from_config(PathBuf::new(), config).unwrap();

        assert_eq!(
            bindings
                .resolve(
                    &[KeyContext::Normal],
                    KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)
                )
                .code,
            KeyCode::Char('q')
        );
        assert_eq!(
            bindings
                .resolve(
                    &[KeyContext::Normal],
                    KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
                )
                .code,
            KeyCode::Null
        );
    }

    #[test]
    fn keeps_escape_as_a_safety_fallback() {
        let mut config = default_config();
        config
            .keybindings
            .insert("quit.cancel".into(), vec!["backspace".into()]);
        let bindings = KeyBindings::from_config(PathBuf::new(), config).unwrap();

        assert_eq!(
            bindings
                .resolve(
                    &[KeyContext::ConfirmQuit],
                    KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
                )
                .code,
            KeyCode::Esc
        );
    }

    #[test]
    fn modified_commands_do_not_consume_plain_composer_text() {
        let bindings = KeyBindings::defaults();

        let typed = bindings.resolve(
            &[KeyContext::ChatInput],
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE),
        );

        assert_eq!(typed.code, KeyCode::Char('u'));
        assert!(typed.modifiers.is_empty());
    }

    #[test]
    fn ctrl_s_opens_side_chat_from_input_and_scroll_modes() {
        let bindings = KeyBindings::defaults();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);

        for context in [
            KeyContext::ChatInput,
            KeyContext::ChatInputEmpty,
            KeyContext::ChatScroll,
        ] {
            let resolved = bindings.resolve(&[context], ctrl_s);

            assert_eq!(resolved.code, KeyCode::Char('s'));
            assert!(resolved.modifiers.contains(KeyModifiers::CONTROL));
        }
    }

    #[test]
    fn chat_escape_and_interrupt_bindings_can_be_swapped() {
        let mut config = default_config();
        config
            .keybindings
            .insert("chat_input.focus_tree".into(), vec!["ctrl+c".into()]);
        config
            .keybindings
            .insert("chat_input.interrupt".into(), vec!["esc".into()]);
        let bindings = KeyBindings::from_config(PathBuf::new(), config).unwrap();

        let interrupt = bindings.resolve(
            &[KeyContext::ChatInput],
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        );
        let focus_tree = bindings.resolve(
            &[KeyContext::ChatInput],
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        );

        assert_eq!(interrupt.code, KeyCode::Char('c'));
        assert!(interrupt.modifiers.contains(KeyModifiers::CONTROL));
        assert_eq!(focus_tree.code, KeyCode::Esc);
    }

    #[test]
    fn rejects_conflicts_within_a_mode() {
        let mut config = default_config();
        config
            .keybindings
            .insert("normal.quit".into(), vec!["?".into()]);

        let error = KeyBindings::from_config(PathBuf::new(), config)
            .err()
            .unwrap();

        assert!(error.to_string().contains("assigned to both"));
    }

    #[test]
    fn rejects_conflicts_between_normal_and_selected_item_actions() {
        let mut config = default_config();
        config
            .keybindings
            .insert("normal.repository.remove".into(), vec!["q".into()]);

        let error = KeyBindings::from_config(PathBuf::new(), config)
            .err()
            .unwrap();

        assert!(error.to_string().contains("normal.quit"));
        assert!(error.to_string().contains("normal.repository.remove"));
    }

    #[test]
    fn missing_actions_use_their_new_defaults() {
        let config = ConfigFile {
            version: CONFIG_VERSION,
            keybindings: BTreeMap::new(),
        };
        let bindings = KeyBindings::from_config(PathBuf::new(), config).unwrap();

        assert_eq!(
            bindings
                .resolve(
                    &[KeyContext::Normal],
                    KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)
                )
                .code,
            KeyCode::Char('q')
        );
        assert_eq!(
            bindings
                .resolve(
                    &[KeyContext::ChatScroll],
                    KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)
                )
                .code,
            KeyCode::Char('l')
        );
    }
}
