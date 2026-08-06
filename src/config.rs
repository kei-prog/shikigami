use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub repositories: Vec<Repository>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Repository {
    pub name: String,
    pub path: PathBuf,
}

impl Repository {
    pub fn from_path(path: PathBuf, name: Option<String>) -> Result<Self> {
        let path = fs::canonicalize(&path)
            .with_context(|| format!("repository path does not exist: {}", path.display()))?;
        let inferred_name = path
            .file_name()
            .and_then(|part| part.to_str())
            .context("repository path has no valid directory name")?;
        let name = name.unwrap_or_else(|| inferred_name.to_owned());
        validate_name(&name)?;

        Ok(Self { name, path })
    }
}

impl Config {
    pub fn add_repository(&mut self, repository: Repository) -> Result<()> {
        if self
            .repositories
            .iter()
            .any(|item| item.name == repository.name)
        {
            bail!("repository name already registered: {}", repository.name);
        }
        if self
            .repositories
            .iter()
            .any(|item| item.path == repository.path)
        {
            bail!(
                "repository path already registered: {}",
                repository.path.display()
            );
        }
        self.repositories.push(repository);
        self.repositories.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(())
    }

    pub fn remove_repository(&mut self, name: &str) -> Result<()> {
        let previous_len = self.repositories.len();
        self.repositories
            .retain(|repository| repository.name != name);
        if self.repositories.len() == previous_len {
            bail!("repository is not registered: {name}");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
    data_dir: PathBuf,
}

impl ConfigStore {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "kei-prog", "wyard")
            .context("cannot determine wyard config directory")?;
        Ok(Self {
            path: dirs.config_dir().join("config.toml"),
            data_dir: dirs.data_local_dir().to_path_buf(),
        })
    }

    #[cfg(test)]
    pub fn at(path: PathBuf) -> Self {
        let data_dir = path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("data");
        Self { path, data_dir }
    }

    pub fn load(&self) -> Result<Config> {
        if !self.path.exists() {
            return Ok(Config::default());
        }
        let contents = fs::read_to_string(&self.path)
            .with_context(|| format!("read {}", self.path.display()))?;
        toml::from_str(&contents).with_context(|| format!("parse {}", self.path.display()))
    }

    pub fn save(&self, config: &Config) -> Result<()> {
        let parent = self.path.parent().context("config path has no parent")?;
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let contents = toml::to_string_pretty(config).context("serialize config")?;
        let temporary = self.path.with_extension("toml.tmp");
        fs::write(&temporary, contents)
            .with_context(|| format!("write {}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .with_context(|| format!("replace {}", self.path.display()))?;
        Ok(())
    }

    pub fn workspace_path(&self, repository: &Repository, workspace_name: &str) -> Result<PathBuf> {
        validate_name(workspace_name)?;
        Ok(self
            .data_dir
            .join("workspaces")
            .join(&repository.name)
            .join(workspace_name))
    }
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("name must contain only ASCII letters, numbers, '.', '_' or '-'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn config_round_trip() {
        let temp = tempdir().unwrap();
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        let store = ConfigStore::at(temp.path().join("config.toml"));
        let mut config = Config::default();
        config
            .add_repository(Repository::from_path(repo, None).unwrap())
            .unwrap();

        store.save(&config).unwrap();

        assert_eq!(store.load().unwrap().repositories, config.repositories);
    }

    #[test]
    fn rejects_unsafe_names() {
        assert!(validate_name("feature-one").is_ok());
        assert!(validate_name("../outside").is_err());
        assert!(validate_name("two words").is_err());
    }
}
