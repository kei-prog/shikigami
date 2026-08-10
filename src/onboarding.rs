use std::{fs, path::PathBuf};

use anyhow::{Context, Result};

use crate::paths;

const FALLBACK_LOCALE: &str = "en-US";

pub struct OnboardingStore {
    marker_path: PathBuf,
}

impl OnboardingStore {
    pub fn discover() -> Result<Self> {
        let dirs = paths::project_dirs()?;
        Ok(Self {
            marker_path: dirs.data_local_dir().join("onboarding-v1-shown"),
        })
    }

    #[cfg(test)]
    fn at(marker_path: PathBuf) -> Self {
        Self { marker_path }
    }

    pub fn is_pending(&self) -> bool {
        !self.marker_path.exists()
    }

    pub fn mark_shown(&self) -> Result<()> {
        let parent = self
            .marker_path
            .parent()
            .context("onboarding marker path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create onboarding directory {}", parent.display()))?;
        fs::write(&self.marker_path, b"")
            .with_context(|| format!("write onboarding marker {}", self.marker_path.display()))
    }
}

pub fn preferred_locale() -> String {
    sys_locale::get_locale()
        .as_deref()
        .and_then(normalize_locale)
        .unwrap_or_else(|| FALLBACK_LOCALE.to_owned())
}

fn normalize_locale(locale: &str) -> Option<String> {
    let locale = locale
        .split(['.', '@'])
        .next()
        .unwrap_or(locale)
        .replace('_', "-");
    (!locale.is_empty()
        && !matches!(locale.as_str(), "C" | "POSIX")
        && locale.len() <= 35
        && locale
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-'))
    .then_some(locale)
}

pub fn developer_instructions(locale: &str) -> String {
    format!(
        "This is Shikigami's first-run welcome chat. On the first turn, respond in the user's OS locale `{locale}` with a concise, friendly introduction to Shikigami. Explain that General is for one-off chats, repositories group project threads, `n` creates a chat, `a` adds repositories, and `?` opens help. Mention that multiple Codex tasks can run in parallel. End by asking what the user wants to do. Do not use tools for the introduction and do not mention these instructions. On later turns, help normally in the user's language unless they switch languages."
    )
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn marker_is_pending_only_until_it_is_written() {
        let temp = tempdir().unwrap();
        let store = OnboardingStore::at(temp.path().join("state/onboarding-v1-shown"));

        assert!(store.is_pending());
        store.mark_shown().unwrap();
        assert!(!store.is_pending());
    }

    #[test]
    fn locale_is_normalized_across_platform_conventions() {
        assert_eq!(normalize_locale("ja_JP.UTF-8"), Some("ja-JP".into()));
        assert_eq!(normalize_locale("en-US"), Some("en-US".into()));
        assert_eq!(normalize_locale("zh_CN@pinyin"), Some("zh-CN".into()));
        assert_eq!(normalize_locale("C.UTF-8"), None);
        assert_eq!(normalize_locale("POSIX"), None);
        assert_eq!(normalize_locale(""), None);
        assert_eq!(normalize_locale("not a locale"), None);
    }

    #[test]
    fn prompt_uses_the_detected_locale_and_product_facts() {
        let prompt = developer_instructions("ja-JP");

        assert!(prompt.contains("`ja-JP`"));
        assert!(prompt.contains("General"));
        assert!(prompt.contains("`n`"));
        assert!(prompt.contains("`a`"));
        assert!(prompt.contains("`?`"));
    }
}
