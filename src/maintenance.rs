use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

use crate::paths;

const BACKUPS_DIRECTORY: &str = "maintenance-backups";

#[derive(Clone, Copy)]
enum Root {
    Data,
    Config,
    Cache,
}

#[derive(Clone, Copy)]
struct TrackedFile {
    root: Root,
    name: &'static str,
}

const TRACKED_FILES: &[TrackedFile] = &[
    TrackedFile {
        root: Root::Data,
        name: "onboarding-v1-shown",
    },
    TrackedFile {
        root: Root::Data,
        name: "initial-home-scan-complete",
    },
    TrackedFile {
        root: Root::Data,
        name: "repositories.json",
    },
    TrackedFile {
        root: Root::Data,
        name: "repository-ui.json",
    },
    TrackedFile {
        root: Root::Data,
        name: "repository-roots.json",
    },
    TrackedFile {
        root: Root::Data,
        name: "threads.json",
    },
    TrackedFile {
        root: Root::Data,
        name: "thread-title-cache.json",
    },
    TrackedFile {
        root: Root::Data,
        name: "side-chats.json",
    },
    TrackedFile {
        root: Root::Data,
        name: "attention.json",
    },
    TrackedFile {
        root: Root::Data,
        name: "settings.json",
    },
    TrackedFile {
        root: Root::Config,
        name: "config.json",
    },
    TrackedFile {
        root: Root::Cache,
        name: "repository-candidates.json",
    },
    TrackedFile {
        root: Root::Cache,
        name: "performance-v1.jsonl",
    },
];

struct StatePaths {
    data: PathBuf,
    config: PathBuf,
    cache: PathBuf,
}

impl StatePaths {
    fn discover() -> Result<Self> {
        let dirs = paths::project_dirs()?;
        Ok(Self {
            data: dirs.data_local_dir().to_path_buf(),
            config: dirs.config_dir().to_path_buf(),
            cache: dirs.cache_dir().to_path_buf(),
        })
    }

    #[cfg(test)]
    fn at(root: &Path) -> Self {
        Self {
            data: root.join("data"),
            config: root.join("config"),
            cache: root.join("cache"),
        }
    }

    fn root(&self, root: Root) -> &Path {
        match root {
            Root::Data => &self.data,
            Root::Config => &self.config,
            Root::Cache => &self.cache,
        }
    }

    fn file(&self, tracked: TrackedFile) -> PathBuf {
        self.root(tracked.root).join(tracked.name)
    }

    fn backups(&self) -> PathBuf {
        self.data.join(BACKUPS_DIRECTORY)
    }
}

#[derive(Debug)]
pub struct ResetOutcome {
    pub backup: Option<String>,
    pub file_count: usize,
}

#[derive(Debug)]
pub struct RestoreOutcome {
    pub safety_backup: Option<String>,
    pub file_count: usize,
}

pub fn reset() -> Result<ResetOutcome> {
    reset_at(&StatePaths::discover()?)
}

pub fn restore(backup: &str) -> Result<RestoreOutcome> {
    restore_at(&StatePaths::discover()?, backup)
}

pub fn list_backups() -> Result<Vec<String>> {
    list_backups_at(&StatePaths::discover()?)
}

fn reset_at(paths: &StatePaths) -> Result<ResetOutcome> {
    let existing = existing_files(paths);
    let backup = backup_files(paths, &existing)?;
    clear_files(paths)?;
    Ok(ResetOutcome {
        backup,
        file_count: existing.len(),
    })
}

fn restore_at(paths: &StatePaths, backup: &str) -> Result<RestoreOutcome> {
    validate_backup_name(backup)?;
    let source = paths.backups().join(backup);
    if !source.is_dir() {
        bail!("maintenance backup does not exist: {backup}");
    }

    let current = existing_files(paths);
    let safety_backup = backup_files(paths, &current)?;
    clear_files(paths)?;

    let mut restored = 0;
    for tracked in TRACKED_FILES {
        let backup_file = backup_file(&source, *tracked);
        if !backup_file.is_file() {
            continue;
        }
        let destination = paths.file(*tracked);
        let parent = destination
            .parent()
            .context("tracked state file has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create state directory {}", parent.display()))?;
        fs::copy(&backup_file, &destination).with_context(|| {
            format!(
                "restore state file {} to {}",
                backup_file.display(),
                destination.display()
            )
        })?;
        restored += 1;
    }

    Ok(RestoreOutcome {
        safety_backup,
        file_count: restored,
    })
}

fn existing_files(paths: &StatePaths) -> Vec<TrackedFile> {
    TRACKED_FILES
        .iter()
        .copied()
        .filter(|tracked| paths.file(*tracked).is_file())
        .collect()
}

fn backup_files(paths: &StatePaths, files: &[TrackedFile]) -> Result<Option<String>> {
    if files.is_empty() {
        return Ok(None);
    }

    let backups = paths.backups();
    fs::create_dir_all(&backups)
        .with_context(|| format!("create maintenance backup directory {}", backups.display()))?;
    let backup = unique_backup_name(&backups)?;
    let temporary = backups.join(format!(".{backup}.tmp"));
    fs::create_dir(&temporary)
        .with_context(|| format!("create temporary backup {}", temporary.display()))?;

    let copied = (|| -> Result<()> {
        for tracked in files {
            let source = paths.file(*tracked);
            let destination = backup_file(&temporary, *tracked);
            let parent = destination.parent().context("backup file has no parent")?;
            fs::create_dir_all(parent)
                .with_context(|| format!("create backup directory {}", parent.display()))?;
            fs::copy(&source, &destination).with_context(|| {
                format!(
                    "back up state file {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
        }
        Ok(())
    })();
    if let Err(error) = copied {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }

    fs::rename(&temporary, backups.join(&backup))
        .with_context(|| format!("finalize maintenance backup {backup}"))?;
    Ok(Some(backup))
}

fn clear_files(paths: &StatePaths) -> Result<()> {
    for tracked in TRACKED_FILES {
        let path = paths.file(*tracked);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("remove state file {}", path.display()));
            }
        }
    }
    Ok(())
}

fn backup_file(root: &Path, tracked: TrackedFile) -> PathBuf {
    let directory = match tracked.root {
        Root::Data => "data",
        Root::Config => "config",
        Root::Cache => "cache",
    };
    root.join(directory).join(tracked.name)
}

fn unique_backup_name(backups: &Path) -> Result<String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs();
    let base = format!("reset-{timestamp}");
    if !backups.join(&base).exists() && !backups.join(format!(".{base}.tmp")).exists() {
        return Ok(base);
    }
    for suffix in 2.. {
        let candidate = format!("{base}-{suffix}");
        if !backups.join(&candidate).exists() && !backups.join(format!(".{candidate}.tmp")).exists()
        {
            return Ok(candidate);
        }
    }
    unreachable!("backup suffix range is unbounded")
}

fn validate_backup_name(backup: &str) -> Result<()> {
    let mut components = Path::new(backup).components();
    let is_single_normal =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if !is_single_normal || !backup.starts_with("reset-") {
        bail!("invalid maintenance backup identifier: {backup}");
    }
    Ok(())
}

fn list_backups_at(paths: &StatePaths) -> Result<Vec<String>> {
    let backups = paths.backups();
    if !backups.exists() {
        return Ok(Vec::new());
    }
    let mut names = fs::read_dir(&backups)
        .with_context(|| format!("read maintenance backups {}", backups.display()))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| validate_backup_name(name).is_ok())
        .collect::<Vec<_>>();
    names.sort_unstable_by(|left, right| right.cmp(left));
    Ok(names)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn reset_backs_up_tracked_state_without_touching_worktrees_or_unknown_files() {
        let temp = tempdir().unwrap();
        let paths = StatePaths::at(temp.path());
        fs::create_dir_all(paths.data.join("worktrees/example")).unwrap();
        fs::create_dir_all(&paths.cache).unwrap();
        fs::write(paths.data.join("threads.json"), "threads").unwrap();
        fs::write(paths.cache.join("repository-candidates.json"), "candidates").unwrap();
        fs::write(paths.data.join("worktrees/example/file"), "worktree").unwrap();
        fs::write(paths.data.join("unknown"), "unknown").unwrap();

        let outcome = reset_at(&paths).unwrap();
        let backup = outcome.backup.unwrap();

        assert_eq!(outcome.file_count, 2);
        assert!(!paths.data.join("threads.json").exists());
        assert!(!paths.cache.join("repository-candidates.json").exists());
        assert_eq!(
            fs::read_to_string(paths.data.join("worktrees/example/file")).unwrap(),
            "worktree"
        );
        assert_eq!(
            fs::read_to_string(paths.data.join("unknown")).unwrap(),
            "unknown"
        );
        assert_eq!(
            fs::read_to_string(paths.backups().join(backup).join("data/threads.json")).unwrap(),
            "threads"
        );
    }

    #[test]
    fn restore_saves_current_state_and_replaces_it_with_selected_backup() {
        let temp = tempdir().unwrap();
        let paths = StatePaths::at(temp.path());
        fs::create_dir_all(&paths.data).unwrap();
        fs::write(paths.data.join("threads.json"), "before").unwrap();
        let original = reset_at(&paths).unwrap().backup.unwrap();
        fs::write(paths.data.join("threads.json"), "after").unwrap();
        fs::write(paths.data.join("settings.json"), "settings").unwrap();

        let outcome = restore_at(&paths, &original).unwrap();

        assert_eq!(outcome.file_count, 1);
        assert_eq!(
            fs::read_to_string(paths.data.join("threads.json")).unwrap(),
            "before"
        );
        assert!(!paths.data.join("settings.json").exists());
        let safety = outcome.safety_backup.unwrap();
        assert_eq!(
            fs::read_to_string(paths.backups().join(safety).join("data/threads.json")).unwrap(),
            "after"
        );
    }

    #[test]
    fn restore_rejects_paths_outside_the_backup_directory() {
        let temp = tempdir().unwrap();
        let paths = StatePaths::at(temp.path());

        let error = restore_at(&paths, "../reset-123").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("invalid maintenance backup identifier")
        );
    }
}
