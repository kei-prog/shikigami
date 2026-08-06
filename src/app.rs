use anyhow::{Result, bail};

use crate::{
    ghq::{self, Repository},
    git_workspace::{self, Workspace},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    Repositories,
    Workspaces,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    Normal,
    AddWorkspace(String),
    ConfirmRemove(String),
    Help,
}

pub struct App {
    pub repositories: Vec<Repository>,
    pub workspaces: Vec<Workspace>,
    pub repository_index: usize,
    pub workspace_index: usize,
    pub focus: Focus,
    pub mode: Mode,
    pub message: Option<String>,
    pub should_quit: bool,
}

impl App {
    pub fn load() -> Result<Self> {
        let repositories = ghq::discover_repositories()?;
        let mut app = Self {
            repositories,
            workspaces: Vec::new(),
            repository_index: 0,
            workspace_index: 0,
            focus: Focus::Repositories,
            mode: Mode::Normal,
            message: None,
            should_quit: false,
        };
        app.refresh_workspaces();
        Ok(app)
    }

    pub fn selected_repository(&self) -> Option<&Repository> {
        self.repositories.get(self.repository_index)
    }

    pub fn selected_workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(self.workspace_index)
    }

    pub fn move_up(&mut self) {
        match self.focus {
            Focus::Repositories => {
                self.repository_index = self.repository_index.saturating_sub(1);
                self.workspace_index = 0;
                self.refresh_workspaces();
            }
            Focus::Workspaces => self.workspace_index = self.workspace_index.saturating_sub(1),
        }
    }

    pub fn move_down(&mut self) {
        match self.focus {
            Focus::Repositories => {
                if self.repository_index + 1 < self.repositories.len() {
                    self.repository_index += 1;
                    self.workspace_index = 0;
                    self.refresh_workspaces();
                }
            }
            Focus::Workspaces => {
                if self.workspace_index + 1 < self.workspaces.len() {
                    self.workspace_index += 1;
                }
            }
        }
    }

    pub fn refresh_workspaces(&mut self) {
        self.workspaces.clear();
        let Some(repository) = self.selected_repository().cloned() else {
            return;
        };
        match git_workspace::list_workspaces(&repository.path) {
            Ok(workspaces) => self.workspaces = workspaces,
            Err(error) => self.message = Some(error.to_string()),
        }
        self.workspace_index = self
            .workspace_index
            .min(self.workspaces.len().saturating_sub(1));
    }

    pub fn add_workspace(&mut self, name: &str) -> Result<()> {
        let repository = self
            .selected_repository()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("register a repository first"))?;
        ghq::validate_name(name)?;
        if self
            .workspaces
            .iter()
            .any(|workspace| workspace.name == name)
        {
            bail!("workspace already exists: {name}");
        }
        let destination = ghq::workspace_path(&repository, name)?;
        git_workspace::add_workspace(&repository.path, name, &destination)?;
        self.refresh_workspaces();
        self.workspace_index = self
            .workspaces
            .iter()
            .position(|workspace| workspace.name == name)
            .unwrap_or(0);
        Ok(())
    }

    pub fn remove_selected_workspace(&mut self) -> Result<()> {
        let repository = self
            .selected_repository()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no repository selected"))?;
        let workspace = self
            .selected_workspace()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no workspace selected"))?;
        if workspace.is_primary {
            bail!("the primary workspace cannot be removed from wyard");
        }
        git_workspace::remove_workspace(&repository.path, &workspace.path)?;
        self.refresh_workspaces();
        Ok(())
    }

    pub fn selected_workspace_status(&self) -> Result<String> {
        let workspace = self
            .selected_workspace()
            .ok_or_else(|| anyhow::anyhow!("no workspace selected"))?;
        git_workspace::workspace_status(&workspace.path)
    }

    pub fn refresh_repositories(&mut self) {
        let selected_path = self
            .selected_repository()
            .map(|repository| repository.path.clone());
        match ghq::discover_repositories() {
            Ok(repositories) => {
                self.repositories = repositories;
                self.repository_index = selected_path
                    .and_then(|path| {
                        self.repositories
                            .iter()
                            .position(|repository| repository.path == path)
                    })
                    .unwrap_or(0)
                    .min(self.repositories.len().saturating_sub(1));
            }
            Err(error) => self.message = Some(error.to_string()),
        }
        self.workspace_index = 0;
        self.refresh_workspaces();
    }
}
