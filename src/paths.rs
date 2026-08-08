use anyhow::{Context, Result};
use directories::ProjectDirs;

const QUALIFIER: &str = "dev";
const ORGANIZATION: &str = "kei-prog";

pub fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, "shikigami")
        .context("cannot determine Shikigami data directory")
}
