use std::{
    collections::HashMap,
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::{
    cursor::{MoveTo, SetCursorStyle},
    event::{
        Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        Clear as ClearTerminal, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use futures::{StreamExt, future::join_all};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast::error::RecvError, mpsc};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{App, AttentionKind, ChatPane, Focus, Mode, ThreadDeletionPhase, TreeRow},
    app_server::{AppServer, AppServerRequest, TurnSettings},
    chat::{
        ChatMessage, ChatMode, ChatRole, ChatState, CommandPalette, EditorTarget, PaletteCommand,
        PaletteEntry,
    },
    clipboard,
    git_workspace::{self, Workspace},
    registry::ThreadRecord,
    settings::ExecutionMode,
};

type Tui = Terminal<CrosstermBackend<Stdout>>;

const REDRAW_INTERVAL: Duration = Duration::from_millis(16);
const PREVIEW_TURN_LIMIT: u32 = 5;
const PREVIEW_CACHE_CAPACITY: usize = 20;
const MAX_THREAD_NAME_CHARS: usize = 100;

struct ChatPreview {
    generation: u64,
    result: std::result::Result<ChatState, String>,
}

#[derive(Deserialize)]
struct StartThreadToolArguments {
    prompt: String,
    workspace: StartThreadWorkspace,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StartThreadWorkspace {
    Current,
    NewWorktree,
}

struct StartedThread {
    id: String,
    title: String,
    workspace: PathBuf,
}

#[derive(Default)]
struct RenderCache {
    chats: HashMap<String, ChatRenderCache>,
}

#[derive(Default)]
struct ChatRenderCache {
    generation: u64,
    width: usize,
    messages: Vec<Option<CachedRenderedMessage>>,
}

enum UiAction {
    CopyEditorCommand { cwd: PathBuf, target: EditorTarget },
    DeleteThread(ThreadRecord),
}

enum ThreadDeletionEvent {
    Phase(ThreadDeletionPhase),
    Finished {
        thread_id: String,
        result: std::result::Result<(), String>,
    },
}

struct ApprovalPrompt {
    thread_title: String,
    method: String,
    detail: String,
}

pub async fn run(mut app: App) -> Result<()> {
    let server = AppServer::spawn("codex", Duration::from_secs(30)).await?;
    cleanup_abandoned_side_chats(&mut app, &server).await;
    refresh_thread_names(&mut app, &server).await;
    match server.list_models().await {
        Ok(models) => app.set_models(models),
        Err(error) => app.message = Some(format!("Could not load models: {error}")),
    }
    let mut terminal = match init_terminal() {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = server.shutdown().await;
            return Err(error);
        }
    };
    let result = run_loop(&mut terminal, &mut app, Arc::clone(&server)).await;
    unsubscribe_all_threads(&mut app, &server).await;
    let restore_result = restore_terminal(&mut terminal);
    let shutdown_result = server.shutdown().await;
    result?;
    restore_result?;
    shutdown_result
}

fn init_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        ClearTerminal(ClearType::All),
        SetCursorStyle::BlinkingBar,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        MoveTo(0, 0)
    )?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        SetCursorStyle::DefaultUserShape,
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

async fn run_loop(terminal: &mut Tui, app: &mut App, server: Arc<AppServer>) -> Result<()> {
    let mut inputs = EventStream::new();
    let mut server_events = server.subscribe();
    let preview_generation = Arc::new(AtomicU64::new(0));
    let (preview_sender, mut preview_receiver) = mpsc::unbounded_channel();
    let (deletion_sender, mut deletion_receiver) = mpsc::unbounded_channel();
    let mut preview_task = None;
    let mut redraw_ticker = tokio::time::interval(REDRAW_INTERVAL);
    redraw_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    let mut render_cache = RenderCache::default();
    let mut needs_draw = true;
    while !app.should_quit {
        tokio::select! {
            _ = redraw_ticker.tick() => {
                if needs_draw {
                    terminal.draw(|frame| render(frame, app, &mut render_cache))?;
                    needs_draw = false;
                    redraw_ticker.reset();
                }
            }
            _ = ticker.tick() => {
                if app.scanning {
                    app.poll_scan();
                    needs_draw = true;
                }
                if app.visible_chat_has_active_turn() {
                    needs_draw = true;
                }
                if app.thread_deletion.is_some() {
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
                            &mut preview_task,
                        ).await?;
                        match action {
                            Some(UiAction::CopyEditorCommand { cwd, target }) => {
                                app.message = Some(match copy_editor_command(&cwd, &target).await {
                                    Ok(()) => format!(
                                        "Copied editor command: {}:{}",
                                        target.path.display(),
                                        target.line
                                    ),
                                    Err(error) => format!("Could not copy editor command: {error}"),
                                });
                            }
                            Some(UiAction::DeleteThread(record)) => {
                                spawn_thread_deletion(
                                    Arc::clone(&server),
                                    record,
                                    deletion_sender.clone(),
                                );
                            }
                            None => {}
                        }
                        reconcile_thread_subscriptions(app, &server).await;
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
                            let thread_id = chat.thread_id.clone();
                            let still_selected = app.selected_tree_is_thread()
                                && app.selected_thread().is_some_and(|thread| {
                                    thread.record.id == thread_id
                                });
                            if still_selected {
                                if !app.show_cached_chat(&thread_id) {
                                    app.cache_chat_preview(
                                        chat,
                                        PREVIEW_CACHE_CAPACITY,
                                        true,
                                    );
                                }
                            } else if !app.chats.contains_key(&thread_id) {
                                app.cache_chat_preview(chat, PREVIEW_CACHE_CAPACITY, false);
                            }
                            reconcile_thread_subscriptions(app, &server).await;
                        }
                        Err(error) => app.message = Some(error),
                    }
                    preview_task = None;
                    needs_draw = true;
                }
            }
            deletion = deletion_receiver.recv() => {
                if let Some(deletion) = deletion {
                    match deletion {
                        ThreadDeletionEvent::Phase(phase) => {
                            app.set_thread_deletion_phase(phase);
                        }
                        ThreadDeletionEvent::Finished { thread_id, result } => {
                            app.end_thread_deletion();
                            app.message = Some(match result {
                                Ok(()) => match app.complete_thread_deletion(&thread_id) {
                                    Ok(()) => "thread permanently deleted".into(),
                                    Err(error) => format!(
                                        "Codex deletion completed, but local registry cleanup failed: {error}"
                                    ),
                                },
                                Err(error) => format!("Could not delete thread: {error}"),
                            });
                        }
                    }
                    needs_draw = true;
                }
            }
            event = server_events.recv() => {
                match event {
                    Ok(event) => {
                        app.apply_chat_event(&event);
                        reconcile_thread_subscriptions(app, &server).await;
                        needs_draw = true;
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        resync_subscribed_threads(app, &server, skipped).await;
                        reconcile_thread_subscriptions(app, &server).await;
                        needs_draw = true;
                    }
                    Err(RecvError::Closed) => bail!("Codex App Server event stream closed"),
                }
            }
            request = server.next_server_request() => {
                if let Some(request) = request {
                    if is_approval(&request) {
                        let unscoped = request.thread_id.is_none();
                        app.enqueue_approval(request);
                        if unscoped {
                            app.mode = Mode::Approval;
                        }
                        needs_draw = true;
                    } else if request.method == "item/tool/call" {
                        handle_dynamic_tool_call(app, &server, request).await?;
                        needs_draw = true;
                    } else {
                        server.respond(request.id, Value::Null).await?;
                    }
                }
            }
        }
    }
    if let Some(task) = preview_task.take() {
        task.abort();
    }
    Ok(())
}

async fn handle_key(
    app: &mut App,
    key: KeyEvent,
    server: &Arc<AppServer>,
    preview_generation: &Arc<AtomicU64>,
    preview_sender: &mpsc::UnboundedSender<ChatPreview>,
    preview_task: &mut Option<JoinHandle<()>>,
) -> Result<Option<UiAction>> {
    if app.thread_deletion.is_some() {
        return Ok(None);
    }
    if app.mode == Mode::Chat && app.focus == Focus::Chat && app.active_chat_has_pending_approval()
    {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                resolve_active_chat_approval(app, server, true).await?;
                return Ok(None);
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                resolve_active_chat_approval(app, server, false).await?;
                return Ok(None);
            }
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.focus = Focus::Navigation;
                return Ok(None);
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                app.toggle_chat_pane();
                return Ok(None);
            }
            _ => return Ok(None),
        }
    }
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
                    if let Some(chat) = app.chat()
                        && let Some(target) = chat.visible_editor_target.clone()
                    {
                        action = Some(UiAction::CopyEditorCommand {
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
                if let Err(error) = cleanup_unused_main_chat(app, server).await {
                    app.message = Some(format!("Could not remove unused thread: {error}"));
                }
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
                    chat.clear_composer();
                    chat.selected_skills.clear();
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(chat) = app.chat_mut() {
                    chat.move_composer_line_start();
                }
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(chat) = app.chat_mut() {
                    chat.move_composer_line_end();
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
                    chat.backspace_composer();
                }
            }
            KeyCode::Delete => {
                if let Some(chat) = app.chat_mut() {
                    chat.delete_composer();
                }
            }
            KeyCode::Left => {
                if let Some(chat) = app.chat_mut() {
                    chat.move_composer_left();
                }
            }
            KeyCode::Right => {
                if let Some(chat) = app.chat_mut() {
                    chat.move_composer_right();
                }
            }
            KeyCode::Up => {
                if let Some(chat) = app.chat_mut() {
                    chat.move_composer_up();
                }
            }
            KeyCode::Down => {
                if let Some(chat) = app.chat_mut() {
                    chat.move_composer_down();
                }
            }
            KeyCode::Home => {
                if let Some(chat) = app.chat_mut() {
                    chat.move_composer_line_start();
                }
            }
            KeyCode::End => {
                if let Some(chat) = app.chat_mut() {
                    chat.move_composer_line_end();
                }
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if let Some(chat) = app.chat_mut() {
                    chat.insert_composer_newline();
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
                if app.active_chat_is_read_only() {
                    app.message = Some(
                        "Read-only: this thread is open in another Codex session; use /threads to retry"
                            .into(),
                    );
                } else if let Some(chat) = app.chat_mut() {
                    chat.insert_composer_char(character);
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
        Mode::ChoosePermissions => match key.code {
            KeyCode::Esc => app.mode = Mode::Chat,
            KeyCode::Up | KeyCode::Char('k') => app.move_permission_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_permission_down(),
            KeyCode::Enter => {
                if let Err(error) = app.choose_permission() {
                    app.message = Some(format!("Could not save execution mode: {error}"));
                }
            }
            _ => {}
        },
        Mode::ConfirmDangerous => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Err(error) = app.confirm_dangerous() {
                    app.message = Some(format!("Could not save execution mode: {error}"));
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.mode = Mode::ChoosePermissions;
            }
            _ => {}
        },
        Mode::ChooseSideChat => match key.code {
            KeyCode::Esc => app.cancel_side_chat_picker(),
            KeyCode::Up | KeyCode::Char('k') => app.move_side_chat_picker_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_side_chat_picker_down(),
            KeyCode::Enter => app.select_side_chat_from_picker(),
            _ => {}
        },
        Mode::ChooseThread => match key.code {
            KeyCode::Esc => app.cancel_thread_picker(),
            KeyCode::Up => app.move_thread_picker_up(),
            KeyCode::Down => app.move_thread_picker_down(),
            KeyCode::Char('k') if app.thread_picker_query.is_empty() => app.move_thread_picker_up(),
            KeyCode::Char('j') if app.thread_picker_query.is_empty() => {
                app.move_thread_picker_down()
            }
            KeyCode::Backspace => {
                if app.thread_picker_query.is_empty() {
                    app.cancel_thread_picker();
                } else {
                    app.pop_thread_picker_query();
                }
            }
            KeyCode::Enter => {
                if app.activate_selected_thread_picker() {
                    cancel_chat_preview(preview_generation, preview_task);
                    if let Err(error) = focus_selected_chat(app, server).await {
                        app.message = Some(format!("Could not open thread: {error}"));
                    }
                }
            }
            KeyCode::Char('y') if app.thread_picker_query.is_empty() => {
                copy_selected_thread_value(app, ThreadCopy::Id);
            }
            KeyCode::Char('Y') if app.thread_picker_query.is_empty() => {
                copy_selected_thread_value(app, ThreadCopy::ResumeCommand);
            }
            KeyCode::Char('R') => app.open_thread_rename(true),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                app.push_thread_picker_query(character);
            }
            _ => {}
        },
        Mode::RenameThread => match key.code {
            KeyCode::Esc => app.close_thread_rename(),
            KeyCode::Backspace => {
                app.rename_input.pop();
                app.message = None;
            }
            KeyCode::Enter => submit_thread_rename(app, server).await,
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if app.rename_input.graphemes(true).count() < MAX_THREAD_NAME_CHARS {
                    app.rename_input.push(character);
                    app.message = None;
                } else {
                    app.message = Some(format!(
                        "Thread names can be at most {MAX_THREAD_NAME_CHARS} characters"
                    ));
                }
            }
            _ => {}
        },
        Mode::Attention => match key.code {
            KeyCode::Esc => app.close_attention(),
            KeyCode::Up | KeyCode::Char('k') => app.move_attention_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_attention_down(),
            KeyCode::Char('d') | KeyCode::Char('x') => app.dismiss_selected_attention(),
            KeyCode::Enter => app.activate_selected_attention(),
            _ => {}
        },
        Mode::ConfirmQuit => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if prepare_to_quit(app, server).await {
                    app.should_quit = true;
                } else {
                    app.mode = Mode::Normal;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.mode = Mode::Normal,
            _ => {}
        },
        Mode::Approval => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                resolve_unscoped_approval(app, server, true).await?
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                resolve_unscoped_approval(app, server, false).await?
            }
            _ => {}
        },
        Mode::AddRepositories => match key.code {
            KeyCode::Char('q') if app.repositories.is_empty() => request_quit(app),
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
        Mode::ConfirmDeleteThread => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                action = Some(UiAction::DeleteThread(app.begin_thread_deletion()?));
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.mode = Mode::Normal,
            _ => {}
        },
        Mode::DeletingThread => {}
        Mode::ConfirmRemoveRepository => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.mode = Mode::Normal;
                app.message = Some(match app.unregister_selected_repository() {
                    Ok(()) => "repository removed from Shikigami".into(),
                    Err(error) => error.to_string(),
                });
            }
            _ => app.mode = Mode::Normal,
        },
        Mode::Help => app.mode = Mode::Normal,
        Mode::Normal => match key.code {
            KeyCode::Char('q') => request_quit(app),
            KeyCode::Char('?') => app.mode = Mode::Help,
            KeyCode::Char('!') => app.open_attention(),
            KeyCode::Char('/') => app.open_thread_picker(),
            KeyCode::Esc if app.selected_tree_is_thread() => app.select_parent_repository(),
            KeyCode::Char('h') | KeyCode::Left => app.collapse_selected_repository(),
            KeyCode::Char('l') | KeyCode::Right => app.expand_selected_repository(),
            KeyCode::Tab if app.chat().is_some() => {
                app.focus = Focus::Chat;
                app.mode = Mode::Chat;
                if let Some(chat) = app.chat_mut() {
                    chat.enter_scroll_mode();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.move_up();
                schedule_selected_chat_preview(
                    app,
                    server,
                    preview_generation,
                    preview_sender,
                    preview_task,
                );
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.move_down();
                schedule_selected_chat_preview(
                    app,
                    server,
                    preview_generation,
                    preview_sender,
                    preview_task,
                );
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
                } else {
                    refresh_thread_names(app, server).await;
                }
            }
            KeyCode::Char('R') if app.selected_tree_is_thread() => {
                app.open_thread_rename(false);
            }
            KeyCode::Char('y') if app.selected_tree_is_thread() => {
                copy_selected_thread_value(app, ThreadCopy::Id);
            }
            KeyCode::Char('Y') if app.selected_tree_is_thread() => {
                copy_selected_thread_value(app, ThreadCopy::ResumeCommand);
            }
            KeyCode::Char('d') if app.selected_tree_is_repository() => {
                app.mode = Mode::ConfirmRemoveRepository;
            }
            KeyCode::Char('d')
                if permanent_delete_shortcut_available(
                    app.show_archived,
                    app.selected_tree_is_thread(),
                ) =>
            {
                match app.selected_thread_delete_target() {
                    Ok(_) => app.mode = Mode::ConfirmDeleteThread,
                    Err(error) => app.message = Some(error.to_string()),
                }
            }
            KeyCode::Char('x') if app.selected_tree_is_thread() => {
                if app.show_archived {
                    app.message = Some(match app.unarchive_selected_thread() {
                        Ok(()) => "thread restored".into(),
                        Err(error) => error.to_string(),
                    });
                } else {
                    match app.selected_thread_has_active_turn() {
                        Ok(true) => {
                            app.message =
                                Some("response is running; stop it before archiving".into());
                        }
                        Ok(false) => {
                            app.message = Some(match app.archive_selected_thread() {
                                Ok(()) => "thread archived".into(),
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
                cancel_chat_preview(preview_generation, preview_task);
                if let Err(error) = focus_selected_chat(app, server).await {
                    app.message = Some(format!("Could not open thread: {error}"));
                }
            }
            _ => {}
        },
    }
    Ok(action)
}

async fn copy_editor_command(cwd: &Path, target: &EditorTarget) -> Result<()> {
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

    let output = tokio::process::Command::new("git")
        .args(["var", "GIT_EDITOR"])
        .current_dir(&root)
        .output()
        .await
        .context("could not resolve Git editor")?;
    if !output.status.success() {
        bail!("git var GIT_EDITOR exited with {}", output.status);
    }

    let editor = String::from_utf8(output.stdout).context("Git editor is not valid UTF-8")?;
    let editor = editor.trim();
    if editor.is_empty() {
        bail!("git var GIT_EDITOR returned an empty editor");
    }
    let path = path.to_str().context("file path is not valid UTF-8")?;
    let command = editor_command(editor, path, target.line);
    clipboard::copy(&command).context("copy command to clipboard")?;
    Ok(())
}

fn editor_command(editor: &str, path: &str, line: usize) -> String {
    let program = editor_program_name(editor);
    match program.as_str() {
        "vi" | "vim" | "nvim" | "gvim" | "mvim" => {
            format!("{editor} +{line} -- {}", shell_quote(path))
        }
        "code" | "code-insiders" | "codium" | "cursor" => {
            format!("{editor} --goto {}", shell_quote(&format!("{path}:{line}")))
        }
        "hx" | "helix" | "zed" | "zeditor" | "subl" | "sublime_text" => {
            format!("{editor} {}", shell_quote(&format!("{path}:{line}")))
        }
        "emacs" | "emacsclient" => {
            format!("{editor} +{line} {}", shell_quote(path))
        }
        "nano" => format!("{editor} +{line},1 {}", shell_quote(path)),
        "notepad++" => format!("{editor} -n{line} {}", shell_quote(path)),
        "idea" | "clion" | "goland" | "phpstorm" | "pycharm" | "rider" | "rubymine"
        | "rustrover" | "webstorm" => {
            format!("{editor} --line {line} {}", shell_quote(path))
        }
        _ => format!("{editor} {}", shell_quote(path)),
    }
}

fn editor_program_name(editor: &str) -> String {
    let editor = editor.trim_start();
    let program = match editor.as_bytes().first() {
        Some(b'\'') => editor[1..].split('\'').next().unwrap_or_default(),
        Some(b'"') => editor[1..].split('"').next().unwrap_or_default(),
        _ => editor.split_whitespace().next().unwrap_or_default(),
    };
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let lowercase = name.to_ascii_lowercase();
    lowercase
        .strip_suffix(".exe")
        .or_else(|| lowercase.strip_suffix(".cmd"))
        .or_else(|| lowercase.strip_suffix(".bat"))
        .unwrap_or(&lowercase)
        .to_owned()
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn shell_quote(value: &str) -> String {
    format!("\"{value}\"")
}

#[cfg(not(any(unix, windows)))]
fn shell_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\\\""))
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

#[derive(Clone, Copy)]
enum ThreadCopy {
    Id,
    ResumeCommand,
}

fn selected_thread_action_id(app: &App) -> Option<String> {
    let thread = match app.mode {
        Mode::ChooseThread => app.selected_thread_picker(),
        Mode::Normal if app.selected_tree_is_thread() => app.selected_thread(),
        _ => None,
    }?;
    Some(thread.record.id.clone())
}

fn copy_selected_thread_value(app: &mut App, value: ThreadCopy) {
    let Some(thread_id) = selected_thread_action_id(app) else {
        app.message = Some("No thread selected".into());
        return;
    };
    let (text, success, failure) = match value {
        ThreadCopy::Id => (
            thread_id.clone(),
            "Copied thread ID",
            "Could not copy thread ID",
        ),
        ThreadCopy::ResumeCommand => (
            codex_resume_command(&thread_id),
            "Copied Codex resume command",
            "Could not copy Codex resume command",
        ),
    };
    app.message = Some(match clipboard::copy(&text) {
        Ok(()) => format!("{success}: {thread_id}"),
        Err(error) => format!("{failure}: {error}"),
    });
}

fn codex_resume_command(thread_id: &str) -> String {
    format!("codex resume {thread_id}")
}

async fn submit_thread_rename(app: &mut App, server: &Arc<AppServer>) {
    let name = match validate_thread_name(&app.rename_input) {
        Ok(name) => name,
        Err(error) => {
            app.message = Some(error);
            return;
        }
    };
    let Some(thread_id) = app.rename_thread_id.clone() else {
        app.message = Some("No thread selected".into());
        app.close_thread_rename();
        return;
    };
    match server.set_thread_name(&thread_id, &name).await {
        Ok(()) => {
            app.apply_thread_name(&thread_id, Some(name));
            app.close_thread_rename();
            app.message = Some("Thread renamed".into());
        }
        Err(error) => {
            app.message = Some(format!("Could not rename thread: {error}"));
        }
    }
}

fn validate_thread_name(input: &str) -> std::result::Result<String, String> {
    let name = input.trim();
    if name.is_empty() {
        return Err("Thread name cannot be empty".into());
    }
    if name.graphemes(true).count() > MAX_THREAD_NAME_CHARS {
        return Err(format!(
            "Thread names can be at most {MAX_THREAD_NAME_CHARS} characters"
        ));
    }
    Ok(name.to_owned())
}

async fn refresh_thread_names(app: &mut App, server: &Arc<AppServer>) {
    let thread_ids = match app.registered_thread_ids() {
        Ok(thread_ids) => thread_ids,
        Err(error) => {
            app.message = Some(format!("Could not list threads for name refresh: {error}"));
            return;
        }
    };
    let results = futures::stream::iter(thread_ids.into_iter().map(|thread_id| {
        let server = Arc::clone(server);
        async move {
            let result = server.read_thread_name(&thread_id).await;
            (thread_id, result)
        }
    }))
    .buffer_unordered(16)
    .collect::<Vec<_>>()
    .await;
    let mut first_error = None;
    for (thread_id, result) in results {
        match result {
            Ok(name) => app.apply_thread_name(&thread_id, name),
            Err(error) if is_missing_thread_error(&error) => {}
            Err(error) => {
                first_error.get_or_insert_with(|| error.to_string());
            }
        };
    }
    if let Some(error) = first_error {
        app.message = Some(format!("Could not refresh some thread names: {error}"));
    }
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
            app.open_thread_picker();
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
        Some(PaletteEntry::Command(PaletteCommand::Permissions)) => {
            app.open_permissions_picker();
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
        Some(PaletteEntry::Command(PaletteCommand::SidePromote)) => {
            let target = app
                .chat()
                .map(|chat| (chat.thread_id.clone(), chat.title.clone()));
            app.message = Some(match target {
                Some((thread_id, title)) => {
                    match server.set_thread_name(&thread_id, &title).await {
                        Ok(()) => {
                            app.apply_thread_name(&thread_id, Some(title));
                            match app.promote_side_chat() {
                                Ok((title, None)) => {
                                    format!("Promoted '{title}' to a persistent thread")
                                }
                                Ok((title, Some(warning))) => format!(
                                    "Promoted '{title}', but temporary state cleanup failed: {warning}"
                                ),
                                Err(error) => format!("Could not promote side chat: {error}"),
                            }
                        }
                        Err(error) => format!("Could not name side chat before promotion: {error}"),
                    }
                }
                None => "Could not promote side chat: no side chat selected".into(),
            });
        }
        Some(PaletteEntry::Command(PaletteCommand::Attention)) => {
            app.open_attention();
        }
        Some(PaletteEntry::Command(PaletteCommand::Status)) => {
            let execution_status = execution_status(app.execution_mode);
            if let Some(chat) = app.chat_mut() {
                chat.push_notice(format!(
                    "Thread: {}\nWorkspace: {}\nModel: {} ({})\nPermissions: {execution_status}",
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
    let (side_thread_id, history) = match server
        .fork_thread(&parent_thread_id, &cwd, false, app.execution_mode)
        .await
    {
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
    app.mark_thread_opened(side_thread_id.clone());
    if let Err(error) = app.show_side_chat(parent_thread_id, side_chat) {
        let cleanup = server.delete_thread(&side_thread_id).await;
        app.forget_thread_subscription(&side_thread_id);
        app.message = Some(match cleanup {
            Ok(()) => format!("Could not track side chat: {error}"),
            Err(cleanup_error) => format!(
                "Could not track side chat: {error}; persistent thread cleanup also failed: {cleanup_error}"
            ),
        });
        return Ok(());
    }
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
        return Ok(());
    }
    let Some(thread_id) = app.side_chat().map(|chat| chat.thread_id.clone()) else {
        return Ok(());
    };
    if let Err(error) = delete_temporary_thread(server, &thread_id).await {
        app.message = Some(format!("Could not delete side chat: {error}"));
        return Ok(());
    }
    if let Err(error) = app.complete_side_chat_deletion(&thread_id) {
        app.message = Some(format!(
            "Side chat was deleted, but local cleanup failed: {error}"
        ));
    }
    Ok(())
}

async fn cleanup_abandoned_side_chats(app: &mut App, server: &Arc<AppServer>) {
    let thread_ids = match app.abandoned_side_chat_ids() {
        Ok(thread_ids) => thread_ids,
        Err(error) => {
            app.message = Some(format!("Could not inspect abandoned side chats: {error}"));
            return;
        }
    };
    let mut cleaned = 0;
    let mut failures = Vec::new();
    for thread_id in thread_ids {
        if let Err(error) = delete_temporary_thread(server, &thread_id).await {
            failures.push(format!("{thread_id}: {error}"));
            continue;
        }
        match app.forget_temporary_side_chat(&thread_id) {
            Ok(()) => cleaned += 1,
            Err(error) => failures.push(format!("{thread_id}: {error}")),
        }
    }
    if !failures.is_empty() {
        app.message = Some(format!(
            "Cleaned {cleaned} abandoned side chat(s); {} cleanup(s) failed",
            failures.len()
        ));
    } else if cleaned > 0 {
        app.message = Some(format!("Cleaned {cleaned} abandoned side chat(s)"));
    }
}

async fn cleanup_open_side_chats(app: &mut App, server: &Arc<AppServer>) -> bool {
    let mut failures = Vec::new();
    for (thread_id, turn_id) in app.side_chat_cleanup_targets() {
        if let Some(turn_id) = turn_id
            && let Err(error) = server.interrupt_turn(&thread_id, &turn_id).await
        {
            failures.push(format!("{thread_id}: {error}"));
            continue;
        }
        if let Err(error) = delete_temporary_thread(server, &thread_id).await {
            failures.push(format!("{thread_id}: {error}"));
            continue;
        }
        if let Err(error) = app.complete_side_chat_deletion(&thread_id) {
            failures.push(format!("{thread_id}: local cleanup failed: {error}"));
        }
    }
    if failures.is_empty() {
        true
    } else {
        app.message = Some(format!(
            "Could not clean up {} side chat(s); quit cancelled",
            failures.len()
        ));
        false
    }
}

async fn prepare_to_quit(app: &mut App, server: &Arc<AppServer>) -> bool {
    if !cleanup_open_side_chats(app, server).await {
        return false;
    }
    interrupt_owned_turns(app, server).await
}

async fn interrupt_owned_turns(app: &mut App, server: &Arc<AppServer>) -> bool {
    let mut failures = Vec::new();
    for (thread_id, turn_id) in app.owned_turn_targets() {
        match server.interrupt_turn(&thread_id, &turn_id).await {
            Ok(()) => app.forget_owned_turn(&thread_id, &turn_id),
            Err(error) if is_inactive_turn_error(&error) => {
                app.forget_owned_turn(&thread_id, &turn_id);
            }
            Err(error) => failures.push(format!("{thread_id}: {error}")),
        }
    }
    if failures.is_empty() {
        true
    } else {
        app.message = Some(format!(
            "Could not stop {} response(s); quit cancelled",
            failures.len()
        ));
        false
    }
}

fn is_inactive_turn_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("no active turn")
        || message.contains("turn not found")
        || message.contains("turn is not active")
}

async fn delete_temporary_thread(server: &Arc<AppServer>, thread_id: &str) -> Result<()> {
    match server.delete_thread(thread_id).await {
        Ok(()) => Ok(()),
        Err(error) if is_missing_thread_error(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn spawn_thread_deletion(
    server: Arc<AppServer>,
    record: ThreadRecord,
    sender: mpsc::UnboundedSender<ThreadDeletionEvent>,
) {
    tokio::spawn(async move {
        let thread_id = record.id.clone();
        let result = perform_thread_deletion(&server, &record, &sender)
            .await
            .map_err(|error| error.to_string());
        let _ = sender.send(ThreadDeletionEvent::Finished { thread_id, result });
    });
}

async fn perform_thread_deletion(
    server: &Arc<AppServer>,
    record: &ThreadRecord,
    sender: &mpsc::UnboundedSender<ThreadDeletionEvent>,
) -> Result<()> {
    if record.managed_worktree && record.cwd.is_dir() {
        let cwd = record.cwd.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            anyhow::ensure!(
                git_workspace::workspace_is_clean(&cwd)?,
                "worktree has changes; restore the thread and clean it before deleting"
            );
            Ok(())
        })
        .await
        .context("check managed worktree task")??;
    }

    let _ = sender.send(ThreadDeletionEvent::Phase(
        ThreadDeletionPhase::DeletingHistory,
    ));
    match server.delete_thread(&record.id).await {
        Ok(()) => {}
        Err(error) if is_missing_thread_history_error(&error) => {}
        Err(error) => return Err(error),
    }

    if record.managed_worktree && record.cwd.is_dir() {
        let _ = sender.send(ThreadDeletionEvent::Phase(
            ThreadDeletionPhase::RemovingWorktree,
        ));
        let repository_path = record.repository_path.clone();
        let workspace_path = record.cwd.clone();
        let branch = record.worktree_branch.clone();
        tokio::task::spawn_blocking(move || {
            git_workspace::remove_managed_workspace(
                &repository_path,
                &workspace_path,
                branch.as_deref(),
            )
        })
        .await
        .context("remove managed worktree task")??;
    }
    Ok(())
}

async fn cleanup_unused_main_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    let Some(thread_id) = app.unused_main_chat_cleanup_target()? else {
        return Ok(());
    };
    delete_temporary_thread(server, &thread_id).await?;
    app.remove_unused_main_chat(&thread_id)
}

fn is_missing_thread_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("no rollout found for thread id")
        || message.contains("thread not found")
        || message.contains("thread not loaded:")
        || (message.contains("thread ") && message.contains(" not found"))
}

fn is_missing_thread_history_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("no rollout found for thread id")
        || message.contains("thread not found")
        || (message.contains("thread ") && message.contains(" not found"))
}

fn is_recoverable_empty_thread(title: &str, error: &anyhow::Error) -> bool {
    title == "Untitled thread"
        && (is_missing_thread_error(error) || is_unavailable_thread_read_error(error))
}

fn is_unavailable_thread_read_error(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    let Some(payload) = message.strip_prefix("Codex thread/read error: ") else {
        return false;
    };
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(payload) else {
        return false;
    };
    payload.get("code").and_then(serde_json::Value::as_i64) == Some(-32600)
        && payload
            .get("message")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|message| message.starts_with("thread "))
}

async fn interrupt_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    let Some((thread_id, turn_id)) = app.chat().and_then(|chat| {
        chat.active_turn_id
            .as_ref()
            .map(|turn_id| (chat.thread_id.clone(), turn_id.clone()))
    }) else {
        return Ok(());
    };
    match server.interrupt_turn(&thread_id, &turn_id).await {
        Ok(()) => {
            if let Some(chat) = app.chat_mut() {
                chat.mark_interrupt_requested();
            }
        }
        Err(error) => {
            if let Some(chat) = app.chat_mut() {
                chat.push_notice(format!("✗ Could not stop response: {error}"));
            }
        }
    }
    Ok(())
}

async fn open_new_chat(app: &mut App, server: &Arc<AppServer>, workspace: Workspace) -> Result<()> {
    let model_settings = app.default_model_settings();
    let thread_id = server
        .start_thread(
            &workspace.path,
            model_settings.as_ref().map(|(model, _, _)| model.as_str()),
            app.execution_mode,
        )
        .await?;
    app.register_app_server_thread(thread_id.clone(), workspace.path.clone())?;
    app.mark_thread_opened(thread_id.clone());
    let mut chat = ChatState::new(thread_id, workspace.path, "Untitled thread".into());
    if let Some((model, display_name, effort)) = model_settings {
        chat.set_model(model, display_name, effort);
    }
    app.show_chat(chat);
    app.focus = Focus::Chat;
    app.mode = Mode::Chat;
    Ok(())
}

async fn handle_dynamic_tool_call(
    app: &mut App,
    server: &Arc<AppServer>,
    request: AppServerRequest,
) -> Result<()> {
    let response = if is_start_thread_tool(&request) {
        match start_thread_from_tool(app, server, &request).await {
            Ok(started) => dynamic_tool_response(
                true,
                format!(
                    "Started independent thread.\nThread ID: {}\nTitle: {}\nWorkspace: {}",
                    started.id,
                    started.title,
                    started.workspace.display()
                ),
            ),
            Err(error) => dynamic_tool_response(false, format!("Could not start thread: {error}")),
        }
    } else {
        dynamic_tool_response(false, "Unsupported Shikigami tool".into())
    };
    server.respond(request.id, response).await
}

fn is_start_thread_tool(request: &AppServerRequest) -> bool {
    let namespace = request.params.get("namespace").and_then(Value::as_str);
    let tool = request.params.get("tool").and_then(Value::as_str);
    request.method == "item/tool/call"
        && ((namespace == Some("shikigami") && tool == Some("start_thread"))
            || tool == Some("shikigami.start_thread"))
}

fn dynamic_tool_response(success: bool, text: String) -> Value {
    json!({
        "contentItems": [{"type": "inputText", "text": text}],
        "success": success
    })
}

async fn start_thread_from_tool(
    app: &mut App,
    server: &Arc<AppServer>,
    request: &AppServerRequest,
) -> Result<StartedThread> {
    let source_thread_id = request
        .thread_id
        .as_deref()
        .context("tool request did not identify its source thread")?;
    let arguments: StartThreadToolArguments = serde_json::from_value(
        request
            .params
            .get("arguments")
            .cloned()
            .context("tool request did not include arguments")?,
    )
    .context("invalid start_thread arguments")?;
    let prompt = arguments.prompt.trim();
    anyhow::ensure!(!prompt.is_empty(), "prompt cannot be empty");
    let create_worktree = matches!(arguments.workspace, StartThreadWorkspace::NewWorktree);
    let (repository, workspace) =
        app.prepare_thread_workspace(source_thread_id, create_worktree)?;
    let model_settings = app.default_model_settings();
    let thread_id = match server
        .start_thread(
            &workspace.path,
            model_settings.as_ref().map(|(model, _, _)| model.as_str()),
            app.execution_mode,
        )
        .await
    {
        Ok(thread_id) => thread_id,
        Err(error) => {
            let cleanup =
                cleanup_created_tool_workspace(&repository.path, &workspace, create_worktree);
            return Err(with_cleanup_error(error, cleanup));
        }
    };
    if let Err(error) = app.register_app_server_thread_in_repository(
        thread_id.clone(),
        repository.path.clone(),
        workspace.path.clone(),
    ) {
        let server_cleanup = delete_temporary_thread(server, &thread_id).await.err();
        let workspace_cleanup =
            cleanup_created_tool_workspace(&repository.path, &workspace, create_worktree).err();
        return Err(with_cleanup_errors(
            error,
            server_cleanup,
            workspace_cleanup,
        ));
    }
    app.mark_thread_opened(thread_id.clone());
    let mut chat = ChatState::new(
        thread_id.clone(),
        workspace.path.clone(),
        "Untitled thread".into(),
    );
    if let Some((model, display_name, effort)) = model_settings {
        chat.set_model(model, display_name, effort);
    }
    app.add_background_chat(chat);
    let model = app.chats[&thread_id].model.clone();
    let effort = app.chats[&thread_id].reasoning_effort.clone();
    let turn_id = match server
        .start_turn(
            &thread_id,
            &workspace.path,
            prompt,
            &[],
            TurnSettings {
                model: model.as_deref(),
                effort: effort.as_deref(),
                execution_mode: app.execution_mode,
            },
        )
        .await
    {
        Ok(turn_id) => turn_id,
        Err(error) => {
            return Err(
                rollback_tool_thread(app, server, &thread_id, create_worktree, error).await,
            );
        }
    };
    app.record_owned_turn(thread_id.clone(), turn_id.clone());
    if let Some(chat) = app.chats.get_mut(&thread_id) {
        chat.begin_user_turn(prompt.to_owned(), turn_id);
    }
    let title = prompt_title(prompt);
    if let Err(error) = server.set_thread_name(&thread_id, &title).await {
        app.message = Some(format!(
            "Thread started, but it could not be named: {error}"
        ));
    } else {
        app.apply_thread_name(&thread_id, Some(title.clone()));
    }
    Ok(StartedThread {
        id: thread_id,
        title,
        workspace: workspace.path,
    })
}

fn prompt_title(prompt: &str) -> String {
    prompt
        .lines()
        .next()
        .unwrap_or(prompt)
        .chars()
        .take(80)
        .collect()
}

fn cleanup_created_tool_workspace(
    repository_path: &Path,
    workspace: &Workspace,
    created: bool,
) -> Result<()> {
    if !created || !workspace.path.is_dir() {
        return Ok(());
    }
    let branch = git_workspace::current_branch(&workspace.path)?;
    git_workspace::remove_managed_workspace(repository_path, &workspace.path, branch.as_deref())
}

fn with_cleanup_error(error: anyhow::Error, cleanup: Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => anyhow!("{error}; workspace cleanup also failed: {cleanup_error}"),
    }
}

fn with_cleanup_errors(
    error: anyhow::Error,
    server_cleanup: Option<anyhow::Error>,
    local_cleanup: Option<anyhow::Error>,
) -> anyhow::Error {
    let mut message = error.to_string();
    if let Some(cleanup) = server_cleanup {
        message.push_str(&format!("; Codex cleanup also failed: {cleanup}"));
    }
    if let Some(cleanup) = local_cleanup {
        message.push_str(&format!("; local cleanup also failed: {cleanup}"));
    }
    anyhow!(message)
}

async fn rollback_tool_thread(
    app: &mut App,
    server: &Arc<AppServer>,
    thread_id: &str,
    remove_workspace: bool,
    error: anyhow::Error,
) -> anyhow::Error {
    let server_cleanup = delete_temporary_thread(server, thread_id).await.err();
    let local_cleanup = app
        .rollback_unstarted_thread(thread_id, remove_workspace)
        .err();
    with_cleanup_errors(error, server_cleanup, local_cleanup)
}

fn schedule_selected_chat_preview(
    app: &mut App,
    server: &Arc<AppServer>,
    generation: &Arc<AtomicU64>,
    sender: &mpsc::UnboundedSender<ChatPreview>,
    preview_task: &mut Option<JoinHandle<()>>,
) {
    cancel_chat_preview(generation, preview_task);
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
    *preview_task = Some(tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if generation.load(Ordering::Relaxed) != current_generation {
            return;
        }
        let mut chat = ChatState::new(thread.record.id, thread.record.cwd, thread.record.title);
        if let Some((model, display_name, effort)) = model_settings {
            chat.set_model(model, display_name, effort);
        }
        let result = match server
            .read_thread_preview(&chat.thread_id, PREVIEW_TURN_LIMIT)
            .await
        {
            Ok(history) => {
                chat.load_history(&history);
                chat.mark_history_partial();
                Ok(chat)
            }
            Err(error) if is_recoverable_empty_thread(&chat.title, &error) => Ok(chat),
            Err(error) => Err(format!("Could not load thread: {error}")),
        };
        if generation.load(Ordering::Relaxed) == current_generation {
            let _ = sender.send(ChatPreview {
                generation: current_generation,
                result,
            });
        }
    }));
}

fn cancel_chat_preview(generation: &Arc<AtomicU64>, preview_task: &mut Option<JoinHandle<()>>) {
    generation.fetch_add(1, Ordering::Relaxed);
    if let Some(task) = preview_task.take() {
        task.abort();
    }
}

async fn load_selected_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    if !app.selected_tree_is_thread() {
        return Ok(());
    }
    let Some(thread) = app.selected_thread().cloned() else {
        return Ok(());
    };
    if app.show_cached_chat(&thread.record.id)
        && app.chat().is_some_and(ChatState::history_is_complete)
    {
        return Ok(());
    }
    let thread_id = thread.record.id;
    let history = match server.read_thread(&thread_id).await {
        Ok(history) => Some(history),
        Err(error) if is_recoverable_empty_thread(&thread.record.title, &error) => None,
        Err(error) => return Err(error),
    };
    if let Some(history) = history.as_ref() {
        app.apply_thread_name(
            &thread_id,
            history
                .pointer("/thread/name")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
    }
    if let Some(chat) = app.chats.get_mut(&thread_id) {
        if let Some(history) = history {
            chat.load_history(&history);
        }
        app.mark_chat_history_complete(&thread_id);
        return Ok(());
    }
    let mut chat = ChatState::new(thread_id, thread.record.cwd, thread.record.title);
    if let Some((model, display_name, effort)) = app.default_model_settings() {
        chat.set_model(model, display_name, effort);
    }
    if let Some(history) = history {
        chat.load_history(&history);
    }
    app.show_chat(chat);
    Ok(())
}

async fn focus_selected_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    load_selected_chat(app, server).await?;
    let Some(chat) = app.chat() else {
        return Ok(());
    };
    let thread_id = chat.thread_id.clone();
    let cwd = chat.cwd.clone();
    let model = chat.model.clone();
    let title = chat.title.clone();
    if !app.resumed_threads.contains(&thread_id) {
        match server
            .resume_thread(&thread_id, &cwd, model.as_deref(), app.execution_mode)
            .await
        {
            Ok(()) => {
                app.mark_thread_opened(thread_id.clone());
                app.set_thread_read_only(&thread_id, false);
            }
            Err(error) if is_active_writer_conflict(&error) => {
                app.set_thread_read_only(&thread_id, true);
                app.message = Some(
                    "Opened read-only because another Codex session owns this thread; close it and use /threads to retry"
                        .into(),
                );
            }
            Err(error) if is_recoverable_empty_thread(&title, &error) => {
                let replacement_id = server
                    .start_thread(&cwd, model.as_deref(), app.execution_mode)
                    .await?;
                if let Err(error) = app.replace_empty_thread_id(&thread_id, replacement_id.clone())
                {
                    let _ = server.unsubscribe_thread(&replacement_id).await;
                    return Err(error);
                }
                app.mark_thread_opened(replacement_id.clone());
                app.set_thread_read_only(&replacement_id, false);
            }
            Err(error) => return Err(error),
        }
    }
    app.focus = Focus::Chat;
    app.mode = Mode::Chat;
    Ok(())
}

async fn submit_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    let Some(chat) = app.chat() else {
        return Ok(());
    };
    if app.active_chat_is_read_only() {
        app.message =
            Some("Read-only: close the other Codex session, then use /threads to retry".into());
        return Ok(());
    }
    if chat.composer.trim().is_empty() {
        return Ok(());
    }
    let prompt = chat.composer.clone();
    let thread_id = chat.thread_id.clone();
    let active_turn_id = chat.active_turn_id.clone();
    let cwd = chat.cwd.clone();
    let model = chat.model.clone();
    let effort = chat.reasoning_effort.clone();
    let skills = chat.skills_for_prompt(&prompt);
    if let Some(turn_id) = active_turn_id {
        if let Err(error) = server
            .steer_turn(&thread_id, &turn_id, &prompt, &skills)
            .await
        {
            app.message = Some(format!("Could not send follow-up: {error}"));
            return Ok(());
        }
        if let Some(chat) = app.chat_mut() {
            chat.clear_composer();
            chat.steer_submitted(prompt);
        }
        return Ok(());
    }
    let turn_id = server
        .start_turn(
            &thread_id,
            &cwd,
            &prompt,
            &skills,
            TurnSettings {
                model: model.as_deref(),
                effort: effort.as_deref(),
                execution_mode: app.execution_mode,
            },
        )
        .await?;
    app.record_owned_turn(thread_id.clone(), turn_id.clone());
    if let Some(chat) = app.chat_mut() {
        let first_side_message = chat.is_side_chat && !chat.side_chat_has_activity;
        chat.clear_composer();
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
    if app.thread_is_registered(&thread_id) && !app.thread_has_name(&thread_id) {
        let name = prompt_title(&prompt);
        match server.set_thread_name(&thread_id, &name).await {
            Ok(()) => app.apply_thread_name(&thread_id, Some(name)),
            Err(error) => app.message = Some(format!("Message sent, but naming failed: {error}")),
        }
    }
    Ok(())
}

async fn reconcile_thread_subscriptions(app: &mut App, server: &Arc<AppServer>) {
    let targets = app.thread_subscription_targets();
    let mut releases = app
        .resumed_threads
        .difference(&targets)
        .cloned()
        .collect::<Vec<_>>();
    releases.sort();
    for thread_id in releases {
        match server.unsubscribe_thread(&thread_id).await {
            Ok(()) => app.mark_thread_unsubscribed(&thread_id),
            Err(error) => {
                app.message = Some(format!(
                    "Could not release background thread {thread_id}: {error}"
                ));
            }
        }
    }

    let mut acquisitions = targets
        .difference(&app.resumed_threads)
        .filter(|thread_id| !app.read_only_threads.contains(thread_id.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    acquisitions.sort();
    for thread_id in acquisitions {
        let Some((cwd, model)) = app
            .chats
            .get(&thread_id)
            .map(|chat| (chat.cwd.clone(), chat.model.clone()))
        else {
            continue;
        };
        match server
            .resume_thread(&thread_id, &cwd, model.as_deref(), app.execution_mode)
            .await
        {
            Ok(()) => {
                app.mark_thread_opened(thread_id.clone());
                app.set_thread_read_only(&thread_id, false);
                match server.read_thread(&thread_id).await {
                    Ok(history) => {
                        if let Some(chat) = app.chats.get_mut(&thread_id) {
                            chat.load_history(&history);
                        }
                    }
                    Err(error) => {
                        app.message = Some(format!(
                            "Reopened thread {thread_id}, but could not refresh it: {error}"
                        ));
                    }
                }
            }
            Err(error) if is_active_writer_conflict(&error) => {
                app.set_thread_read_only(&thread_id, true);
                app.message = Some(format!(
                    "Thread {thread_id} is now read-only because another Codex session owns it"
                ));
            }
            Err(error) => {
                app.message = Some(format!("Could not reopen thread {thread_id}: {error}"));
            }
        }
    }
}

async fn resync_subscribed_threads(app: &mut App, server: &Arc<AppServer>, skipped: u64) {
    let mut thread_ids = app.resumed_threads.iter().cloned().collect::<Vec<_>>();
    thread_ids.sort();
    let reads = thread_ids
        .iter()
        .map(|thread_id| server.read_thread(thread_id));
    let results = join_all(reads).await;
    let mut refreshed = 0usize;
    let mut failures = Vec::new();

    for (thread_id, result) in thread_ids.into_iter().zip(results) {
        match result {
            Ok(history) => {
                let Some(chat) = app.chats.get_mut(&thread_id) else {
                    failures.push(format!("{thread_id}: thread is not cached"));
                    continue;
                };
                chat.load_history(&history);
                app.reconcile_owned_turn_after_resync(&thread_id);
                refreshed += 1;
            }
            Err(error) => failures.push(format!("{thread_id}: {error}")),
        }
    }

    app.message = Some(if failures.is_empty() {
        format!(
            "Recovered after skipping {skipped} App Server events; refreshed {refreshed} threads"
        )
    } else {
        format!(
            "Skipped {skipped} App Server events; refreshed {refreshed} threads, but could not refresh {}",
            failures.join("; ")
        )
    });
}

async fn unsubscribe_all_threads(app: &mut App, server: &Arc<AppServer>) {
    let thread_ids = app.resumed_threads.drain().collect::<Vec<_>>();
    let requests = thread_ids
        .iter()
        .map(|thread_id| server.unsubscribe_thread(thread_id));
    let _ = tokio::time::timeout(Duration::from_secs(1), futures::future::join_all(requests)).await;
}

fn is_active_writer_conflict(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("thread-store conflict") && message.contains("active writer")
}

fn execution_status(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Auto => "AUTO · workspace-write · approvals on-request",
        ExecutionMode::Dangerous => "DANGEROUS · danger-full-access · approvals never",
    }
}

fn is_approval(request: &AppServerRequest) -> bool {
    matches!(
        request.method.as_str(),
        "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
    )
}

async fn resolve_active_chat_approval(
    app: &mut App,
    server: &Arc<AppServer>,
    accept: bool,
) -> Result<()> {
    let Some(request) = app.take_active_chat_approval() else {
        return Ok(());
    };
    respond_to_approval(app, server, request, accept).await
}

async fn resolve_unscoped_approval(
    app: &mut App,
    server: &Arc<AppServer>,
    accept: bool,
) -> Result<()> {
    let Some(request) = app.take_unscoped_pending_approval() else {
        app.mode = if app.chat().is_some() && app.focus == Focus::Chat {
            Mode::Chat
        } else {
            Mode::Normal
        };
        return Ok(());
    };
    respond_to_approval(app, server, request, accept).await?;
    app.mode = if app.unscoped_pending_approval().is_some() {
        Mode::Approval
    } else if app.chat().is_some() && app.focus == Focus::Chat {
        Mode::Chat
    } else {
        Mode::Normal
    };
    Ok(())
}

async fn respond_to_approval(
    app: &mut App,
    server: &Arc<AppServer>,
    request: AppServerRequest,
    accept: bool,
) -> Result<()> {
    let thread_id = request.thread_id.clone();
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
    app.approval_resolved(thread_id.as_deref());
    Ok(())
}

fn render(frame: &mut Frame, app: &mut App, render_cache: &mut RenderCache) {
    render_cache.chats.retain(|thread_id, _| {
        app.visible_chat_id.as_deref() == Some(thread_id)
            || app.side_chat_id.as_deref() == Some(thread_id)
    });
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(area);
    let read_only = app.active_chat_is_read_only();
    let chat_status = app.chat().map(|chat| {
        let state = if read_only {
            "read-only"
        } else if chat.active_turn_id.is_some() {
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
        " Shikigami ",
        Style::default().add_modifier(Modifier::BOLD),
    )];
    if read_only {
        header.push(Span::styled(
            " READ ONLY ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        header.push(Span::raw(" "));
    } else if app.chat().is_some() {
        let (label, color) = match app.execution_mode {
            ExecutionMode::Auto => (" AUTO ", Color::Green),
            ExecutionMode::Dangerous => (" DANGEROUS ", Color::Red),
        };
        header.push(Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        header.push(Span::raw(" "));
    }
    if app.attention_count() > 0 {
        header.push(Span::styled(
            format!(" ATTENTION {} ", app.attention_count()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
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
    render_chat_area(frame, panes[1], app, render_cache);

    let default_status = if app.attention_count() > 0 {
        format!(
            "! attention ({}) · j/k move/preview · Enter focus · / search · n new · q quit",
            app.attention_count()
        )
    } else {
        "? help · j/k move/preview · h/l collapse/expand · Enter focus · / search · n new · q quit"
            .into()
    };
    let status = app.message.as_deref().unwrap_or(&default_status);
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
        Mode::ConfirmDeleteThread => render_thread_delete_confirm(frame, area, app),
        Mode::DeletingThread => render_thread_deletion_progress(frame, area, app),
        Mode::ChooseModel => render_model_picker(frame, area, app),
        Mode::ChooseReasoningEffort => render_reasoning_effort_picker(frame, area, app),
        Mode::ChoosePermissions => render_permissions_picker(frame, area, app),
        Mode::ConfirmDangerous => render_dangerous_confirm(frame, area),
        Mode::ChooseSideChat => render_side_chat_picker(frame, area, app),
        Mode::ChooseThread => render_thread_picker(frame, area, app),
        Mode::RenameThread => render_thread_rename(frame, area, app),
        Mode::Attention => render_attention(frame, area, app),
        Mode::ConfirmQuit => render_quit_confirm(frame, area, app),
        Mode::Help => render_help(frame, area, app),
        Mode::Normal | Mode::Chat | Mode::Approval => {}
    }
    if app.mode == Mode::Approval
        && let Some(request) = app.unscoped_pending_approval()
    {
        let prompt = approval_prompt("Codex", request);
        render_approval(frame, area, &prompt, true);
    }
    if app.thread_deletion.is_some() && app.mode != Mode::DeletingThread {
        render_thread_deletion_progress(frame, area, app);
    }
}

fn render_chat_area(frame: &mut Frame, area: Rect, app: &mut App, render_cache: &mut RenderCache) {
    if app.has_side_chat() {
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        render_chat_pane(frame, panes[0], app, ChatPane::Main, render_cache);
        render_chat_pane(frame, panes[1], app, ChatPane::Side, render_cache);
    } else {
        render_chat_pane(frame, area, app, ChatPane::Main, render_cache);
    }
}

fn render_chat_pane(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    pane: ChatPane,
    render_cache: &mut RenderCache,
) {
    let pane_active = app.active_chat_pane == pane;
    let chat_focused = app.focus == Focus::Chat && pane_active;
    let has_side_chat = app.has_side_chat();
    let side_chat_position = app.current_side_chat_position();
    let chat_id = match pane {
        ChatPane::Main => app.visible_chat_id.clone(),
        ChatPane::Side => app.side_chat_id.clone(),
    };
    let read_only = chat_id
        .as_deref()
        .is_some_and(|thread_id| app.read_only_threads.contains(thread_id));
    let show_composer_cursor =
        app.mode == Mode::Chat && app.focus == Focus::Chat && pane_active && !read_only;
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
    let approval = app.pending_approval_for_thread(&chat_id).map(|request| {
        let title = app
            .chats
            .get(&chat_id)
            .map(|chat| chat.title.as_str())
            .unwrap_or("thread");
        approval_prompt(title, request)
    });
    let Some(chat) = app.chats.get_mut(&chat_id) else {
        return;
    };
    let pending_prompts = chat.pending_steer_prompts();
    let pending_lines = pending_follow_up_lines(
        &pending_prompts,
        area.width.saturating_sub(3).max(1) as usize,
    );
    let pending_height_limit = area.height.saturating_sub(10);
    let show_pending = !pending_lines.is_empty() && pending_height_limit >= 3;
    let pending_height = u16::try_from(pending_lines.len().saturating_add(2))
        .unwrap_or(u16::MAX)
        .min(pending_height_limit);
    let mut constraints = vec![Constraint::Min(5)];
    if show_pending {
        constraints.push(Constraint::Length(pending_height));
    }
    constraints.extend([Constraint::Length(4), Constraint::Length(1)]);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let chat_area = chunks[0];
    let pending_area = show_pending.then_some(chunks[1]);
    let message_index = usize::from(show_pending) + 1;
    let message_area = chunks[message_index];
    let help_area = chunks[message_index + 1];
    let visible_height = chat_area.height.saturating_sub(2).max(1) as usize;
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
    let text_width = chat_block.inner(chat_area).width.max(1);
    let RenderedChat {
        lines,
        height,
        selected_range,
        editor_targets,
    } = rendered_chat_cached(
        chat,
        text_width as usize,
        render_cache.chats.entry(chat_id).or_default(),
    );
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let paragraph = paragraph.block(chat_block);
    chat.update_scroll_metrics(height, visible_height);
    if chat.take_message_selection_scroll_request()
        && let Some((start, end)) = selected_range
    {
        chat.reveal_line_range(start, end);
    }
    chat.visible_editor_target =
        visible_editor_target(editor_targets, chat.scroll_top, visible_height);
    let scroll = u16::try_from(chat.scroll_top).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((scroll, 0)), chat_area);
    if let Some(pending_area) = pending_area {
        let pending_block = Block::default()
            .title(pending_follow_up_title(pending_prompts.len()))
            .borders(Borders::ALL)
            .padding(Padding::right(1))
            .border_style(Style::default().fg(Color::Yellow));
        let pending_visible_height = pending_block.inner(pending_area).height.max(1) as usize;
        let pending_scroll = pending_lines.len().saturating_sub(pending_visible_height);
        frame.render_widget(
            Paragraph::new(Text::from(
                pending_lines
                    .iter()
                    .map(|line| Line::from(line.clone()))
                    .collect::<Vec<_>>(),
            ))
            .scroll((u16::try_from(pending_scroll).unwrap_or(u16::MAX), 0))
            .block(pending_block),
            pending_area,
        );
    }
    let pending_steers = chat.pending_steer_count();
    let stopping = chat.interrupt_is_requested();
    let has_live_status = pending_steers > 0 || stopping;
    let message_border = if has_live_status && !read_only {
        Color::Yellow
    } else if chat.mode == ChatMode::Input && chat_focused && !read_only {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let message_block = Block::default()
        .title(composer_title(read_only, pending_steers, stopping))
        .borders(Borders::ALL)
        .padding(Padding::right(1))
        .border_style(Style::default().fg(message_border));
    let message_inner = message_block.inner(message_area);
    let composer_width = message_inner.width.max(1) as usize;
    chat.set_composer_width(composer_width);
    let (composer_lines, cursor_line, cursor_column) = if read_only {
        (
            wrap_composer(
                "Another Codex session owns this thread. Use /threads to retry.",
                composer_width,
            ),
            0,
            0,
        )
    } else {
        let layout = chat.composer_layout();
        (layout.lines, layout.cursor_line, layout.cursor_column)
    };
    let composer_height = message_inner.height.max(1) as usize;
    let composer_scroll = cursor_line
        .saturating_add(1)
        .saturating_sub(composer_height)
        .min(composer_lines.len().saturating_sub(composer_height));
    frame.render_widget(
        Paragraph::new(Text::from(
            composer_lines
                .iter()
                .map(|line| Line::from(line.clone()))
                .collect::<Vec<_>>(),
        ))
        .scroll((u16::try_from(composer_scroll).unwrap_or(u16::MAX), 0))
        .block(message_block),
        message_area,
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
        let visible_cursor_line = cursor_line.saturating_sub(composer_scroll);
        let y = message_inner
            .y
            .saturating_add(u16::try_from(visible_cursor_line).unwrap_or(u16::MAX))
            .min(message_inner.bottom().saturating_sub(1));
        frame.set_cursor_position((x, y));
    }
    let help = chat_help(
        read_only,
        chat.mode,
        has_side_chat,
        chat.active_turn_id.is_some(),
    );
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        help_area,
    );
    if pane_active && let Some(palette) = &chat.palette {
        render_command_palette(frame, area, palette);
    }
    if let Some(approval) = approval {
        render_approval(
            frame,
            area,
            &approval,
            app.mode == Mode::Chat && chat_focused,
        );
    }
}

fn chat_help(read_only: bool, mode: ChatMode, has_side_chat: bool, active_turn: bool) -> String {
    let controls = if read_only {
        "READ ONLY · / palette · /threads retry · Tab scroll · Esc threads".into()
    } else {
        match (mode, has_side_chat) {
            (ChatMode::Input, true) => format!(
                "INPUT · Shift-Enter newline · Ctrl-G pane · Ctrl-N/P side · Enter {}",
                if active_turn { "steer" } else { "send" }
            ),
            (ChatMode::Input, false) => format!(
                "INPUT · Shift-Enter newline · Enter {} · / palette · Ctrl-R effort · Ctrl-U clear · Tab scroll · Esc threads",
                if active_turn { "steer" } else { "send" }
            ),
            (ChatMode::Scroll, true) => {
                "SCROLL · j/k line · J/K msg · e editor cmd · y copy · i input".into()
            }
            (ChatMode::Scroll, false) => {
                "SCROLL · j/k line · J/K msg · e editor cmd · y/Y copy · u/d half · i input".into()
            }
        }
    };
    if active_turn {
        format!("Ctrl-C stop · {controls}")
    } else {
        controls
    }
}

fn composer_title(read_only: bool, pending_steers: usize, stopping: bool) -> String {
    if read_only {
        return " Read only ".into();
    }
    if stopping {
        return " Message · Stopping response… ".into();
    }
    match pending_steers {
        0 => " Message ".into(),
        1 => " Message · Follow-up sent · waiting for Codex… ".into(),
        count => format!(" Message · {count} follow-ups sent · waiting for Codex… "),
    }
}

fn pending_follow_up_title(count: usize) -> String {
    match count {
        1 => " Follow-up waiting ".into(),
        count => format!(" {count} follow-ups waiting "),
    }
}

fn pending_follow_up_lines(prompts: &[String], max_width: usize) -> Vec<String> {
    let content_width = max_width.saturating_sub(2).max(1);
    let mut lines = Vec::new();
    for prompt in prompts {
        let mut first_line = true;
        for logical_line in prompt.split('\n') {
            let mut wrapped_lines = wrap_composer(logical_line, content_width);
            if wrapped_lines.len() > 1 && wrapped_lines.last().is_some_and(String::is_empty) {
                wrapped_lines.pop();
            }
            for wrapped_line in wrapped_lines {
                let prefix = if first_line { "› " } else { "  " };
                lines.push(format!("{prefix}{wrapped_line}"));
                first_line = false;
            }
        }
    }
    lines
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

fn render_permissions_picker(frame: &mut Frame, area: Rect, app: &App) {
    let modes = [ExecutionMode::Auto, ExecutionMode::Dangerous];
    let items = modes.into_iter().map(|mode| {
        let current = if mode == app.execution_mode {
            " · current"
        } else {
            ""
        };
        let color = if mode == ExecutionMode::Dangerous {
            Color::Red
        } else {
            Color::Green
        };
        ListItem::new(Text::from(vec![
            Line::styled(
                format!("{}{}", mode.label(), current),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Line::styled(mode.description(), Style::default().fg(Color::DarkGray)),
        ]))
    });
    let popup = centered_rect(72, 8, area);
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Permissions ")
                .title_bottom(Line::from(" j/k select · Enter apply · Esc close "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_symbol("› ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    let mut state = ListState::default().with_selected(Some(app.permission_index.min(1)));
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_dangerous_confirm(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(76, 10, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(
            "Dangerous mode grants Codex full system access and disables approval prompts.\n\nEnable Dangerous mode? [y/N]",
        )
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" Enable Dangerous mode ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
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

fn render_thread_picker(frame: &mut Frame, area: Rect, app: &App) {
    let thread_count = app.thread_picker_matches.len();
    let height = u16::try_from(thread_count.saturating_mul(2).saturating_add(2))
        .unwrap_or(u16::MAX)
        .clamp(7, area.height.saturating_sub(4).max(7));
    let popup = centered_rect(78, height, area);
    let items = app.thread_picker_threads().map(|thread| {
        let repository = app.repository_name_for_thread(thread);
        let status = app.thread_picker_status(&thread.record.id);
        let color = match status {
            "working" => Color::Yellow,
            "attention" => Color::Red,
            "read-only" => Color::Yellow,
            "current" => Color::Green,
            _ => Color::Cyan,
        };
        ListItem::new(Text::from(vec![
            Line::styled(
                thread.record.title.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Line::styled(
                format!("{repository} · {} · {status}", thread.location_name),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    });
    let title = if app.thread_picker_query.is_empty() {
        format!(" Threads · {thread_count} ")
    } else {
        format!(" Threads · {} · {thread_count} ", app.thread_picker_query)
    };
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .title_bottom(Line::from(
                    " type filter · ↑/↓ · Enter open · R rename · y ID · Esc ",
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_symbol("› ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    let mut state = ListState::default().with_selected(
        (thread_count > 0).then_some(app.thread_picker_index.min(thread_count.saturating_sub(1))),
    );
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_thread_rename(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(64, 5, area);
    let count = app.rename_input.graphemes(true).count();
    let input = Paragraph::new(app.rename_input.as_str())
        .block(
            Block::default()
                .title(" Rename thread ")
                .title_bottom(Line::from(format!(
                    " {count}/{MAX_THREAD_NAME_CHARS} · Enter save · Esc cancel "
                )))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(Clear, popup);
    frame.render_widget(input, popup);
}

fn render_quit_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let side_chat_count = app.side_chat_count();
    let turn_count = app.owned_turn_count();
    let mut warnings = Vec::new();
    if turn_count > 0 {
        warnings.push(format!(
            "• {turn_count} running response{} will be stopped",
            if turn_count == 1 { "" } else { "s" }
        ));
    }
    if side_chat_count > 0 {
        warnings.push(format!(
            "• {side_chat_count} temporary side chat{} will be deleted",
            if side_chat_count == 1 { "" } else { "s" }
        ));
    }
    let popup = centered_rect(68, u16::try_from(warnings.len()).unwrap_or(2) + 6, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!("{}\n\nStop and quit? [y/N]", warnings.join("\n")))
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title(" Quit Shikigami ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
        popup,
    );
}

#[derive(Clone)]
struct RenderedMessage {
    lines: Vec<Line<'static>>,
    editor_targets: Vec<(usize, EditorTarget)>,
}

#[derive(Clone)]
struct CachedRenderedMessage {
    revision: u64,
    message: RenderedMessage,
    height: usize,
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
    height: usize,
    selected_range: Option<(usize, usize)>,
    editor_targets: Vec<(usize, EditorTarget)>,
}

#[cfg(test)]
fn rendered_chat(chat: &ChatState, available_width: usize) -> RenderedChat {
    rendered_chat_cached(chat, available_width, &mut ChatRenderCache::default())
}

fn rendered_chat_cached(
    chat: &ChatState,
    available_width: usize,
    cache: &mut ChatRenderCache,
) -> RenderedChat {
    if cache.generation != chat.render_generation() || cache.width != available_width {
        cache.generation = chat.render_generation();
        cache.width = available_width;
        cache.messages.clear();
    }
    cache.messages.truncate(chat.messages.len());
    cache.messages.resize_with(chat.messages.len(), || None);

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
        let animate_activity = animated_activity_index == Some(index);
        let cached = (!animate_activity)
            .then(|| cache.messages[index].as_ref())
            .flatten()
            .filter(|cached| cached.revision == message.render_revision())
            .cloned();
        let CachedRenderedMessage {
            message:
                RenderedMessage {
                    lines: mut message_lines,
                    editor_targets: message_targets,
                },
            height: message_height,
            ..
        } = cached.unwrap_or_else(|| {
            let rendered = chat_message_lines(message, available_width, animate_activity);
            let height = Paragraph::new(Text::from(rendered.lines.clone()))
                .wrap(Wrap { trim: false })
                .line_count(line_count_width);
            let cached = CachedRenderedMessage {
                revision: message.render_revision(),
                message: rendered,
                height,
            };
            if !animate_activity {
                cache.messages[index] = Some(cached.clone());
            }
            cached
        });
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
    if chat.active_turn_id.is_some() && animated_activity_index.is_none() {
        let status = if chat.is_waiting_for_activity() {
            "Thinking…"
        } else {
            "Working…"
        };
        let status_lines =
            activity_message_lines(&format!("{} {status}", thinking_frame()), available_width);
        rendered_height = rendered_height.saturating_add(
            Paragraph::new(Text::from(status_lines.clone()))
                .wrap(Wrap { trim: false })
                .line_count(line_count_width),
        );
        lines.extend(status_lines);
    }
    RenderedChat {
        lines,
        height: rendered_height,
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

fn approval_prompt(thread_title: &str, request: &AppServerRequest) -> ApprovalPrompt {
    ApprovalPrompt {
        thread_title: thread_title.to_owned(),
        method: request.method.clone(),
        detail: request.params.to_string(),
    }
}

fn render_approval(frame: &mut Frame, area: Rect, prompt: &ApprovalPrompt, interactive: bool) {
    let popup = centered_rect(90, 12, area);
    let instruction = if interactive {
        "Approve this request? [y/N] · Esc switch threads"
    } else {
        "Focus this chat to approve or decline"
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "{}\n{}\n\n{}\n\n{instruction}",
            prompt.thread_title, prompt.method, prompt.detail,
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
    let attention_by_thread = app
        .attention_items
        .iter()
        .map(|item| (item.thread_id.as_str(), item.kind))
        .collect::<HashMap<_, _>>();
    let attention_by_repository = app.repository_attention_counts();
    let rows = app.tree_rows();
    let items = rows.iter().map(|row| match row {
        TreeRow::Repository { repository_index } => {
            let repository = &app.repositories[*repository_index];
            let attention_count = attention_by_repository
                .get(&repository.path)
                .copied()
                .unwrap_or(0);
            let marker = if app.repository_is_expanded(*repository_index) {
                "▾"
            } else {
                "▸"
            };
            let attention = if attention_count == 0 {
                String::new()
            } else {
                format!(" [{attention_count}]")
            };
            ListItem::new(Text::from(vec![
                Line::styled(
                    format!("{marker}{attention} {}", repository.name),
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
            let attention_kind = attention_by_thread.get(thread.record.id.as_str()).copied();
            let marker = if awaiting_approval || attention_kind == Some(AttentionKind::Approval) {
                "!"
            } else if attention_kind == Some(AttentionKind::Failed) {
                "×"
            } else if attention_kind == Some(AttentionKind::Completed) {
                "◆"
            } else if working {
                "◉"
            } else if visible {
                "●"
            } else {
                "•"
            };
            let title_style =
                if awaiting_approval || attention_kind == Some(AttentionKind::Approval) {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if attention_kind == Some(AttentionKind::Failed) {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else if attention_kind == Some(AttentionKind::Completed) {
                    Style::default()
                        .fg(Color::Magenta)
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
    let mut block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(focus_style(app.focus == Focus::Navigation));
    if app.show_archived {
        block = block.title_bottom(Line::from(
            " R rename · x restore · d delete · A active threads ",
        ));
    } else {
        block = block.title_bottom(Line::from(" R rename · x archive · A archived threads "));
    }
    let list = List::new(items)
        .block(block)
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

fn render_repository_remove_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(68, 8, area);
    let name = app
        .selected_repository()
        .map(|repository| repository.name.as_str())
        .unwrap_or("repository");
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "Remove '{name}' from Shikigami?\n\nFiles and Codex threads are not deleted. [y/N]"
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

fn render_thread_delete_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(76, 10, area);
    let title = app
        .selected_thread()
        .map(|thread| thread.record.title.as_str())
        .unwrap_or("thread");
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "Permanently delete thread '{title}'?\n\nCodex history cannot be recovered.\nA clean Shikigami worktree is removed; other worktrees are kept. [y/N]"
        ))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" Delete thread ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        ),
        popup,
    );
}

fn render_thread_deletion_progress(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(68, 8, area);
    let Some(deletion) = app.thread_deletion.as_ref() else {
        return;
    };
    let elapsed = deletion.started_at.elapsed();
    let spinner = deletion_spinner(elapsed);
    let phase = match deletion.phase {
        ThreadDeletionPhase::CheckingWorktree => "Checking managed worktree",
        ThreadDeletionPhase::DeletingHistory => "Deleting Codex history",
        ThreadDeletionPhase::RemovingWorktree => "Removing managed worktree",
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "{spinner} {phase}…\n\n{} · {:.1}s elapsed",
            deletion.title,
            elapsed.as_secs_f64()
        ))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" Permanently deleting thread ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        ),
        popup,
    );
}

fn deletion_spinner(elapsed: Duration) -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let index = (elapsed.as_millis() / 100) as usize % FRAMES.len();
    FRAMES[index]
}

fn permanent_delete_shortcut_available(show_archived: bool, thread_selected: bool) -> bool {
    show_archived && thread_selected
}

fn render_attention(frame: &mut Frame, area: Rect, app: &App) {
    let height = u16::try_from(
        app.attention_items
            .len()
            .saturating_mul(2)
            .saturating_add(2),
    )
    .unwrap_or(u16::MAX)
    .clamp(7, area.height.saturating_sub(4).max(7));
    let popup = centered_rect(72, height, area);
    let items = app.attention_items.iter().map(|item| {
        let title = app
            .chats
            .get(&item.thread_id)
            .map(|chat| chat.title.as_str())
            .or_else(|| {
                app.threads
                    .iter()
                    .find(|thread| thread.record.id == item.thread_id)
                    .map(|thread| thread.record.title.as_str())
            })
            .unwrap_or("Unknown thread");
        let location = app
            .threads
            .iter()
            .find(|thread| thread.record.id == item.thread_id)
            .map(|thread| thread.location_name.as_str())
            .unwrap_or("side chat");
        let (label, color) = match item.kind {
            AttentionKind::Completed => ("completed", Color::Magenta),
            AttentionKind::Failed => ("failed", Color::Red),
            AttentionKind::Approval => ("approval", Color::Yellow),
        };
        ListItem::new(Text::from(vec![
            Line::from(vec![
                Span::styled(
                    format!("{label} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::styled(
                format!("{} · {}", location, item.thread_id),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    });
    let list = List::new(items)
        .block(
            Block::default()
                .title(format!(" Attention · {} ", app.attention_count()))
                .title_bottom(Line::from(
                    " j/k select · Enter open · d dismiss · Esc close ",
                ))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .highlight_symbol("› ")
        .highlight_style(Style::default().add_modifier(Modifier::BOLD));
    let mut state = ListState::default().with_selected(
        (!app.attention_items.is_empty()).then_some(
            app.attention_index
                .min(app.attention_items.len().saturating_sub(1)),
        ),
    );
    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_help(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(72, 34, area);
    let help = format!(
        "j / k / ↑↓  move and preview selected thread\nh / ←        collapse repository / select parent\nl / →        expand repository\nEnter        expand/focus / send or steer in chat\nShift-Enter  insert a newline in chat input\n←/→/↑/↓     move the chat input cursor\nCtrl-A/E     move to start/end of the current input line\nTab          focus chat / enter scroll mode\nJ / K        next / previous message in scroll mode\ne            copy an editor command for the visible diff hunk\ny / Y        copy thread ID / resume command in thread lists\ny / Y        copy selected message / full chat in scroll mode\nCtrl-C       stop the current response\nCtrl-g       switch main / side chat focus\nCtrl-n / p   next / previous side chat\n/            search threads (tree) / commands (chat)\n/permissions choose Auto or Dangerous execution\nR            rename selected thread (tree / thread picker)\n!            show threads that need attention\nEsc          return to repository tree / cancel\na            add repositories\nn            create thread in selected repository\nx            archive / restore thread\nA            active / archived threads\nd            unregister repository / delete archived thread\nr            reload repositories and names\n?            help\nq            quit\n\n● visible · ◉ working · ◆ completed · × failed · ! approval\nPermissions: {}\nPress any key to close",
        execution_status(app.execution_mode)
    );
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

fn request_quit(app: &mut App) {
    if quit_requires_confirmation(app.side_chat_count(), app.owned_turn_count()) {
        app.mode = Mode::ConfirmQuit;
    } else {
        app.should_quit = true;
    }
}

fn quit_requires_confirmation(side_chat_count: usize, owned_turn_count: usize) -> bool {
    side_chat_count > 0 || owned_turn_count > 0
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
    use std::time::Instant;

    use crate::app_server::AppServerEvent;

    use super::*;

    #[test]
    fn recognizes_only_the_shikigami_start_thread_tool() {
        let request = AppServerRequest {
            id: json!(1),
            method: "item/tool/call".into(),
            params: json!({
                "namespace": "shikigami",
                "tool": "start_thread",
                "arguments": {"prompt": "Investigate", "workspace": "current"},
                "threadId": "source",
                "turnId": "turn"
            }),
            thread_id: Some("source".into()),
            turn_id: Some("turn".into()),
        };

        assert!(is_start_thread_tool(&request));
        let mut other = request;
        other.params["tool"] = json!("get_thread_status");
        assert!(!is_start_thread_tool(&other));
    }

    #[test]
    fn dynamic_tool_responses_use_the_app_server_shape() {
        assert_eq!(
            dynamic_tool_response(true, "started".into()),
            json!({
                "contentItems": [{"type": "inputText", "text": "started"}],
                "success": true
            })
        );
    }

    #[test]
    fn navigation_width_is_bounded_and_preserves_chat_space() {
        assert_eq!(navigation_width(200), 42);
        assert_eq!(navigation_width(120), 34);
        assert_eq!(navigation_width(80), 34);
        assert_eq!(navigation_width(30), 10);
    }

    #[test]
    fn permanent_delete_shortcut_is_only_available_for_archived_threads() {
        assert!(!permanent_delete_shortcut_available(false, true));
        assert!(!permanent_delete_shortcut_available(true, false));
        assert!(permanent_delete_shortcut_available(true, true));
    }

    #[test]
    fn deletion_spinner_advances_every_hundred_milliseconds() {
        assert_eq!(deletion_spinner(Duration::ZERO), "⠋");
        assert_eq!(deletion_spinner(Duration::from_millis(100)), "⠙");
        assert_eq!(deletion_spinner(Duration::from_millis(800)), "⠋");
    }

    #[test]
    fn running_turns_or_side_chats_require_quit_confirmation() {
        assert!(!quit_requires_confirmation(0, 0));
        assert!(quit_requires_confirmation(1, 0));
        assert!(quit_requires_confirmation(0, 1));
        assert!(quit_requires_confirmation(3, 2));
    }

    #[test]
    fn active_chat_help_shows_steer_and_interrupt_shortcuts() {
        let active = chat_help(false, ChatMode::Input, false, true);
        let idle = chat_help(false, ChatMode::Input, false, false);

        assert!(active.starts_with("Ctrl-C stop · "));
        assert!(active.contains("Enter steer"));
        assert!(!idle.contains("Ctrl-C stop"));
        assert!(idle.contains("Enter send"));
    }

    #[test]
    fn composer_title_shows_live_chat_status() {
        let one = composer_title(false, 1, false);
        let multiple = composer_title(false, 2, false);
        let stopping = composer_title(false, 2, true);

        assert!(one.contains("Follow-up sent · waiting for Codex…"));
        assert!(multiple.contains("2 follow-ups sent · waiting for Codex…"));
        assert!(stopping.contains("Stopping response…"));
        assert!(!stopping.contains("follow-ups"));
    }

    #[test]
    fn pending_follow_ups_show_their_contents_in_submission_order() {
        let prompts = vec!["first follow-up".into(), "second\nfollow-up".into()];

        assert_eq!(
            pending_follow_up_lines(&prompts, 40),
            ["› first follow-up", "› second", "  follow-up"]
        );
        assert_eq!(pending_follow_up_title(2), " 2 follow-ups waiting ");
    }

    #[test]
    fn pending_follow_up_contents_wrap_with_an_indented_continuation() {
        let prompts = vec!["abcdefgh".into()];

        assert_eq!(pending_follow_up_lines(&prompts, 6), ["› abcd", "  efgh"]);
    }

    #[test]
    fn missing_rollouts_are_already_cleaned_up() {
        assert!(is_missing_thread_error(&anyhow::anyhow!(
            "Codex thread/delete error: no rollout found for thread id test"
        )));
        assert!(is_missing_thread_error(&anyhow::anyhow!(
            "thread not found"
        )));
        assert!(is_missing_thread_error(&anyhow::anyhow!(
            "{}",
            r#"Codex thread/read error: {"code":-32600,"message":"thread 019fdb56-fc38-79d3-81b8-85fd9af00000 not found"}"#
        )));
        assert!(is_missing_thread_error(&anyhow::anyhow!(
            "{}",
            r#"Codex thread/read error: {"code":-32600,"message":"thread not loaded: 019fdb56-fc38-79d3-81b8-85fd9af21df9"}"#
        )));
        assert!(!is_missing_thread_error(&anyhow::anyhow!(
            "permission denied"
        )));
    }

    #[test]
    fn permanent_deletion_does_not_treat_an_unloaded_thread_as_deleted() {
        assert!(is_missing_thread_history_error(&anyhow::anyhow!(
            "no rollout found for thread id test"
        )));
        assert!(is_missing_thread_history_error(&anyhow::anyhow!(
            "thread test not found"
        )));
        assert!(!is_missing_thread_history_error(&anyhow::anyhow!(
            "thread not loaded: test"
        )));
    }

    #[test]
    fn only_missing_untitled_threads_are_recoverable() {
        let missing = anyhow::anyhow!("no rollout found for thread id test");
        assert!(is_recoverable_empty_thread("Untitled thread", &missing));
        assert!(!is_recoverable_empty_thread("Existing thread", &missing));
        assert!(!is_recoverable_empty_thread(
            "Untitled thread",
            &anyhow::anyhow!("permission denied")
        ));
        let unavailable = anyhow::anyhow!(
            "{}",
            r#"Codex thread/read error: {"code":-32600,"message":"thread 019fdb5c-fc45-73b2-bac8-5a8804ff74ce is unavailable"}"#
        );
        assert!(is_recoverable_empty_thread("Untitled thread", &unavailable));
        assert!(!is_recoverable_empty_thread(
            "Existing thread",
            &unavailable
        ));
        assert!(!is_recoverable_empty_thread(
            "Untitled thread",
            &anyhow::anyhow!(
                "{}",
                r#"Codex thread/read error: {"code":-32600,"message":"permission denied"}"#
            )
        ));
    }

    #[test]
    fn active_writer_conflicts_are_detected_without_masking_other_errors() {
        assert!(is_active_writer_conflict(&anyhow::anyhow!(
            "thread-store conflict: thread test already has an active writer"
        )));
        assert!(!is_active_writer_conflict(&anyhow::anyhow!(
            "thread not found"
        )));
    }

    #[test]
    fn execution_status_describes_both_permission_modes() {
        assert_eq!(
            execution_status(ExecutionMode::Auto),
            "AUTO · workspace-write · approvals on-request"
        );
        assert_eq!(
            execution_status(ExecutionMode::Dangerous),
            "DANGEROUS · danger-full-access · approvals never"
        );
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
    fn codex_resume_command_targets_the_selected_thread() {
        assert_eq!(
            codex_resume_command("019-test-thread"),
            "codex resume 019-test-thread"
        );
    }

    #[test]
    #[cfg(unix)]
    fn editor_commands_use_known_line_number_syntax() {
        let path = "/tmp/project/file name.rs";

        assert_eq!(
            editor_command("nvim", path, 42),
            "nvim +42 -- '/tmp/project/file name.rs'"
        );
        assert_eq!(
            editor_command("code --wait", path, 42),
            "code --wait --goto '/tmp/project/file name.rs:42'"
        );
        assert_eq!(
            editor_command("zed", path, 42),
            "zed '/tmp/project/file name.rs:42'"
        );
        assert_eq!(
            editor_command("idea", path, 42),
            "idea --line 42 '/tmp/project/file name.rs'"
        );
    }

    #[test]
    #[cfg(unix)]
    fn unknown_editor_opens_the_file_without_guessing_line_syntax() {
        assert_eq!(
            editor_command("my-editor --reuse", "/tmp/file.rs", 42),
            "my-editor --reuse '/tmp/file.rs'"
        );
    }

    #[test]
    fn editor_program_name_handles_paths_options_and_windows_extensions() {
        assert_eq!(editor_program_name("/usr/local/bin/nvim -f"), "nvim");
        assert_eq!(
            editor_program_name(r#""C:\Program Files\Microsoft VS Code\bin\code.cmd" --wait"#),
            "code"
        );
    }

    #[test]
    #[cfg(unix)]
    fn shell_quote_escapes_apostrophes() {
        assert_eq!(shell_quote("/tmp/it's.rs"), "'/tmp/it'\\''s.rs'");
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
    fn active_turn_renders_working_between_completed_items() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.load_history(&json!({"thread":{"turns":[{
            "id":"turn",
            "status":"inProgress",
            "items":[{
                "id":"edit-1",
                "type":"fileChange",
                "status":"completed",
                "changes":[{
                    "path":"src/main.rs",
                    "kind":"update",
                    "diff":"@@ -1 +1 @@\n-old\n+new"
                }]
            }]
        }]}}));

        let text = rendered_chat(&chat, 40)
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("✓ Edited: src/main.rs"));
        assert!(text.contains("Working…"));
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
    fn message_is_highlighted_only_after_selection_navigation() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("selected".into(), "turn".into());
        chat.enter_scroll_mode();

        assert!(rendered_chat(&chat, 30).selected_range.is_none());

        chat.move_message_selection(false);
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

    #[test]
    #[ignore = "manual render performance measurement"]
    fn measures_long_streaming_chat_render_time() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        for index in 0..1_000 {
            chat.push_notice(format!(
                "Completed activity {index}\n{}",
                "output that needs wrapping across the terminal width ".repeat(4)
            ));
        }
        chat.begin_user_turn("question".into(), "turn".into());
        let iterations = 50;
        let mut cache = ChatRenderCache::default();
        let started = Instant::now();
        for _ in 0..iterations {
            std::hint::black_box(rendered_chat_cached(&chat, 80, &mut cache));
        }
        let elapsed = started.elapsed();
        eprintln!(
            "rendered {} messages {} times in {:?} ({:?}/render)",
            chat.messages.len(),
            iterations,
            elapsed,
            elapsed / iterations
        );
    }

    #[test]
    fn render_cache_refreshes_only_a_changed_message() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("question".into(), "turn".into());
        chat.apply(&AppServerEvent {
            method: "item/agentMessage/delta".into(),
            params: json!({"threadId":"t", "turnId":"turn", "delta":"first"}),
            thread_id: Some("t".into()),
            turn_id: Some("turn".into()),
        });
        let mut cache = ChatRenderCache::default();
        rendered_chat_cached(&chat, 40, &mut cache);
        let user_revision = cache.messages[0].as_ref().unwrap().revision;
        let assistant_revision = cache.messages[1].as_ref().unwrap().revision;

        chat.apply(&AppServerEvent {
            method: "item/agentMessage/delta".into(),
            params: json!({"threadId":"t", "turnId":"turn", "delta":" second"}),
            thread_id: Some("t".into()),
            turn_id: Some("turn".into()),
        });
        rendered_chat_cached(&chat, 40, &mut cache);

        assert_eq!(cache.messages[0].as_ref().unwrap().revision, user_revision);
        assert_ne!(
            cache.messages[1].as_ref().unwrap().revision,
            assistant_revision
        );
        assert!(
            cache.messages[1]
                .as_ref()
                .unwrap()
                .message
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content.contains("first second"))
        );
    }

    #[test]
    fn render_cache_resets_for_width_and_history_changes() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.push_notice("a message that wraps at narrow widths".into());
        let mut cache = ChatRenderCache::default();
        rendered_chat_cached(&chat, 20, &mut cache);
        let first_generation = cache.generation;

        rendered_chat_cached(&chat, 40, &mut cache);
        assert_eq!(cache.width, 40);

        chat.load_history(&json!({"thread":{"turns":[]}}));
        rendered_chat_cached(&chat, 40, &mut cache);
        assert_ne!(cache.generation, first_generation);
        assert!(cache.messages.is_empty());
    }

    #[test]
    fn thread_name_validation_trims_and_rejects_empty_or_overlong_input() {
        assert_eq!(validate_thread_name("  New name  ").unwrap(), "New name");
        assert!(validate_thread_name(" \n\t ").is_err());
        assert!(validate_thread_name(&"a".repeat(MAX_THREAD_NAME_CHARS)).is_ok());
        assert!(validate_thread_name(&"a".repeat(MAX_THREAD_NAME_CHARS + 1)).is_err());
        assert!(validate_thread_name(&"👨‍👩‍👧‍👦".repeat(MAX_THREAD_NAME_CHARS)).is_ok());
    }
}
