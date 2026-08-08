use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionMode {
    #[default]
    Auto,
    Dangerous,
}

impl ExecutionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::Dangerous => "DANGEROUS",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Auto => "Write inside the workspace; ask before elevated actions",
            Self::Dangerous => "Full system access without approval prompts",
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
struct SettingsData {
    #[serde(default)]
    execution_mode: ExecutionMode,
}

pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn discover() -> Result<Self> {
        let dirs = paths::project_dirs()?;
        Ok(Self {
            path: dirs.data_local_dir().join("settings.json"),
        })
    }

    #[cfg(test)]
    fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Result<ExecutionMode> {
        if !self.path.exists() {
            return Ok(ExecutionMode::default());
        }
        let bytes = fs::read(&self.path)
            .with_context(|| format!("read settings {}", self.path.display()))?;
        let settings: SettingsData = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse settings {}", self.path.display()))?;
        Ok(settings.execution_mode)
    }

    pub fn save(&self, execution_mode: ExecutionMode) -> Result<()> {
        let parent = self.path.parent().context("settings path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create settings directory {}", parent.display()))?;
        let temporary = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(&SettingsData { execution_mode })?;
        fs::write(&temporary, data)
            .with_context(|| format!("write settings {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("replace settings {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn defaults_to_auto_when_settings_do_not_exist() {
        let temp = tempdir().unwrap();
        let store = SettingsStore::at(temp.path().join("settings.json"));

        assert_eq!(store.load().unwrap(), ExecutionMode::Auto);
    }

    #[test]
    fn saves_and_restores_execution_mode() {
        let temp = tempdir().unwrap();
        let store = SettingsStore::at(temp.path().join("settings.json"));

        store.save(ExecutionMode::Dangerous).unwrap();

        assert_eq!(store.load().unwrap(), ExecutionMode::Dangerous);
    }

    #[test]
    fn missing_execution_mode_in_old_settings_defaults_to_auto() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("settings.json");
        fs::write(&path, "{}").unwrap();

        assert_eq!(SettingsStore::at(path).load().unwrap(), ExecutionMode::Auto);
    }
}
