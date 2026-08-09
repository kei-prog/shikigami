use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

use crate::{paths, settings::ExecutionMode};

type PendingResponses = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

enum OutgoingMessage {
    Json(Value),
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct AppServerEvent {
    pub method: String,
    pub params: Value,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Debug)]
pub struct AppServerRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortMetadata {
    pub reasoning_effort: String,
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelMetadata {
    pub id: String,
    pub model: String,
    pub display_name: String,
    pub description: String,
    pub default_reasoning_effort: String,
    pub supported_reasoning_efforts: Vec<ReasoningEffortMetadata>,
    #[serde(default = "default_input_modalities")]
    pub input_modalities: Vec<String>,
    pub is_default: bool,
}

impl ModelMetadata {
    pub fn supports_images(&self) -> bool {
        self.input_modalities
            .iter()
            .any(|modality| modality == "image")
    }
}

fn default_input_modalities() -> Vec<String> {
    vec!["text".into(), "image".into()]
}

#[derive(Clone, Copy, Debug)]
pub struct TurnSettings<'a> {
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub execution_mode: ExecutionMode,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelListResponse {
    data: Vec<ModelMetadata>,
    next_cursor: Option<String>,
}

pub struct AppServer {
    writer: mpsc::Sender<OutgoingMessage>,
    writer_task: Mutex<Option<JoinHandle<()>>>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
    child: Mutex<Option<Child>>,
    pending: PendingResponses,
    next_id: AtomicU64,
    events: broadcast::Sender<AppServerEvent>,
    server_requests: Mutex<mpsc::Receiver<AppServerRequest>>,
    version: String,
    request_timeout: Duration,
}

impl AppServer {
    pub async fn spawn(command: &str, request_timeout: Duration) -> Result<Arc<Self>> {
        let version_output = Command::new(command)
            .arg("--version")
            .output()
            .await
            .with_context(|| format!("run {command} --version"))?;
        if !version_output.status.success() {
            bail!("{command} --version failed");
        }
        let version = String::from_utf8_lossy(&version_output.stdout)
            .trim()
            .to_owned();
        let cache_dir = paths::project_dirs()?.cache_dir().to_path_buf();
        fs::create_dir_all(&cache_dir).with_context(|| {
            format!("create App Server cache directory {}", cache_dir.display())
        })?;
        let log_path = cache_dir.join("app-server.log");
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_path)
            .with_context(|| format!("open App Server log {}", log_path.display()))?;
        let mut child = Command::new(command)
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start {command} app-server"))?;
        let mut child_stdin = child.stdin.take().context("open App Server stdin")?;
        let child_stdout = child.stdout.take().context("open App Server stdout")?;
        let (writer, mut writer_rx) = mpsc::channel::<OutgoingMessage>(128);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(1024);
        let (request_tx, request_rx) = mpsc::channel(128);

        let writer_task = tokio::spawn(async move {
            while let Some(message) = writer_rx.recv().await {
                match message {
                    OutgoingMessage::Json(message) => {
                        let Ok(mut encoded) = serde_json::to_vec(&message) else {
                            continue;
                        };
                        encoded.push(b'\n');
                        if child_stdin.write_all(&encoded).await.is_err()
                            || child_stdin.flush().await.is_err()
                        {
                            break;
                        }
                    }
                    OutgoingMessage::Shutdown => break,
                }
            }
        });

        let reader_pending = Arc::clone(&pending);
        let reader_events = events.clone();
        let reader_task = tokio::spawn(async move {
            let mut lines = BufReader::new(child_stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                dispatch_message(
                    &reader_pending,
                    &reader_events,
                    &request_tx,
                    line.as_bytes(),
                )
                .await;
            }
            let mut pending = reader_pending.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err("Codex app-server exited".into()));
            }
            let _ = reader_events.send(AppServerEvent {
                method: "shikigami/processExited".into(),
                params: Value::Null,
                thread_id: None,
                turn_id: None,
            });
        });

        let server = Arc::new(Self {
            writer,
            writer_task: Mutex::new(Some(writer_task)),
            reader_task: Mutex::new(Some(reader_task)),
            child: Mutex::new(Some(child)),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            events: events.clone(),
            server_requests: Mutex::new(request_rx),
            version,
            request_timeout,
        });

        if let Err(error) = server.initialize().await {
            let _ = server.shutdown().await;
            return Err(error);
        }
        Ok(server)
    }

    async fn initialize(&self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "shikigami",
                    "title": "Shikigami",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {"experimentalApi": true}
            }),
        )
        .await
        .context("initialize Codex app-server")?;
        self.notify("initialized", json!({})).await
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AppServerEvent> {
        self.events.subscribe()
    }

    pub async fn next_server_request(&self) -> Option<AppServerRequest> {
        self.server_requests.lock().await.recv().await
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        if let Err(error) = self
            .write(&json!({"id": id, "method": method, "params": params}))
            .await
        {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match timeout(self.request_timeout, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => Err(anyhow!("Codex {method} error: {error}")),
            Ok(Err(_)) => Err(anyhow!("Codex {method} response channel closed")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(anyhow!("Codex {method} timed out"))
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({"method": method, "params": params}))
            .await
    }

    pub async fn respond(&self, id: Value, result: Value) -> Result<()> {
        self.write(&json!({"id": id, "result": result})).await
    }

    pub async fn shutdown(&self) -> Result<()> {
        let _ = self.writer.send(OutgoingMessage::Shutdown).await;
        if let Some(task) = self.writer_task.lock().await.take() {
            finish_task(task).await;
        }
        if let Some(mut child) = self.child.lock().await.take() {
            match timeout(Duration::from_secs(2), child.wait()).await {
                Ok(result) => {
                    result.context("wait for Codex app-server")?;
                }
                Err(_) => {
                    child.start_kill().context("kill Codex app-server")?;
                    child.wait().await.context("wait for Codex app-server")?;
                }
            }
        }
        if let Some(task) = self.reader_task.lock().await.take() {
            finish_task(task).await;
        }
        Ok(())
    }

    pub async fn list_models(&self) -> Result<Vec<ModelMetadata>> {
        let mut models = Vec::new();
        let mut cursor = None;
        loop {
            let response = self
                .request(
                    "model/list",
                    json!({
                        "cursor": cursor,
                        "includeHidden": false,
                        "limit": 100
                    }),
                )
                .await?;
            let page = decode_model_list_response(response)?;
            models.extend(page.data);
            let Some(next_cursor) = page.next_cursor else {
                break;
            };
            cursor = Some(next_cursor);
        }
        Ok(models)
    }

    pub async fn start_thread(
        &self,
        cwd: &Path,
        model: Option<&str>,
        execution_mode: ExecutionMode,
    ) -> Result<String> {
        let (approval_policy, sandbox) = execution_policy(execution_mode);
        let response = self
            .request(
                "thread/start",
                json!({
                    "cwd": cwd,
                    "model": model,
                    "approvalPolicy": approval_policy,
                    "sandbox": sandbox,
                    "ephemeral": false,
                    "dynamicTools": shikigami_dynamic_tools()
                }),
            )
            .await?;
        extract_thread_id(&response, "thread/start")
    }

    pub async fn resume_thread(
        &self,
        thread_id: &str,
        cwd: &Path,
        model: Option<&str>,
        execution_mode: ExecutionMode,
    ) -> Result<()> {
        let (approval_policy, sandbox) = execution_policy(execution_mode);
        self.request(
            "thread/resume",
            json!({
                "threadId": thread_id,
                "cwd": cwd,
                "model": model,
                "approvalPolicy": approval_policy,
                "sandbox": sandbox,
                "dynamicTools": shikigami_dynamic_tools()
            }),
        )
        .await?;
        Ok(())
    }

    pub async fn fork_thread(
        &self,
        thread_id: &str,
        cwd: &Path,
        ephemeral: bool,
        execution_mode: ExecutionMode,
    ) -> Result<(String, Value)> {
        let (approval_policy, sandbox) = execution_policy(execution_mode);
        let response = self
            .request(
                "thread/fork",
                json!({
                    "threadId": thread_id,
                    "cwd": cwd,
                    "approvalPolicy": approval_policy,
                    "sandbox": sandbox,
                    "ephemeral": ephemeral
                }),
            )
            .await?;
        let thread_id = extract_thread_id(&response, "thread/fork")?;
        Ok((thread_id, response))
    }

    pub async fn read_thread(&self, thread_id: &str) -> Result<Value> {
        self.request(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        )
        .await
    }

    pub async fn read_thread_preview(&self, thread_id: &str, limit: u32) -> Result<Value> {
        let response = self
            .request(
                "thread/turns/list",
                json!({
                    "threadId": thread_id,
                    "limit": limit,
                    "sortDirection": "desc",
                    "itemsView": "full"
                }),
            )
            .await?;
        preview_history(response)
    }

    pub async fn unsubscribe_thread(&self, thread_id: &str) -> Result<()> {
        self.request("thread/unsubscribe", json!({"threadId": thread_id}))
            .await?;
        Ok(())
    }

    pub async fn delete_thread(&self, thread_id: &str) -> Result<()> {
        self.request("thread/delete", json!({"threadId": thread_id}))
            .await?;
        Ok(())
    }

    pub async fn list_skills(&self, cwd: &Path, force_reload: bool) -> Result<Vec<SkillMetadata>> {
        let response = self
            .request(
                "skills/list",
                json!({"cwds": [cwd], "forceReload": force_reload}),
            )
            .await?;
        extract_skills(&response, cwd)
    }

    pub async fn start_turn(
        &self,
        thread_id: &str,
        cwd: &Path,
        prompt: &str,
        skills: &[SkillMetadata],
        local_images: &[PathBuf],
        settings: TurnSettings<'_>,
    ) -> Result<String> {
        let (approval_policy, _) = execution_policy(settings.execution_mode);
        let sandbox_policy = turn_sandbox_policy(settings.execution_mode);
        let response = self
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "cwd": cwd,
                    "input": turn_input(prompt, skills, local_images),
                    "model": settings.model,
                    "effort": settings.effort,
                    "approvalPolicy": approval_policy,
                    "sandboxPolicy": {"type": sandbox_policy}
                }),
            )
            .await?;
        response
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("turn/start response missing turn.id")
    }

    pub async fn steer_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
        prompt: &str,
        skills: &[SkillMetadata],
        local_images: &[PathBuf],
    ) -> Result<()> {
        let response = self
            .request(
                "turn/steer",
                json!({
                    "threadId": thread_id,
                    "input": turn_input(prompt, skills, local_images),
                    "expectedTurnId": turn_id,
                }),
            )
            .await?;
        let accepted_turn_id = response
            .get("turnId")
            .and_then(Value::as_str)
            .context("turn/steer response missing turnId")?;
        if accepted_turn_id != turn_id {
            bail!("turn/steer returned unexpected turn id {accepted_turn_id}");
        }
        Ok(())
    }

    pub async fn interrupt_turn(&self, thread_id: &str, turn_id: &str) -> Result<()> {
        self.request(
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        )
        .await?;
        Ok(())
    }

    async fn write(&self, message: &Value) -> Result<()> {
        self.writer
            .send(OutgoingMessage::Json(message.clone()))
            .await
            .context("write App Server message")
    }
}

fn execution_policy(mode: ExecutionMode) -> (&'static str, &'static str) {
    match mode {
        ExecutionMode::Auto => ("on-request", "workspace-write"),
        ExecutionMode::Dangerous => ("never", "danger-full-access"),
    }
}

fn turn_sandbox_policy(mode: ExecutionMode) -> &'static str {
    match mode {
        ExecutionMode::Auto => "workspaceWrite",
        ExecutionMode::Dangerous => "dangerFullAccess",
    }
}

fn turn_input(prompt: &str, skills: &[SkillMetadata], local_images: &[PathBuf]) -> Vec<Value> {
    let mut input =
        Vec::with_capacity(usize::from(!prompt.is_empty()) + skills.len() + local_images.len());
    if !prompt.is_empty() {
        input.push(json!({"type": "text", "text": prompt}));
    }
    input.extend(
        skills
            .iter()
            .map(|skill| json!({"type": "skill", "name": skill.name, "path": skill.path})),
    );
    input.extend(
        local_images
            .iter()
            .map(|path| json!({"type": "localImage", "path": path})),
    );
    input
}

#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    pub fn acquire() -> Result<Self> {
        let cache_dir = paths::project_dirs()?.cache_dir().to_path_buf();
        fs::create_dir_all(&cache_dir)
            .with_context(|| format!("create Shikigami cache directory {}", cache_dir.display()))?;
        Self::acquire_in(&cache_dir)
    }

    fn acquire_in(cache_dir: &Path) -> Result<Self> {
        let path = cache_dir.join("shikigami.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open Shikigami instance lock {}", path.display()))?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Self { _file: file }),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                bail!("Shikigami is already running")
            }
            Err(error) => {
                Err(error).with_context(|| format!("lock Shikigami instance {}", path.display()))
            }
        }
    }
}

async fn finish_task(mut task: JoinHandle<()>) {
    if timeout(Duration::from_secs(1), &mut task).await.is_err() {
        task.abort();
        let _ = task.await;
    }
}

fn decode_model_list_response(response: Value) -> Result<ModelListResponse> {
    serde_json::from_value(response).context("decode model/list response")
}

fn extract_skills(response: &Value, cwd: &Path) -> Result<Vec<SkillMetadata>> {
    let entries = response
        .get("data")
        .and_then(Value::as_array)
        .context("skills/list response missing data")?;
    let entry = entries
        .iter()
        .find(|entry| entry.get("cwd").and_then(Value::as_str) == cwd.to_str())
        .or_else(|| entries.first())
        .context("skills/list response missing cwd entry")?;
    Ok(entry
        .get("skills")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|skill| skill.get("enabled").and_then(Value::as_bool) != Some(false))
        .filter_map(|skill| {
            Some(SkillMetadata {
                name: skill.get("name")?.as_str()?.to_owned(),
                description: skill
                    .pointer("/interface/shortDescription")
                    .and_then(Value::as_str)
                    .or_else(|| skill.get("shortDescription").and_then(Value::as_str))
                    .or_else(|| skill.get("description").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_owned(),
                path: PathBuf::from(skill.get("path")?.as_str()?),
                scope: skill
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("user")
                    .to_owned(),
            })
        })
        .collect())
}

fn extract_thread_id(response: &Value, method: &str) -> Result<String> {
    response
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("{method} response missing thread.id"))
}

fn shikigami_dynamic_tools() -> Value {
    json!([{
        "type": "namespace",
        "name": "shikigami",
        "description": "Create and start independent Shikigami threads when the user requests them",
        "tools": [{
            "type": "function",
            "name": "start_thread",
            "description": "Create a new independent thread in the current repository and immediately start the requested task. Call this only when the user explicitly asks to create or start a separate thread. This does not copy conversation history. Use current to reuse the same worktree, which can conflict with concurrent edits, or new_worktree to isolate code changes in a new managed worktree.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The first user prompt for the new thread"
                    },
                    "workspace": {
                        "type": "string",
                        "enum": ["current", "new_worktree"],
                        "description": "Where the new thread should work"
                    }
                },
                "required": ["prompt", "workspace"],
                "additionalProperties": false
            }
        }]
    }])
}

fn preview_history(mut response: Value) -> Result<Value> {
    let turns = response
        .get_mut("data")
        .and_then(Value::as_array_mut)
        .context("thread/turns/list response did not contain data")?;
    turns.reverse();
    Ok(json!({"thread": {"turns": std::mem::take(turns)}}))
}

async fn dispatch_message(
    pending: &PendingResponses,
    events: &broadcast::Sender<AppServerEvent>,
    request_tx: &mpsc::Sender<AppServerRequest>,
    encoded: &[u8],
) {
    let Ok(message) = serde_json::from_slice::<Value>(encoded) else {
        let _ = events.send(AppServerEvent {
            method: "shikigami/invalidJson".into(),
            params: json!({"raw": String::from_utf8_lossy(encoded)}),
            thread_id: None,
            turn_id: None,
        });
        return;
    };
    if message.get("id").is_some()
        && (message.get("result").is_some() || message.get("error").is_some())
    {
        dispatch_response(pending, &message).await;
    } else if let (Some(id), Some(method)) = (
        message.get("id"),
        message.get("method").and_then(Value::as_str),
    ) {
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let _ = request_tx
            .send(AppServerRequest {
                id: id.clone(),
                method: method.to_owned(),
                thread_id: params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                turn_id: params
                    .get("turnId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                params,
            })
            .await;
    } else if let Some(method) = message.get("method").and_then(Value::as_str) {
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let _ = events.send(AppServerEvent {
            method: method.to_owned(),
            thread_id: params
                .get("threadId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            turn_id: params
                .get("turnId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            params,
        });
    }
}

async fn dispatch_response(pending: &PendingResponses, message: &Value) {
    if let Some(id) = message.get("id").and_then(Value::as_u64)
        && let Some(sender) = pending.lock().await.remove(&id)
    {
        let result = if let Some(error) = message.get("error") {
            Err(error.to_string())
        } else {
            Ok(message.get("result").cloned().unwrap_or(Value::Null))
        };
        let _ = sender.send(result);
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn execution_modes_map_to_app_server_policies() {
        assert_eq!(
            execution_policy(ExecutionMode::Auto),
            ("on-request", "workspace-write")
        );
        assert_eq!(
            execution_policy(ExecutionMode::Dangerous),
            ("never", "danger-full-access")
        );
        assert_eq!(turn_sandbox_policy(ExecutionMode::Auto), "workspaceWrite");
        assert_eq!(
            turn_sandbox_policy(ExecutionMode::Dangerous),
            "dangerFullAccess"
        );
    }

    #[test]
    fn shikigami_exposes_only_the_start_thread_tool() {
        let tools = shikigami_dynamic_tools();

        assert_eq!(tools.as_array().unwrap().len(), 1);
        assert_eq!(tools[0]["name"], "shikigami");
        assert_eq!(tools[0]["tools"].as_array().unwrap().len(), 1);
        assert_eq!(tools[0]["tools"][0]["name"], "start_thread");
        assert_eq!(
            tools[0]["tools"][0]["inputSchema"]["required"],
            json!(["prompt", "workspace"])
        );
    }

    #[tokio::test]
    async fn new_threads_receive_the_shikigami_tool() {
        let (writer, mut messages) = mpsc::channel(1);
        let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(1);
        let (_request_tx, request_rx) = mpsc::channel(1);
        let server = Arc::new(AppServer {
            writer,
            writer_task: Mutex::new(None),
            reader_task: Mutex::new(None),
            child: Mutex::new(None),
            pending: pending.clone(),
            next_id: AtomicU64::new(0),
            events,
            server_requests: Mutex::new(request_rx),
            version: "test".into(),
            request_timeout: Duration::from_secs(1),
        });

        let task = tokio::spawn({
            let server = server.clone();
            async move {
                server
                    .start_thread(Path::new("/tmp/project"), None, ExecutionMode::Auto)
                    .await
            }
        });
        let OutgoingMessage::Json(message) = messages.recv().await.unwrap() else {
            panic!("unexpected shutdown message");
        };
        assert_eq!(message["params"]["dynamicTools"], shikigami_dynamic_tools());
        dispatch_response(
            &pending,
            &json!({"id":message["id"],"result":{"thread":{"id":"thread-1"}}}),
        )
        .await;

        assert_eq!(task.await.unwrap().unwrap(), "thread-1");
    }

    #[tokio::test]
    async fn auto_turn_uses_workspace_sandbox_and_on_request_approvals() {
        let (writer, mut messages) = mpsc::channel(1);
        let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(1);
        let (_request_tx, request_rx) = mpsc::channel(1);
        let server = Arc::new(AppServer {
            writer,
            writer_task: Mutex::new(None),
            reader_task: Mutex::new(None),
            child: Mutex::new(None),
            pending: pending.clone(),
            next_id: AtomicU64::new(0),
            events,
            server_requests: Mutex::new(request_rx),
            version: "test".into(),
            request_timeout: Duration::from_secs(1),
        });

        let task = tokio::spawn({
            let server = server.clone();
            async move {
                server
                    .start_turn(
                        "thread-1",
                        Path::new("/tmp/project"),
                        "run tests",
                        &[],
                        &[PathBuf::from("/tmp/start.png")],
                        TurnSettings {
                            model: None,
                            effort: None,
                            execution_mode: ExecutionMode::Auto,
                        },
                    )
                    .await
            }
        });
        let OutgoingMessage::Json(message) = messages.recv().await.unwrap() else {
            panic!("unexpected shutdown message");
        };
        assert_eq!(message["method"], "turn/start");
        assert_eq!(
            message["params"]["input"],
            json!([
                {"type":"text","text":"run tests"},
                {"type":"localImage","path":"/tmp/start.png"}
            ])
        );
        assert_eq!(message["params"]["approvalPolicy"], "on-request");
        assert_eq!(
            message["params"]["sandboxPolicy"],
            json!({"type":"workspaceWrite"})
        );
        dispatch_response(
            &pending,
            &json!({"id":message["id"],"result":{"turn":{"id":"turn-1"}}}),
        )
        .await;

        assert_eq!(task.await.unwrap().unwrap(), "turn-1");
    }

    #[test]
    fn second_shikigami_instance_is_rejected_until_first_exits() {
        let directory = tempdir().unwrap();
        let first = InstanceLock::acquire_in(directory.path()).unwrap();
        let error = InstanceLock::acquire_in(directory.path()).unwrap_err();
        assert!(error.to_string().contains("already running"));
        drop(first);
        InstanceLock::acquire_in(directory.path()).unwrap();
    }

    #[test]
    fn extracts_thread_ids_from_start_and_fork_responses() {
        let response = json!({"thread": {"id": "thread-1"}});
        assert_eq!(
            extract_thread_id(&response, "thread/start").unwrap(),
            "thread-1"
        );
        assert!(extract_thread_id(&Value::Null, "thread/start").is_err());
    }

    #[test]
    fn turn_input_includes_text_and_selected_skills() {
        let skill = SkillMetadata {
            name: "review".into(),
            description: "Review changes".into(),
            path: "/tmp/review/SKILL.md".into(),
            scope: "user".into(),
        };

        assert_eq!(
            turn_input(
                "$review inspect this",
                &[skill],
                &[PathBuf::from("/tmp/screenshot.png")],
            ),
            vec![
                json!({"type":"text","text":"$review inspect this"}),
                json!({"type":"skill","name":"review","path":"/tmp/review/SKILL.md"}),
                json!({"type":"localImage","path":"/tmp/screenshot.png"}),
            ]
        );
        assert_eq!(
            turn_input("", &[], &[PathBuf::from("/tmp/image-only.png")]),
            vec![json!({"type":"localImage","path":"/tmp/image-only.png"})]
        );
    }

    #[test]
    fn extracts_enabled_skills_for_the_workspace() {
        let response = json!({"data":[{
            "cwd":"/tmp/project",
            "skills":[
                {
                    "name":"review",
                    "description":"Long description",
                    "enabled":true,
                    "path":"/tmp/review/SKILL.md",
                    "scope":"user",
                    "interface":{"shortDescription":"Review changes"}
                },
                {
                    "name":"disabled",
                    "description":"Disabled",
                    "enabled":false,
                    "path":"/tmp/disabled/SKILL.md",
                    "scope":"repo"
                }
            ],
            "errors":[]
        }]});
        let skills = extract_skills(&response, Path::new("/tmp/project")).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "review");
        assert_eq!(skills[0].description, "Review changes");
    }

    #[test]
    fn decodes_models_and_reasoning_efforts() {
        let response = json!({
            "data": [{
                "id": "gpt-test",
                "model": "gpt-test",
                "displayName": "GPT Test",
                "description": "Test model",
                "defaultReasoningEffort": "medium",
                "supportedReasoningEfforts": [{
                    "reasoningEffort": "medium",
                    "description": "Balanced"
                }],
                "isDefault": true
            }],
            "nextCursor": null
        });
        let page = decode_model_list_response(response).unwrap();
        assert_eq!(page.data[0].model, "gpt-test");
        assert_eq!(
            page.data[0].supported_reasoning_efforts[0].reasoning_effort,
            "medium"
        );
        assert!(page.data[0].is_default);
        assert!(page.data[0].supports_images());
    }

    #[test]
    fn model_image_support_uses_live_input_modalities() {
        let response = json!({
            "data": [{
                "id": "text-only",
                "model": "text-only",
                "displayName": "Text only",
                "description": "Test model",
                "defaultReasoningEffort": "medium",
                "supportedReasoningEfforts": [],
                "inputModalities": ["text"],
                "isDefault": true
            }],
            "nextCursor": null
        });

        let page = decode_model_list_response(response).unwrap();
        assert!(!page.data[0].supports_images());
    }

    #[tokio::test]
    async fn responses_are_multiplexed_by_request_id() {
        let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));
        let (first_tx, first_rx) = oneshot::channel();
        let (second_tx, second_rx) = oneshot::channel();
        pending.lock().await.insert(1, first_tx);
        pending.lock().await.insert(2, second_tx);
        dispatch_response(&pending, &json!({"id": 2, "result": {"value": "second"}})).await;
        dispatch_response(&pending, &json!({"id": 1, "result": {"value": "first"}})).await;
        assert_eq!(first_rx.await.unwrap().unwrap()["value"], "first");
        assert_eq!(second_rx.await.unwrap().unwrap()["value"], "second");
    }

    #[tokio::test]
    async fn steer_turn_sends_the_active_turn_id_and_input() {
        let (writer, mut messages) = mpsc::channel(1);
        let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(1);
        let (_request_tx, request_rx) = mpsc::channel(1);
        let server = Arc::new(AppServer {
            writer,
            writer_task: Mutex::new(None),
            reader_task: Mutex::new(None),
            child: Mutex::new(None),
            pending: pending.clone(),
            next_id: AtomicU64::new(0),
            events,
            server_requests: Mutex::new(request_rx),
            version: "test".into(),
            request_timeout: Duration::from_secs(1),
        });

        let task = tokio::spawn({
            let server = server.clone();
            async move {
                server
                    .steer_turn(
                        "thread-1",
                        "turn-1",
                        "focus on tests",
                        &[],
                        &[PathBuf::from("/tmp/follow-up.png")],
                    )
                    .await
            }
        });
        let OutgoingMessage::Json(message) = messages.recv().await.unwrap() else {
            panic!("unexpected shutdown message");
        };
        assert_eq!(message["method"], "turn/steer");
        assert_eq!(message["params"]["threadId"], "thread-1");
        assert_eq!(message["params"]["expectedTurnId"], "turn-1");
        assert_eq!(
            message["params"]["input"],
            json!([
                {"type":"text","text":"focus on tests"},
                {"type":"localImage","path":"/tmp/follow-up.png"}
            ])
        );
        dispatch_response(
            &pending,
            &json!({"id":message["id"],"result":{"turnId":"turn-1"}}),
        )
        .await;

        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn thread_preview_requests_recent_full_turns_in_display_order() {
        let (writer, mut messages) = mpsc::channel(1);
        let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(1);
        let (_request_tx, request_rx) = mpsc::channel(1);
        let server = Arc::new(AppServer {
            writer,
            writer_task: Mutex::new(None),
            reader_task: Mutex::new(None),
            child: Mutex::new(None),
            pending: pending.clone(),
            next_id: AtomicU64::new(0),
            events,
            server_requests: Mutex::new(request_rx),
            version: "test".into(),
            request_timeout: Duration::from_secs(1),
        });

        let task = tokio::spawn({
            let server = server.clone();
            async move { server.read_thread_preview("thread-1", 5).await }
        });
        let OutgoingMessage::Json(message) = messages.recv().await.unwrap() else {
            panic!("unexpected shutdown message");
        };
        assert_eq!(message["method"], "thread/turns/list");
        assert_eq!(message["params"]["threadId"], "thread-1");
        assert_eq!(message["params"]["limit"], 5);
        assert_eq!(message["params"]["sortDirection"], "desc");
        assert_eq!(message["params"]["itemsView"], "full");
        dispatch_response(
            &pending,
            &json!({
                "id": message["id"],
                "result": {"data": [{"id": "new"}, {"id": "old"}]}
            }),
        )
        .await;

        let history = task.await.unwrap().unwrap();
        assert_eq!(
            history.pointer("/thread/turns").unwrap(),
            &json!([{"id": "old"}, {"id": "new"}])
        );
    }

    #[tokio::test]
    #[ignore = "requires an installed Codex binary"]
    async fn installed_codex_initializes_over_stdio() {
        let server = AppServer::spawn("codex", Duration::from_secs(10))
            .await
            .expect("initialize installed Codex App Server");
        assert!(server.version().starts_with("codex-cli "));
        server
            .list_skills(Path::new(env!("CARGO_MANIFEST_DIR")), false)
            .await
            .expect("list skills from installed Codex App Server");
        let models = server
            .list_models()
            .await
            .expect("list models from installed Codex App Server");
        assert!(!models.is_empty());
        server
            .shutdown()
            .await
            .expect("stop installed Codex App Server");
    }
}
