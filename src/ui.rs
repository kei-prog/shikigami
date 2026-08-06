use std::{
    io::{self, Stdout},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
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
use ratatui::prelude::Stylize;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap},
};
use serde_json::{Value, json};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    app::{App, Focus, Mode, TreeRow},
    app_server::{AppServer, AppServerRequest},
    chat::{
        ChatMessage, ChatMode, ChatRole, ChatState, CommandPalette, PaletteCommand, PaletteEntry,
    },
    git_workspace::Workspace,
};

type Tui = Terminal<CrosstermBackend<Stdout>>;

pub async fn run(mut app: App) -> Result<()> {
    let server = AppServer::spawn("codex", Duration::from_secs(30)).await?;
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

async fn run_loop(terminal: &mut Tui, app: &mut App, server: Arc<AppServer>) -> Result<()> {
    let mut inputs = EventStream::new();
    let mut server_events = server.subscribe();
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
            }
            input = inputs.next() => {
                if let Some(Ok(Event::Key(key))) = input
                    && key.kind == KeyEventKind::Press
                {
                    handle_key(app, key, &server).await?;
                    needs_draw = true;
                }
            }
            event = server_events.recv() => {
                if let Ok(event) = event
                    && let Some(chat) = &mut app.chat
                {
                    if event.method == "skills/changed" {
                        chat.skills_stale = true;
                    } else {
                        chat.apply(&event);
                    }
                    needs_draw = true;
                }
            }
            request = server.next_server_request() => {
                if let Some(request) = request {
                    if is_approval(&request) {
                        app.pending_approval = Some(request);
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

async fn handle_key(app: &mut App, key: KeyEvent, server: &Arc<AppServer>) -> Result<()> {
    match app.mode {
        Mode::Chat if app.chat.as_ref().is_some_and(|chat| chat.palette.is_some()) => {
            handle_palette_key(app, key);
        }
        Mode::Chat if app.chat.as_ref().map(|chat| chat.mode) == Some(ChatMode::Scroll) => {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    interrupt_chat(app, server).await?;
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(chat) = &mut app.chat {
                        chat.scroll_half_page_up();
                    }
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(chat) = &mut app.chat {
                        chat.scroll_half_page_down();
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(chat) = &mut app.chat {
                        chat.scroll_up(1);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(chat) = &mut app.chat {
                        chat.scroll_down(1);
                    }
                }
                KeyCode::PageUp => {
                    if let Some(chat) = &mut app.chat {
                        chat.scroll_page_up();
                    }
                }
                KeyCode::PageDown => {
                    if let Some(chat) = &mut app.chat {
                        chat.scroll_page_down();
                    }
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    if let Some(chat) = &mut app.chat {
                        chat.scroll_to_top();
                    }
                }
                KeyCode::End | KeyCode::Char('G') => {
                    if let Some(chat) = &mut app.chat {
                        chat.scroll_to_bottom();
                    }
                }
                KeyCode::Char('i') | KeyCode::Enter | KeyCode::Tab | KeyCode::Esc => {
                    if let Some(chat) = &mut app.chat {
                        chat.mode = ChatMode::Input;
                    }
                }
                _ => {}
            }
        }
        Mode::Chat => match key.code {
            KeyCode::Tab => {
                if let Some(chat) = &mut app.chat {
                    chat.mode = ChatMode::Scroll;
                }
            }
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.focus = Focus::Navigation;
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                interrupt_chat(app, server).await?;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(chat) = &mut app.chat {
                    chat.composer.clear();
                    chat.selected_skills.clear();
                }
            }
            KeyCode::Backspace => {
                if let Some(chat) = &mut app.chat {
                    chat.composer.pop();
                }
            }
            KeyCode::Enter => submit_chat(app, server).await?,
            KeyCode::Char('/')
                if app
                    .chat
                    .as_ref()
                    .is_some_and(|chat| chat.composer.is_empty()) =>
            {
                open_command_palette(app, server).await?;
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(chat) = &mut app.chat {
                    chat.composer.push(character);
                }
            }
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
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Char('?') => app.mode = Mode::Help,
            KeyCode::Esc if app.selected_tree_is_thread() => app.select_parent_repository(),
            KeyCode::Char('h') | KeyCode::Left => app.collapse_selected_repository(),
            KeyCode::Char('l') | KeyCode::Right => app.expand_selected_repository(),
            KeyCode::Tab if app.chat.is_some() => {
                app.focus = Focus::Chat;
                app.mode = Mode::Chat;
            }
            KeyCode::Up | KeyCode::Char('k') => app.move_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_down(),
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
                open_existing_chat(app, server).await?;
            }
            _ => {}
        },
    }
    Ok(())
}

async fn open_command_palette(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    let Some(chat) = &app.chat else {
        return Ok(());
    };
    let should_load = !chat.skills_loaded || chat.skills_stale;
    let cwd = chat.cwd.clone();
    let force_reload = chat.skills_stale;
    if should_load {
        match server.list_skills(&cwd, force_reload).await {
            Ok(skills) => {
                if let Some(chat) = &mut app.chat {
                    chat.available_skills = skills;
                    chat.skills_loaded = true;
                    chat.skills_stale = false;
                }
            }
            Err(error) => {
                if let Some(chat) = &mut app.chat {
                    chat.push_notice(format!("Could not load skills: {error}"));
                }
            }
        }
    }
    if let Some(chat) = &mut app.chat {
        chat.open_palette();
    }
    Ok(())
}

fn handle_palette_key(app: &mut App, key: KeyEvent) {
    let Some(chat) = &mut app.chat else {
        return;
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
        KeyCode::Enter => select_palette_entry(app),
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
}

fn select_palette_entry(app: &mut App) {
    let Some(chat) = &mut app.chat else {
        return;
    };
    let entry = chat
        .palette
        .as_ref()
        .and_then(CommandPalette::selected_entry);
    chat.palette = None;
    match entry {
        Some(PaletteEntry::Skill(skill)) => chat.select_skill(skill),
        Some(PaletteEntry::Command(PaletteCommand::Threads)) => {
            app.mode = Mode::Normal;
            app.focus = Focus::Navigation;
        }
        Some(PaletteEntry::Command(PaletteCommand::Scroll)) => chat.mode = ChatMode::Scroll,
        Some(PaletteEntry::Command(PaletteCommand::Status)) => chat.push_notice(format!(
            "Thread: {}\nWorkspace: {}",
            chat.thread_id,
            chat.cwd.display()
        )),
        None => {}
    }
}

async fn interrupt_chat(app: &App, server: &Arc<AppServer>) -> Result<()> {
    if let Some(chat) = &app.chat
        && let Some(turn_id) = &chat.active_turn_id
    {
        server.interrupt_turn(&chat.thread_id, turn_id).await?;
    }
    Ok(())
}

async fn open_new_chat(app: &mut App, server: &Arc<AppServer>, workspace: Workspace) -> Result<()> {
    let thread_id = server.start_thread(&workspace.path).await?;
    app.register_app_server_thread(thread_id.clone(), workspace.path.clone())?;
    app.chat = Some(ChatState::new(
        thread_id,
        workspace.path,
        "Untitled thread".into(),
    ));
    app.focus = Focus::Chat;
    app.mode = Mode::Chat;
    Ok(())
}

async fn open_existing_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    let Some(thread) = app.selected_thread().cloned() else {
        return Ok(());
    };
    server
        .resume_thread(&thread.record.id, &thread.record.cwd)
        .await?;
    let history = server.read_thread(&thread.record.id).await?;
    let mut chat = ChatState::new(thread.record.id, thread.record.cwd, thread.record.title);
    chat.load_history(&history);
    app.chat = Some(chat);
    app.focus = Focus::Chat;
    app.mode = Mode::Chat;
    Ok(())
}

async fn submit_chat(app: &mut App, server: &Arc<AppServer>) -> Result<()> {
    let Some(chat) = &app.chat else {
        return Ok(());
    };
    if chat.active_turn_id.is_some() || chat.composer.trim().is_empty() {
        return Ok(());
    }
    let prompt = chat.composer.clone();
    let thread_id = chat.thread_id.clone();
    let cwd = chat.cwd.clone();
    let skills = chat.skills_for_prompt(&prompt);
    let turn_id = server
        .start_turn(&thread_id, &cwd, &prompt, &skills)
        .await?;
    if let Some(chat) = &mut app.chat {
        chat.composer.clear();
        chat.begin_user_turn(prompt.clone(), turn_id);
    }
    app.update_thread_title(&thread_id, &prompt)?;
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
    let Some(request) = app.pending_approval.take() else {
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
    app.mode = if app.chat.is_some() && app.focus == Focus::Chat {
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
    let chat_status = app.chat.as_ref().map(|chat| {
        let state = if chat.active_turn_id.is_some() {
            "working"
        } else {
            "ready"
        };
        format!("{} · {state}", chat.title)
    });
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            " wyard ".bold(),
            chat_status
                .as_deref()
                .unwrap_or("repositories / threads")
                .dark_gray(),
        ])),
        chunks[0],
    );

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
        .split(chunks[1]);
    render_navigation_tree(frame, app, panes[0]);
    render_chat_pane(frame, panes[1], app);

    let status = app.message.as_deref().unwrap_or(
        "? help · j/k move · h/l collapse/expand · Enter open · n new · Tab chat · q quit",
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
        Mode::Help => render_help(frame, area),
        Mode::Normal | Mode::Chat | Mode::Approval => {}
    }
    if app.mode == Mode::Approval {
        render_approval(frame, area, app);
    }
}

fn render_chat_pane(frame: &mut Frame, area: Rect, app: &mut App) {
    let show_composer_cursor = app.mode == Mode::Chat && app.focus == Focus::Chat;
    let Some(chat) = &mut app.chat else {
        frame.render_widget(
            Paragraph::new("Select a thread or press n to create one")
                .style(Style::default().fg(Color::DarkGray))
                .block(
                    Block::default()
                        .title(" Chat ")
                        .borders(Borders::ALL)
                        .border_style(focus_style(false)),
                ),
            area,
        );
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
    let chat_border = if chat.mode == ChatMode::Scroll {
        Color::Cyan
    } else {
        focus_style(app.focus == Focus::Chat)
            .fg
            .unwrap_or(Color::DarkGray)
    };
    let chat_block = Block::default()
        .title(" Chat ")
        .borders(Borders::ALL)
        .padding(Padding::right(1))
        .border_style(Style::default().fg(chat_border));
    let text_width = chat_block.inner(chunks[0]).width.max(1);
    let lines = chat
        .messages
        .iter()
        .flat_map(|message| chat_message_lines(message, text_width as usize))
        .collect::<Vec<_>>();
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total_lines = paragraph.line_count(text_width);
    let paragraph = paragraph.block(chat_block);
    chat.update_scroll_metrics(total_lines, visible_height);
    let scroll = u16::try_from(chat.scroll_top).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((scroll, 0)), chunks[0]);
    let message_border = if chat.mode == ChatMode::Input && app.focus == Focus::Chat {
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
            "INPUT · Enter send · / palette · Ctrl-U clear · Tab scroll · Esc threads"
        }
        ChatMode::Scroll => {
            "SCROLL · j/k line · Ctrl-U/D half · PgUp/PgDn page · g/G ends · i/Tab/Esc input"
        }
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
    if let Some(palette) = &chat.palette {
        render_command_palette(frame, area, palette);
    }
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

fn chat_message_lines(message: &ChatMessage, available_width: usize) -> Vec<Line<'static>> {
    if message.role == ChatRole::Activity {
        return message
            .content
            .lines()
            .map(|line| Line::styled(format!("  {line}"), Style::default().fg(Color::DarkGray)))
            .collect();
    }
    if message.role == ChatRole::User {
        return user_message_lines(message, available_width);
    }
    let mut lines = Vec::new();
    lines.extend(
        message
            .content
            .lines()
            .map(|line| Line::from(line.to_owned())),
    );
    lines.push(Line::from(""));
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
    let request = app.pending_approval.as_ref();
    let method = request
        .map(|value| value.method.as_str())
        .unwrap_or("approval");
    let detail = request
        .map(|value| value.params.to_string())
        .unwrap_or_default();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "{method}\n\n{detail}\n\nApprove this request? [y/N]"
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
    let active_thread_id = app.chat.as_ref().map(|chat| chat.thread_id.as_str());
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
            let active = active_thread_id == Some(thread.record.id.as_str());
            let marker = if active { "●" } else { "•" };
            let title_style = if active {
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
    let popup = centered_rect(64, 20, area);
    let help = "j / k / ↑↓  move through repository tree\nh / ←        collapse repository / select parent\nl / →        expand repository\nEnter        expand repository / open thread\nTab          focus the open chat\nEsc          return to repository tree / cancel\na            add repositories\nn            create thread in selected repository\nx            archive / restore thread\nA            active / archived threads\nd            unregister repository / remove thread\nr            reload registered repositories\n?            help\nq            quit\n\nOnly clean worktrees created by wyard can be removed on archive.\nPress any key to close";
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
    fn user_message_is_a_full_width_colored_band() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("a message that wraps across the row".into(), "u".into());
        let lines = chat_message_lines(&chat.messages[0], 30);

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
}
