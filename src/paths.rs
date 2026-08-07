use std::{fs, path::Path};

use anyhow::{Context, Result};
use directories::ProjectDirs;

const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "kei-prog";

pub fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, "shikigami")
        .context("cannot determine Shikigami data directory")
}

pub fn legacy_project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, "wyard")
        .context("cannot determine legacy wyard data directory")
}

pub fn migrate_file(legacy: &Path, current: &Path) -> Result<()> {
    if current.exists() || !legacy.is_file() {
        return Ok(());
    }
    let parent = current
        .parent()
        .context("Shikigami state path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create state directory {}", parent.display()))?;
    let temporary = current.with_extension("migration.tmp");
    fs::copy(legacy, &temporary).with_context(|| {
        format!(
            "migrate legacy state {} to {}",
            legacy.display(),
            current.display()
        )
    })?;
    fs::rename(&temporary, current)
        .with_context(|| format!("activate migrated state {}", current.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn migrates_a_legacy_file_once() {
        let temp = tempdir().unwrap();
        let legacy = temp.path().join("legacy/state.json");
        let current = temp.path().join("current/state.json");
        fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        fs::write(&legacy, "legacy").unwrap();

        migrate_file(&legacy, &current).unwrap();
        assert_eq!(fs::read_to_string(&current).unwrap(), "legacy");

        fs::write(&legacy, "changed").unwrap();
        migrate_file(&legacy, &current).unwrap();
        assert_eq!(fs::read_to_string(&current).unwrap(), "legacy");
    }
}
