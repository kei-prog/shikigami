use std::{
    collections::HashSet,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TemporarySideChatRecord {
    pub id: String,
    pub parent_thread_id: String,
    pub created_at: u64,
}

#[derive(Default, Deserialize, Serialize)]
struct SideChatRegistryData {
    side_chats: Vec<TemporarySideChatRecord>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PersistentAttentionKind {
    Completed,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttentionRecord {
    pub thread_id: String,
    pub kind: PersistentAttentionKind,
    pub created_at: u64,
}

#[derive(Default, Deserialize, Serialize)]
struct AttentionRegistryData {
    items: Vec<AttentionRecord>,
}

#[derive(Debug)]
pub struct Registry {
    path: PathBuf,
}

#[derive(Debug)]
pub struct SideChatRegistry {
    path: PathBuf,
}

#[derive(Debug)]
pub struct AttentionRegistry {
    path: PathBuf,
}

impl AttentionRegistry {
    pub fn discover() -> Result<Self> {
        let dirs = paths::project_dirs()?;
        Ok(Self {
            path: dirs.data_local_dir().join("attention.json"),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn reconcile(
        &self,
        registered_thread_ids: &HashSet<String>,
    ) -> Result<Vec<AttentionRecord>> {
        let mut items = self.load()?;
        let original_len = items.len();
        items.retain(|item| registered_thread_ids.contains(&item.thread_id));
        if items.len() != original_len {
            self.save(&items)?;
        }
        Ok(items)
    }

    pub fn sync(&self, desired: &[(String, PersistentAttentionKind)]) -> Result<()> {
        if desired.is_empty() && !self.path.exists() {
            return Ok(());
        }
        let existing = self
            .load()?
            .into_iter()
            .map(|item| (item.thread_id.clone(), item))
            .collect::<std::collections::HashMap<_, _>>();
        let now = now_seconds()?;
        let items = desired
            .iter()
            .map(|(thread_id, kind)| AttentionRecord {
                thread_id: thread_id.clone(),
                kind: *kind,
                created_at: existing
                    .get(thread_id)
                    .map(|item| item.created_at)
                    .unwrap_or(now),
            })
            .collect::<Vec<_>>();
        self.save(&items)
    }

    fn load(&self) -> Result<Vec<AttentionRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&self.path)
            .with_context(|| format!("read attention registry {}", self.path.display()))?;
        let data: AttentionRegistryData = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse attention registry {}", self.path.display()))?;
        Ok(data.items)
    }

    fn save(&self, items: &[AttentionRecord]) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("attention registry has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create registry directory {}", parent.display()))?;
        let temporary = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(&AttentionRegistryData {
            items: items.to_vec(),
        })?;
        fs::write(&temporary, data)
            .with_context(|| format!("write attention registry {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("replace attention registry {}", self.path.display()))?;
        Ok(())
    }
}

impl SideChatRegistry {
    pub fn discover() -> Result<Self> {
        let dirs = paths::project_dirs()?;
        Ok(Self {
            path: dirs.data_local_dir().join("side-chats.json"),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<Vec<TemporarySideChatRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&self.path)
            .with_context(|| format!("read side chat registry {}", self.path.display()))?;
        let data: SideChatRegistryData = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse side chat registry {}", self.path.display()))?;
        Ok(data.side_chats)
    }

    pub fn register(&self, id: String, parent_thread_id: String) -> Result<()> {
        let mut side_chats = self.load()?;
        if !side_chats.iter().any(|side_chat| side_chat.id == id) {
            side_chats.push(TemporarySideChatRecord {
                id,
                parent_thread_id,
                created_at: now_seconds()?,
            });
        }
        self.save(&side_chats)
    }

    pub fn remove(&self, thread_id: &str) -> Result<()> {
        let mut side_chats = self.load()?;
        side_chats.retain(|side_chat| side_chat.id != thread_id);
        self.save(&side_chats)
    }

    pub fn reconcile(&self, registered_thread_ids: &HashSet<String>) -> Result<Vec<String>> {
        let mut side_chats = self.load()?;
        let original_len = side_chats.len();
        side_chats.retain(|side_chat| !registered_thread_ids.contains(&side_chat.id));
        if side_chats.len() != original_len {
            self.save(&side_chats)?;
        }
        Ok(side_chats
            .into_iter()
            .map(|side_chat| side_chat.id)
            .collect())
    }

    fn save(&self, side_chats: &[TemporarySideChatRecord]) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("side chat registry has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create registry directory {}", parent.display()))?;
        let temporary = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(&SideChatRegistryData {
            side_chats: side_chats.to_vec(),
        })?;
        fs::write(&temporary, data)
            .with_context(|| format!("write side chat registry {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("replace side chat registry {}", self.path.display()))?;
        Ok(())
    }
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

    pub fn replace_thread_id(&self, old_thread_id: &str, new_thread_id: String) -> Result<()> {
        let mut threads = self.load()?;
        if old_thread_id != new_thread_id && threads.iter().any(|thread| thread.id == new_thread_id)
        {
            anyhow::bail!("replacement thread is already registered");
        }
        let thread = threads
            .iter_mut()
            .find(|thread| thread.id == old_thread_id)
            .context("thread not found")?;
        thread.id = new_thread_id;
        self.save(&threads)
    }

    pub fn register_thread(
        &self,
        thread_id: String,
        repository_path: &Path,
        cwd: &Path,
    ) -> Result<()> {
        self.register_thread_named(thread_id, repository_path, cwd, "Untitled thread")
    }

    pub fn register_thread_named(
        &self,
        thread_id: String,
        repository_path: &Path,
        cwd: &Path,
        title: &str,
    ) -> Result<()> {
        let now = now_seconds()?;
        let worktree_branch = git_workspace::current_branch(cwd).ok().flatten();
        self.upsert(ThreadRecord {
            id: thread_id,
            repository_path: repository_path.to_path_buf(),
            cwd: cwd.to_path_buf(),
            title: normalized_title(title),
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
            thread.title = normalized_title(title);
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

fn normalized_title(title: &str) -> String {
    title
        .lines()
        .next()
        .unwrap_or(title)
        .chars()
        .take(80)
        .collect()
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
    fn registers_a_named_thread_in_one_write() {
        let temp = tempdir().unwrap();
        let registry = Registry::at(temp.path().join("threads.json"));
        let repository = temp.path().join("repo");
        fs::create_dir(&repository).unwrap();

        registry
            .register_thread_named(
                "thread-1".into(),
                &repository,
                &repository,
                "Promoted side chat\nextra",
            )
            .unwrap();

        assert_eq!(registry.load().unwrap()[0].title, "Promoted side chat");
    }

    #[test]
    fn tracks_temporary_side_chats_until_they_are_removed() {
        let temp = tempdir().unwrap();
        let registry = SideChatRegistry::at(temp.path().join("side-chats.json"));

        registry
            .register("side-1".into(), "parent-1".into())
            .unwrap();
        registry
            .register("side-1".into(), "parent-1".into())
            .unwrap();
        assert_eq!(registry.load().unwrap().len(), 1);
        assert_eq!(registry.load().unwrap()[0].parent_thread_id, "parent-1");

        registry.remove("side-1").unwrap();
        assert!(registry.load().unwrap().is_empty());
    }

    #[test]
    fn reconciliation_keeps_only_unpromoted_side_chats() {
        let temp = tempdir().unwrap();
        let registry = SideChatRegistry::at(temp.path().join("side-chats.json"));
        registry
            .register("promoted".into(), "parent".into())
            .unwrap();
        registry
            .register("abandoned".into(), "parent".into())
            .unwrap();

        let pending = registry
            .reconcile(&HashSet::from(["promoted".into()]))
            .unwrap();

        assert_eq!(pending, vec!["abandoned"]);
        assert_eq!(registry.load().unwrap()[0].id, "abandoned");
    }

    #[test]
    fn attention_registry_restores_only_registered_threads() {
        let temp = tempdir().unwrap();
        let registry = AttentionRegistry::at(temp.path().join("attention.json"));
        registry
            .sync(&[
                ("kept".into(), PersistentAttentionKind::Completed),
                ("removed".into(), PersistentAttentionKind::Failed),
            ])
            .unwrap();

        let restored = registry.reconcile(&HashSet::from(["kept".into()])).unwrap();

        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].thread_id, "kept");
        assert_eq!(restored[0].kind, PersistentAttentionKind::Completed);
    }

    #[test]
    fn attention_sync_preserves_the_original_timestamp() {
        let temp = tempdir().unwrap();
        let registry = AttentionRegistry::at(temp.path().join("attention.json"));
        registry
            .sync(&[("thread".into(), PersistentAttentionKind::Completed)])
            .unwrap();
        let created_at = registry.load().unwrap()[0].created_at;

        registry
            .sync(&[("thread".into(), PersistentAttentionKind::Failed)])
            .unwrap();

        let item = registry.load().unwrap().remove(0);
        assert_eq!(item.created_at, created_at);
        assert_eq!(item.kind, PersistentAttentionKind::Failed);
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
    fn replaces_only_a_thread_id() {
        let temp = tempdir().unwrap();
        let registry = Registry::at(temp.path().join("threads.json"));
        let repository = temp.path().join("repo");
        fs::create_dir(&repository).unwrap();
        registry
            .register_thread("missing-thread".into(), &repository, &repository)
            .unwrap();
        let original = registry.load().unwrap().remove(0);

        registry
            .replace_thread_id("missing-thread", "replacement-thread".into())
            .unwrap();

        let replacement = registry.load().unwrap().remove(0);
        assert_eq!(replacement.id, "replacement-thread");
        assert_eq!(replacement.repository_path, original.repository_path);
        assert_eq!(replacement.cwd, original.cwd);
        assert_eq!(replacement.title, original.title);
        assert_eq!(replacement.created_at, original.created_at);
        assert_eq!(replacement.updated_at, original.updated_at);
        assert_eq!(replacement.archived_at, original.archived_at);
        assert_eq!(replacement.managed_worktree, original.managed_worktree);
        assert_eq!(replacement.worktree_branch, original.worktree_branch);
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
