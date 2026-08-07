use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{git_workspace, paths};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ThreadRecord {
    pub id: String,
    pub repository_path: PathBuf,
    pub cwd: PathBuf,
    pub title: String,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub archived_at: Option<u64>,
    #[serde(default)]
    pub managed_worktree: bool,
    #[serde(default)]
    pub worktree_branch: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct RegistryData {
    threads: Vec<ThreadRecord>,
}

#[derive(Debug)]
pub struct Registry {
    path: PathBuf,
}

impl Registry {
    pub fn discover() -> Result<Self> {
        let dirs = paths::project_dirs()?;
        let legacy_dirs = paths::legacy_project_dirs()?;
        let path = dirs.data_local_dir().join("threads.json");
        paths::migrate_file(&legacy_dirs.data_local_dir().join("threads.json"), &path)?;
        Ok(Self { path })
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

    pub fn register_thread(
        &self,
        thread_id: String,
        repository_path: &Path,
        cwd: &Path,
    ) -> Result<()> {
        let now = now_seconds()?;
        let worktree_branch = git_workspace::current_branch(cwd).ok().flatten();
        self.upsert(ThreadRecord {
            id: thread_id,
            repository_path: repository_path.to_path_buf(),
            cwd: cwd.to_path_buf(),
            title: "Untitled thread".into(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            managed_worktree: git_workspace::is_managed_workspace(cwd, worktree_branch.as_deref()),
            worktree_branch,
        })
    }

    pub fn set_title(&self, thread_id: &str, title: &str) -> Result<()> {
        let mut threads = self.load()?;
        let thread = threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
            .context("thread not found")?;
        if thread.title == "Untitled thread" {
            thread.title = title
                .lines()
                .next()
                .unwrap_or(title)
                .chars()
                .take(80)
                .collect();
        }
        thread.updated_at = now_seconds()?;
        self.save(&threads)
    }

    pub fn set_archived(&self, thread_id: &str, archived: bool) -> Result<()> {
        let mut threads = self.load()?;
        let thread = threads
            .iter_mut()
            .find(|thread| thread.id == thread_id)
            .context("thread not found")?;
        thread.archived_at = archived.then(now_seconds).transpose()?;
        self.save(&threads)
    }

    fn upsert(&self, mut record: ThreadRecord) -> Result<()> {
        let mut threads = self.load()?;
        if let Some(existing) = threads.iter_mut().find(|thread| thread.id == record.id) {
            record.created_at = existing.created_at;
            if existing.title != "Untitled thread" {
                record.title.clone_from(&existing.title);
            }
            record.archived_at = existing.archived_at;
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

fn now_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn registers_and_titles_a_thread() {
        let temp = tempdir().unwrap();
        let registry = Registry::at(temp.path().join("threads.json"));
        let repository = temp.path().join("repo");
        fs::create_dir(&repository).unwrap();
        registry
            .register_thread("thread-1".into(), &repository, &repository)
            .unwrap();
        registry
            .set_title("thread-1", "Build the feature\nwith tests")
            .unwrap();

        let threads = registry.load().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "thread-1");
        assert_eq!(threads[0].title, "Build the feature");
    }

    #[test]
    fn removes_a_thread_from_shikigami_only() {
        let temp = tempdir().unwrap();
        let registry = Registry::at(temp.path().join("threads.json"));
        let repository = temp.path().join("repo");
        fs::create_dir(&repository).unwrap();
        registry
            .register_thread("thread-1".into(), &repository, &repository)
            .unwrap();
        registry.remove("thread-1").unwrap();
        assert!(registry.load().unwrap().is_empty());
    }

    #[test]
    fn archives_and_restores_a_thread() {
        let temp = tempdir().unwrap();
        let registry = Registry::at(temp.path().join("threads.json"));
        let repository = temp.path().join("repo");
        fs::create_dir(&repository).unwrap();
        registry
            .register_thread("thread-1".into(), &repository, &repository)
            .unwrap();

        registry.set_archived("thread-1", true).unwrap();
        assert!(registry.load().unwrap()[0].archived_at.is_some());

        registry.set_archived("thread-1", false).unwrap();
        assert_eq!(registry.load().unwrap()[0].archived_at, None);
    }

    #[test]
    fn loads_records_written_before_archive_support() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("threads.json");
        fs::write(
            &path,
            r#"{"threads":[{"id":"old","repository_path":"/tmp/repo","cwd":"/tmp/repo","title":"Old","created_at":1,"updated_at":2}]}"#,
        )
        .unwrap();
        let thread = Registry::at(path).load().unwrap().remove(0);
        assert_eq!(thread.archived_at, None);
        assert!(!thread.managed_worktree);
        assert_eq!(thread.worktree_branch, None);
    }
}
