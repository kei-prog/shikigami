use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};

use crate::chat::{ChatMessage, ChatRole, ChatState};

const PREVIEW_LIFETIME: Duration = Duration::from_secs(10 * 60);
const MERMAID_SCRIPT: &str = "https://cdn.jsdelivr.net/npm/mermaid@11.12.0/dist/mermaid.min.js";
static NEXT_PREVIEW_FILE: AtomicU64 = AtomicU64::new(0);
static PREVIEW_FILES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

pub struct PreviewSnapshot {
    title: String,
    messages: Vec<ChatMessage>,
    selected: Option<usize>,
}

impl PreviewSnapshot {
    pub fn from_chat(chat: &ChatState) -> Self {
        Self {
            title: chat.title.clone(),
            messages: chat.messages.clone(),
            selected: chat.selected_message_index,
        }
    }
}

pub async fn open(snapshot: PreviewSnapshot) -> Result<bool> {
    let selected = snapshot.selected;
    let path = tokio::task::spawn_blocking(move || {
        let html = render_document(&snapshot.title, &snapshot.messages, selected);
        write_preview_file(&html)
    })
    .await
    .context("render browser preview")??;
    preview_files()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(path.clone());
    let mut child = match launch_browser(&path) {
        Ok(child) => child,
        Err(error) => {
            remove_preview_file(&path);
            return Err(error);
        }
    };
    tokio::spawn(async move {
        let _ = child.wait().await;
        tokio::time::sleep(PREVIEW_LIFETIME).await;
        remove_preview_file(&path);
    });
    Ok(selected.is_some())
}

pub fn cleanup() {
    let paths = preview_files()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .drain()
        .collect::<Vec<_>>();
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

fn preview_files() -> &'static Mutex<HashSet<PathBuf>> {
    PREVIEW_FILES.get_or_init(|| Mutex::new(HashSet::new()))
}

fn remove_preview_file(path: &Path) {
    let _ = fs::remove_file(path);
    preview_files()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(path);
}

fn render_document(title: &str, messages: &[ChatMessage], selected: Option<usize>) -> String {
    let mut body = String::new();
    for (index, message) in messages.iter().enumerate() {
        if message.content.is_empty() {
            continue;
        }
        let (role, class) = match message.role {
            ChatRole::User => ("You", "user"),
            ChatRole::Assistant => ("Codex", "assistant"),
            ChatRole::Activity => ("Activity", "activity"),
            ChatRole::Diff => ("Changes", "diff"),
        };
        let selection_class = if selected == Some(index) {
            " selected"
        } else {
            ""
        };
        body.push_str(&format!(
            "<article id=\"message-{}\" class=\"message {}{}\"><header>{}</header><div class=\"markdown\">{}</div></article>",
            index + 1,
            class,
            selection_class,
            role,
            markdown_html(&message.content),
        ));
    }
    let selected_script = selected.map_or_else(String::new, |index| {
        format!(
            "document.getElementById('message-{}')?.scrollIntoView({{block:'center'}});",
            index + 1
        )
    });
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline' https://cdn.jsdelivr.net; style-src 'unsafe-inline'; img-src data:; connect-src 'none'; font-src 'none'; base-uri 'none'; form-action 'none'">
<title>{title}</title>
<style>
:root {{ color-scheme: light dark; --bg:#0f1115; --panel:#171a21; --muted:#9299a6; --text:#e6e8ec; --accent:#70b7ff; --user:#172438; --activity:#15171b; --border:#303642; --selected:#e6b450; }}
* {{ box-sizing:border-box; }}
html {{ scroll-behavior:smooth; }}
body {{ margin:0; background:var(--bg); color:var(--text); font:15px/1.55 ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; }}
main {{ width:min(100% - 32px, 1040px); margin:0 auto; padding:32px 0 72px; }}
h1 {{ margin:0 0 28px; font-size:22px; overflow-wrap:anywhere; }}
.message {{ margin:0 0 18px; padding:18px 20px; border:1px solid var(--border); border-radius:10px; background:var(--panel); scroll-margin:15vh 0; }}
.message.user {{ background:var(--user); }}
.message.activity,.message.diff {{ background:var(--activity); color:#c4c9d1; }}
.message.selected {{ border-color:var(--selected); box-shadow:0 0 0 2px color-mix(in srgb,var(--selected) 35%,transparent); animation:selected 2.4s ease-out; }}
@keyframes selected {{ from {{ background:#4a3c20; }} }}
header {{ margin-bottom:10px; color:var(--accent); font-size:12px; font-weight:700; letter-spacing:.08em; text-transform:uppercase; }}
.markdown > :first-child {{ margin-top:0; }} .markdown > :last-child {{ margin-bottom:0; }}
h2,h3,h4,h5,h6 {{ line-height:1.25; }}
a {{ color:var(--accent); }}
blockquote {{ margin-left:0; padding-left:14px; border-left:3px solid var(--border); color:#bac0ca; }}
code,pre {{ font-family:ui-monospace,SFMono-Regular,Consolas,"Liberation Mono",monospace; }}
code {{ padding:.12em .32em; border-radius:4px; background:#242933; }}
pre {{ overflow:auto; padding:14px; border-radius:7px; background:#0a0c10; line-height:1.45; }}
pre code {{ padding:0; background:transparent; }}
table {{ display:block; max-width:100%; overflow-x:auto; border-collapse:collapse; }}
th,td {{ padding:7px 10px; border:1px solid var(--border); text-align:left; vertical-align:top; }}
th {{ background:#20252e; }}
.mermaid {{ background:#fff; color:#111; }}
hr {{ border:0; border-top:1px solid var(--border); }}
@media (prefers-color-scheme:light) {{ :root {{ --bg:#f6f7f9; --panel:#fff; --muted:#646b76; --text:#1e222a; --accent:#0969da; --user:#edf5ff; --activity:#f1f3f5; --border:#d5d9df; --selected:#9a6700; }} pre {{ background:#161b22; color:#f0f3f6; }} code {{ background:#e8ebef; }} pre code {{ background:transparent; }} }}
</style>
</head>
<body><main><h1>{title}</h1>{body}</main>
<script src="{mermaid_script}"></script>
<script>
document.querySelectorAll('a').forEach(a => {{ a.target='_blank'; a.rel='noopener noreferrer'; }});
document.querySelectorAll('pre > code.language-mermaid').forEach(code => {{
  const diagram=document.createElement('pre'); diagram.className='mermaid'; diagram.textContent=code.textContent; code.parentElement.replaceWith(diagram);
}});
if(window.mermaid) {{ mermaid.initialize({{startOnLoad:false,securityLevel:'strict',theme:'default'}}); mermaid.run({{querySelector:'.mermaid'}}).catch(() => {{}}); }}
{selected_script}
</script>
</body></html>"#,
        title = escape_html(title),
        body = body,
        mermaid_script = MERMAID_SCRIPT,
        selected_script = selected_script,
    )
}

fn markdown_html(markdown: &str) -> String {
    let parser = Parser::new_ext(
        markdown,
        Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TABLES
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES,
    );
    let safe_events = parser.map(|event| match event {
        Event::Html(value) | Event::InlineHtml(value) => Event::Text(value),
        Event::Start(Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) => Event::Start(Tag::Link {
            link_type,
            dest_url: safe_web_destination(dest_url),
            title,
            id,
        }),
        Event::Start(Tag::Image {
            link_type,
            dest_url: _,
            title,
            id,
        }) => Event::Start(Tag::Image {
            link_type,
            dest_url: "".into(),
            title,
            id,
        }),
        event => event,
    });
    let mut output = String::new();
    html::push_html(&mut output, safe_events);
    output
}

fn safe_web_destination(destination: CowStr<'_>) -> CowStr<'_> {
    if destination.starts_with("https://") || destination.starts_with("http://") {
        destination
    } else {
        "#".into()
    }
}

fn write_preview_file(contents: &str) -> Result<PathBuf> {
    let sequence = NEXT_PREVIEW_FILE.fetch_add(1, Ordering::Relaxed);
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "shikigami-chat-preview-{}-{created_at}-{sequence}.html",
        std::process::id(),
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    restrict_to_current_user(&mut options);
    let mut file = options
        .open(&path)
        .with_context(|| format!("create browser preview {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("write browser preview {}", path.display()))?;
    Ok(path)
}

#[cfg(unix)]
fn restrict_to_current_user(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn restrict_to_current_user(_options: &mut OpenOptions) {}

fn launch_browser(path: &Path) -> Result<tokio::process::Child> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = tokio::process::Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = tokio::process::Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler").arg(path);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = {
        let mut command = tokio::process::Command::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("launch the system browser for {}", path.display()))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_tables_and_mermaid_source() {
        let rendered = markdown_html(
            "| Name | State |\n| --- | --- |\n| Shi | Ready |\n\n```mermaid\nflowchart LR\nA --> B\n```",
        );

        assert!(rendered.contains("<table>"));
        assert!(rendered.contains("class=\"language-mermaid\""));
        assert!(rendered.contains("flowchart LR"));
    }

    #[test]
    fn markdown_escapes_raw_html_and_disables_unsafe_links() {
        let rendered = markdown_html("<script>alert(1)</script> [bad](javascript:alert)");

        assert!(rendered.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!rendered.contains("href=\"javascript:"));
    }

    #[test]
    fn document_marks_and_scrolls_to_selected_message() {
        let messages = vec![
            ChatMessage::for_preview_test(ChatRole::User, "question"),
            ChatMessage::for_preview_test(ChatRole::Assistant, "answer"),
        ];
        let rendered = render_document("A <thread>", &messages, Some(1));

        assert!(rendered.contains("<title>A &lt;thread&gt;</title>"));
        assert!(rendered.contains("id=\"message-2\" class=\"message assistant selected\""));
        assert!(rendered.contains("getElementById('message-2')"));
        assert!(rendered.contains(MERMAID_SCRIPT));
    }

    #[test]
    fn cleanup_removes_tracked_preview_files() {
        let path = write_preview_file("preview").unwrap();
        preview_files()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(path.clone());

        cleanup();

        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn preview_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let path = write_preview_file("preview").unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        fs::remove_file(path).unwrap();

        assert_eq!(mode, 0o600);
    }
}
