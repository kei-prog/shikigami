use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ThreadRecord {
    pub id: String,
    pub repository_path: PathBuf,
    pub cwd: PathBuf,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Default, Deserialize, Serialize)]
struct RegistryData {
    threads: Vec<ThreadRecord>,
}

#[derive(Debug)]
pub struct Registry {
    path: PathBuf,
}

#[derive(Deserialize)]
struct Notification {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "thread-id")]
    thread_id: String,
    cwd: PathBuf,
    #[serde(rename = "input-messages", default)]
    input_messages: Vec<String>,
}

impl Registry {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "kei-prog", "wyard")
            .context("cannot determine wyard data directory")?;
        Ok(Self {
            path: dirs.data_local_dir().join("threads.json"),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<Vec<ThreadRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&self.path)
            .with_context(|| format!("read thread registry {}", self.path.display()))?;
        let data: RegistryData = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse thread registry {}", self.path.display()))?;
        Ok(data.threads)
    }

    pub fn remove(&self, thread_id: &str) -> Result<()> {
        let mut threads = self.load()?;
        threads.retain(|thread| thread.id != thread_id);
        self.save(&threads)
    }

    fn upsert(&self, mut record: ThreadRecord) -> Result<()> {
        let mut threads = self.load()?;
        if let Some(existing) = threads.iter_mut().find(|thread| thread.id == record.id) {
            record.created_at = existing.created_at;
            record.title.clone_from(&existing.title);
            *existing = record;
        } else {
            threads.push(record);
        }
        self.save(&threads)
    }

    fn save(&self, threads: &[ThreadRecord]) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("thread registry has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create registry directory {}", parent.display()))?;
        let temporary = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(&RegistryData {
            threads: threads.to_vec(),
        })?;
        fs::write(&temporary, data)
            .with_context(|| format!("write thread registry {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("replace thread registry {}", self.path.display()))?;
        Ok(())
    }
}

pub fn capture_notification(repository_path: &Path, payload: &str) -> Result<()> {
    capture_into(&Registry::discover()?, repository_path, payload)
}

fn capture_into(registry: &Registry, repository_path: &Path, payload: &str) -> Result<()> {
    let notification: Notification =
        serde_json::from_str(payload).context("parse Codex notification")?;
    if notification.kind != "agent-turn-complete" {
        return Ok(());
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs();
    let title = notification
        .input_messages
        .iter()
        .find_map(|message| message.lines().find(|line| !line.trim().is_empty()))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .unwrap_or("Untitled thread")
        .chars()
        .take(80)
        .collect();

    registry.upsert(ThreadRecord {
        id: notification.thread_id,
        repository_path: repository_path.to_path_buf(),
        cwd: notification.cwd,
        title,
        created_at: now,
        updated_at: now,
    })
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn captures_and_updates_only_completed_threads() {
        let temp = tempdir().unwrap();
        let registry = Registry::at(temp.path().join("threads.json"));
        let repository = temp.path().join("repo");
        let first = format!(
            r#"{{"type":"agent-turn-complete","thread-id":"thread-1","cwd":"{}","input-messages":["Build the feature\nwith tests"]}}"#,
            repository.display()
        );
        capture_into(&registry, &repository, &first).unwrap();

        let ignored = format!(
            r#"{{"type":"other","thread-id":"thread-2","cwd":"{}"}}"#,
            repository.display()
        );
        capture_into(&registry, &repository, &ignored).unwrap();

        let threads = registry.load().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "thread-1");
        assert_eq!(threads[0].title, "Build the feature");
    }

    #[test]
    fn removes_a_thread_from_wyard_only() {
        let temp = tempdir().unwrap();
        let registry = Registry::at(temp.path().join("threads.json"));
        let repository = temp.path().join("repo");
        let payload = format!(
            r#"{{"type":"agent-turn-complete","thread-id":"thread-1","cwd":"{}","input-messages":[]}}"#,
            repository.display()
        );
        capture_into(&registry, &repository, &payload).unwrap();
        registry.remove("thread-1").unwrap();
        assert!(registry.load().unwrap().is_empty());
    }
}
