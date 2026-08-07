use std::{
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::{MoveTo, SetCursorStyle},
    event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{
        Clear as ClearTerminal, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use futures::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{App, ChatPane, Focus, Mode, TreeRow},
    app_server::{AppServer, AppServerRequest},
    chat::{
        ChatMessage, ChatMode, ChatRole, ChatState, CommandPalette, EditorTarget, PaletteCommand,
        PaletteEntry,
    },
    clipboard,
    git_workspace::Workspace,
};

type Tui = Terminal<CrosstermBackend<Stdout>>;

struct ChatPreview {
    generation: u64,
    result: std::result::Result<ChatState, String>,
}

enum UiAction {
    OpenEditor { cwd: PathBuf, target: EditorTarget },
}

pub async fn run(mut app: App) -> Result<()> {
    let server = AppServer::spawn("codex", Duration::from_secs(30)).await?;
    match server.list_models().await {
        Ok(models) => app.set_models(models),
        Err(error) => app.message = Some(format!("Could not load models: {error}")),
    }
    let mut terminal = init_terminal()?;
    let result = run_loop(&mut terminal, &mut app, server).await;
    restore_terminal(&mut terminal)?;
    result
}

fn init_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        ClearTerminal(ClearType::All),
        SetCursorStyle::BlinkingBar,
        MoveTo(0, 0)
    )?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        SetCursorStyle::DefaultUserShape,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

fn resume_terminal(terminal: &mut Tui) -> Result<()> {
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        ClearTerminal(ClearType::All),
        SetCursorStyle::BlinkingBar,
        MoveTo(0, 0)
    )?;
    terminal.resize(terminal.size()?.into())?;
    Ok(())
}

async fn run_loop(terminal: &mut Tui, app: &mut App, server: Arc<AppServer>) -> Result<()> {
    let mut inputs = EventStream::new();
    let mut server_events = server.subscribe();
    let preview_generation = Arc::new(AtomicU64::new(0));
    let (preview_sender, mut preview_receiver) = mpsc::unbounded_channel();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let mut needs_draw = true;
    while !app.should_quit {
        if needs_draw {
            terminal.draw(|frame| render(frame, app))?;
            needs_draw = false;
        }
        tokio::select! {
            _ = ticker.tick() => {
                if app.scanning {
                    app.poll_scan();
                    needs_draw = true;
                }
                if app.visible_chat_has_active_turn() {
                    needs_draw = true;
                }
            }
            input = inputs.next() => {
                match input {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        let action = handle_key(
                            app,
                            key,
                            &server,
                            &preview_generation,
                            &preview_sender,
                        ).await?;
                        if let Some(UiAction::OpenEditor { cwd, target }) = action
                            && let Err(error) = open_in_neovim(terminal, &cwd, &target).await
                        {
                            app.message = Some(format!("Could not open Neovim: {error}"));
                        }
                        needs_draw = true;
                    }
                    Some(Ok(Event::Resize(_, _))) => {
                        needs_draw = true;
                    }
                    _ => {}
                }
            }
            preview = preview_receiver.recv() => {
                if let Some(preview) = preview
                    && preview.generation == preview_generation.load(Ordering::Relaxed)
                {
                    match preview.result {
                        Ok(chat) => {
                            let still_selected = app.selected_tree_is_thread()
                                && app.selected_thread().is_some_and(|thread| {
                                    thread.record.id == chat.thread_id
                                });
                            if still_selected {
                                app.show_chat(chat);
                            } else {
                                app.chats.entry(chat.thread_id.clone()).or_insert(chat);
                            }
                        }
                        Err(error) => app.message = Some(error),
                    }
                    needs_draw = true;
                }
            }
            event = server_events.recv() => {
                if let Ok(event) = event {
                    app.apply_chat_event(&event);
                    needs_draw = true;
                }
            }
            request = server.next_server_request() => {
                if let Some(request) = request {
                    if is_approval(&request) {
                        app.pending_approvals.push_back(request);
                        app.mode = Mode::Approval;
                        needs_draw = true;
                    } else {
                        server.respond(request.id, Value::Null).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    server: &Arc<AppServer>,
    preview_generation: &Arc<AtomicU64>,
    preview_sender: &mpsc::UnboundedSender<ChatPreview>,
) -> Result<Option<UiAction>> {
    let mut action = None;
    match app.mode {
        Mode::Chat if app.chat().is_some_and(|chat| chat.palette.is_some()) => {
            handle_palette_key(app, key, server).await?;
        }
        Mode::Chat if app.chat().map(|chat| chat.mode) == Some(ChatMode::Scroll) => {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    interrupt_chat(app, server).await?;
                }
                KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.toggle_chat_pane();
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.cycle_side_chat(true);
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.cycle_side_chat(false);
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(chat) = app.chat_mut() {
                        chat.scroll_half_page_up();
                    }
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(chat) = app.chat_mut() {
                        chat.scroll_half_page_down();
                    }
                }
                KeyCode::Char('u') => {
                    if let Some(chat) = app.chat_mut() {
                        chat.scroll_half_page_up();
                    }
                }
                KeyCode::Char('d') => {
                    if let Some(chat) = app.chat_mut() {
                        chat.scroll_half_page_down();
                    }
                }
                KeyCode::Char('K') => {
                    if let Some(chat) = app.chat_mut() {
                        chat.move_message_selection(false);
                    }
                }
                KeyCode::Char('J') => {
                    if let Some(chat) = app.chat_mut() {
                        chat.move_message_selection(true);
                    }
                }
                KeyCode::Char('y') => copy_selected_message(app),
                KeyCode::Char('Y') => copy_conversation(app),
                KeyCode::Char('e') => {
                    if app.has_active_turn() {
                        app.message = Some("Wait for active turns before opening Neovim".into());
                    } else if let Some(chat) = app.chat()
                        && let Some(target) = chat.visible_editor_target.clone()
                    {
                        action = Some(UiAction::OpenEditor {
                            cwd: chat.cwd.clone(),
                            target,
                        });
                    } else {
                        app.message = Some("Scroll a diff hunk into view first".into());
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(chat) = app.chat_mut() {
                        chat.scroll_up(1);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(chat) = app.chat_mut() {
                        chat.scroll_down(1);
                    }
                }
                KeyCode::PageUp => {
                    if let Some(chat) = app.chat_mut() {
                        chat.scroll_page_up();
                    }
                }
                KeyCode::PageDown => {
                    if let Some(chat) = app.chat_mut() {
                        chat.scroll_page_down();
                    }
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    if let Some(chat) = app.chat_mut() {
                        chat.scroll_to_top();
                    }
                }
                KeyCode::End | KeyCode::Char('G') => {
                    if let Some(chat) = app.chat_mut() {
                        chat.scroll_to_bottom();
                    }
                }
                KeyCode::Char('i') | KeyCode::Enter | KeyCode::Tab | KeyCode::Esc => {
                    if let Some(chat) = app.chat_mut() {
                        chat.mode = ChatMode::Input;
                    }
                }
                _ => {}
            }
        }
        Mode::Chat => match key.code {
            KeyCode::Tab => {
                if let Some(chat) = app.chat_mut() {
                    chat.enter_scroll_mode();
                }
            }
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.focus = Focus::Navigation;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                interrupt_chat(app, server).await?;
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.toggle_chat_pane();
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.cycle_side_chat(true);
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.cycle_side_chat(false);
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(chat) = app.chat_mut() {
                    chat.composer.clear();
                    chat.selected_skills.clear();
                }
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if app.models.is_empty() {
                    app.message = Some("No models are available from Codex".into());
                } else {
                    app.open_current_reasoning_effort_picker();
                }
            }
            KeyCode::Backspace => {
                if let Some(chat) = app.chat_mut() {
                    chat.composer.pop();
                }
            }
            KeyCode::Enter => submit_chat(app, server).await?,
            KeyCode::Char('/') if app.chat().is_some_and(|chat| chat.composer.is_empty()) => {
                open_command_palette(app, server).await?;
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(chat) = app.chat_mut() {
                    chat.composer.push(character);
                }
            }
            _ => {}
        },
        Mode::ChooseModel => match key.code {
            KeyCode::Esc => app.mode = Mode::Chat,
            KeyCode::Up | KeyCode::Char('k') => app.move_model_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_model_down(),
            KeyCode::Enter => {
                if app
                    .selected_model()
                    .is_some_and(|model| model.supported_reasoning_efforts.is_empty())
                {
                    app.apply_selected_model();
                } else if app.selected_model().is_some() {
                    app.open_selected_reasoning_effort_picker();
                }
            }
            _ => {}
        },
        Mode::ChooseReasoningEffort => match key.code {
            KeyCode::Esc => app.cancel_reasoning_effort_picker(),
            KeyCode::Up | KeyCode::Char('k') => app.move_reasoning_effort_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_reasoning_effort_down(),
            KeyCode::Enter => app.apply_selected_model(),
            _ => {}
        },
        Mode::ChooseSideChat => match key.code {
            KeyCode::Esc => app.cancel_side_chat_picker(),
            KeyCode::Up | KeyCode::Char('k') => app.move_side_chat_picker_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_side_chat_picker_down(),
            KeyCode::Enter => app.select_side_chat_from_picker(),
            _ => {}
        },
        Mode::ConfirmQuitSideChats => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => app.should_quit = true,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.mode = Mode::Normal,
            _ => {}
        },
        Mode::Approval => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => resolve_approval(app, server, true).await?,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                resolve_approval(app, server, false).await?
            }
            _ => {}
        },
        Mode::AddRepositories => match key.code {
            KeyCode::Char('q') if app.repositories.is_empty() => app.should_quit = true,
            KeyCode::Esc if !app.repositories.is_empty() => app.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => app.move_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_down(),
            KeyCode::Char(' ') => app.toggle_selected_candidate(),
            KeyCode::Char('/') => app.mode = Mode::FilterRepositories,
            KeyCode::Char('s') => app.start_home_scan(),
            KeyCode::Char('b') => app.open_browser(),
            KeyCode::Enter => {
                app.message = Some(match app.register_candidates() {
                    Ok(()) => "repository registered".into(),
                    Err(error) => error.to_string(),
                });
            }
            _ => {}
        },
        Mode::FilterRepositories => match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                app.candidate_index = 0;
                app.mode = Mode::AddRepositories;
            }
            KeyCode::Backspace => {
                app.repository_query.pop();
                app.candidate_index = 0;
            }
            KeyCode::Char(character) => {
                app.repository_query.push(character);
                app.candidate_index = 0;
            }
            _ => {}
        },
        Mode::BrowseDirectory => match key.code {
            KeyCode::Esc => app.mode = Mode::AddRepositories,
            KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left => app.browse_parent(),
            KeyCode::Up | KeyCode::Char('k') => app.move_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_down(),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => app.browse_into_selected(),
            KeyCode::Char('a') => {
                app.message = Some(match app.register_browse_path() {
                    Ok(()) => "repository registered".into(),
                    Err(error) => error.to_string(),
                });
            }
            _ => {}
        },
        Mode::ChooseThreadTarget => match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => app.move_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_down(),
            KeyCode::Enter => match app.thread_target_index {
                0 => {
                    if let Some(workspace) = app.primary_location().cloned() {
                        open_new_chat(app, server, workspace).await?;
                    }
                }
                1 => match app.create_generated_worktree() {
                    Ok(workspace) => {
                        open_new_chat(app, server, workspace).await?;
                    }
                    Err(error) => app.message = Some(error.to_string()),
                },
                2 if app.existing_worktrees().is_empty() => {
                    app.message = Some("no existing worktrees".into());
                }
                2 => {
                    app.location_index = 0;
                    app.mode = Mode::ChooseExistingWorktree;
                }
                _ => {}
            },
            _ => {}
        },
        Mode::ChooseExistingWorktree => match key.code {
            KeyCode::Esc => app.mode = Mode::ChooseThreadTarget,
            KeyCode::Up | KeyCode::Char('k') => app.move_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_down(),
            KeyCode::Enter => {
                if let Some(workspace) = app.selected_existing_worktree().cloned() {
                    open_new_chat(app, server, workspace).await?;
                }
            }
            _ => {}
        },
        Mode::ConfirmRemoveThread => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.mode = Mode::Normal;
                app.message = Some(match app.remove_selected_thread() {
                    Ok(()) => "removed from wyard; Codex history remains".into(),
                    Err(error) => error.to_string(),
                });
            }
            _ => app.mode = Mode::Normal,
        },
        Mode::ConfirmArchiveCleanup => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.mode = Mode::Normal;
                app.message = Some(match app.archive_selected_thread(true) {
                    Ok(()) => "archived; clean wyard worktree removed".into(),
                    Err(error) => error.to_string(),
                });
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                app.mode = Mode::Normal;
                app.message = Some(match app.archive_selected_thread(false) {
                    Ok(()) => "archived; worktree kept".into(),
                    Err(error) => error.to_string(),
                });
            }
            KeyCode::Esc => app.mode = Mode::Normal,
            _ => {}
        },
        Mode::ConfirmRemoveRepository => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.mode = Mode::Normal;
                app.message = Some(match app.unregister_selected_repository() {
                    Ok(()) => "repository removed from wyard".into(),
                    Err(error) => error.to_string(),
                });
            }
            _ => app.mode = Mode::Normal,
        },
        Mode::Help => app.mode = Mode::Normal,
        Mode::Normal => match key.code {
            KeyCode::Char('q') => {
                if !quit_requires_side_chat_confirmation(app.side_chat_count()) {
                    app.should_quit = true;
                } else {
                    app.mode = Mode::ConfirmQuitSideChats;
                }
            }
            KeyCode::Char('?') => app.mode = Mode::Help,
            KeyCode::Esc if app.selected_tree_is_thread() => app.select_parent_repository(),
            KeyCode::Char('h') | KeyCode::Left => app.collapse_selected_repository(),
            KeyCode::Char('l') | KeyCode::Right => app.expand_selected_repository(),
            KeyCode::Tab if app.chat().is_some() => {
                app.focus = Focus::Chat;
                app.mode = Mode::Chat;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.move_up();
                schedule_selected_chat_preview(app, server, preview_generation, preview_sender);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.move_down();
                schedule_selected_chat_preview(app, server, preview_generation, preview_sender);
            }
            KeyCode::Char('a') => app.open_repository_add(),
            KeyCode::Char('A') => app.toggle_archive_view(),
            KeyCode::Char('n') => {
                if app.locations.is_empty() {
                    app.message = Some("no available repository location".into());
                } else {
                    app.thread_target_index = 0;
                    app.mode = Mode::ChooseThreadTarget;
                }
            }
            KeyCode::Char('r') => {
                if let Err(error) = app.refresh_repositories() {
                    app.message = Some(error.to_string());
                }
            }
            KeyCode::Char('d') if app.selected_tree_is_repository() => {
                app.mode = Mode::ConfirmRemoveRepository;
            }
            KeyCode::Char('d') if app.selected_tree_is_thread() => {
                app.mode = Mode::ConfirmRemoveThread;
            }
            KeyCode::Char('x') if app.selected_tree_is_thread() => {
                if app.show_archived {
                    app.message = Some(match app.unarchive_selected_thread() {
                        Ok(()) => "thread restored".into(),
                        Err(error) => error.to_string(),
                    });
                } else {
                    match app.selected_thread_has_clean_managed_worktree() {
                        Ok(true) => app.mode = Mode::ConfirmArchiveCleanup,
                        Ok(false) => {
                            app.message = Some(match app.archive_selected_thread(false) {
                                Ok(()) => "archived; worktree kept".into(),
                                Err(error) => error.to_string(),
                            });
                        }
                        Err(error) => app.message = Some(error.to_string()),
                    }
                }
            }
            KeyCode::Enter if app.selected_tree_is_repository() => {
                app.toggle_selected_repository();
            }
            KeyCode::Enter if app.selected_tree_is_thread() && !app.show_archived => {
                preview_generation.fetch_add(1, Ordering::Relaxed);
                if let Err(error) = focus_selected_chat(app, server).await {
                    app.message = Some(format!("Could not open thread: {error}"));
                }
            }
            _ => {}
        },
    }
    Ok(action)
}

async fn open_in_neovim(terminal: &mut Tui, cwd: &Path, target: &EditorTarget) -> Result<()> {
    let root = cwd
        .canonicalize()
        .with_context(|| format!("workspace does not exist: {}", cwd.display()))?;
    let candidate = if target.path.is_absolute() {
        target.path.clone()
    } else {
        root.join(&target.path)
    };
    let path = candidate
        .canonicalize()
        .with_context(|| format!("file does not exist: {}", candidate.display()))?;
    if !path.starts_with(&root) {
        bail!("file is outside the workspace: {}", path.display());
    }

    restore_terminal(terminal)?;
    let editor_result = tokio::process::Command::new("nvim")
        .arg(format!("+{}", target.line))
        .arg(&path)
        .current_dir(&root)
        .status()
        .await;
    resume_terminal(terminal)?;

    let status = editor_result.context("could not start nvim")?;
    if !status.success() {
        bail!("nvim exited with {status}");
    }
    Ok(())
}

async fn open_command_palette(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    let Some(chat) = app.chat() else {
        return Ok(());
    };
    let should_load = !chat.skills_loaded || chat.skills_stale;
    let cwd = chat.cwd.clone();
    let force_reload = chat.skills_stale;
    if should_load {
        match server.list_skills(&cwd, force_reload).await {
            Ok(skills) => {
                if let Some(chat) = app.chat_mut() {
                    chat.available_skills = skills;
                    chat.skills_loaded = true;
                    chat.skills_stale = false;
                }
            }
            Err(error) => {
                if let Some(chat) = app.chat_mut() {
                    chat.push_notice(format!("Could not load skills: {error}"));
                }
            }
        }
    }
    if let Some(chat) = app.chat_mut() {
        chat.open_palette();
    }
    Ok(())
}

fn copy_selected_message(app: &mut App) {
    let content = app
        .chat()
        .and_then(ChatState::selected_message)
        .map(|message| message.content.clone());
    app.message = Some(match content {
        Some(content) => match clipboard::copy(&content) {
            Ok(()) => "Copied selected message".into(),
            Err(error) => format!("Could not copy message: {error}"),
        },
        None => "No message selected".into(),
    });
}

fn copy_conversation(app: &mut App) {
    let content = app.chat().map(ChatState::conversation_text);
    app.message = Some(match content {
        Some(content) if !content.is_empty() => match clipboard::copy(&content) {
            Ok(()) => "Copied full chat".into(),
            Err(error) => format!("Could not copy chat: {error}"),
        },
        _ => "Chat is empty".into(),
    });
}

async fn handle_palette_key(app: &mut App, key: KeyEvent, server: &Arc<AppServer>) -> Result<()> {
    let Some(chat) = app.chat_mut() else {
        return Ok(());
    };
    match key.code {
        KeyCode::Esc => chat.palette = None,
        KeyCode::Up => {
            if let Some(palette) = &mut chat.palette {
                palette.move_up();
            }
        }
        KeyCode::Down => {
            if let Some(palette) = &mut chat.palette {
                palette.move_down();
            }
        }
        KeyCode::Char('k')
            if chat
                .palette
                .as_ref()
                .is_some_and(|palette| palette.query.is_empty()) =>
        {
            if let Some(palette) = &mut chat.palette {
                palette.move_up();
            }
        }
        KeyCode::Char('j')
            if chat
                .palette
                .as_ref()
                .is_some_and(|palette| palette.query.is_empty()) =>
        {
            if let Some(palette) = &mut chat.palette {
                palette.move_down();
            }
        }
        KeyCode::Backspace => {
            if let Some(palette) = &mut chat.palette {
                if palette.query.is_empty() {
                    chat.palette = None;
                } else {
                    palette.pop_query();
                }
            }
        }
        KeyCode::Enter => select_palette_entry(app, server).await?,
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            if let Some(palette) = &mut chat.palette {
                palette.push_query(character);
            }
        }
        _ => {}
    }
    Ok(())
}

async fn select_palette_entry(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    let entry = app.chat_mut().and_then(|chat| {
        let entry = chat
            .palette
            .as_ref()
            .and_then(CommandPalette::selected_entry);
        chat.palette = None;
        entry
    });
    match entry {
        Some(PaletteEntry::Skill(skill)) => {
            if let Some(chat) = app.chat_mut() {
                chat.select_skill(skill);
            }
        }
        Some(PaletteEntry::Command(PaletteCommand::Threads)) => {
            app.mode = Mode::Normal;
            app.focus = Focus::Navigation;
        }
        Some(PaletteEntry::Command(PaletteCommand::Scroll)) => {
            if let Some(chat) = app.chat_mut() {
                chat.enter_scroll_mode();
            }
        }
        Some(PaletteEntry::Command(PaletteCommand::Model)) => {
            if app.models.is_empty() {
                app.message = Some("No models are available from Codex".into());
            } else {
                app.open_model_picker();
            }
        }
        Some(PaletteEntry::Command(PaletteCommand::SideChat)) => {
            open_side_chat(app, server).await?;
        }
        Some(PaletteEntry::Command(PaletteCommand::Sides)) => {
            app.open_side_chat_picker();
        }
        Some(PaletteEntry::Command(PaletteCommand::SideClose)) => {
            close_side_chat(app, server).await?;
        }
        Some(PaletteEntry::Command(PaletteCommand::Status)) => {
            if let Some(chat) = app.chat_mut() {
                chat.push_notice(format!(
                    "Thread: {}\nWorkspace: {}\nModel: {} ({})\nSandbox: danger-full-access",
                    chat.thread_id,
                    chat.cwd.display(),
                    chat.model.as_deref().unwrap_or("Codex default"),
                    chat.reasoning_effort.as_deref().unwrap_or("default")
                ));
            }
        }
        None => {}
    }
    Ok(())
}

async fn open_side_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    let Some(main_chat) = app.main_chat() else {
        return Ok(());
    };
    if main_chat.active_turn_id.is_some() {
        app.message = Some("Wait for the current turn before forking a side chat".into());
        return Ok(());
    }
    if main_chat.messages.is_empty() {
        app.message = Some("Send a message before forking a side chat".into());
        return Ok(());
    }
    let parent_thread_id = main_chat.thread_id.clone();
    let cwd = main_chat.cwd.clone();
    let model = main_chat.model.clone();
    let model_display_name = main_chat.model_display_name.clone();
    let reasoning_effort = main_chat.reasoning_effort.clone();
    let side_chat_number = app.current_side_chats().len() + 1;
    let (side_thread_id, history) = match server.fork_thread(&parent_thread_id, &cwd, true).await {
        Ok(result) => result,
        Err(error) => {
            app.message = Some(format!("Could not fork side chat: {error}"));
            return Ok(());
        }
    };
    let mut side_chat = ChatState::new(
        side_thread_id.clone(),
        cwd,
        format!("Sidechat {side_chat_number}"),
    );
    if let (Some(model), Some(display_name)) = (model, model_display_name) {
        side_chat.set_model(model, display_name, reasoning_effort);
    }
    side_chat.load_history(&history);
    side_chat.mark_as_side_chat();
    app.resumed_threads.insert(side_thread_id);
    app.show_side_chat(parent_thread_id, side_chat);
    app.focus = Focus::Chat;
    app.mode = Mode::Chat;
    Ok(())
}

async fn close_side_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    let active_turn = app.side_chat().and_then(|chat| {
        chat.active_turn_id
            .as_ref()
            .map(|turn_id| (chat.thread_id.clone(), turn_id.clone()))
    });
    if let Some((thread_id, turn_id)) = active_turn
        && let Err(error) = server.interrupt_turn(&thread_id, &turn_id).await
    {
        app.message = Some(format!("Could not interrupt side chat: {error}"));
    }
    app.close_side_chat();
    Ok(())
}

async fn interrupt_chat(app: &App, server: &Arc<AppServer>) -> Result<()> {
    if let Some(chat) = app.chat()
        && let Some(turn_id) = &chat.active_turn_id
    {
        server.interrupt_turn(&chat.thread_id, turn_id).await?;
    }
    Ok(())
}

async fn open_new_chat(app: &mut App, server: &Arc<AppServer>, workspace: Workspace) -> Result<()> {
    let model_settings = app.default_model_settings();
    let thread_id = server
        .start_thread(
            &workspace.path,
            model_settings.as_ref().map(|(model, _, _)| model.as_str()),
        )
        .await?;
    app.register_app_server_thread(thread_id.clone(), workspace.path.clone())?;
    app.resumed_threads.insert(thread_id.clone());
    let mut chat = ChatState::new(thread_id, workspace.path, "Untitled thread".into());
    if let Some((model, display_name, effort)) = model_settings {
        chat.set_model(model, display_name, effort);
    }
    app.show_chat(chat);
    app.focus = Focus::Chat;
    app.mode = Mode::Chat;
    Ok(())
}

fn schedule_selected_chat_preview(
    app: &mut App,
    server: &Arc<AppServer>,
    generation: &Arc<AtomicU64>,
    sender: &mpsc::UnboundedSender<ChatPreview>,
) {
    let current_generation = generation.fetch_add(1, Ordering::Relaxed) + 1;
    if !app.selected_tree_is_thread() {
        return;
    }
    let Some(thread) = app.selected_thread().cloned() else {
        return;
    };
    if app.show_cached_chat(&thread.record.id) {
        return;
    }
    let server = Arc::clone(server);
    let model_settings = app.default_model_settings();
    let generation = Arc::clone(generation);
    let sender = sender.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if generation.load(Ordering::Relaxed) != current_generation {
            return;
        }
        let result = match server.read_thread(&thread.record.id).await {
            Ok(history) => {
                let mut chat =
                    ChatState::new(thread.record.id, thread.record.cwd, thread.record.title);
                if let Some((model, display_name, effort)) = model_settings {
                    chat.set_model(model, display_name, effort);
                }
                chat.load_history(&history);
                Ok(chat)
            }
            Err(error) => Err(format!("Could not load thread: {error}")),
        };
        if generation.load(Ordering::Relaxed) == current_generation {
            let _ = sender.send(ChatPreview {
                generation: current_generation,
                result,
            });
        }
    });
}

async fn preview_selected_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    if !app.selected_tree_is_thread() {
        return Ok(());
    }
    let Some(thread) = app.selected_thread().cloned() else {
        return Ok(());
    };
    if app.show_cached_chat(&thread.record.id) {
        return Ok(());
    }
    let history = server.read_thread(&thread.record.id).await?;
    let mut chat = ChatState::new(thread.record.id, thread.record.cwd, thread.record.title);
    if let Some((model, display_name, effort)) = app.default_model_settings() {
        chat.set_model(model, display_name, effort);
    }
    chat.load_history(&history);
    app.show_chat(chat);
    Ok(())
}

async fn focus_selected_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    preview_selected_chat(app, server).await?;
    let Some(chat) = app.chat() else {
        return Ok(());
    };
    let thread_id = chat.thread_id.clone();
    let cwd = chat.cwd.clone();
    let model = chat.model.clone();
    if !app.resumed_threads.contains(&thread_id) {
        server
            .resume_thread(&thread_id, &cwd, model.as_deref())
            .await?;
        app.resumed_threads.insert(thread_id);
    }
    app.focus = Focus::Chat;
    app.mode = Mode::Chat;
    Ok(())
}

async fn submit_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    let Some(chat) = app.chat() else {
        return Ok(());
    };
    if chat.active_turn_id.is_some() || chat.composer.trim().is_empty() {
        return Ok(());
    }
    let prompt = chat.composer.clone();
    let thread_id = chat.thread_id.clone();
    let cwd = chat.cwd.clone();
    let model = chat.model.clone();
    let effort = chat.reasoning_effort.clone();
    let skills = chat.skills_for_prompt(&prompt);
    let turn_id = server
        .start_turn(
            &thread_id,
            &cwd,
            &prompt,
            &skills,
            model.as_deref(),
            effort.as_deref(),
        )
        .await?;
    if let Some(chat) = app.chat_mut() {
        let first_side_message = chat.is_side_chat && !chat.side_chat_has_activity;
        chat.composer.clear();
        chat.begin_user_turn(prompt.clone(), turn_id);
        if first_side_message {
            chat.title = prompt
                .lines()
                .next()
                .unwrap_or(&prompt)
                .chars()
                .take(40)
                .collect();
        }
    }
    if app.thread_is_registered(&thread_id) {
        app.update_thread_title(&thread_id, &prompt)?;
    }
    Ok(())
}

fn is_approval(request: &AppServerRequest) -> bool {
    matches!(
        request.method.as_str(),
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
    )
}

async fn resolve_approval(app: &mut App, server: &Arc<AppServer>, accept: bool) -> Result<()> {
    let Some(request) = app.pending_approvals.pop_front() else {
        return Ok(());
    };
    let result = if request.method == "item/permissions/requestApproval" {
        if accept {
            json!({"permissions": request.params.get("permissions").cloned().unwrap_or_else(|| json!({})), "scope":"turn"})
        } else {
            json!({"permissions":{"fileSystem":{"entries":[]},"network":{"enabled":false}},"scope":"turn"})
        }
    } else {
        json!({"decision": if accept { "accept" } else { "decline" }})
    };
    server.respond(request.id, result).await?;
    app.mode = if !app.pending_approvals.is_empty() {
        Mode::Approval
    } else if app.chat().is_some() && app.focus == Focus::Chat {
        Mode::Chat
    } else {
        Mode::Normal
    };
    Ok(())
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(area);
    let chat_status = app.chat().map(|chat| {
        let state = if chat.active_turn_id.is_some() {
            "working"
        } else {
            "ready"
        };
        format!(
            "{} · {state} · {} · {}",
            chat.title,
            chat.model_display_name
                .as_deref()
                .unwrap_or("Codex default"),
            chat.reasoning_effort.as_deref().unwrap_or("default")
        )
    });
    let mut header = vec![Span::styled(
        " wyard ",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if app.chat().is_some() {
        header.push(Span::styled(
            " DANGEROUS ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        header.push(Span::raw(" "));
    }
    header.push(Span::styled(
        chat_status.as_deref().unwrap_or("repositories / threads"),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(Line::from(header)), chunks[0]);

    let navigation_width = navigation_width(chunks[1].width);
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(navigation_width), Constraint::Min(20)])
        .split(chunks[1]);
    render_navigation_tree(frame, app, panes[0]);
    render_chat_area(frame, panes[1], app);

    let status = app.message.as_deref().unwrap_or(
        "? help · j/k move/preview · h/l collapse/expand · Enter focus · n new · q quit",
    );
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    match app.mode {
        Mode::AddRepositories | Mode::FilterRepositories => render_repository_add(frame, area, app),
        Mode::BrowseDirectory => render_browser(frame, area, app),
        Mode::ChooseThreadTarget => render_thread_targets(frame, area, app),
        Mode::ChooseExistingWorktree => render_existing_worktrees(frame, area, app),
        Mode::ConfirmRemoveRepository => render_repository_remove_confirm(frame, area, app),
        Mode::ConfirmRemoveThread => render_remove_confirm(frame, area, app),
        Mode::ConfirmArchiveCleanup => render_archive_confirm(frame, area, app),
        Mode::ChooseModel => render_model_picker(frame, area, app),
        Mode::ChooseReasoningEffort => render_reasoning_effort_picker(frame, area, app),
        Mode::ChooseSideChat => render_side_chat_picker(frame, area, app),
        Mode::ConfirmQuitSideChats => render_quit_side_chats_confirm(frame, area, app),
        Mode::Help => render_help(frame, area),
        Mode::Normal | Mode::Chat | Mode::Approval => {}
    }
    if app.mode == Mode::Approval {
        render_approval(frame, area, app);
    }
}

fn render_chat_area(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.has_side_chat() {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        render_chat_pane(frame, panes[0], app, ChatPane::Main);
        render_chat_pane(frame, panes[1], app, ChatPane::Side);
    } else {
        render_chat_pane(frame, area, app, ChatPane::Main);
    }
}

fn render_chat_pane(frame: &mut Frame, area: Rect, app: &mut App, pane: ChatPane) {
    let pane_active = app.active_chat_pane == pane;
    let show_composer_cursor = app.mode == Mode::Chat && app.focus == Focus::Chat && pane_active;
    let chat_focused = app.focus == Focus::Chat && pane_active;
    let has_side_chat = app.has_side_chat();
    let side_chat_position = app.current_side_chat_position();
    let chat_id = match pane {
        ChatPane::Main => app.visible_chat_id.clone(),
        ChatPane::Side => app.side_chat_id.clone(),
    };
    let message_position = chat_id
        .as_ref()
        .and_then(|thread_id| app.chats.get(thread_id))
        .filter(|chat| chat.mode == ChatMode::Scroll)
        .and_then(ChatState::selected_message_position)
        .map(|(index, count)| format!(" · {index}/{count}"))
        .unwrap_or_default();
    let title = match pane {
        ChatPane::Main => format!(" Chat{message_position} "),
        ChatPane::Side => chat_id
            .as_ref()
            .and_then(|thread_id| app.chats.get(thread_id))
            .map(|chat| {
                let position = side_chat_position
                    .map(|(index, count)| format!(" {index}/{count}"))
                    .unwrap_or_default();
                format!(" Sidechat{position} · {}{message_position} ", chat.title)
            })
            .unwrap_or_else(|| " Sidechat ".into()),
    };
    let Some(chat_id) = chat_id else {
        frame.render_widget(
            Paragraph::new("Select a thread or press n to create one")
                .style(Style::default().fg(Color::DarkGray))
                .block(
                    Block::default()
                        .title(title.as_str())
                        .borders(Borders::ALL)
                        .border_style(focus_style(false)),
                ),
            area,
        );
        return;
    };
    let Some(chat) = app.chats.get_mut(&chat_id) else {
        return;
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(4),
            Constraint::Length(1),
        ])
        .split(area);
    let visible_height = chunks[0].height.saturating_sub(2).max(1) as usize;
    let chat_border = if pane_active && chat.mode == ChatMode::Scroll {
        Color::Cyan
    } else {
        focus_style(chat_focused).fg.unwrap_or(Color::DarkGray)
    };
    let chat_block = Block::default()
        .title(title.as_str())
        .borders(Borders::ALL)
        .padding(Padding::right(1))
        .border_style(Style::default().fg(chat_border));
    let text_width = chat_block.inner(chunks[0]).width.max(1);
    let RenderedChat {
        lines,
        selected_range,
        editor_targets,
    } = rendered_chat(chat, text_width as usize);
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total_lines = paragraph.line_count(text_width);
    let paragraph = paragraph.block(chat_block);
    chat.update_scroll_metrics(total_lines, visible_height);
    if chat.take_message_selection_scroll_request()
        && let Some((start, end)) = selected_range
    {
        chat.reveal_line_range(start, end);
    }
    chat.visible_editor_target =
        visible_editor_target(editor_targets, chat.scroll_top, visible_height);
    let scroll = u16::try_from(chat.scroll_top).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((scroll, 0)), chunks[0]);
    let message_border = if chat.mode == ChatMode::Input && chat_focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let message_block = Block::default()
        .title(" Message ")
        .borders(Borders::ALL)
        .padding(Padding::right(1))
        .border_style(Style::default().fg(message_border));
    let message_inner = message_block.inner(chunks[1]);
    let composer_width = message_inner.width.max(1) as usize;
    let composer_lines = wrap_composer(&chat.composer, composer_width);
    let composer_height = message_inner.height.max(1) as usize;
    let composer_scroll = composer_lines.len().saturating_sub(composer_height);
    let cursor_column = composer_lines
        .last()
        .map(|line| UnicodeWidthStr::width(line.as_str()))
        .unwrap_or(0);
    frame.render_widget(
        Paragraph::new(Text::from(
            composer_lines
                .iter()
                .map(|line| Line::from(line.clone()))
                .collect::<Vec<_>>(),
        ))
        .scroll((u16::try_from(composer_scroll).unwrap_or(u16::MAX), 0))
        .block(message_block),
        chunks[1],
    );
    if show_composer_cursor
        && chat.mode == ChatMode::Input
        && chat.palette.is_none()
        && !message_inner.is_empty()
    {
        let x = message_inner
            .x
            .saturating_add(u16::try_from(cursor_column).unwrap_or(u16::MAX))
            .min(message_inner.right().saturating_sub(1));
        let visible_cursor_line = composer_lines
            .len()
            .saturating_sub(1)
            .saturating_sub(composer_scroll);
        let y = message_inner
            .y
            .saturating_add(u16::try_from(visible_cursor_line).unwrap_or(u16::MAX))
            .min(message_inner.bottom().saturating_sub(1));
        frame.set_cursor_position((x, y));
    }
    let help = match chat.mode {
        ChatMode::Input => {
            if has_side_chat {
                "INPUT · Ctrl-G pane · Ctrl-N/P side · Enter send"
            } else {
                "INPUT · Enter send · / palette · Ctrl-R effort · Ctrl-U clear · Tab scroll · Esc threads"
            }
        }
        ChatMode::Scroll => {
            if has_side_chat {
                "SCROLL · j/k line · J/K msg · e nvim · y copy · i input"
            } else {
                "SCROLL · j/k line · J/K msg · e nvim · y/Y copy · u/d half · i input"
            }
        }
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
    if pane_active && let Some(palette) = &chat.palette {
        render_command_palette(frame, area, palette);
    }
}

fn thinking_frame() -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let frame = (elapsed.as_millis() / 100) as usize % FRAMES.len();
    FRAMES[frame]
}

fn render_command_palette(frame: &mut Frame, area: Rect, palette: &CommandPalette) {
    let entries = palette.visible_entries();
    let height = u16::try_from(entries.len().saturating_mul(2).saturating_add(2))
        .unwrap_or(u16::MAX)
        .clamp(6, area.height.saturating_sub(4).max(6));
    let popup = centered_rect(78, height, area);
    let items = entries.iter().map(|entry| {
        let (kind, color) = match entry {
            PaletteEntry::Command(_) => ("command", Color::Cyan),
            PaletteEntry::Skill(skill) => (skill.scope.as_str(), Color::Green),
        };
        ListItem::new(Text::from(vec![
            Line::styled(
                entry.label(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!("{} · {kind}", entry.description()),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    });
    let title = if palette.query.is_empty() {
        " Command palette ".into()
    } else {
        format!(" Command palette · {} ", palette.query)
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .title_bottom(Line::from(
                    " type to filter · ↑/↓ select · Enter choose · Esc close ",
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_symbol("› ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    let mut state = ListState::default().with_selected(
        (!entries.is_empty()).then_some(palette.selected.min(entries.len().saturating_sub(1))),
    );
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_model_picker(frame: &mut Frame, area: Rect, app: &App) {
    let height = u16::try_from(app.models.len().saturating_mul(2).saturating_add(2))
        .unwrap_or(u16::MAX)
        .clamp(7, area.height.saturating_sub(4).max(7));
    let popup = centered_rect(72, height, area);
    let items = app.models.iter().map(|model| {
        let default = if model.is_default { " · default" } else { "" };
        ListItem::new(Text::from(vec![
            Line::styled(
                format!("{}{}", model.display_name, default),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(&model.description, Style::default().fg(Color::DarkGray)),
        ]))
    });
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Model ")
                .title_bottom(Line::from(" j/k select · Enter effort · Esc close "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_symbol("› ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    let mut state = ListState::default().with_selected(
        (!app.models.is_empty()).then_some(app.model_index.min(app.models.len() - 1)),
    );
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_reasoning_effort_picker(frame: &mut Frame, area: Rect, app: &App) {
    let Some(model) = app.selected_model() else {
        return;
    };
    let efforts = &model.supported_reasoning_efforts;
    let selected = app
        .reasoning_effort_index
        .min(efforts.len().saturating_sub(1));
    let mut slider = Vec::new();
    for (index, effort) in efforts.iter().enumerate() {
        if index > 0 {
            slider.push(Span::styled(" ─ ", Style::default().fg(Color::DarkGray)));
        }
        let active = index == selected;
        slider.push(Span::styled(
            format!(
                "{} {}",
                if active { "●" } else { "○" },
                effort.reasoning_effort
            ),
            if active {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    }
    let description = efforts
        .get(selected)
        .map(|effort| effort.description.as_str())
        .unwrap_or("No reasoning effort options are available");
    let popup = centered_rect(78, 9, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(""),
            Line::from(slider),
            Line::from(""),
            Line::styled(description, Style::default().fg(Color::DarkGray)),
        ]))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(format!(" {} · reasoning effort ", model.display_name))
                .title_bottom(Line::from(" j/k change · Enter apply · Esc cancel "))
                .borders(Borders::ALL)
                .padding(Padding::horizontal(2))
                .border_style(Style::default().fg(Color::Green)),
        ),
        popup,
    );
}

fn render_side_chat_picker(frame: &mut Frame, area: Rect, app: &App) {
    let side_chats = app.current_side_chats();
    let height = u16::try_from(side_chats.len().saturating_mul(2).saturating_add(2))
        .unwrap_or(u16::MAX)
        .clamp(7, area.height.saturating_sub(4).max(7));
    let popup = centered_rect(64, height, area);
    let items = side_chats.iter().enumerate().map(|(index, chat)| {
        let state = if chat.active_turn_id.is_some() {
            "working"
        } else {
            "ready"
        };
        ListItem::new(Text::from(vec![
            Line::styled(
                format!("{}. {}", index + 1, chat.title),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!(
                    "{state} · {}",
                    chat.reasoning_effort.as_deref().unwrap_or("default")
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    });
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Side chats ")
                .title_bottom(Line::from(" j/k select · Enter open · Esc close "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_symbol("› ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    let mut state = ListState::default().with_selected(
        (!side_chats.is_empty()).then_some(
            app.side_chat_picker_index
                .min(side_chats.len().saturating_sub(1)),
        ),
    );
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_quit_side_chats_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let count = app.side_chat_count();
    let popup = centered_rect(64, 7, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "{count} side chat{} will be discarded when wyard exits.\n\nQuit? [y/N]",
            if count == 1 { "" } else { "s" }
        ))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" Unsaved side chats ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        popup,
    );
}

struct RenderedMessage {
    lines: Vec<Line<'static>>,
    editor_targets: Vec<(usize, EditorTarget)>,
}

fn chat_message_lines(
    message: &ChatMessage,
    available_width: usize,
    animate_activity: bool,
) -> RenderedMessage {
    if message.role == ChatRole::Diff {
        return diff_message_lines(message, available_width);
    }
    if message.role == ChatRole::Activity {
        let content = if animate_activity {
            format!("{} {}", thinking_frame(), message.content)
        } else {
            message.content.clone()
        };
        return RenderedMessage {
            lines: activity_message_lines(&content, available_width),
            editor_targets: Vec::new(),
        };
    }
    if message.role == ChatRole::User {
        return RenderedMessage {
            lines: user_message_lines(message, available_width),
            editor_targets: Vec::new(),
        };
    }
    let mut lines = Vec::new();
    lines.extend(
        message
            .content
            .lines()
            .map(|line| Line::from(line.to_owned())),
    );
    lines.push(Line::from(""));
    RenderedMessage {
        lines,
        editor_targets: Vec::new(),
    }
}

struct RenderedChat {
    lines: Vec<Line<'static>>,
    selected_range: Option<(usize, usize)>,
    editor_targets: Vec<(usize, EditorTarget)>,
}

fn rendered_chat(chat: &ChatState, available_width: usize) -> RenderedChat {
    let mut lines = Vec::new();
    let mut selected_range = None;
    let mut editor_targets = Vec::new();
    let mut rendered_height = 0usize;
    let line_count_width = u16::try_from(available_width).unwrap_or(u16::MAX);
    let animated_activity_index = chat.active_turn_id.as_ref().and_then(|_| {
        chat.messages
            .last()
            .is_some_and(|message| {
                message.role == ChatRole::Activity && activity_is_in_progress(&message.content)
            })
            .then_some(chat.messages.len().saturating_sub(1))
    });
    for (index, message) in chat.messages.iter().enumerate() {
        let RenderedMessage {
            lines: mut message_lines,
            editor_targets: message_targets,
        } = chat_message_lines(
            message,
            available_width,
            animated_activity_index == Some(index),
        );
        let message_height = Paragraph::new(Text::from(message_lines.clone()))
            .wrap(Wrap { trim: false })
            .line_count(line_count_width);
        if chat.mode == ChatMode::Scroll && chat.selected_message_index == Some(index) {
            highlight_message_lines(&mut message_lines, available_width);
            selected_range = Some((
                rendered_height,
                rendered_height.saturating_add(message_height),
            ));
        }
        editor_targets.extend(
            message_targets
                .into_iter()
                .map(|(line, target)| (rendered_height.saturating_add(line), target)),
        );
        lines.extend(message_lines);
        rendered_height = rendered_height.saturating_add(message_height);
    }
    if chat.is_waiting_for_activity() {
        lines.extend(activity_message_lines(
            &format!("{} Thinking…", thinking_frame()),
            available_width,
        ));
    }
    RenderedChat {
        lines,
        selected_range,
        editor_targets,
    }
}

fn highlight_message_lines(lines: &mut [Line<'static>], available_width: usize) {
    let background = Color::Rgb(52, 63, 72);
    for line in lines {
        for span in &mut line.spans {
            span.style = span.style.bg(background);
        }
        let padding = available_width.saturating_sub(line.width());
        if padding > 0 {
            line.spans.push(Span::styled(
                " ".repeat(padding),
                Style::default().bg(background),
            ));
        }
        line.style = line.style.bg(background);
    }
}

fn activity_message_lines(content: &str, available_width: usize) -> Vec<Line<'static>> {
    let background = Color::Rgb(32, 34, 36);
    let header_color = activity_header_color(content);
    if available_width < 3 {
        let mut lines = wrap_activity(content, available_width.max(1))
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                Line::styled(
                    line,
                    Style::default()
                        .fg(if index == 0 {
                            header_color
                        } else {
                            Color::Gray
                        })
                        .bg(background),
                )
            })
            .collect::<Vec<_>>();
        lines.push(Line::from(""));
        return lines;
    }

    let content_width = available_width - 2;
    let mut lines = wrap_activity(content, content_width)
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let width = UnicodeWidthStr::width(line.as_str());
            let foreground = if index == 0 {
                header_color
            } else {
                Color::Gray
            };
            Line::from(vec![
                Span::styled("  ", Style::default().bg(background)),
                Span::styled(
                    format!("{line}{}", " ".repeat(content_width.saturating_sub(width))),
                    Style::default().fg(foreground).bg(background),
                ),
            ])
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(""));
    lines
}

fn activity_header_color(content: &str) -> Color {
    let content = content
        .strip_prefix(|character| {
            matches!(character, '⠋' | '⠙' | '⠹' | '⠸' | '⠼' | '⠴' | '⠦' | '⠧')
        })
        .map(str::trim_start)
        .unwrap_or(content);
    if content.starts_with('✓') || content.starts_with("Thought") {
        Color::Green
    } else if content.starts_with('✗') {
        Color::Red
    } else {
        Color::Yellow
    }
}

fn diff_message_lines(message: &ChatMessage, available_width: usize) -> RenderedMessage {
    let background = Color::Rgb(32, 34, 36);
    let left_padding = usize::from(available_width >= 3) * 2;
    let content_width = available_width.saturating_sub(left_padding).max(1);
    let mut editor_targets = Vec::new();
    let mut targets = message.diff_targets().iter().peekable();
    let mut lines = Vec::new();
    for (index, source_line) in message.content.split('\n').enumerate() {
        while targets
            .peek()
            .is_some_and(|target| target.content_line == index)
        {
            editor_targets.push((
                lines.len(),
                targets
                    .next()
                    .expect("peeked diff target exists")
                    .editor
                    .clone(),
            ));
        }
        let foreground = if index == 0 {
            activity_header_color(source_line)
        } else if source_line.starts_with("@@") {
            Color::Cyan
        } else if source_line.starts_with('+') {
            Color::Green
        } else if source_line.starts_with('-') {
            Color::Red
        } else {
            Color::Gray
        };
        lines.extend(
            wrap_activity(source_line, content_width)
                .into_iter()
                .map(|line| {
                    let width = UnicodeWidthStr::width(line.as_str());
                    let mut spans = Vec::with_capacity(2);
                    if left_padding > 0 {
                        spans.push(Span::styled(
                            " ".repeat(left_padding),
                            Style::default().bg(background),
                        ));
                    }
                    spans.push(Span::styled(
                        format!("{line}{}", " ".repeat(content_width.saturating_sub(width))),
                        Style::default().fg(foreground).bg(background),
                    ));
                    Line::from(spans)
                }),
        );
    }
    lines.push(Line::from(""));
    RenderedMessage {
        lines,
        editor_targets,
    }
}

fn visible_editor_target(
    targets: Vec<(usize, EditorTarget)>,
    scroll_top: usize,
    viewport_height: usize,
) -> Option<EditorTarget> {
    let viewport_end = scroll_top.saturating_add(viewport_height);
    let viewport_center = scroll_top.saturating_add(viewport_height / 2);
    targets
        .into_iter()
        .filter(|(line, _)| *line >= scroll_top && *line < viewport_end)
        .min_by_key(|(line, _)| line.abs_diff(viewport_center))
        .map(|(_, target)| target)
}

fn activity_is_in_progress(content: &str) -> bool {
    [
        "Thinking…",
        "Running:",
        "Editing:",
        "Tool:",
        "Searching:",
        "Compacting context…",
        "Plan:",
    ]
    .iter()
    .any(|prefix| content.starts_with(prefix))
}

fn wrap_activity(content: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    content
        .split('\n')
        .flat_map(|line| {
            let body_start = line
                .find(|character: char| !character.is_whitespace())
                .unwrap_or(line.len());
            let (indent, body) = line.split_at(body_start);
            let indent_width = UnicodeWidthStr::width(indent);
            if body.is_empty() || indent_width >= max_width {
                return wrap_cells(line, max_width);
            }
            wrap_message_line(body, max_width - indent_width)
                .into_iter()
                .map(|part| format!("{indent}{part}"))
                .collect()
        })
        .collect()
}

fn wrap_cells(line: &str, max_width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut lines = vec![String::new()];
    let mut width = 0;
    for grapheme in line.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > max_width && !lines.last().is_some_and(String::is_empty) {
            lines.push(String::new());
            width = 0;
        }
        lines
            .last_mut()
            .expect("wrapped activity always has a line")
            .push_str(grapheme);
        width += grapheme_width;
    }
    lines
}

fn user_message_lines(message: &ChatMessage, available_width: usize) -> Vec<Line<'static>> {
    if available_width < 3 {
        return vec![Line::styled(
            message.content.clone(),
            Style::default().fg(Color::White).bg(Color::Rgb(38, 45, 50)),
        )];
    }
    let background = Color::Rgb(38, 45, 50);
    let content_width = available_width - 2;
    let padding_line = Line::styled(" ".repeat(available_width), Style::default().bg(background));
    let mut lines = vec![padding_line.clone()];
    lines.extend(
        wrap_message(&message.content, content_width)
            .into_iter()
            .enumerate()
            .map(|(index, content)| {
                let width = UnicodeWidthStr::width(content.as_str());
                let prefix = if index == 0 { "› " } else { "  " };
                Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Cyan).bg(background)),
                    Span::styled(
                        format!(
                            "{content}{}",
                            " ".repeat(content_width.saturating_sub(width))
                        ),
                        Style::default().fg(Color::White).bg(background),
                    ),
                ])
            }),
    );
    lines.push(padding_line);
    lines
}

fn wrap_message(message: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    message
        .split('\n')
        .flat_map(|line| wrap_message_line(line, max_width))
        .collect()
}

fn wrap_message_line(line: &str, max_width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut wrapped = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for segment in line.split_word_bounds() {
        let segment_width = UnicodeWidthStr::width(segment);
        if segment.chars().all(char::is_whitespace) {
            if !current.is_empty() && current_width + segment_width <= max_width {
                current.push_str(segment);
                current_width += segment_width;
            }
            continue;
        }
        if current_width + segment_width <= max_width {
            current.push_str(segment);
            current_width += segment_width;
            continue;
        }
        if !current.trim_end().is_empty() {
            wrapped.push(current.trim_end().to_owned());
            current.clear();
            current_width = 0;
        }
        for grapheme in segment.graphemes(true) {
            let width = UnicodeWidthStr::width(grapheme);
            if current_width + width > max_width && !current.is_empty() {
                wrapped.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push_str(grapheme);
            current_width += width;
        }
    }
    if !current.trim_end().is_empty() || wrapped.is_empty() {
        wrapped.push(current.trim_end().to_owned());
    }
    wrapped
}

fn wrap_composer(composer: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    let mut lines = vec![String::new()];
    let mut width = 0;
    for grapheme in composer.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > max_width && !lines.last().is_some_and(String::is_empty) {
            lines.push(String::new());
            width = 0;
        }
        lines
            .last_mut()
            .expect("composer always has a line")
            .push_str(grapheme);
        width += grapheme_width;
    }
    if width == max_width {
        lines.push(String::new());
    }
    lines
}

fn render_approval(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(76, 12, area);
    let request = app.pending_approvals.front();
    let thread_title = request
        .and_then(|request| request.thread_id.as_deref())
        .and_then(|thread_id| app.chats.get(thread_id))
        .map(|chat| chat.title.as_str())
        .unwrap_or("thread");
    let method = request
        .map(|value| value.method.as_str())
        .unwrap_or("approval");
    let detail = request
        .map(|value| value.params.to_string())
        .unwrap_or_default();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "{thread_title}\n{method}\n\n{detail}\n\nApprove this request? [y/N]"
        ))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" Codex approval ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        popup,
    );
}

fn render_navigation_tree(frame: &mut Frame, app: &App, area: Rect) {
    let visible_thread_id = app.main_chat().map(|chat| chat.thread_id.as_str());
    let rows = app.tree_rows();
    let items = rows.iter().map(|row| match row {
        TreeRow::Repository { repository_index } => {
            let repository = &app.repositories[*repository_index];
            let marker = if app.repository_is_expanded(*repository_index) {
                "▾"
            } else {
                "▸"
            };
            ListItem::new(Text::from(vec![
                Line::styled(
                    format!("{marker} {}", repository.name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    format!("  {}", repository.path.display()),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        }
        TreeRow::Thread { thread_index, .. } => {
            let thread = &app.threads[*thread_index];
            let visible = visible_thread_id == Some(thread.record.id.as_str());
            let working = app
                .chats
                .get(&thread.record.id)
                .is_some_and(|chat| chat.active_turn_id.is_some());
            let awaiting_approval = app
                .pending_approvals
                .iter()
                .any(|request| request.thread_id.as_deref() == Some(thread.record.id.as_str()));
            let marker = if awaiting_approval {
                "!"
            } else if working {
                "◉"
            } else if visible {
                "●"
            } else {
                "•"
            };
            let title_style = if awaiting_approval {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if working {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if visible {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let kind = if thread.is_primary {
                "primary"
            } else {
                "worktree"
            };
            ListItem::new(Text::from(vec![
                Line::styled(format!("  {marker} {}", thread.record.title), title_style),
                Line::styled(
                    format!("    {} · {kind}", thread.location_name),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        }
    });
    let title = if app.show_archived {
        " Repositories · archived "
    } else {
        " Repositories "
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(focus_style(app.focus == Focus::Navigation)),
        )
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default()
        .with_selected((!rows.is_empty()).then_some(app.tree_index.min(rows.len() - 1)));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_thread_targets(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(64, 9, area);
    let existing_count = app.existing_worktrees().len();
    let items = [
        ListItem::new(Text::from(vec![
            Line::from("Primary repository"),
            Line::styled(
                "Start in the main checkout",
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        ListItem::new(Text::from(vec![
            Line::from("New worktree"),
            Line::styled(
                "Create an automatically named branch and worktree",
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        ListItem::new(Text::from(vec![
            Line::from(format!("Existing worktree ({existing_count})")),
            Line::styled(
                "Choose from git worktree list",
                Style::default().fg(Color::DarkGray),
            ),
        ])),
    ];
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Start new thread ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default().with_selected(Some(app.thread_target_index));
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_existing_worktrees(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(72, 12, area);
    let worktrees = app.existing_worktrees();
    let items = worktrees.iter().map(|location| {
        ListItem::new(Text::from(vec![
            Line::from(location.name.clone()),
            Line::styled(
                location.path.display().to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    });
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Existing worktrees ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state =
        ListState::default().with_selected((!worktrees.is_empty()).then_some(app.location_index));
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_repository_add(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(90, area.height.saturating_sub(4), area);
    let candidates = app.visible_candidates();
    let items = candidates.iter().map(|repository| {
        let checked = if app.selected_candidates.contains(&repository.path) {
            "[x]"
        } else {
            "[ ]"
        };
        ListItem::new(Text::from(vec![
            Line::from(format!("{checked} {}", repository.name)),
            Line::styled(
                repository.path.display().to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    });
    let scan = if app.scanning { " · scanning…" } else { "" };
    let filter = if app.repository_query.is_empty() {
        String::new()
    } else {
        format!(" · /{}", app.repository_query)
    };
    let title = format!(
        " Add repositories · {} found{scan}{filter} ",
        candidates.len()
    );
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .title_bottom(Line::from(
                    " j/k move · Space select · Enter register · / filter · b browse · s scan home · Esc back ",
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state =
        ListState::default().with_selected((!candidates.is_empty()).then_some(app.candidate_index));
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_browser(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(90, area.height.saturating_sub(4), area);
    let items = app.browse_directories.iter().map(|path| {
        ListItem::new(
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        )
    });
    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Browse · {} ", app.browse_path.display()))
                .title_bottom(Line::from(
                    " Enter/l open · h/Backspace parent · a register current directory · Esc back ",
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default()
        .with_selected((!app.browse_directories.is_empty()).then_some(app.browse_index));
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_remove_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(68, 8, area);
    let title = app
        .selected_thread()
        .map(|thread| thread.record.title.as_str())
        .unwrap_or("thread");
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "Remove '{title}' from wyard?\n\nCodex history is not deleted. [y/N]"
        ))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" Remove thread ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        popup,
    );
}

fn render_repository_remove_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(68, 8, area);
    let name = app
        .selected_repository()
        .map(|repository| repository.name.as_str())
        .unwrap_or("repository");
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "Remove '{name}' from wyard?\n\nFiles and Codex threads are not deleted. [y/N]"
        ))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" Remove repository ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        popup,
    );
}

fn render_archive_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(72, 9, area);
    let title = app
        .selected_thread()
        .map(|thread| thread.record.title.as_str())
        .unwrap_or("thread");
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "Archive '{title}'?\n\nThe wyard-created worktree is clean.\nRemove it now? The branch and Codex history remain. [y/N]"
        ))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" Archive thread ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        popup,
    );
}

fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(64, 25, area);
    let help = "j / k / ↑↓  move and preview selected thread\nh / ←        collapse repository / select parent\nl / →        expand repository\nEnter        expand repository / focus chat input\nTab          focus chat / enter scroll mode\nJ / K        next / previous message in scroll mode\ne            open the visible diff hunk in Neovim\ny / Y        copy selected message / full chat\nCtrl-g       switch main / side chat focus\nCtrl-n / p   next / previous side chat\n/            commands, skills, model, and side chat\nEsc          return to repository tree / cancel\na            add repositories\nn            create thread in selected repository\nx            archive / restore thread\nA            active / archived threads\nd            unregister repository / remove thread\nr            reload registered repositories\n?            help\nq            quit\n\n● visible · ◉ working · ! approval waiting\nAll Codex turns use danger-full-access without approval prompts.\nPress any key to close";
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(help).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" Keymap ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        popup,
    );
}

fn centered_rect(width_percent: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(height.min(area.height)),
            Constraint::Fill(1),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn navigation_width(total_width: u16) -> u16 {
    (total_width / 4)
        .clamp(34, 42)
        .min(total_width.saturating_sub(20))
}

fn quit_requires_side_chat_confirmation(side_chat_count: usize) -> bool {
    side_chat_count > 0
}

fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_width_is_bounded_and_preserves_chat_space() {
        assert_eq!(navigation_width(200), 42);
        assert_eq!(navigation_width(120), 34);
        assert_eq!(navigation_width(80), 34);
        assert_eq!(navigation_width(30), 10);
    }

    #[test]
    fn any_remaining_side_chat_requires_quit_confirmation() {
        assert!(!quit_requires_side_chat_confirmation(0));
        assert!(quit_requires_side_chat_confirmation(1));
        assert!(quit_requires_side_chat_confirmation(3));
    }

    #[test]
    fn user_message_is_a_full_width_colored_band() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("a message that wraps across the row".into(), "u".into());
        let lines = chat_message_lines(&chat.messages[0], 30, false).lines;

        assert_eq!(lines[1].spans[0].content, "› ");
        assert!(lines.len() >= 3);
        for line in &lines {
            assert_eq!(line.width(), 30);
            assert!(
                line.style.bg == Some(Color::Rgb(38, 45, 50))
                    || line
                        .spans
                        .iter()
                        .all(|span| span.style.bg == Some(Color::Rgb(38, 45, 50)))
            );
        }
    }

    #[test]
    fn activity_is_a_full_width_status_band_with_a_separator() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.push_notice("✓ build\n  preserved output".into());
        let lines = chat_message_lines(&chat.messages[0], 30, false).lines;

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].width(), 30);
        assert_eq!(lines[1].width(), 30);
        assert_eq!(lines[2].width(), 0);
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Green));
        assert!(lines[1].spans[1].content.starts_with("  preserved"));
        for line in &lines[..2] {
            assert!(
                line.spans
                    .iter()
                    .all(|span| span.style.bg == Some(Color::Rgb(32, 34, 36)))
            );
        }
    }

    #[test]
    fn activity_status_color_distinguishes_running_and_failure() {
        assert_eq!(activity_header_color("Running: tests"), Color::Yellow);
        assert_eq!(activity_header_color("✗ tests [failed]"), Color::Red);
        assert_eq!(activity_header_color("Thought\nDone"), Color::Green);
    }

    #[test]
    fn app_server_diff_lines_are_colored_without_external_tools() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.load_history(&json!({"thread":{"turns":[{"items":[{
            "id":"edit-1",
            "type":"fileChange",
            "status":"completed",
            "changes":[{
                "path":"src/main.rs",
                "kind":"update",
                "diff":"diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new"
            }]
        }]}]}}));
        let rendered = diff_message_lines(&chat.messages[0], 50);
        let lines = rendered.lines;

        assert_eq!(lines[0].spans.last().unwrap().style.fg, Some(Color::Green));
        assert_eq!(lines[2].spans.last().unwrap().style.fg, Some(Color::Red));
        assert_eq!(lines[3].spans.last().unwrap().style.fg, Some(Color::Green));
        assert_eq!(lines[4].spans.last().unwrap().style.fg, Some(Color::Cyan));
        assert_eq!(lines[5].spans.last().unwrap().style.fg, Some(Color::Red));
        assert_eq!(lines[6].spans.last().unwrap().style.fg, Some(Color::Green));
        assert!(lines[..7].iter().all(|line| line.width() == 50));
        assert_eq!(lines[7].width(), 0);
        assert_eq!(rendered.editor_targets[0].0, 4);
        assert_eq!(
            rendered.editor_targets[0].1.path,
            PathBuf::from("src/main.rs")
        );
        assert_eq!(rendered.editor_targets[0].1.line, 1);
    }

    #[test]
    fn editor_target_uses_the_hunk_nearest_the_viewport_center() {
        let target = |path: &str, line| EditorTarget {
            path: PathBuf::from(path),
            line,
        };
        let selected = visible_editor_target(
            vec![
                (5, target("early.rs", 5)),
                (14, target("middle.rs", 14)),
                (19, target("late.rs", 19)),
            ],
            10,
            10,
        )
        .expect("visible target");

        assert_eq!(selected.path, PathBuf::from("middle.rs"));
        assert_eq!(selected.line, 14);
        assert!(visible_editor_target(vec![(9, target("hidden.rs", 9))], 10, 10).is_none());
    }

    #[test]
    fn bubble_wrapper_respects_unicode_cell_width() {
        let wrapped = wrap_message("日本語の長い入力とabcdefghijk", 10);
        assert!(
            wrapped
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 10)
        );
    }

    #[test]
    fn composer_wraps_by_terminal_cell_width() {
        assert_eq!(wrap_composer("日本語", 4), vec!["日本", "語"]);
        assert_eq!(wrap_composer("abcd", 4), vec!["abcd", ""]);
        assert_eq!(wrap_composer("", 4), vec![""]);
    }

    #[test]
    fn chat_renders_thinking_while_waiting_for_activity() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("question".into(), "turn".into());

        let lines = rendered_chat(&chat, 40).lines;
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("Thinking…"));
    }

    #[test]
    fn active_reasoning_activity_gets_an_animated_frame() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("question".into(), "turn".into());
        chat.push_notice("Thinking…".into());

        let text = rendered_chat(&chat, 40)
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("Thinking…"));
        assert!(
            ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"]
                .iter()
                .any(|frame| text.contains(frame))
        );
    }

    #[test]
    fn selected_message_is_highlighted_in_scroll_mode() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("selected".into(), "turn".into());
        chat.enter_scroll_mode();

        let rendered = rendered_chat(&chat, 30);
        let (start, end) = rendered.selected_range.expect("selected range");
        assert!(rendered.lines[start..end].iter().all(|line| {
            line.style.bg == Some(Color::Rgb(52, 63, 72))
                || line
                    .spans
                    .iter()
                    .all(|span| span.style.bg == Some(Color::Rgb(52, 63, 72)))
        }));
    }
}
