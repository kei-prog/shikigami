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
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_loop(terminal: &mut Tui, app: &mut App) -> Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| render(frame, app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        handle_key(terminal, app, key)?;
    }
    Ok(())
}

fn handle_key(terminal: &mut Tui, app: &mut App, key: KeyEvent) -> Result<()> {
    match &mut app.mode {
        Mode::AddRepository(input) | Mode::AddWorkspace(input) => match key.code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(character) => input.push(character),
            KeyCode::Enter => submit_input(app),
            _ => {}
        },
        Mode::ConfirmForget(_) => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.mode = Mode::Normal;
                if let Err(error) = app.forget_selected_workspace() {
                    app.message = Some(error.to_string());
                } else {
                    app.message = Some("workspace forgotten; files remain on disk".into());
                }
            }
            _ => app.mode = Mode::Normal,
        },
        Mode::ConfirmRemoveRepository => match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                app.mode = Mode::Normal;
                if let Err(error) = app.remove_selected_repository() {
                    app.message = Some(error.to_string());
                } else {
                    app.message = Some("repository removed from wyard; files are untouched".into());
                }
            }
            _ => app.mode = Mode::Normal,
        },
        Mode::Help => app.mode = Mode::Normal,
        Mode::Normal => match key.code {
            KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Char('?') => app.mode = Mode::Help,
            KeyCode::Tab | KeyCode::Right => app.focus = Focus::Workspaces,
            KeyCode::Left => app.focus = Focus::Repositories,
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('h') => app.move_up(),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('l') => app.move_down(),
            KeyCode::Char('a') => app.mode = Mode::AddRepository(String::new()),
            KeyCode::Char('n') => app.mode = Mode::AddWorkspace(String::new()),
            KeyCode::Char('r') => app.refresh_workspaces(),
            KeyCode::Char('d') if app.focus == Focus::Repositories => {
                app.mode = Mode::ConfirmRemoveRepository
            }
            KeyCode::Char('d') if app.focus == Focus::Workspaces => {
                match app.selected_workspace_status() {
                    Ok(status) => app.mode = Mode::ConfirmForget(status),
                    Err(error) => app.message = Some(error.to_string()),
                }
            }
            KeyCode::Enter if app.focus == Focus::Workspaces => launch_codex(terminal, app)?,
            _ => {}
        },
    }
    Ok(())
}

fn submit_input(app: &mut App) {
    let mode = std::mem::replace(&mut app.mode, Mode::Normal);
    let result = match mode {
        Mode::AddRepository(path) => app.add_repository(path.trim()),
        Mode::AddWorkspace(name) => app.add_workspace(name.trim()),
        _ => return,
    };
    app.message = Some(match result {
        Ok(()) => "saved".into(),
        Err(error) => error.to_string(),
    });
}

fn launch_codex(terminal: &mut Tui, app: &mut App) -> Result<()> {
    let Some(workspace) = app.selected_workspace().cloned() else {
        return Ok(());
    };
    restore_terminal(terminal)?;
    let result = codex::run(&workspace.path);
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        ClearTerminal(ClearType::All),
        MoveTo(0, 0)
    )?;
    app.message = Some(match result {
        Ok(()) => format!("Codex exited: {}", workspace.name),
        Err(error) => error.to_string(),
    });
    app.refresh_workspaces();
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
            "repositories / workspaces".dark_gray(),
        ])),
        chunks[0],
    );

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(chunks[1]);
    render_repositories(frame, app, panes[0]);
    render_workspaces(frame, app, panes[1]);

    let status = app
        .message
        .as_deref()
        .unwrap_or("? help  a add repository  n new workspace  Enter open Codex  q quit");
    frame.render_widget(
        Paragraph::new(status).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    match &app.mode {
        Mode::AddRepository(input) => {
            render_input(frame, area, "Register repository", "Path", input)
        }
        Mode::AddWorkspace(input) => render_input(frame, area, "Create workspace", "Name", input),
        Mode::ConfirmForget(status) => render_forget_confirm(frame, area, app, status),
        Mode::ConfirmRemoveRepository => render_repository_confirm(frame, area, app),
        Mode::Help => render_help(frame, area),
        Mode::Normal => {}
    }
}

fn render_repositories(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .config
        .repositories
        .iter()
        .map(|repo| {
            ListItem::new(Text::from(vec![
                Line::from(repo.name.clone()),
                Line::styled(
                    repo.path.display().to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();
    let block = Block::default()
        .title(" Repositories ")
        .borders(Borders::ALL)
        .border_style(focus_style(app.focus == Focus::Repositories));
    let list = List::new(items)
        .block(block)
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default()
        .with_selected((!app.config.repositories.is_empty()).then_some(app.repository_index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_workspaces(frame: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .workspaces
        .iter()
        .map(|workspace| {
            ListItem::new(Text::from(vec![
                Line::from(workspace.name.clone()),
                Line::styled(
                    workspace.path.display().to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();
    let block = Block::default()
        .title(" Workspaces ")
        .borders(Borders::ALL)
        .border_style(focus_style(app.focus == Focus::Workspaces));
    let list = List::new(items)
        .block(block)
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default()
        .with_selected((!app.workspaces.is_empty()).then_some(app.workspace_index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_input(frame: &mut Frame, area: Rect, title: &str, label: &str, input: &str) {
    let popup = centered_rect(72, 5, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!("{label}: {input}")).block(
            Block::default()
                .title(format!(" {title} "))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        ),
        popup,
    );
}

fn render_forget_confirm(frame: &mut Frame, area: Rect, app: &App, status: &str) {
    let popup = centered_rect(72, 10, area);
    let name = app
        .selected_workspace()
        .map(|workspace| workspace.name.as_str())
        .unwrap_or("workspace");
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "Forget '{name}'?\n\n{status}\n\nFiles will remain on disk. [y/N]"
        ))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" Forget workspace ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        ),
        popup,
    );
}

fn render_repository_confirm(frame: &mut Frame, area: Rect, app: &App) {
    let popup = centered_rect(64, 6, area);
    let name = app
        .selected_repository()
        .map(|repository| repository.name.as_str())
        .unwrap_or("repository");
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(format!(
            "Remove '{name}' from wyard?\nRepository files are untouched. [y/N]"
        ))
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
    let help = "h / k / ↑   move up\nl / j / ↓   move down\n← / → / Tab switch pane\na           register repository\nn           create workspace\nd           remove selected item\nr           refresh\nEnter       open Codex CLI\n?           help\nq           quit\n\nPress any key to close";
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
