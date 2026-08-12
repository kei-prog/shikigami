use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{git_workspace, paths};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadScope {
    #[default]
    Repository,
    General,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadKind {
    #[default]
    Regular,
    ShikigamiHelp,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ThreadRecord {
    pub id: String,
    #[serde(default)]
    pub scope: ThreadScope,
    #[serde(default)]
    pub kind: ThreadKind,
    pub repository_path: PathBuf,
    pub cwd: PathBuf,
    #[serde(skip, default = "untitled_thread")]
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

#[derive(Default, Deserialize, Serialize)]
struct ThreadTitleCacheData {
    titles: HashMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SideChatRecord {
    pub id: String,
    pub parent_thread_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub model_display_name: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub has_activity: bool,
    #[serde(default)]
    pub pending_deletion: bool,
    pub created_at: u64,
}

#[derive(Default, Deserialize, Serialize)]
struct SideChatRegistryData {
    side_chats: Vec<SideChatRecord>,
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
pub struct ThreadTitleCache {
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

impl ThreadTitleCache {
    pub fn discover() -> Result<Self> {
        let dirs = paths::project_dirs()?;
        Ok(Self {
            path: dirs.data_local_dir().join("thread-title-cache.json"),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<HashMap<String, String>> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let bytes = fs::read(&self.path)
            .with_context(|| format!("read thread title cache {}", self.path.display()))?;
        let data: ThreadTitleCacheData = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse thread title cache {}", self.path.display()))?;
        Ok(data.titles)
    }

    pub fn sync(&self, names: &HashMap<String, Option<String>>) -> Result<()> {
        let titles = names
            .iter()
            .filter_map(|(thread_id, name)| {
                name.as_ref().map(|name| (thread_id.clone(), name.clone()))
            })
            .collect();
        let parent = self
            .path
            .parent()
            .context("thread title cache has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create cache directory {}", parent.display()))?;
        let temporary = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(&ThreadTitleCacheData { titles })?;
        fs::write(&temporary, data)
            .with_context(|| format!("write thread title cache {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("replace thread title cache {}", self.path.display()))?;
        Ok(())
    }
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

    pub fn load(&self) -> Result<Vec<SideChatRecord>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&self.path)
            .with_context(|| format!("read side chat registry {}", self.path.display()))?;
        let data: SideChatRegistryData = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse side chat registry {}", self.path.display()))?;
        Ok(data.side_chats)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &self,
        id: String,
        parent_thread_id: String,
        title: String,
        model: Option<String>,
        model_display_name: Option<String>,
        reasoning_effort: Option<String>,
        has_activity: bool,
    ) -> Result<()> {
        let mut side_chats = self.load()?;
        if !side_chats.iter().any(|side_chat| side_chat.id == id) {
            side_chats.push(SideChatRecord {
                id,
                parent_thread_id,
                title: Some(title),
                model,
                model_display_name,
                reasoning_effort,
                has_activity,
                pending_deletion: false,
                created_at: now_seconds()?,
            });
        }
        self.save(&side_chats)
    }

    pub fn update_metadata(
        &self,
        thread_id: &str,
        title: String,
        model: Option<String>,
        model_display_name: Option<String>,
        reasoning_effort: Option<String>,
        has_activity: bool,
    ) -> Result<()> {
        let mut side_chats = self.load()?;
        if let Some(side_chat) = side_chats
            .iter_mut()
            .find(|side_chat| side_chat.id == thread_id)
        {
            side_chat.title = Some(title);
            side_chat.model = model;
            side_chat.model_display_name = model_display_name;
            side_chat.reasoning_effort = reasoning_effort;
            side_chat.has_activity = has_activity;
            self.save(&side_chats)?;
        }
        Ok(())
    }

    pub fn remove(&self, thread_id: &str) -> Result<()> {
        let mut side_chats = self.load()?;
        side_chats.retain(|side_chat| side_chat.id != thread_id);
        self.save(&side_chats)
    }

    pub fn mark_for_deletion(&self, thread_id: &str) -> Result<()> {
        let mut side_chats = self.load()?;
        if let Some(side_chat) = side_chats
            .iter_mut()
            .find(|side_chat| side_chat.id == thread_id)
        {
            side_chat.pending_deletion = true;
            self.save(&side_chats)?;
        }
        Ok(())
    }

    pub fn pending_deletion_ids(&self) -> Result<Vec<String>> {
        Ok(self
            .load()?
            .into_iter()
            .filter(|side_chat| side_chat.pending_deletion)
            .map(|side_chat| side_chat.id)
            .collect())
    }

    pub fn remove_for_parent(&self, parent_thread_id: &str) -> Result<()> {
        let mut side_chats = self.load()?;
        side_chats.retain(|side_chat| side_chat.parent_thread_id != parent_thread_id);
        self.save(&side_chats)
    }

    fn save(&self, side_chats: &[SideChatRecord]) -> Result<()> {
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
        let path = dirs.data_local_dir().join("threads.json");
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
        let now = now_seconds()?;
        let worktree_branch = git_workspace::current_branch(cwd).ok().flatten();
        self.upsert(ThreadRecord {
            id: thread_id,
            scope: ThreadScope::Repository,
            kind: ThreadKind::Regular,
            repository_path: repository_path.to_path_buf(),
            cwd: cwd.to_path_buf(),
            title: untitled_thread(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            managed_worktree: git_workspace::is_managed_workspace(cwd, worktree_branch.as_deref()),
            worktree_branch,
        })
    }

    pub fn register_general_thread(&self, thread_id: String, cwd: &Path) -> Result<()> {
        self.register_general_thread_with_kind(thread_id, cwd, ThreadKind::Regular)
    }

    pub fn register_general_thread_with_kind(
        &self,
        thread_id: String,
        cwd: &Path,
        kind: ThreadKind,
    ) -> Result<()> {
        let now = now_seconds()?;
        self.upsert(ThreadRecord {
            id: thread_id,
            scope: ThreadScope::General,
            kind,
            repository_path: cwd.to_path_buf(),
            cwd: cwd.to_path_buf(),
            title: untitled_thread(),
            created_at: now,
            updated_at: now,
            archived_at: None,
            managed_worktree: false,
            worktree_branch: None,
        })
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

fn untitled_thread() -> String {
    "Untitled thread".into()
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn registry_does_not_persist_thread_names() {
        let temp = tempdir().unwrap();
        let registry = Registry::at(temp.path().join("threads.json"));
        let repository = temp.path().join("repo");
        fs::create_dir(&repository).unwrap();
        registry
            .register_thread("thread-1".into(), &repository, &repository)
            .unwrap();
        let threads = registry.load().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].id, "thread-1");
        assert_eq!(threads[0].title, "Untitled thread");
        let saved = fs::read_to_string(temp.path().join("threads.json")).unwrap();
        assert!(!saved.contains("title"));
    }

    #[test]
    fn thread_title_cache_persists_only_observed_titles() {
        let temp = tempdir().unwrap();
        let cache = ThreadTitleCache::at(temp.path().join("thread-title-cache.json"));
        cache
            .sync(&HashMap::from([
                ("named".into(), Some("Cached title".into())),
                ("untitled".into(), None),
            ]))
            .unwrap();

        assert_eq!(
            cache.load().unwrap(),
            HashMap::from([("named".into(), "Cached title".into())])
        );
    }

    #[test]
    fn tracks_temporary_side_chats_until_they_are_removed() {
        let temp = tempdir().unwrap();
        let registry = SideChatRegistry::at(temp.path().join("side-chats.json"));

        registry
            .register(
                "side-1".into(),
                "parent-1".into(),
                "First title".into(),
                Some("model".into()),
                Some("Model".into()),
                Some("high".into()),
                false,
            )
            .unwrap();
        registry
            .register(
                "side-1".into(),
                "parent-1".into(),
                "Ignored title".into(),
                None,
                None,
                None,
                false,
            )
            .unwrap();
        assert_eq!(registry.load().unwrap().len(), 1);
        assert_eq!(registry.load().unwrap()[0].parent_thread_id, "parent-1");
        assert_eq!(
            registry.load().unwrap()[0].title.as_deref(),
            Some("First title")
        );

        registry
            .update_metadata(
                "side-1",
                "Updated title".into(),
                Some("new-model".into()),
                Some("New Model".into()),
                Some("medium".into()),
                true,
            )
            .unwrap();
        assert_eq!(
            registry.load().unwrap()[0].title.as_deref(),
            Some("Updated title")
        );
        assert_eq!(
            registry.load().unwrap()[0].model.as_deref(),
            Some("new-model")
        );
        assert!(registry.load().unwrap()[0].has_activity);

        registry.mark_for_deletion("side-1").unwrap();
        assert_eq!(registry.pending_deletion_ids().unwrap(), vec!["side-1"]);

        registry.remove("side-1").unwrap();
        assert!(registry.load().unwrap().is_empty());
    }

    #[test]
    fn loads_side_chats_saved_before_titles_were_persisted() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("side-chats.json");
        fs::write(
            &path,
            br#"{"side_chats":[{"id":"side-1","parent_thread_id":"parent","created_at":1}]}"#,
        )
        .unwrap();

        let records = SideChatRegistry::at(path).load().unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].title, None);
    }

    #[test]
    fn removes_all_side_chats_for_a_parent() {
        let temp = tempdir().unwrap();
        let registry = SideChatRegistry::at(temp.path().join("side-chats.json"));
        registry
            .register(
                "side-1".into(),
                "parent".into(),
                "One".into(),
                None,
                None,
                None,
                false,
            )
            .unwrap();
        registry
            .register(
                "side-2".into(),
                "parent".into(),
                "Two".into(),
                None,
                None,
                None,
                false,
            )
            .unwrap();
        registry
            .register(
                "other".into(),
                "other-parent".into(),
                "Other".into(),
                None,
                None,
                None,
                false,
            )
            .unwrap();

        registry.remove_for_parent("parent").unwrap();

        assert_eq!(registry.load().unwrap().len(), 1);
        assert_eq!(registry.load().unwrap()[0].id, "other");
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
    fn registers_general_threads_without_git_metadata() {
        let temp = tempdir().unwrap();
        let registry = Registry::at(temp.path().join("threads.json"));
        let workspace = temp.path().join("new-chat");
        fs::create_dir(&workspace).unwrap();

        registry
            .register_general_thread("general-1".into(), &workspace)
            .unwrap();

        let thread = registry.load().unwrap().remove(0);
        assert_eq!(thread.scope, ThreadScope::General);
        assert_eq!(thread.kind, ThreadKind::Regular);
        assert_eq!(thread.cwd, workspace);
        assert!(!thread.managed_worktree);
        assert_eq!(thread.worktree_branch, None);
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
        assert_eq!(thread.title, "Untitled thread");
        assert_eq!(thread.scope, ThreadScope::Repository);
        assert_eq!(thread.kind, ThreadKind::Regular);
        assert_eq!(thread.archived_at, None);
        assert!(!thread.managed_worktree);
        assert_eq!(thread.worktree_branch, None);
    }

    #[test]
    fn registers_a_shikigami_help_thread_distinctly() {
        let temp = tempdir().unwrap();
        let registry = Registry::at(temp.path().join("threads.json"));
        let workspace = temp.path().join("new-chat");
        fs::create_dir(&workspace).unwrap();

        registry
            .register_general_thread_with_kind(
                "help-1".into(),
                &workspace,
                ThreadKind::ShikigamiHelp,
            )
            .unwrap();

        let thread = registry.load().unwrap().remove(0);
        assert_eq!(thread.scope, ThreadScope::General);
        assert_eq!(thread.kind, ThreadKind::ShikigamiHelp);
    }
}
