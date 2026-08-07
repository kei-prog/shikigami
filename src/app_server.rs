use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, Command},
    sync::{Mutex, broadcast, mpsc, oneshot},
    time::timeout,
};

type PendingResponses = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

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
    pub is_default: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelListResponse {
    data: Vec<ModelMetadata>,
    next_cursor: Option<String>,
}

pub struct AppServer {
    writer: Arc<Mutex<ChildStdin>>,
    pending: PendingResponses,
    next_id: AtomicU64,
    events: broadcast::Sender<AppServerEvent>,
    server_requests: Mutex<mpsc::Receiver<AppServerRequest>>,
    pid: u32,
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
        let mut child = Command::new(command)
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start {command} app-server"))?;
        let pid = child.id().context("Codex app-server has no pid")?;
        let stdin = child.stdin.take().context("open app-server stdin")?;
        let stdout = child.stdout.take().context("open app-server stdout")?;
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(1024);
        let (request_tx, request_rx) = mpsc::channel(128);

        let server = Arc::new(Self {
            writer: Arc::new(Mutex::new(stdin)),
            pending: pending.clone(),
            next_id: AtomicU64::new(1),
            events: events.clone(),
            server_requests: Mutex::new(request_rx),
            pid,
            version,
            request_timeout,
        });

        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    let _ = events.send(AppServerEvent {
                        method: "wyard/invalidJson".into(),
                        params: json!({"raw": line}),
                        thread_id: None,
                        turn_id: None,
                    });
                    continue;
                };
                if message.get("id").is_some()
                    && (message.get("result").is_some() || message.get("error").is_some())
                {
                    dispatch_response(&pending, &message).await;
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
            let mut pending = pending.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err("Codex app-server exited".into()));
            }
            let _ = events.send(AppServerEvent {
                method: "wyard/processExited".into(),
                params: Value::Null,
                thread_id: None,
                turn_id: None,
            });
            let _ = child.wait().await;
        });

        server.initialize().await?;
        Ok(server)
    }

    async fn initialize(&self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "wyard",
                    "title": "wyard",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {"experimentalApi": true}
            }),
        )
        .await
        .context("initialize Codex app-server")?;
        self.notify("initialized", json!({})).await
    }

    pub fn pid(&self) -> u32 {
        self.pid
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

    pub async fn start_thread(&self, cwd: &Path, model: Option<&str>) -> Result<String> {
        let response = self
            .request(
                "thread/start",
                json!({
                    "cwd": cwd,
                    "model": model,
                    "approvalPolicy": "never",
                    "sandbox": "danger-full-access",
                    "ephemeral": false
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
    ) -> Result<()> {
        self.request(
            "thread/resume",
            json!({
                "threadId": thread_id,
                "cwd": cwd,
                "model": model,
                "approvalPolicy": "never",
                "sandbox": "danger-full-access"
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
    ) -> Result<(String, Value)> {
        let response = self
            .request(
                "thread/fork",
                json!({
                    "threadId": thread_id,
                    "cwd": cwd,
                    "approvalPolicy": "never",
                    "sandbox": "danger-full-access",
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
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<String> {
        let mut input = vec![json!({"type": "text", "text": prompt})];
        input.extend(
            skills
                .iter()
                .map(|skill| json!({"type": "skill", "name": skill.name, "path": skill.path})),
        );
        let response = self
            .request(
                "turn/start",
                json!({
                    "threadId": thread_id,
                    "cwd": cwd,
                    "input": input,
                    "model": model,
                    "effort": effort,
                    "approvalPolicy": "never",
                    "sandboxPolicy": {"type": "dangerFullAccess"}
                }),
            )
            .await?;
        response
            .pointer("/turn/id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .context("turn/start response missing turn.id")
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
        let mut encoded = serde_json::to_vec(message)?;
        encoded.push(b'\n');
        let mut writer = self.writer.lock().await;
        writer
            .write_all(&encoded)
            .await
            .context("write App Server message")?;
        writer.flush().await.context("flush App Server message")
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
    use super::*;

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
    }
}
