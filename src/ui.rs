use std::{
    io::{self, Stdout},
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{
        Clear as ClearTerminal, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::prelude::Stylize;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    app::{App, Focus, Mode},
    codex,
};

type Tui = Terminal<CrosstermBackend<Stdout>>;

enum CodexLaunch {
    New,
    Resume,
}

pub fn run(mut app: App) -> Result<()> {
    let mut terminal = init_terminal()?;
    let result = run_loop(&mut terminal, &mut app);
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
        MoveTo(0, 0)
    )?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_loop(terminal: &mut Tui, app: &mut App) -> Result<()> {
    while !app.should_quit {
        app.poll_scan();
        terminal.draw(|frame| render(frame, app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Press {
            handle_key(terminal, app, key)?;
        }
    }
    Ok(())
}

fn handle_key(terminal: &mut Tui, app: &mut App, key: KeyEvent) -> Result<()> {
    match app.mode {
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
        Mode::ChooseLocation => match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => app.move_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_down(),
            KeyCode::Enter => launch_codex(terminal, app, CodexLaunch::New)?,
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
            KeyCode::Esc if app.focus == Focus::Threads => app.focus = Focus::Repositories,
            KeyCode::Char('h') | KeyCode::Left => app.focus = Focus::Repositories,
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => app.focus = Focus::Threads,
            KeyCode::Up | KeyCode::Char('k') => app.move_up(),
            KeyCode::Down | KeyCode::Char('j') => app.move_down(),
            KeyCode::Char('a') => app.open_repository_add(),
            KeyCode::Char('n') => {
                if app.locations.is_empty() {
                    app.message = Some("no available repository location".into());
                } else {
                    app.location_index = 0;
                    app.mode = Mode::ChooseLocation;
                }
            }
            KeyCode::Char('r') => {
                if let Err(error) = app.refresh_repositories() {
                    app.message = Some(error.to_string());
                }
            }
            KeyCode::Char('d')
                if app.focus == Focus::Repositories && !app.repositories.is_empty() =>
            {
                app.mode = Mode::ConfirmRemoveRepository;
            }
            KeyCode::Char('d') if app.focus == Focus::Threads && !app.threads.is_empty() => {
                app.mode = Mode::ConfirmRemoveThread;
            }
            KeyCode::Enter if app.focus == Focus::Repositories => app.focus = Focus::Threads,
            KeyCode::Enter if app.focus == Focus::Threads && !app.threads.is_empty() => {
                launch_codex(terminal, app, CodexLaunch::Resume)?;
            }
            _ => {}
        },
    }
    Ok(())
}

fn launch_codex(terminal: &mut Tui, app: &mut App, launch: CodexLaunch) -> Result<()> {
    let (label, result) = match launch {
        CodexLaunch::New => {
            let Some(repository) = app.selected_repository().cloned() else {
                return Ok(());
            };
            let Some(location) = app.selected_location().cloned() else {
                return Ok(());
            };
            app.mode = Mode::Normal;
            restore_terminal(terminal)?;
            let result = codex::run_new(&location.path, &repository.path);
            (format!("new thread in {}", location.name), result)
        }
        CodexLaunch::Resume => {
            let Some(thread) = app.selected_thread().cloned() else {
                return Ok(());
            };
            restore_terminal(terminal)?;
            let result = codex::resume(&thread.record);
            (thread.record.title, result)
        }
    };

    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    let area = terminal.size()?.into();
    terminal.resize(area)?;
    app.refresh_current();
    app.message = Some(match result {
        Ok(()) => format!("Codex exited: {label}"),
        Err(error) => error.to_string(),
    });
    Ok(())
}

fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            " wyard ".bold(),
            "repositories / threads".dark_gray(),
        ])),
        chunks[0],
    );

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(chunks[1]);
    render_repositories(frame, app, panes[0]);
    render_threads(frame, app, panes[1]);

    let status = app
        .message
        .as_deref()
        .unwrap_or("? help  a add repository  n new thread  Enter open  q quit");
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    match app.mode {
        Mode::AddRepositories | Mode::FilterRepositories => render_repository_add(frame, area, app),
        Mode::BrowseDirectory => render_browser(frame, area, app),
        Mode::ChooseLocation => render_locations(frame, area, app),
        Mode::ConfirmRemoveRepository => render_repository_remove_confirm(frame, area, app),
        Mode::ConfirmRemoveThread => render_remove_confirm(frame, area, app),
        Mode::Help => render_help(frame, area),
        Mode::Normal => {}
    }
}

fn render_repositories(frame: &mut Frame, app: &App, area: Rect) {
    let items = app.repositories.iter().map(|repo| {
        ListItem::new(Text::from(vec![
            Line::from(repo.name.clone()),
            Line::styled(
                repo.path.display().to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    });
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Repositories ")
                .borders(Borders::ALL)
                .border_style(focus_style(app.focus == Focus::Repositories)),
        )
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default()
        .with_selected((!app.repositories.is_empty()).then_some(app.repository_index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_threads(frame: &mut Frame, app: &App, area: Rect) {
    let items = app.threads.iter().map(|thread| {
        let kind = if thread.is_primary {
            "primary"
        } else {
            "worktree"
        };
        ListItem::new(Text::from(vec![
            Line::from(thread.record.title.clone()),
            Line::styled(
                format!(
                    "{} [{kind}] · {}",
                    thread.location_name,
                    thread.record.cwd.display()
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    });
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Threads ")
                .borders(Borders::ALL)
                .border_style(focus_style(app.focus == Focus::Threads)),
        )
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state =
        ListState::default().with_selected((!app.threads.is_empty()).then_some(app.thread_index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_locations(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(72, 12, area);
    let items = app.locations.iter().map(|location| {
        let kind = if location.is_primary {
            "primary"
        } else {
            "worktree"
        };
        ListItem::new(Text::from(vec![
            Line::from(format!("{} [{kind}]", location.name)),
            Line::styled(
                location.path.display().to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
    });
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Start thread in ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default().with_selected(Some(app.location_index));
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

fn render_help(frame: &mut Frame, area: Rect) {
    let popup = centered_rect(64, 16, area);
    let help = "j / k / ↑↓  move within pane\nh / l / ←→  switch pane\nEnter       select / resume thread\nEsc         move back / cancel\na           add repositories\nn           create thread\nd           unregister repository / thread\nr           reload registered repositories\n?           help\nq           quit\n\nNew threads can start in the primary repo or a worktree.\nPress any key to close";
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
