use std::path::PathBuf;

use serde_json::Value;

use crate::app_server::{AppServerEvent, SkillMetadata};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatRole {
    User,
    Assistant,
    Activity,
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
    Status,
}

impl PaletteCommand {
    pub fn label(self) -> &'static str {
        match self {
            Self::Threads => "/threads",
            Self::Scroll => "/scroll",
            Self::Status => "/status",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Threads => "Return to the thread list",
            Self::Scroll => "Enter chat scroll mode",
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

fn fuzzy_score(candidate: &str, query: &str) -> Option<i32> {
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
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    item_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ChatState {
    pub thread_id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub messages: Vec<ChatMessage>,
    pub composer: String,
    pub active_turn_id: Option<String>,
    pub mode: ChatMode,
    pub scroll_top: usize,
    pub max_scroll: usize,
    pub viewport_height: usize,
    pub follow_tail: bool,
    pub palette: Option<CommandPalette>,
    pub available_skills: Vec<SkillMetadata>,
    pub selected_skills: Vec<SkillMetadata>,
    pub skills_loaded: bool,
    pub skills_stale: bool,
    streaming_message: Option<usize>,
    pending_user_message: Option<String>,
}

impl ChatState {
    pub fn new(thread_id: String, cwd: PathBuf, title: String) -> Self {
        Self {
            thread_id,
            cwd,
            title,
            messages: Vec::new(),
            composer: String::new(),
            active_turn_id: None,
            mode: ChatMode::Input,
            scroll_top: 0,
            max_scroll: 0,
            viewport_height: 1,
            follow_tail: true,
            palette: None,
            available_skills: Vec::new(),
            selected_skills: Vec::new(),
            skills_loaded: false,
            skills_stale: false,
            streaming_message: None,
            pending_user_message: None,
        }
    }

    pub fn load_history(&mut self, response: &Value) {
        self.messages.clear();
        let Some(turns) = response.pointer("/thread/turns").and_then(Value::as_array) else {
            return;
        };
        for item in turns
            .iter()
            .filter_map(|turn| turn.get("items").and_then(Value::as_array))
            .flatten()
        {
            self.push_completed_item(item);
        }
    }

    pub fn begin_user_turn(&mut self, prompt: String, turn_id: String) {
        self.messages.push(ChatMessage {
            role: ChatRole::User,
            content: prompt.clone(),
            item_id: None,
        });
        self.pending_user_message = Some(prompt);
        self.active_turn_id = Some(turn_id);
        self.streaming_message = None;
        self.selected_skills.clear();
    }

    pub fn open_palette(&mut self) {
        self.palette = Some(CommandPalette::new(&self.available_skills));
    }

    pub fn select_skill(&mut self, skill: SkillMetadata) {
        self.composer = format!("${} ", skill.name);
        if !self
            .selected_skills
            .iter()
            .any(|selected| selected.path == skill.path)
        {
            self.selected_skills.push(skill);
        }
        self.palette = None;
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
                    self.push_started_item(item);
                }
            }
            "item/agentMessage/delta" => {
                let Some(delta) = event.params.get("delta").and_then(Value::as_str) else {
                    return;
                };
                let index = *self.streaming_message.get_or_insert_with(|| {
                    self.messages.push(ChatMessage {
                        role: ChatRole::Assistant,
                        content: String::new(),
                        item_id: None,
                    });
                    self.messages.len() - 1
                });
                self.messages[index].content.push_str(delta);
            }
            "item/commandExecution/outputDelta" => {
                if let (Some(item_id), Some(delta)) = (
                    event.params.get("itemId").and_then(Value::as_str),
                    event.params.get("delta").and_then(Value::as_str),
                ) {
                    self.append_activity_output(item_id, delta);
                }
            }
            "item/reasoning/summaryTextDelta" => {
                if let (Some(item_id), Some(delta)) = (
                    event.params.get("itemId").and_then(Value::as_str),
                    event.params.get("delta").and_then(Value::as_str),
                ) {
                    self.append_reasoning_summary(item_id, delta);
                }
            }
            "turn/plan/updated" => self.update_plan(&event.params),
            "item/completed" => {
                if let Some(item) = event.params.get("item") {
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
                self.active_turn_id = None;
                self.streaming_message = None;
                self.pending_user_message = None;
            }
            "error" => {
                let message = event
                    .params
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("Codex error");
                self.messages.push(ChatMessage {
                    role: ChatRole::Activity,
                    content: message.to_owned(),
                    item_id: None,
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
                    self.messages.push(ChatMessage {
                        role: ChatRole::User,
                        content,
                        item_id: None,
                    });
                }
            }
            Some("agentMessage") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    self.messages.push(ChatMessage {
                        role: ChatRole::Assistant,
                        content: text.to_owned(),
                        item_id: None,
                    });
                }
            }
            Some("commandExecution") => self.finish_command(item),
            Some("fileChange") => self.finish_activity(item, "Edited", file_change_detail(item)),
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
        let content = match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => "Thinking…".into(),
            Some("commandExecution") => format!("Running: {}", command_detail(item)),
            Some("fileChange") => format!("Editing: {}", file_change_detail(item)),
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
        });
    }
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

    fn event(method: &str, params: Value) -> AppServerEvent {
        AppServerEvent {
            method: method.into(),
            params,
            thread_id: Some("t".into()),
            turn_id: Some("u".into()),
        }
    }
}
