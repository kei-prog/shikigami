use std::time::{SystemTime, UNIX_EPOCH};

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::chat::{ChatMessage, ChatMode, ChatRole, ChatState, EditorTarget};

#[derive(Default)]
pub(super) struct ChatRenderCache {
    pub(super) generation: u64,
    pub(super) width: usize,
    pub(super) messages: Vec<Option<CachedRenderedMessage>>,
}

#[derive(Clone)]
pub(super) struct RenderedMessage {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) editor_targets: Vec<(usize, EditorTarget)>,
}

#[derive(Clone)]
pub(super) struct CachedRenderedMessage {
    pub(super) revision: u64,
    pub(super) message: RenderedMessage,
    pub(super) height: usize,
}

pub(super) fn chat_message_lines(
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
    RenderedMessage {
        lines: markdown_message_lines(&message.content, available_width),
        editor_targets: Vec::new(),
    }
}

enum MarkdownList {
    Bullet,
    Ordered(u64),
}

struct MarkdownTable {
    alignments: Vec<Alignment>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    in_header: bool,
}

impl MarkdownTable {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            header: Vec::new(),
            rows: Vec::new(),
            current_row: Vec::new(),
            in_header: false,
        }
    }

    fn finish_row(&mut self) {
        if self.current_row.is_empty() {
            return;
        }
        let row = std::mem::take(&mut self.current_row);
        if self.in_header {
            self.header = row;
        } else {
            self.rows.push(row);
        }
    }
}

struct MarkdownRenderer {
    available_width: usize,
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    styles: Vec<Style>,
    lists: Vec<MarkdownList>,
    pending_item_prefix: Option<String>,
    quote_depth: usize,
    code_block: bool,
    code_line: String,
    links: Vec<String>,
    active_link: Option<usize>,
    table: Option<MarkdownTable>,
}

impl MarkdownRenderer {
    fn new(available_width: usize) -> Self {
        Self {
            available_width: available_width.max(1),
            lines: Vec::new(),
            spans: Vec::new(),
            styles: Vec::new(),
            lists: Vec::new(),
            pending_item_prefix: None,
            quote_depth: 0,
            code_block: false,
            code_line: String::new(),
            links: Vec::new(),
            active_link: None,
            table: None,
        }
    }

    fn render(mut self, content: &str) -> Vec<Line<'static>> {
        let parser = Parser::new_ext(content, markdown_options());
        for event in parser {
            self.event(event);
        }
        if let Some(mut table) = self.table.take() {
            table.finish_row();
            self.render_table(table);
        }
        if self.code_block {
            self.finish_code_line(false);
        } else {
            self.finish_line(false);
        }
        while self.lines.last().is_some_and(|line| line.width() == 0) {
            self.lines.pop();
        }
        self.lines.push(Line::from(""));
        self.lines
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                if self.code_block {
                    self.push_code_text(&text);
                } else {
                    self.push_text(&text);
                }
            }
            Event::Code(code) => self.push_styled(
                &code,
                Style::default()
                    .fg(Color::Yellow)
                    .bg(Color::Rgb(32, 34, 36)),
            ),
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak if self.table.is_some() => self.push_styled("\n", Style::default()),
            Event::HardBreak => self.finish_line(true),
            Event::Rule => {
                self.finish_line(false);
                self.lines.push(Line::styled(
                    "─".repeat(self.available_width),
                    Style::default().fg(Color::DarkGray),
                ));
                self.blank_line();
            }
            Event::TaskListMarker(checked) => {
                self.push_text(if checked { "[x] " } else { "[ ] " });
            }
            Event::FootnoteReference(reference) => {
                self.push_styled(&format!("[^{reference}]"), Style::default().fg(Color::Cyan))
            }
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { .. } => self.styles.push(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Tag::Strong => self
                .styles
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Emphasis => self
                .styles
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.active_link = web_link_number(&mut self.links, &dest_url);
                self.styles.push(
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Image { .. } => self.styles.push(
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::ITALIC),
            ),
            Tag::BlockQuote(_) => self.quote_depth += 1,
            Tag::List(start) => self.lists.push(match start {
                Some(start) => MarkdownList::Ordered(start),
                None => MarkdownList::Bullet,
            }),
            Tag::Item => {
                let indentation = "  ".repeat(self.lists.len().saturating_sub(1));
                let marker = match self.lists.last_mut() {
                    Some(MarkdownList::Ordered(next)) => {
                        let marker = format!("{next}. ");
                        *next = next.saturating_add(1);
                        marker
                    }
                    _ => "• ".into(),
                };
                self.pending_item_prefix = Some(format!("{indentation}{marker}"));
            }
            Tag::CodeBlock(kind) => {
                self.finish_line(false);
                self.code_block = true;
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.is_empty()
                {
                    self.lines.push(Line::styled(
                        format!(" {language} "),
                        Style::default()
                            .fg(Color::DarkGray)
                            .bg(Color::Rgb(32, 34, 36)),
                    ));
                }
            }
            Tag::Table(alignments) => {
                self.finish_line(false);
                self.table = Some(MarkdownTable::new(alignments));
            }
            Tag::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.in_header = true;
                    table.current_row.clear();
                }
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.current_row.clear();
                }
            }
            Tag::TableCell => self.spans.clear(),
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.finish_line(false);
                if self.lists.is_empty() {
                    self.blank_line();
                }
            }
            TagEnd::Heading(_) => {
                self.finish_line(false);
                self.styles.pop();
                self.blank_line();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some(number) = self.active_link.take() {
                    self.push_link_number(number);
                }
            }
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Image => {
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.finish_line(false);
                self.quote_depth = self.quote_depth.saturating_sub(1);
                self.blank_line();
            }
            TagEnd::Item => {
                self.finish_line(false);
                self.pending_item_prefix = None;
            }
            TagEnd::List(_) => {
                self.finish_line(false);
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank_line();
                }
            }
            TagEnd::CodeBlock => {
                self.finish_code_line(false);
                self.code_block = false;
                self.blank_line();
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.spans)
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect();
                if let Some(table) = self.table.as_mut() {
                    table.current_row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.finish_row();
                    table.in_header = false;
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.finish_row();
                }
            }
            TagEnd::Table => {
                if let Some(mut table) = self.table.take() {
                    table.finish_row();
                    self.render_table(table);
                    self.blank_line();
                }
            }
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        if self.active_link.is_none() && !self.code_block {
            self.push_text_with_bare_links(text);
            return;
        }
        self.push_plain_text(text);
    }

    fn push_plain_text(&mut self, text: &str) {
        let style = self.current_style();
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 {
                self.finish_line(true);
            }
            if !part.is_empty() {
                self.ensure_prefix();
                self.spans.push(Span::styled(part.to_owned(), style));
            }
        }
    }

    fn push_text_with_bare_links(&mut self, text: &str) {
        let mut remaining = text;
        while let Some((start, end)) = next_bare_web_link(remaining) {
            self.push_plain_text(&remaining[..start]);
            let url = &remaining[start..end];
            if let Some(number) = web_link_number(&mut self.links, url) {
                self.push_styled(
                    url,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                );
                self.push_link_number(number);
            }
            remaining = &remaining[end..];
        }
        self.push_plain_text(remaining);
    }

    fn push_link_number(&mut self, number: usize) {
        self.push_styled(
            &format!("[{number}]"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    }

    fn push_styled(&mut self, text: &str, style: Style) {
        self.ensure_prefix();
        self.spans.push(Span::styled(
            text.to_owned(),
            self.current_style().patch(style),
        ));
    }

    fn push_code_text(&mut self, text: &str) {
        for part in text.split_inclusive('\n') {
            let (content, ended) = part
                .strip_suffix('\n')
                .map_or((part, false), |content| (content, true));
            self.code_line.push_str(content);
            if ended {
                self.finish_code_line(true);
            }
        }
    }

    fn ensure_prefix(&mut self) {
        if !self.spans.is_empty() {
            return;
        }
        if self.quote_depth > 0 {
            self.spans.push(Span::styled(
                "│ ".repeat(self.quote_depth),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if let Some(prefix) = self.pending_item_prefix.take() {
            self.spans
                .push(Span::styled(prefix, Style::default().fg(Color::Cyan)));
        }
    }

    fn finish_line(&mut self, force: bool) {
        if !self.spans.is_empty() || force {
            self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        }
    }

    fn finish_code_line(&mut self, force: bool) {
        if self.code_line.is_empty() && !force {
            return;
        }
        let width = UnicodeWidthStr::width(self.code_line.as_str());
        let padding = self.available_width.saturating_sub(width);
        let content = format!(
            "{}{}",
            std::mem::take(&mut self.code_line),
            " ".repeat(padding)
        );
        self.lines.push(Line::styled(
            content,
            Style::default().fg(Color::Gray).bg(Color::Rgb(32, 34, 36)),
        ));
    }

    fn blank_line(&mut self) {
        if self.lines.last().is_none_or(|line| line.width() != 0) {
            self.lines.push(Line::from(""));
        }
    }

    fn current_style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::default(), |style, addition| style.patch(*addition))
    }

    fn render_table(&mut self, table: MarkdownTable) {
        let columns = table
            .alignments
            .len()
            .max(table.header.len())
            .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
        if columns == 0 {
            return;
        }
        let horizontal_overhead = columns.saturating_add(1);
        let minimum_readable_width = horizontal_overhead.saturating_add(columns * 4);
        if self.available_width < minimum_readable_width {
            self.render_stacked_table(&table, columns);
            return;
        }

        let desired_widths = (0..columns)
            .map(|column| {
                std::iter::once(table.header.get(column).map_or("", String::as_str))
                    .chain(
                        table
                            .rows
                            .iter()
                            .map(move |row| row.get(column).map_or("", String::as_str)),
                    )
                    .flat_map(str::lines)
                    .map(UnicodeWidthStr::width)
                    .max()
                    .unwrap_or(1)
                    .max(1)
            })
            .collect::<Vec<_>>();
        let content_width = self.available_width.saturating_sub(horizontal_overhead);
        let widths = allocate_column_widths(&desired_widths, content_width);

        self.push_table_rule('┌', '┬', '┐', &widths);
        self.push_table_row(&table.header, &table.alignments, &widths, true);
        self.push_table_rule('├', '┼', '┤', &widths);
        for (index, row) in table.rows.iter().enumerate() {
            self.push_table_row(row, &table.alignments, &widths, false);
            if index + 1 < table.rows.len() {
                self.push_table_rule('├', '┼', '┤', &widths);
            }
        }
        self.push_table_rule('└', '┴', '┘', &widths);
    }

    fn render_stacked_table(&mut self, table: &MarkdownTable, columns: usize) {
        let empty_row = Vec::new();
        let rows = if table.rows.is_empty() {
            std::slice::from_ref(&empty_row)
        } else {
            table.rows.as_slice()
        };
        for (row_index, row) in rows.iter().enumerate() {
            for column in 0..columns {
                let heading = table.header.get(column).map_or("", String::as_str);
                for line in wrap_message(heading, self.available_width) {
                    self.lines.push(Line::styled(
                        line,
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                let indent = usize::from(self.available_width > 2) * 2;
                let value_width = self.available_width.saturating_sub(indent).max(1);
                let value = row.get(column).map_or("", String::as_str);
                for line in wrap_message(value, value_width) {
                    self.lines
                        .push(Line::from(format!("{}{}", " ".repeat(indent), line)));
                }
            }
            if row_index + 1 < rows.len() {
                self.blank_line();
            }
        }
    }

    fn push_table_rule(&mut self, left: char, middle: char, right: char, widths: &[usize]) {
        let mut rule = String::new();
        rule.push(left);
        for (index, width) in widths.iter().enumerate() {
            rule.push_str(&"─".repeat(*width));
            rule.push(if index + 1 == widths.len() {
                right
            } else {
                middle
            });
        }
        self.lines
            .push(Line::styled(rule, Style::default().fg(Color::DarkGray)));
    }

    fn push_table_row(
        &mut self,
        row: &[String],
        alignments: &[Alignment],
        widths: &[usize],
        header: bool,
    ) {
        let cells = widths
            .iter()
            .enumerate()
            .map(|(column, width)| wrap_message(row.get(column).map_or("", String::as_str), *width))
            .collect::<Vec<_>>();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        let cell_style = if header {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        for line_index in 0..height {
            let mut spans = vec![Span::styled("│", Style::default().fg(Color::DarkGray))];
            for (column, width) in widths.iter().enumerate() {
                let content = cells[column].get(line_index).map_or("", String::as_str);
                let alignment = alignments.get(column).copied().unwrap_or(Alignment::None);
                spans.push(Span::styled(
                    align_table_cell(content, *width, alignment),
                    cell_style,
                ));
                spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));
            }
            self.lines.push(Line::from(spans));
        }
    }
}

fn markdown_options() -> Options {
    Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES
}

fn allocate_column_widths(desired: &[usize], available: usize) -> Vec<usize> {
    let mut widths = vec![1; desired.len()];
    let mut remaining = available.saturating_sub(widths.len());
    while remaining > 0 {
        let unsatisfied = widths
            .iter()
            .zip(desired)
            .filter(|(width, desired)| width < desired)
            .count();
        if unsatisfied == 0 {
            break;
        }
        let share = (remaining / unsatisfied).max(1);
        for (width, desired) in widths.iter_mut().zip(desired) {
            let added = desired.saturating_sub(*width).min(share).min(remaining);
            *width += added;
            remaining -= added;
            if remaining == 0 {
                break;
            }
        }
    }
    widths
}

fn align_table_cell(content: &str, width: usize, alignment: Alignment) -> String {
    let padding = width.saturating_sub(UnicodeWidthStr::width(content));
    let left = match alignment {
        Alignment::Right => padding,
        Alignment::Center => padding / 2,
        Alignment::None | Alignment::Left => 0,
    };
    format!(
        "{}{}{}",
        " ".repeat(left),
        content,
        " ".repeat(padding - left)
    )
}

fn web_link_number(links: &mut Vec<String>, destination: &str) -> Option<usize> {
    if !destination.starts_with("https://") && !destination.starts_with("http://") {
        return None;
    }
    if let Some(index) = links.iter().position(|link| link == destination) {
        return Some(index + 1);
    }
    links.push(destination.to_owned());
    Some(links.len())
}

fn next_bare_web_link(text: &str) -> Option<(usize, usize)> {
    let start = [text.find("https://"), text.find("http://")]
        .into_iter()
        .flatten()
        .min()?;
    let candidate = &text[start..];
    let raw_end = candidate
        .find(char::is_whitespace)
        .unwrap_or(candidate.len());
    let trimmed =
        candidate[..raw_end].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
    Some((start, start + trimmed.len()))
}

pub(super) fn message_web_links(content: &str) -> Vec<String> {
    let mut links = Vec::new();
    let parser = Parser::new_ext(content, markdown_options());
    let mut inside_link = false;
    for event in parser {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                inside_link = true;
                web_link_number(&mut links, &dest_url);
            }
            Event::End(TagEnd::Link) => inside_link = false,
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) if !inside_link => {
                let mut remaining = text.as_ref();
                while let Some((start, end)) = next_bare_web_link(remaining) {
                    web_link_number(&mut links, &remaining[start..end]);
                    remaining = &remaining[end..];
                }
            }
            _ => {}
        }
    }
    links
}

fn markdown_message_lines(content: &str, available_width: usize) -> Vec<Line<'static>> {
    MarkdownRenderer::new(available_width).render(content)
}

pub(super) struct RenderedChat {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) height: usize,
    pub(super) selected_range: Option<(usize, usize)>,
    pub(super) editor_targets: Vec<(usize, EditorTarget)>,
}

#[cfg(test)]
pub(super) fn rendered_chat(chat: &ChatState, available_width: usize) -> RenderedChat {
    rendered_chat_cached(chat, available_width, &mut ChatRenderCache::default())
}

pub(super) fn rendered_chat_cached(
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

pub(super) fn thinking_frame() -> &'static str {
    const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let frame = (elapsed.as_millis() / 100) as usize % FRAMES.len();
    FRAMES[frame]
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

pub(super) fn activity_header_color(content: &str) -> Color {
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

pub(super) fn diff_message_lines(message: &ChatMessage, available_width: usize) -> RenderedMessage {
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

pub(super) fn visible_editor_target(
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

pub(super) fn wrap_message(message: &str, max_width: usize) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn markdown_styles_headings_and_inline_content() {
        let lines =
            markdown_message_lines("# Heading\n\nText with **bold**, *italic*, and `code`.", 80);

        assert_eq!(line_text(&lines[0]), "Heading");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Cyan));
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        let spans = lines
            .iter()
            .flat_map(|line| &line.spans)
            .collect::<Vec<_>>();
        let bold = spans.iter().find(|span| span.content == "bold").unwrap();
        let italic = spans.iter().find(|span| span.content == "italic").unwrap();
        let code = spans.iter().find(|span| span.content == "code").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
        assert_eq!(code.style.fg, Some(Color::Yellow));
    }

    #[test]
    fn markdown_renders_lists_quotes_rules_and_code_blocks() {
        let lines = markdown_message_lines(
            "> quoted\n\n1. first\n2. second\n\n---\n\n```rust\nfn main() {}\n```",
            40,
        );
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(text.iter().any(|line| line == "│ quoted"));
        assert!(text.iter().any(|line| line == "1. first"));
        assert!(text.iter().any(|line| line == "2. second"));
        assert!(text.iter().any(|line| line == &"─".repeat(40)));
        assert!(text.iter().any(|line| line == " rust "));
        let code = lines
            .iter()
            .find(|line| line_text(line).starts_with("fn main() {}"))
            .unwrap();
        assert_eq!(code.style.bg, Some(Color::Rgb(32, 34, 36)));
        assert_eq!(code.width(), 40);
    }

    #[test]
    fn incomplete_streaming_markdown_remains_visible() {
        let lines =
            markdown_message_lines("Working on **unfinished\n\n```rust\nlet value = 1;", 40);
        let text = lines.iter().map(line_text).collect::<String>();

        assert!(text.contains("Working on **unfinished"));
        assert!(text.contains("let value = 1;"));
    }

    #[test]
    fn markdown_renders_a_basic_table() {
        let lines = markdown_message_lines(
            "| Name | Status |\n| --- | --- |\n| parser | ready |\n| renderer | active |",
            40,
        );
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(
            text.iter()
                .any(|line| line.starts_with("┌") && line.ends_with("┐"))
        );
        assert!(
            text.iter()
                .any(|line| line.contains("Name") && line.contains("Status"))
        );
        assert!(
            text.iter()
                .any(|line| line.contains("parser") && line.contains("ready"))
        );
        assert!(
            text.iter()
                .any(|line| line.starts_with("└") && line.ends_with("┘"))
        );
        let header = lines
            .iter()
            .find(|line| line_text(line).contains("Name"))
            .unwrap();
        assert!(header.spans[1].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn markdown_table_respects_column_alignment() {
        let lines = markdown_message_lines(
            "| Left | Center | Right |\n| :--- | :----: | ----: |\n| a | b | c |",
            40,
        );
        let body = lines
            .iter()
            .map(line_text)
            .find(|line| line.contains('a') && line.contains('b') && line.contains('c'))
            .unwrap();

        assert_eq!(body, "│a   │  b   │    c│");
    }

    #[test]
    fn narrow_markdown_table_stacks_without_horizontal_overflow() {
        let lines = markdown_message_lines(
            "Before\n\n| Field | Value |\n| --- | --- |\n| mode | streaming response |\n\nAfter",
            10,
        );
        let text = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(lines.iter().all(|line| line.width() <= 10));
        assert!(text.iter().any(|line| line == "Field"));
        assert!(text.concat().replace(' ', "").contains("streamingresponse"));
        assert!(text.iter().any(|line| line == "Before"));
        assert!(text.iter().any(|line| line == "After"));
    }

    #[test]
    fn markdown_table_wraps_unicode_by_display_width() {
        let lines = markdown_message_lines(
            "| 項目 | 内容 |\n| --- | --- |\n| 状態 | 日本語の長い説明🙂です |",
            18,
        );
        let text = lines.iter().map(line_text).collect::<String>();

        assert!(lines.iter().all(|line| line.width() <= 18));
        assert!(text.contains("項目"));
        assert!(text.contains("日本語"));
        assert!(text.contains('🙂'));
    }

    #[test]
    fn streaming_table_and_existing_markdown_remain_visible() {
        let lines = markdown_message_lines(
            "## Update\n\n**Ready**\n\n| Step | Result |\n| --- | --- |\n| parse | partial",
            24,
        );
        let text = lines.iter().map(line_text).collect::<String>();

        assert!(text.contains("Update"));
        assert!(text.contains("Ready"));
        assert!(text.contains("Step"));
        assert!(text.contains("partial"));
        assert!(lines.iter().all(|line| line.width() <= 24));
    }

    #[test]
    fn markdown_numbers_named_and_bare_web_links_in_encounter_order() {
        let content = "Read [Rust](https://rust-lang.org) then https://example.com/docs. Again [Rust](https://rust-lang.org).";
        let lines = markdown_message_lines(content, 120);
        let text = lines.iter().map(line_text).collect::<String>();

        assert!(text.contains("Rust[1]"));
        assert!(text.contains("https://example.com/docs[2]."));
        assert!(text.ends_with("Rust[1]."));
        assert_eq!(
            message_web_links(content),
            vec!["https://rust-lang.org", "https://example.com/docs"]
        );
    }

    #[test]
    fn markdown_does_not_number_non_web_or_code_links() {
        let content = "[file](../README.md) and `https://example.com/code`";
        let lines = markdown_message_lines(content, 80);
        let text = lines.iter().map(line_text).collect::<String>();

        assert!(!text.contains("[1]"));
        assert!(message_web_links(content).is_empty());
    }
}
