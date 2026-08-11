use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::Deserialize;

const GLOBAL_STATE_FILE: &str = ".codex-global-state.json";

#[derive(Debug, Default, Eq, PartialEq)]
pub struct CodexWorkspaceState {
    pub roots: Vec<PathBuf>,
    pub active_roots: Vec<PathBuf>,
}

#[derive(Default, Deserialize)]
struct GlobalState {
    #[serde(rename = "electron-saved-workspace-roots", default)]
    saved_workspace_roots: Vec<PathBuf>,
    #[serde(rename = "active-workspace-roots", default)]
    active_workspace_roots: Vec<PathBuf>,
    #[serde(rename = "local-projects", default)]
    local_projects: HashMap<String, LocalProject>,
    #[serde(rename = "project-order", default)]
    project_order: Vec<String>,
}

#[derive(Default, Deserialize)]
struct LocalProject {
    #[serde(rename = "rootPaths", default)]
    root_paths: Vec<PathBuf>,
}

pub fn discover() -> Result<Option<CodexWorkspaceState>> {
    let Some(codex_home) = codex_home() else {
        return Ok(None);
    };
    load(&codex_home.join(GLOBAL_STATE_FILE))
}

fn codex_home() -> Option<PathBuf> {
    env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().join(".codex")))
}

fn load(path: &Path) -> Result<Option<CodexWorkspaceState>> {
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let state: GlobalState =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;

    let mut roots = state.saved_workspace_roots;
    if roots.is_empty() {
        roots = ordered_project_roots(&state.local_projects, &state.project_order);
    }
    for root in &state.active_workspace_roots {
        if !roots.contains(root) {
            roots.push(root.clone());
        }
    }
    deduplicate(&mut roots);

    let mut active_roots = state.active_workspace_roots;
    deduplicate(&mut active_roots);
    Ok(Some(CodexWorkspaceState {
        roots,
        active_roots,
    }))
}

fn ordered_project_roots(
    projects: &HashMap<String, LocalProject>,
    project_order: &[String],
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut ordered_ids = project_order.to_vec();
    let mut remaining_ids = projects
        .keys()
        .filter(|id| !project_order.contains(id))
        .cloned()
        .collect::<Vec<_>>();
    remaining_ids.sort();
    ordered_ids.extend(remaining_ids);
    for id in ordered_ids {
        if let Some(project) = projects.get(&id) {
            roots.extend(project.root_paths.iter().cloned());
        }
    }
    roots
}

fn deduplicate(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn loads_saved_and_active_workspace_roots_without_duplicates() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(GLOBAL_STATE_FILE);
        fs::write(
            &path,
            br#"{
                "electron-saved-workspace-roots": ["/one", "/two", "/one"],
                "active-workspace-roots": ["/two", "/three"]
            }"#,
        )
        .unwrap();

        let state = load(&path).unwrap().unwrap();

        assert_eq!(state.roots, ["/one", "/two", "/three"].map(PathBuf::from));
        assert_eq!(state.active_roots, ["/two", "/three"].map(PathBuf::from));
    }

    #[test]
    fn falls_back_to_local_projects_in_project_order() {
        let temp = tempdir().unwrap();
        let path = temp.path().join(GLOBAL_STATE_FILE);
        fs::write(
            &path,
            br#"{
                "local-projects": {
                    "second": {"rootPaths": ["/two"]},
                    "first": {"rootPaths": ["/one"]}
                },
                "project-order": ["first", "second"]
            }"#,
        )
        .unwrap();

        let state = load(&path).unwrap().unwrap();

        assert_eq!(state.roots, ["/one", "/two"].map(PathBuf::from));
        assert!(state.active_roots.is_empty());
    }

    #[test]
    fn missing_global_state_is_not_an_error() {
        let temp = tempdir().unwrap();

        assert_eq!(load(&temp.path().join(GLOBAL_STATE_FILE)).unwrap(), None);
    }
}
