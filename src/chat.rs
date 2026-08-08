use std::{collections::VecDeque, path::PathBuf};

use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app_server::{AppServerEvent, SkillMetadata};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatRole {
    User,
    Assistant,
    Activity,
    Diff,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatMode {
    Input,
    Scroll,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaletteCommand {
    Threads,
    Scroll,
    Model,
    SideChat,
    Sides,
    SideClose,
    SidePromote,
    Attention,
    Status,
}

impl PaletteCommand {
    pub fn label(self) -> &'static str {
        match self {
            Self::Threads => "/threads",
            Self::Scroll => "/scroll",
            Self::Model => "/model",
            Self::SideChat => "/sidechat",
            Self::Sides => "/sides",
            Self::SideClose => "/sideclose",
            Self::SidePromote => "/sidepromote",
            Self::Attention => "/attention",
            Self::Status => "/status",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Threads => "Return to the thread list",
            Self::Scroll => "Enter chat scroll mode",
            Self::Model => "Choose the model and reasoning effort",
            Self::SideChat => "Fork this thread into a side chat",
            Self::Sides => "Choose a side chat for this thread",
            Self::SideClose => "Close the current side chat",
            Self::SidePromote => "Promote the current side chat to a thread",
            Self::Attention => "Show threads that need attention",
            Self::Status => "Show the current thread and workspace",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaletteEntry {
    Command(PaletteCommand),
    Skill(SkillMetadata),
}

impl PaletteEntry {
    pub fn label(&self) -> String {
        match self {
            Self::Command(command) => command.label().into(),
            Self::Skill(skill) => format!("${}", skill.name),
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Command(command) => command.description(),
            Self::Skill(skill) => &skill.description,
        }
    }

    fn search_name(&self) -> &str {
        match self {
            Self::Command(command) => command.label().trim_start_matches('/'),
            Self::Skill(skill) => &skill.name,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CommandPalette {
    pub query: String,
    pub selected: usize,
    pub entries: Vec<PaletteEntry>,
}

impl CommandPalette {
    pub fn new(skills: &[SkillMetadata]) -> Self {
        let mut entries = vec![
            PaletteEntry::Command(PaletteCommand::Threads),
            PaletteEntry::Command(PaletteCommand::Scroll),
            PaletteEntry::Command(PaletteCommand::Model),
            PaletteEntry::Command(PaletteCommand::SideChat),
            PaletteEntry::Command(PaletteCommand::Sides),
            PaletteEntry::Command(PaletteCommand::SideClose),
            PaletteEntry::Command(PaletteCommand::SidePromote),
            PaletteEntry::Command(PaletteCommand::Attention),
            PaletteEntry::Command(PaletteCommand::Status),
        ];
        entries.extend(skills.iter().cloned().map(PaletteEntry::Skill));
        Self {
            query: String::new(),
            selected: 0,
            entries,
        }
    }

    pub fn visible_entries(&self) -> Vec<&PaletteEntry> {
        if self.query.is_empty() {
            return self.entries.iter().collect();
        }
        let mut matches = self
            .entries
            .iter()
            .filter_map(|entry| {
                let name_score = fuzzy_score(entry.search_name(), &self.query);
                let description_score = fuzzy_score(entry.description(), &self.query)
                    .map(|score| score.saturating_sub(2_000));
                name_score
                    .into_iter()
                    .chain(description_score)
                    .max()
                    .map(|score| (score, entry))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|(left, _), (right, _)| right.cmp(left));
        matches.into_iter().map(|(_, entry)| entry).collect()
    }

    pub fn selected_entry(&self) -> Option<PaletteEntry> {
        self.visible_entries().get(self.selected).cloned().cloned()
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        self.selected = self
            .selected
            .saturating_add(1)
            .min(self.visible_entries().len().saturating_sub(1));
    }

    pub fn push_query(&mut self, character: char) {
        self.query.push(character);
        self.selected = 0;
    }

    pub fn pop_query(&mut self) {
        self.query.pop();
        self.selected = 0;
    }
}

pub(crate) fn fuzzy_score(candidate: &str, query: &str) -> Option<i32> {
    let candidate = candidate.to_lowercase();
    let query = query.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    if candidate == query {
        return Some(10_000);
    }
    if candidate.starts_with(&query) {
        return Some(8_000 - candidate.chars().count() as i32);
    }
    if let Some(byte_index) = candidate.find(&query) {
        let character_index = candidate[..byte_index].chars().count() as i32;
        return Some(6_000 - character_index * 10 - candidate.chars().count() as i32);
    }

    let query = query.chars().collect::<Vec<_>>();
    let mut query_index = 0;
    let mut previous_match = None;
    let mut previous_character = None;
    let mut score = 0;
    for (index, character) in candidate.chars().enumerate() {
        if query_index >= query.len() {
            break;
        }
        if character == query[query_index] {
            score += 100;
            if index == 0 || previous_character.is_some_and(is_word_separator) {
                score += 80;
            }
            if previous_match == Some(index.saturating_sub(1)) {
                score += 50;
            }
            score -= index as i32;
            previous_match = Some(index);
            query_index += 1;
        }
        previous_character = Some(character);
    }
    (query_index == query.len()).then_some(score - candidate.chars().count() as i32)
}

fn is_word_separator(character: char) -> bool {
    matches!(character, '-' | '_' | '/' | ' ' | ':')
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditorTarget {
    pub path: PathBuf,
    pub line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffTarget {
    pub content_line: usize,
    pub editor: EditorTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    item_id: Option<String>,
    diff_targets: Vec<DiffTarget>,
}

impl ChatMessage {
    pub fn diff_targets(&self) -> &[DiffTarget] {
        &self.diff_targets
    }
}

#[derive(Clone, Debug)]
pub struct ChatState {
    pub thread_id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub model: Option<String>,
    pub model_display_name: Option<String>,
    pub reasoning_effort: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub composer: String,
    composer_cursor: usize,
    composer_width: usize,
    composer_preferred_column: Option<usize>,
    pub active_turn_id: Option<String>,
    pub mode: ChatMode,
    pub selected_message_index: Option<usize>,
    pub scroll_top: usize,
    pub max_scroll: usize,
    pub viewport_height: usize,
    pub follow_tail: bool,
    pub palette: Option<CommandPalette>,
    pub available_skills: Vec<SkillMetadata>,
    pub selected_skills: Vec<SkillMetadata>,
    pub skills_loaded: bool,
    pub skills_stale: bool,
    pub is_side_chat: bool,
    pub side_chat_has_activity: bool,
    pub visible_editor_target: Option<EditorTarget>,
    waiting_for_activity: bool,
    streaming_message: Option<usize>,
    pending_user_message: Option<String>,
    pending_steers: VecDeque<String>,
    interrupt_requested: bool,
    message_selection_scroll_pending: bool,
}

impl ChatState {
    pub fn new(thread_id: String, cwd: PathBuf, title: String) -> Self {
        Self {
            thread_id,
            cwd,
            title,
            model: None,
            model_display_name: None,
            reasoning_effort: None,
            messages: Vec::new(),
            composer: String::new(),
            composer_cursor: 0,
            composer_width: 1,
            composer_preferred_column: None,
            active_turn_id: None,
            mode: ChatMode::Input,
            selected_message_index: None,
            scroll_top: 0,
            max_scroll: 0,
            viewport_height: 1,
            follow_tail: true,
            palette: None,
            available_skills: Vec::new(),
            selected_skills: Vec::new(),
            skills_loaded: false,
            skills_stale: false,
            is_side_chat: false,
            side_chat_has_activity: false,
            visible_editor_target: None,
            waiting_for_activity: false,
            streaming_message: None,
            pending_user_message: None,
            pending_steers: VecDeque::new(),
            interrupt_requested: false,
            message_selection_scroll_pending: false,
        }
    }

    pub fn set_model(
        &mut self,
        model: String,
        display_name: String,
        reasoning_effort: Option<String>,
    ) {
        self.model = Some(model);
        self.model_display_name = Some(display_name);
        self.reasoning_effort = reasoning_effort;
    }

    pub fn mark_as_side_chat(&mut self) {
        self.is_side_chat = true;
        self.side_chat_has_activity = false;
    }

    pub fn mark_as_main_chat(&mut self) {
        self.is_side_chat = false;
        self.side_chat_has_activity = false;
    }

    pub fn is_unused_main_thread(&self) -> bool {
        !self.is_side_chat
            && self.title == "Untitled thread"
            && self.messages.is_empty()
            && self.active_turn_id.is_none()
    }

    pub fn load_history(&mut self, response: &Value) {
        self.messages.clear();
        self.active_turn_id = None;
        self.streaming_message = None;
        self.pending_user_message = None;
        self.pending_steers.clear();
        self.interrupt_requested = false;
        self.visible_editor_target = None;
        self.selected_message_index = None;
        self.message_selection_scroll_pending = false;
        self.waiting_for_activity = false;
        let Some(turns) = response.pointer("/thread/turns").and_then(Value::as_array) else {
            return;
        };
        for turn in turns {
            let in_progress = turn.get("status").and_then(Value::as_str) == Some("inProgress");
            if in_progress {
                self.active_turn_id = turn.get("id").and_then(Value::as_str).map(str::to_owned);
                self.waiting_for_activity = true;
            }
            if let Some(items) = turn.get("items").and_then(Value::as_array) {
                for item in items {
                    let item_type = item.get("type").and_then(Value::as_str);
                    if in_progress && item_type != Some("userMessage") {
                        self.waiting_for_activity = false;
                    }
                    if in_progress
                        && item.get("status").and_then(Value::as_str) == Some("inProgress")
                    {
                        self.push_started_item(item);
                    } else {
                        self.push_completed_item(item);
                    }
                }
            }
            if turn.get("status").and_then(Value::as_str) == Some("interrupted") {
                self.push_notice("■ Response interrupted".into());
            }
        }
    }

    pub fn begin_user_turn(&mut self, prompt: String, turn_id: String) {
        self.push_optimistic_user_message(prompt);
        self.active_turn_id = Some(turn_id);
        self.streaming_message = None;
        self.interrupt_requested = false;
        self.waiting_for_activity = true;
        if self.is_side_chat {
            self.side_chat_has_activity = true;
        }
    }

    pub fn steer_submitted(&mut self, prompt: String) {
        self.pending_steers.push_back(prompt);
        self.selected_skills.clear();
    }

    pub fn pending_steer_count(&self) -> usize {
        self.pending_steers.len()
    }

    pub fn pending_steer_prompts(&self) -> Vec<String> {
        self.pending_steers.iter().cloned().collect()
    }

    pub fn mark_interrupt_requested(&mut self) {
        self.interrupt_requested = true;
    }

    pub fn interrupt_is_requested(&self) -> bool {
        self.interrupt_requested
    }

    fn push_optimistic_user_message(&mut self, prompt: String) {
        self.scroll_to_bottom();
        self.messages.push(ChatMessage {
            role: ChatRole::User,
            content: prompt.clone(),
            item_id: None,
            diff_targets: Vec::new(),
        });
        self.pending_user_message = Some(prompt);
        self.selected_skills.clear();
    }

    pub fn is_waiting_for_activity(&self) -> bool {
        self.active_turn_id.is_some() && self.waiting_for_activity
    }

    pub fn open_palette(&mut self) {
        self.palette = Some(CommandPalette::new(&self.available_skills));
    }

    pub fn select_skill(&mut self, skill: SkillMetadata) {
        self.composer = format!("${} ", skill.name);
        self.composer_cursor = self.composer.len();
        self.composer_preferred_column = None;
        if !self
            .selected_skills
            .iter()
            .any(|selected| selected.path == skill.path)
        {
            self.selected_skills.push(skill);
        }
        self.palette = None;
    }

    pub fn set_composer_width(&mut self, width: usize) {
        self.composer_width = width.max(1);
    }

    pub fn composer_layout(&self) -> ComposerLayout {
        composer_layout(
            &self.composer,
            self.composer_cursor.min(self.composer.len()),
            self.composer_width,
        )
    }

    pub fn clear_composer(&mut self) {
        self.composer.clear();
        self.composer_cursor = 0;
        self.composer_preferred_column = None;
    }

    pub fn insert_composer_char(&mut self, character: char) {
        self.normalize_composer_cursor();
        self.composer.insert(self.composer_cursor, character);
        self.composer_cursor += character.len_utf8();
        self.composer_preferred_column = None;
    }

    pub fn insert_composer_newline(&mut self) {
        self.insert_composer_char('\n');
    }

    pub fn backspace_composer(&mut self) {
        self.normalize_composer_cursor();
        let Some(previous) = previous_grapheme_boundary(&self.composer, self.composer_cursor)
        else {
            return;
        };
        self.composer.drain(previous..self.composer_cursor);
        self.composer_cursor = previous;
        self.composer_preferred_column = None;
    }

    pub fn delete_composer(&mut self) {
        self.normalize_composer_cursor();
        let Some(next) = next_grapheme_boundary(&self.composer, self.composer_cursor) else {
            return;
        };
        self.composer.drain(self.composer_cursor..next);
        self.composer_preferred_column = None;
    }

    pub fn move_composer_left(&mut self) {
        self.normalize_composer_cursor();
        if let Some(previous) = previous_grapheme_boundary(&self.composer, self.composer_cursor) {
            self.composer_cursor = previous;
        }
        self.composer_preferred_column = None;
    }

    pub fn move_composer_right(&mut self) {
        self.normalize_composer_cursor();
        if let Some(next) = next_grapheme_boundary(&self.composer, self.composer_cursor) {
            self.composer_cursor = next;
        }
        self.composer_preferred_column = None;
    }

    pub fn move_composer_line_start(&mut self) {
        self.normalize_composer_cursor();
        self.composer_cursor = self.composer[..self.composer_cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.composer_preferred_column = None;
    }

    pub fn move_composer_line_end(&mut self) {
        self.normalize_composer_cursor();
        self.composer_cursor = self.composer[self.composer_cursor..]
            .find('\n')
            .map_or(self.composer.len(), |index| self.composer_cursor + index);
        self.composer_preferred_column = None;
    }

    pub fn move_composer_up(&mut self) {
        self.move_composer_vertical(false);
    }

    pub fn move_composer_down(&mut self) {
        self.move_composer_vertical(true);
    }

    fn move_composer_vertical(&mut self, down: bool) {
        self.normalize_composer_cursor();
        let layout = self.composer_layout();
        let target_line = if down {
            layout.cursor_line.checked_add(1)
        } else {
            layout.cursor_line.checked_sub(1)
        };
        let Some(target_line) = target_line.filter(|line| *line < layout.lines.len()) else {
            return;
        };
        let preferred_column = self
            .composer_preferred_column
            .unwrap_or(layout.cursor_column);
        let mut best = None;
        for boundary in composer_boundaries(&self.composer) {
            let position = composer_layout(&self.composer, boundary, self.composer_width);
            if position.cursor_line != target_line {
                continue;
            }
            let distance = position.cursor_column.abs_diff(preferred_column);
            if best.is_none_or(|(_, best_distance)| distance < best_distance) {
                best = Some((boundary, distance));
            }
        }
        if let Some((boundary, _)) = best {
            self.composer_cursor = boundary;
            self.composer_preferred_column = Some(preferred_column);
        }
    }

    fn normalize_composer_cursor(&mut self) {
        self.composer_cursor = self.composer_cursor.min(self.composer.len());
        while !self.composer.is_char_boundary(self.composer_cursor) {
            self.composer_cursor -= 1;
        }
    }

    pub fn skills_for_prompt(&self, prompt: &str) -> Vec<SkillMetadata> {
        self.selected_skills
            .iter()
            .filter(|skill| prompt.contains(&format!("${}", skill.name)))
            .cloned()
            .collect()
    }

    pub fn push_notice(&mut self, content: String) {
        self.messages.push(ChatMessage {
            role: ChatRole::Activity,
            content,
            item_id: None,
            diff_targets: Vec::new(),
        });
    }

    pub fn update_scroll_metrics(&mut self, total_lines: usize, viewport_height: usize) {
        self.viewport_height = viewport_height.max(1);
        self.max_scroll = total_lines.saturating_sub(self.viewport_height);
        if self.follow_tail {
            self.scroll_top = self.max_scroll;
        } else {
            self.scroll_top = self.scroll_top.min(self.max_scroll);
        }
    }

    pub fn enter_scroll_mode(&mut self) {
        self.mode = ChatMode::Scroll;
        self.visible_editor_target = None;
        self.selected_message_index = None;
        self.message_selection_scroll_pending = false;
        self.scroll_to_bottom();
    }

    pub fn move_message_selection(&mut self, forward: bool) {
        let selectable = self.selectable_message_indices().collect::<Vec<_>>();
        if selectable.is_empty() {
            self.selected_message_index = None;
            return;
        }
        if self.selected_message_index.is_none() {
            self.selected_message_index = selectable.last().copied();
            self.message_selection_scroll_pending = true;
            return;
        }
        let current = self
            .selected_message_index
            .and_then(|selected| selectable.iter().position(|index| *index == selected))
            .unwrap_or(selectable.len() - 1);
        let next = if forward {
            current.saturating_add(1).min(selectable.len() - 1)
        } else {
            current.saturating_sub(1)
        };
        self.selected_message_index = Some(selectable[next]);
        self.message_selection_scroll_pending = true;
    }

    pub fn selected_message(&self) -> Option<&ChatMessage> {
        self.selected_message_index
            .and_then(|index| self.messages.get(index))
            .filter(|message| !message.content.is_empty())
    }

    pub fn selected_message_position(&self) -> Option<(usize, usize)> {
        let selectable = self.selectable_message_indices().collect::<Vec<_>>();
        let selected = self.selected_message_index?;
        selectable
            .iter()
            .position(|index| *index == selected)
            .map(|position| (position + 1, selectable.len()))
    }

    pub fn conversation_text(&self) -> String {
        self.messages
            .iter()
            .filter(|message| !message.content.is_empty())
            .map(|message| {
                let role = match message.role {
                    ChatRole::User => "User",
                    ChatRole::Assistant => "Assistant",
                    ChatRole::Activity | ChatRole::Diff => "Activity",
                };
                format!("{role}:\n{}", message.content)
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    pub fn take_message_selection_scroll_request(&mut self) -> bool {
        std::mem::take(&mut self.message_selection_scroll_pending)
    }

    pub fn reveal_line_range(&mut self, start: usize, end: usize) {
        if start < self.scroll_top {
            self.scroll_top = start;
        } else if end > self.scroll_top.saturating_add(self.viewport_height) {
            self.scroll_top = end.saturating_sub(self.viewport_height);
        }
        self.scroll_top = self.scroll_top.min(self.max_scroll);
        self.follow_tail = self.scroll_top == self.max_scroll;
    }

    fn selectable_message_indices(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        self.messages
            .iter()
            .enumerate()
            .filter(|(_, message)| !message.content.is_empty())
            .map(|(index, _)| index)
    }

    pub fn scroll_up(&mut self, lines: usize) {
        let next = self.scroll_top.saturating_sub(lines);
        if next != self.scroll_top || !self.follow_tail {
            self.follow_tail = false;
        }
        self.scroll_top = next;
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_top = self.scroll_top.saturating_add(lines).min(self.max_scroll);
        self.follow_tail = self.scroll_top == self.max_scroll;
    }

    pub fn scroll_half_page_up(&mut self) {
        self.scroll_up((self.viewport_height / 2).max(1));
    }

    pub fn scroll_half_page_down(&mut self) {
        self.scroll_down((self.viewport_height / 2).max(1));
    }

    pub fn scroll_page_up(&mut self) {
        self.scroll_up(self.viewport_height);
    }

    pub fn scroll_page_down(&mut self) {
        self.scroll_down(self.viewport_height);
    }

    pub fn scroll_to_top(&mut self) {
        self.follow_tail = false;
        self.scroll_top = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.follow_tail = true;
        self.scroll_top = self.max_scroll;
    }

    pub fn apply(&mut self, event: &AppServerEvent) {
        if event.thread_id.as_deref() != Some(&self.thread_id) {
            return;
        }
        match event.method.as_str() {
            "turn/started" => {
                self.active_turn_id = event
                    .params
                    .pointer("/turn/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            "item/started" => {
                if let Some(item) = event.params.get("item") {
                    if item.get("type").and_then(Value::as_str) != Some("userMessage") {
                        self.waiting_for_activity = false;
                    }
                    self.push_started_item(item);
                }
            }
            "item/agentMessage/delta" => {
                let Some(delta) = event.params.get("delta").and_then(Value::as_str) else {
                    return;
                };
                self.waiting_for_activity = false;
                let index = *self.streaming_message.get_or_insert_with(|| {
                    self.messages.push(ChatMessage {
                        role: ChatRole::Assistant,
                        content: String::new(),
                        item_id: None,
                        diff_targets: Vec::new(),
                    });
                    self.messages.len() - 1
                });
                self.messages[index].content.push_str(delta);
            }
            "item/commandExecution/outputDelta" => {
                self.waiting_for_activity = false;
                if let (Some(item_id), Some(delta)) = (
                    event.params.get("itemId").and_then(Value::as_str),
                    event.params.get("delta").and_then(Value::as_str),
                ) {
                    self.append_activity_output(item_id, delta);
                }
            }
            "item/reasoning/summaryTextDelta" => {
                self.waiting_for_activity = false;
                if let (Some(item_id), Some(delta)) = (
                    event.params.get("itemId").and_then(Value::as_str),
                    event.params.get("delta").and_then(Value::as_str),
                ) {
                    self.append_reasoning_summary(item_id, delta);
                }
            }
            "turn/plan/updated" => {
                self.waiting_for_activity = false;
                self.update_plan(&event.params);
            }
            "item/completed" => {
                if let Some(item) = event.params.get("item") {
                    if item.get("type").and_then(Value::as_str) != Some("userMessage") {
                        self.waiting_for_activity = false;
                    }
                    if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                        if let Some(index) = self.streaming_message.take()
                            && let Some(text) = item.get("text").and_then(Value::as_str)
                        {
                            self.messages[index].content = text.to_owned();
                        } else {
                            self.push_completed_item(item);
                        }
                    } else {
                        self.push_completed_item(item);
                    }
                }
            }
            "turn/completed" => {
                let interrupted = event.params.pointer("/turn/status").and_then(Value::as_str)
                    == Some("interrupted");
                self.active_turn_id = None;
                self.streaming_message = None;
                self.pending_user_message = None;
                self.pending_steers.clear();
                self.interrupt_requested = false;
                self.waiting_for_activity = false;
                if interrupted {
                    self.push_notice("■ Response interrupted".into());
                }
            }
            "error" => {
                self.pending_steers.clear();
                self.interrupt_requested = false;
                self.waiting_for_activity = false;
                let message = event
                    .params
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex error");
                self.messages.push(ChatMessage {
                    role: ChatRole::Activity,
                    content: message.to_owned(),
                    item_id: None,
                    diff_targets: Vec::new(),
                });
            }
            _ => {}
        }
    }

    fn push_completed_item(&mut self, item: &Value) {
        match item.get("type").and_then(Value::as_str) {
            Some("userMessage") => {
                let content = item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|input| input.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|input| input.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !content.is_empty() {
                    if self.pending_user_message.as_deref() == Some(content.as_str()) {
                        self.pending_user_message = None;
                        return;
                    }
                    if let Some(index) = self
                        .pending_steers
                        .iter()
                        .position(|pending| pending == &content)
                    {
                        self.pending_steers.remove(index);
                    }
                    self.messages.push(ChatMessage {
                        role: ChatRole::User,
                        content,
                        item_id: None,
                        diff_targets: Vec::new(),
                    });
                }
            }
            Some("agentMessage") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    self.messages.push(ChatMessage {
                        role: ChatRole::Assistant,
                        content: text.to_owned(),
                        item_id: None,
                        diff_targets: Vec::new(),
                    });
                }
            }
            Some("commandExecution") => self.finish_command(item),
            Some("fileChange") => self.finish_file_change(item),
            Some("mcpToolCall" | "dynamicToolCall" | "collabToolCall") => {
                self.finish_activity(item, "Tool", tool_detail(item))
            }
            Some("webSearch") => self.finish_activity(item, "Search", web_search_detail(item)),
            Some("reasoning") => self.finish_reasoning(item),
            Some("plan") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    self.upsert_activity(item_id(item), format!("Plan: {}", compact(text, 400)));
                }
            }
            _ => {}
        }
    }

    fn push_started_item(&mut self, item: &Value) {
        let Some(id) = item_id(item) else {
            return;
        };
        if item.get("type").and_then(Value::as_str) == Some("fileChange") {
            let display =
                file_change_content(item, &format!("Editing: {}", file_change_detail(item)));
            self.upsert_file_change(Some(id), display);
            return;
        }
        let content = match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => "Thinking…".into(),
            Some("commandExecution") => format!("Running: {}", command_detail(item)),
            Some("mcpToolCall" | "dynamicToolCall" | "collabToolCall") => {
                format!("Tool: {}", tool_detail(item))
            }
            Some("webSearch") => format!("Searching: {}", web_search_detail(item)),
            Some("contextCompaction") => "Compacting context…".into(),
            Some("imageView") => format!(
                "Viewing: {}",
                item.get("path").and_then(Value::as_str).unwrap_or("image")
            ),
            _ => return,
        };
        self.upsert_activity(Some(id), content);
    }

    fn finish_command(&mut self, item: &Value) {
        let header = completed_header(item, &command_detail(item));
        let output = item
            .get("aggregatedOutput")
            .and_then(Value::as_str)
            .map(output_tail)
            .filter(|output| !output.is_empty());
        let content = output.map_or_else(|| header.clone(), |output| format!("{header}\n{output}"));
        self.upsert_activity(item_id(item), content);
    }

    fn finish_activity(&mut self, item: &Value, kind: &str, detail: String) {
        self.upsert_activity(
            item_id(item),
            completed_header(item, &format!("{kind}: {detail}")),
        );
    }

    fn finish_file_change(&mut self, item: &Value) {
        let header = completed_header(item, &format!("Edited: {}", file_change_detail(item)));
        self.upsert_file_change(item_id(item), file_change_content(item, &header));
    }

    fn finish_reasoning(&mut self, item: &Value) {
        let id = item_id(item);
        let summary = reasoning_summary(item);
        if summary.is_empty() {
            self.upsert_activity(id, "Thought through the task".into());
        } else {
            self.upsert_activity(id, format!("Thought\n{}", compact(&summary, 800)));
        }
    }

    fn append_activity_output(&mut self, item_id: &str, delta: &str) {
        let index = self.activity_index(item_id).unwrap_or_else(|| {
            self.upsert_activity(Some(item_id), "Running command".into());
            self.messages.len() - 1
        });
        let header = self.messages[index]
            .content
            .lines()
            .next()
            .unwrap_or("Running command")
            .to_owned();
        let previous = self.messages[index]
            .content
            .split_once('\n')
            .map(|(_, output)| output)
            .unwrap_or("");
        self.messages[index].content =
            format!("{header}\n{}", output_tail(&format!("{previous}{delta}")));
    }

    fn append_reasoning_summary(&mut self, item_id: &str, delta: &str) {
        let index = self.activity_index(item_id).unwrap_or_else(|| {
            self.upsert_activity(Some(item_id), "Thinking…".into());
            self.messages.len() - 1
        });
        let previous = self.messages[index]
            .content
            .strip_prefix("Thinking…\n")
            .unwrap_or_default();
        let summary = compact(&format!("{previous}{delta}"), 800);
        self.messages[index].content = format!("Thinking…\n{summary}");
    }

    fn update_plan(&mut self, params: &Value) {
        let Some(plan) = params.get("plan").and_then(Value::as_array) else {
            return;
        };
        let step = plan
            .iter()
            .find(|step| step.get("status").and_then(Value::as_str) == Some("inProgress"))
            .or_else(|| {
                plan.iter()
                    .find(|step| step.get("status").and_then(Value::as_str) == Some("pending"))
            })
            .or_else(|| plan.last());
        let Some(step) = step else {
            return;
        };
        let text = step
            .get("step")
            .and_then(Value::as_str)
            .unwrap_or("Planning");
        let status = step
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let key = format!(
            "plan:{}",
            params
                .get("turnId")
                .and_then(Value::as_str)
                .unwrap_or("active")
        );
        self.upsert_activity(
            Some(&key),
            format!("Plan: {text} [{}]", status_label(status)),
        );
    }

    fn activity_index(&self, item_id: &str) -> Option<usize> {
        self.messages.iter().position(|message| {
            message.role == ChatRole::Activity && message.item_id.as_deref() == Some(item_id)
        })
    }

    fn upsert_activity(&mut self, item_id: Option<&str>, content: String) {
        if let Some(item_id) = item_id
            && let Some(index) = self.activity_index(item_id)
        {
            self.messages[index].content = content;
            return;
        }
        self.messages.push(ChatMessage {
            role: ChatRole::Activity,
            content,
            item_id: item_id.map(str::to_owned),
            diff_targets: Vec::new(),
        });
    }

    fn upsert_file_change(&mut self, item_id: Option<&str>, display: FileChangeDisplay) {
        if let Some(item_id) = item_id
            && let Some(index) = self
                .messages
                .iter()
                .position(|message| message.item_id.as_deref() == Some(item_id))
        {
            self.messages[index].role = ChatRole::Diff;
            self.messages[index].content = display.content;
            self.messages[index].diff_targets = display.targets;
            return;
        }
        self.messages.push(ChatMessage {
            role: ChatRole::Diff,
            content: display.content,
            item_id: item_id.map(str::to_owned),
            diff_targets: display.targets,
        });
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposerLayout {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_column: usize,
}

fn composer_layout(composer: &str, cursor: usize, max_width: usize) -> ComposerLayout {
    let max_width = max_width.max(1);
    let cursor = cursor.min(composer.len());
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut width = 0;
    let mut cursor_position = None;

    for (index, grapheme) in composer.grapheme_indices(true) {
        if grapheme == "\n" {
            if index == cursor {
                cursor_position = Some((lines.len(), width));
            }
            lines.push(std::mem::take(&mut current));
            width = 0;
            continue;
        }
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > max_width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            width = 0;
        }
        if index == cursor {
            cursor_position = Some((lines.len(), width));
        }
        current.push_str(grapheme);
        width += grapheme_width;
    }

    if cursor == composer.len() && width == max_width && !current.is_empty() {
        lines.push(std::mem::take(&mut current));
        width = 0;
    }
    let cursor_position = cursor_position.unwrap_or((lines.len(), width));
    lines.push(current);
    ComposerLayout {
        lines,
        cursor_line: cursor_position.0,
        cursor_column: cursor_position.1,
    }
}

fn composer_boundaries(composer: &str) -> impl Iterator<Item = usize> + '_ {
    std::iter::once(0).chain(
        composer
            .grapheme_indices(true)
            .map(|(index, grapheme)| index + grapheme.len()),
    )
}

fn previous_grapheme_boundary(composer: &str, cursor: usize) -> Option<usize> {
    composer_boundaries(composer)
        .take_while(|boundary| *boundary < cursor)
        .last()
}

fn next_grapheme_boundary(composer: &str, cursor: usize) -> Option<usize> {
    composer_boundaries(composer).find(|boundary| *boundary > cursor)
}

fn item_id(item: &Value) -> Option<&str> {
    item.get("id").and_then(Value::as_str)
}

fn command_detail(item: &Value) -> String {
    compact(
        item.get("command")
            .and_then(Value::as_str)
            .unwrap_or("command"),
        240,
    )
}

fn file_change_detail(item: &Value) -> String {
    let paths = item
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|change| change.get("path").and_then(Value::as_str))
        .take(3)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        "files".into()
    } else {
        paths.join(", ")
    }
}

struct FileChangeDisplay {
    content: String,
    targets: Vec<DiffTarget>,
}

fn file_change_content(item: &Value, header: &str) -> FileChangeDisplay {
    let mut parts = vec![header.to_owned()];
    let mut targets = Vec::new();
    let mut content_line = 1;
    for change in item
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(diff) = change
            .get("diff")
            .and_then(Value::as_str)
            .filter(|diff| !diff.is_empty())
        else {
            continue;
        };
        if let Some(path) = change.get("path").and_then(Value::as_str) {
            targets.extend(diff.split('\n').enumerate().filter_map(|(offset, line)| {
                parse_hunk_new_line(line).map(|line| DiffTarget {
                    content_line: content_line + offset,
                    editor: EditorTarget {
                        path: PathBuf::from(path),
                        line,
                    },
                })
            }));
        }
        parts.push(diff.to_owned());
        content_line += diff.split('\n').count();
    }
    FileChangeDisplay {
        content: parts.join("\n"),
        targets,
    }
}

fn parse_hunk_new_line(line: &str) -> Option<usize> {
    let range = line
        .strip_prefix("@@ ")?
        .split_whitespace()
        .find(|part| part.starts_with('+'))?;
    range
        .trim_start_matches('+')
        .split(',')
        .next()?
        .parse()
        .ok()
}

fn tool_detail(item: &Value) -> String {
    let tool = item.get("tool").and_then(Value::as_str).unwrap_or("tool");
    item.get("server")
        .and_then(Value::as_str)
        .map_or_else(|| tool.to_owned(), |server| format!("{server}/{tool}"))
}

fn web_search_detail(item: &Value) -> String {
    item.get("query")
        .and_then(Value::as_str)
        .unwrap_or("web")
        .to_owned()
}

fn completed_header(item: &Value, detail: &str) -> String {
    let status = item
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed");
    let mark = if status == "completed" { "✓" } else { "✗" };
    if status == "completed" {
        format!("{mark} {detail}")
    } else {
        format!("{mark} {detail} [{}]", status_label(status))
    }
}

fn status_label(status: &str) -> &str {
    match status {
        "inProgress" => "in progress",
        value => value,
    }
}

fn reasoning_summary(item: &Value) -> String {
    item.get("summary")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| {
            part.as_str()
                .or_else(|| part.get("text").and_then(Value::as_str))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn compact(value: &str, max_chars: usize) -> String {
    let clean = value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>();
    if clean.chars().count() <= max_chars {
        clean
    } else {
        format!(
            "…{}",
            clean
                .chars()
                .skip(clean.chars().count() - max_chars)
                .collect::<String>()
        )
    }
}

fn output_tail(output: &str) -> String {
    let clean = compact(output, 2_000);
    let lines = clean.lines().rev().take(3).collect::<Vec<_>>();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reduces_streaming_agent_messages() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        for delta in ["hello", " world"] {
            chat.apply(&AppServerEvent {
                method: "item/agentMessage/delta".into(),
                params: json!({"threadId":"t", "delta":delta}),
                thread_id: Some("t".into()),
                turn_id: Some("u".into()),
            });
        }
        chat.apply(&AppServerEvent {
            method: "item/completed".into(),
            params: json!({"threadId":"t", "item":{"type":"agentMessage", "text":"hello world"}}),
            thread_id: Some("t".into()),
            turn_id: Some("u".into()),
        });
        assert_eq!(chat.messages[0].content, "hello world");
    }

    #[test]
    fn loads_user_and_agent_history() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.load_history(&json!({"thread":{"turns":[{"items":[
            {"type":"userMessage","content":[{"type":"text","text":"question"}]},
            {"type":"agentMessage","text":"answer"}
        ]}]}}));
        assert_eq!(chat.messages.len(), 2);
        assert_eq!(chat.messages[0].role, ChatRole::User);
        assert_eq!(chat.messages[1].role, ChatRole::Assistant);
    }

    #[test]
    fn restores_interrupted_turn_notice_from_history() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.load_history(&json!({"thread":{"turns":[{
            "status":"interrupted",
            "items":[{"type":"userMessage","content":[{"type":"text","text":"question"}]}]
        }]}}));

        assert_eq!(chat.messages.len(), 2);
        assert_eq!(chat.messages[1].role, ChatRole::Activity);
        assert_eq!(chat.messages[1].content, "■ Response interrupted");
    }

    #[test]
    fn restores_in_progress_history_and_accepts_new_streaming_events() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.load_history(&json!({"thread":{"turns":[{
            "id":"turn-1",
            "status":"inProgress",
            "items":[
                {"id":"user-1","type":"userMessage","content":[{"type":"text","text":"question"}]},
                {"id":"edit-1","type":"fileChange","changes":[{
                    "path":"src/main.rs",
                    "kind":"update",
                    "diff":"@@ -1 +1 @@\n-old\n+new"
                }],"status":"inProgress"}
            ]
        }]}}));

        assert_eq!(chat.active_turn_id.as_deref(), Some("turn-1"));
        assert_eq!(chat.messages.len(), 2);
        assert_eq!(chat.messages[1].role, ChatRole::Diff);
        assert!(chat.messages[1].content.starts_with("Editing: src/main.rs"));

        chat.apply(&event(
            "item/agentMessage/delta",
            json!({"turnId":"turn-1","delta":"response"}),
        ));

        assert_eq!(chat.messages.len(), 3);
        assert_eq!(chat.messages[2].content, "response");
    }

    #[test]
    fn only_an_untitled_main_chat_without_messages_is_unused() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "Untitled thread".into());
        assert!(chat.is_unused_main_thread());

        chat.begin_user_turn("question".into(), "turn".into());
        assert!(!chat.is_unused_main_thread());

        let mut titled = ChatState::new("t".into(), "/tmp".into(), "Existing".into());
        assert!(!titled.is_unused_main_thread());
        titled.title = "Untitled thread".into();
        titled.mark_as_side_chat();
        assert!(!titled.is_unused_main_thread());
    }

    #[test]
    fn does_not_duplicate_the_optimistic_user_message() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("question".into(), "u".into());
        chat.apply(&AppServerEvent {
            method: "item/completed".into(),
            params: json!({
                "threadId":"t",
                "turnId":"u",
                "item":{"type":"userMessage","content":[{"type":"text","text":"question"}]}
            }),
            thread_id: Some("t".into()),
            turn_id: Some("u".into()),
        });
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].content, "question");
        assert!(chat.is_waiting_for_activity());
    }

    #[test]
    fn steered_user_messages_follow_the_server_event_order() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("question".into(), "u".into());
        chat.apply(&event(
            "item/completed",
            json!({
                "item":{"type":"userMessage","content":[{"type":"text","text":"question"}]}
            }),
        ));
        chat.apply(&event(
            "item/completed",
            json!({"item":{"type":"agentMessage","text":"first response"}}),
        ));
        chat.steer_submitted("focus on tests".into());
        assert_eq!(chat.pending_steer_count(), 1);
        assert_eq!(chat.pending_steer_prompts(), ["focus on tests"]);
        chat.apply(&event(
            "item/completed",
            json!({
                "item":{"type":"userMessage","content":[{"type":"text","text":"focus on tests"}]}
            }),
        ));
        chat.apply(&event(
            "item/completed",
            json!({"item":{"type":"agentMessage","text":"steered response"}}),
        ));

        assert_eq!(chat.messages.len(), 4);
        assert_eq!(chat.messages[0].content, "question");
        assert_eq!(chat.messages[1].content, "first response");
        assert_eq!(chat.messages[2].content, "focus on tests");
        assert_eq!(chat.messages[3].content, "steered response");
        assert_eq!(chat.pending_steer_count(), 0);
        assert!(chat.pending_steer_prompts().is_empty());
    }

    #[test]
    fn interrupted_turn_clears_live_state_and_adds_notice() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("question".into(), "u".into());
        chat.steer_submitted("first".into());
        chat.steer_submitted("second".into());
        chat.mark_interrupt_requested();
        assert_eq!(chat.pending_steer_count(), 2);
        assert!(chat.interrupt_is_requested());

        chat.apply(&event(
            "turn/completed",
            json!({"turn":{"id":"u","status":"interrupted"}}),
        ));

        assert_eq!(chat.pending_steer_count(), 0);
        assert!(!chat.interrupt_is_requested());
        assert_eq!(chat.messages.last().unwrap().role, ChatRole::Activity);
        assert_eq!(
            chat.messages.last().unwrap().content,
            "■ Response interrupted"
        );
    }

    #[test]
    fn completed_turn_does_not_add_an_interruption_notice() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("question".into(), "u".into());
        let message_count = chat.messages.len();

        chat.apply(&event(
            "turn/completed",
            json!({"turn":{"id":"u","status":"completed"}}),
        ));

        assert_eq!(chat.messages.len(), message_count);
    }

    #[test]
    fn thinking_waits_for_the_first_non_user_activity() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("question".into(), "u".into());
        assert!(chat.is_waiting_for_activity());

        chat.apply(&event("turn/started", json!({"turn":{"id":"u"}})));
        assert!(chat.is_waiting_for_activity());

        chat.apply(&event(
            "item/started",
            json!({"item":{"id":"reason-1","type":"reasoning"}}),
        ));
        assert!(!chat.is_waiting_for_activity());
        assert_eq!(chat.messages.last().unwrap().content, "Thinking…");
    }

    #[test]
    fn updates_command_activity_in_place() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.apply(&event(
            "item/started",
            json!({"item":{"id":"cmd-1","type":"commandExecution","command":"cargo test","status":"inProgress"}}),
        ));
        chat.apply(&event(
            "item/commandExecution/outputDelta",
            json!({"itemId":"cmd-1","delta":"first\nsecond\n"}),
        ));
        chat.apply(&event(
            "item/completed",
            json!({"item":{"id":"cmd-1","type":"commandExecution","command":"cargo test","status":"completed","aggregatedOutput":"first\nsecond\n"}}),
        ));

        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].role, ChatRole::Activity);
        assert_eq!(chat.messages[0].content, "✓ cargo test\nfirst\nsecond");
    }

    #[test]
    fn file_change_keeps_the_app_server_diff_and_updates_in_place() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        let changes = json!([{
            "path":"src/main.rs",
            "kind":"update",
            "diff":"@@ -1 +1 @@\n-old\n+new"
        }]);
        chat.apply(&event(
            "item/started",
            json!({"item":{"id":"edit-1","type":"fileChange","changes":changes,"status":"inProgress"}}),
        ));

        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].role, ChatRole::Diff);
        assert_eq!(
            chat.messages[0].content,
            "Editing: src/main.rs\n@@ -1 +1 @@\n-old\n+new"
        );
        assert_eq!(chat.messages[0].diff_targets.len(), 1);
        assert_eq!(chat.messages[0].diff_targets[0].content_line, 1);
        assert_eq!(chat.messages[0].diff_targets[0].editor.line, 1);
        assert_eq!(
            chat.messages[0].diff_targets[0].editor.path,
            PathBuf::from("src/main.rs")
        );

        chat.apply(&event(
            "item/completed",
            json!({"item":{"id":"edit-1","type":"fileChange","changes":changes,"status":"completed"}}),
        ));
        assert_eq!(chat.messages.len(), 1);
        assert_eq!(
            chat.messages[0].content,
            "✓ Edited: src/main.rs\n@@ -1 +1 @@\n-old\n+new"
        );
    }

    #[test]
    fn parses_new_file_lines_from_unified_diff_hunks() {
        assert_eq!(parse_hunk_new_line("@@ -10,4 +20,7 @@ fn test"), Some(20));
        assert_eq!(parse_hunk_new_line("@@ -1 +1 @@"), Some(1));
        assert_eq!(parse_hunk_new_line("+not a hunk"), None);
    }

    #[test]
    fn shows_public_reasoning_summary_but_ignores_raw_reasoning() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.apply(&event(
            "item/started",
            json!({"item":{"id":"reason-1","type":"reasoning"}}),
        ));
        chat.apply(&event(
            "item/reasoning/summaryTextDelta",
            json!({"itemId":"reason-1","delta":"Checking the code"}),
        ));
        chat.apply(&event(
            "item/reasoning/textDelta",
            json!({"itemId":"reason-1","delta":"hidden reasoning"}),
        ));

        assert_eq!(chat.messages.len(), 1);
        assert!(chat.messages[0].content.contains("Checking the code"));
        assert!(!chat.messages[0].content.contains("hidden reasoning"));
    }

    #[test]
    fn updates_plan_in_place() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.apply(&event(
            "turn/plan/updated",
            json!({"turnId":"u","plan":[{"step":"Inspect","status":"inProgress"}]}),
        ));
        chat.apply(&event(
            "turn/plan/updated",
            json!({"turnId":"u","plan":[{"step":"Inspect","status":"completed"},{"step":"Test","status":"inProgress"}]}),
        ));

        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].content, "Plan: Test [in progress]");
    }

    #[test]
    fn keeps_only_the_tail_of_command_output() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.apply(&event(
            "item/started",
            json!({"item":{"id":"cmd-1","type":"commandExecution","command":"build"}}),
        ));
        chat.apply(&event(
            "item/commandExecution/outputDelta",
            json!({"itemId":"cmd-1","delta":"one\ntwo\nthree\nfour\n"}),
        ));

        assert_eq!(chat.messages[0].content, "Running: build\ntwo\nthree\nfour");
    }

    #[test]
    fn scroll_stops_and_resumes_tail_following() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.update_scroll_metrics(100, 20);
        assert_eq!(chat.scroll_top, 80);
        assert!(chat.follow_tail);

        chat.scroll_up(1);
        assert_eq!(chat.scroll_top, 79);
        assert!(!chat.follow_tail);

        chat.update_scroll_metrics(110, 20);
        assert_eq!(chat.scroll_top, 79);
        assert_eq!(chat.max_scroll, 90);

        chat.scroll_to_bottom();
        chat.update_scroll_metrics(120, 20);
        assert_eq!(chat.scroll_top, 100);
        assert!(chat.follow_tail);
    }

    #[test]
    fn submitting_a_message_resumes_tail_following() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.update_scroll_metrics(100, 20);
        chat.scroll_up(10);
        assert!(!chat.follow_tail);

        chat.begin_user_turn("new question".into(), "turn".into());
        chat.update_scroll_metrics(110, 20);

        assert_eq!(chat.scroll_top, 90);
        assert!(chat.follow_tail);
    }

    #[test]
    fn scroll_uses_viewport_sized_steps_and_clamps_to_bounds() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.update_scroll_metrics(100, 20);

        chat.scroll_half_page_up();
        assert_eq!(chat.scroll_top, 70);
        chat.scroll_page_up();
        assert_eq!(chat.scroll_top, 50);
        chat.scroll_to_top();
        chat.scroll_page_up();
        assert_eq!(chat.scroll_top, 0);

        chat.scroll_page_down();
        assert_eq!(chat.scroll_top, 20);
        chat.scroll_to_bottom();
        assert_eq!(chat.scroll_top, 80);
        assert!(chat.follow_tail);
    }

    #[test]
    fn message_navigation_selects_lazily_and_clamps_to_bounds() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.push_notice("first".into());
        chat.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: String::new(),
            item_id: None,
            diff_targets: Vec::new(),
        });
        chat.push_notice("last".into());

        chat.enter_scroll_mode();
        assert_eq!(chat.selected_message(), None);
        assert_eq!(chat.selected_message_position(), None);

        chat.move_message_selection(false);
        assert_eq!(
            chat.selected_message()
                .map(|message| message.content.as_str()),
            Some("last")
        );
        assert_eq!(chat.selected_message_position(), Some((2, 2)));

        chat.move_message_selection(false);
        chat.move_message_selection(false);
        assert_eq!(
            chat.selected_message()
                .map(|message| message.content.as_str()),
            Some("first")
        );
        assert_eq!(chat.selected_message_position(), Some((1, 2)));

        chat.scroll_to_top();
        chat.mode = ChatMode::Input;
        chat.enter_scroll_mode();
        assert_eq!(chat.selected_message(), None);
        assert_eq!(chat.selected_message_position(), None);
        assert_eq!(chat.scroll_top, chat.max_scroll);
        assert!(chat.follow_tail);

        chat.move_message_selection(true);
        chat.move_message_selection(true);
        assert_eq!(
            chat.selected_message()
                .map(|message| message.content.as_str()),
            Some("last")
        );
    }

    #[test]
    fn conversation_copy_uses_raw_text_and_role_labels() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.begin_user_turn("long input without display wrapping".into(), "turn".into());
        chat.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            content: "answer\nsecond line".into(),
            item_id: None,
            diff_targets: Vec::new(),
        });

        assert_eq!(
            chat.conversation_text(),
            "User:\nlong input without display wrapping\n\nAssistant:\nanswer\nsecond line"
        );
    }

    #[test]
    fn command_palette_filters_commands_and_skills() {
        let skills = vec![
            SkillMetadata {
                name: "review".into(),
                description: "Review changes".into(),
                path: "/tmp/review/SKILL.md".into(),
                scope: "user".into(),
            },
            SkillMetadata {
                name: "gh-fix-ci".into(),
                description: "Fix GitHub Actions".into(),
                path: "/tmp/gh-fix-ci/SKILL.md".into(),
                scope: "user".into(),
            },
        ];
        let mut palette = CommandPalette::new(&skills);
        palette.push_query('r');
        palette.push_query('e');
        palette.push_query('v');

        let visible = palette.visible_entries();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].label(), "$review");

        palette.query = "gfc".into();
        let visible = palette.visible_entries();
        assert_eq!(visible[0].label(), "$gh-fix-ci");
    }

    #[test]
    fn fuzzy_search_prioritizes_name_and_prefix_matches() {
        assert!(fuzzy_score("review", "review") > fuzzy_score("code-review", "review"));
        assert!(fuzzy_score("review", "rev") > fuzzy_score("hunk-review", "rev"));
        assert!(fuzzy_score("gh-fix-ci", "gfc").is_some());
        assert!(fuzzy_score("oracle", "xyz").is_none());
    }

    #[test]
    fn command_palette_exposes_side_chat_actions() {
        let mut palette = CommandPalette::new(&[]);
        palette.query = "sidechat".into();
        assert_eq!(palette.visible_entries()[0].label(), "/sidechat");

        palette.query = "sides".into();
        assert_eq!(palette.visible_entries()[0].label(), "/sides");

        palette.query = "sideclose".into();
        assert_eq!(palette.visible_entries()[0].label(), "/sideclose");

        palette.query = "sidepromote".into();
        assert_eq!(palette.visible_entries()[0].label(), "/sidepromote");

        palette.query = "attention".into();
        assert_eq!(palette.visible_entries()[0].label(), "/attention");
    }

    #[test]
    fn side_chat_tracks_only_new_user_activity() {
        let mut chat = ChatState::new("side".into(), "/tmp".into(), "Sidechat 1".into());
        chat.load_history(&json!({"thread":{"turns":[{"items":[]}]}}));
        chat.mark_as_side_chat();
        assert!(!chat.side_chat_has_activity);

        chat.begin_user_turn("new question".into(), "turn".into());
        assert!(chat.side_chat_has_activity);

        chat.mark_as_main_chat();
        assert!(!chat.is_side_chat);
        assert!(!chat.side_chat_has_activity);
    }

    #[test]
    fn selected_skill_is_attached_only_while_mentioned() {
        let skill = SkillMetadata {
            name: "review".into(),
            description: "Review changes".into(),
            path: "/tmp/review/SKILL.md".into(),
            scope: "user".into(),
        };
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.select_skill(skill);

        assert_eq!(chat.composer, "$review ");
        assert_eq!(chat.skills_for_prompt("$review inspect this").len(), 1);
        assert!(chat.skills_for_prompt("inspect this").is_empty());
    }

    #[test]
    fn composer_edits_at_the_cursor_without_splitting_graphemes() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        for character in "a🇯🇵b".chars() {
            chat.insert_composer_char(character);
        }

        chat.move_composer_left();
        chat.backspace_composer();
        chat.insert_composer_char('X');
        assert_eq!(chat.composer, "aXb");

        chat.delete_composer();
        assert_eq!(chat.composer, "aX");
    }

    #[test]
    fn composer_moves_to_logical_line_boundaries() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        for character in "one\ntwo".chars() {
            chat.insert_composer_char(character);
        }

        chat.move_composer_line_start();
        chat.insert_composer_char('X');
        chat.move_composer_line_end();
        chat.insert_composer_char('!');

        assert_eq!(chat.composer, "one\nXtwo!");
    }

    #[test]
    fn composer_vertical_movement_follows_wrapped_display_lines() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.set_composer_width(4);
        for character in "abcdef".chars() {
            chat.insert_composer_char(character);
        }
        assert_eq!(chat.composer_layout().cursor_line, 1);

        chat.move_composer_up();
        chat.insert_composer_char('X');

        assert_eq!(chat.composer, "abXcdef");
        assert_eq!(chat.composer_layout().cursor_column, 3);
    }

    #[test]
    fn composer_layout_preserves_explicit_newlines_and_wide_characters() {
        let mut chat = ChatState::new("t".into(), "/tmp".into(), "test".into());
        chat.set_composer_width(4);
        for character in "日本\n語".chars() {
            chat.insert_composer_char(character);
        }

        assert_eq!(
            chat.composer_layout(),
            ComposerLayout {
                lines: vec!["日本".into(), "語".into()],
                cursor_line: 1,
                cursor_column: 2,
            }
        );
    }

    fn event(method: &str, params: Value) -> AppServerEvent {
        AppServerEvent {
            method: method.into(),
            params,
            thread_id: Some("t".into()),
            turn_id: Some("u".into()),
        }
    }
}
