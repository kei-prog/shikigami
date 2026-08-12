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
    let mut visible_messages = 0usize;
    for (index, message) in messages.iter().enumerate() {
        if message.content.is_empty() {
            continue;
        }
        visible_messages += 1;
        let (role, class, marker) = match message.role {
            ChatRole::User => ("You", "user", "Y"),
            ChatRole::Assistant => ("Codex", "assistant", "C"),
            ChatRole::Activity => ("Activity", "activity", "·"),
            ChatRole::Diff => ("Changes", "diff", "±"),
        };
        let selection_class = if selected == Some(index) {
            " selected"
        } else {
            ""
        };
        let content = markdown_html(&message.content);
        if matches!(message.role, ChatRole::Activity | ChatRole::Diff) {
            let summary = escape_html(&message_summary(&message.content));
            body.push_str(&format!(
                "<details id=\"message-{}\" class=\"message auxiliary {}{}\"><summary><span class=\"role-marker\">{}</span><span class=\"summary-kind\">{}</span><span class=\"summary-text\">{}</span><span class=\"summary-hint\">Details</span></summary><div class=\"markdown auxiliary-content\">{}</div></details>",
                index + 1,
                class,
                selection_class,
                marker,
                role,
                summary,
                content,
            ));
        } else {
            body.push_str(&format!(
                "<article id=\"message-{}\" class=\"message {}{}\"><header><span class=\"role-marker\">{}</span><span>{}</span></header><div class=\"markdown\">{}</div></article>",
                index + 1,
                class,
                selection_class,
                marker,
                role,
                content,
            ));
        }
    }
    let selected_script = selected.map_or_else(String::new, |index| {
        format!(
            "const selectedMessage=document.getElementById('message-{}'); if(selectedMessage?.tagName==='DETAILS') selectedMessage.open=true; selectedMessage?.scrollIntoView({{block:'center'}});",
            index + 1
        )
    });
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'unsafe-inline' https://cdn.jsdelivr.net; style-src 'unsafe-inline'; img-src data: blob:; connect-src 'none'; font-src 'none'; base-uri 'none'; form-action 'none'">
<title>{title}</title>
<style>
:root {{ color-scheme:light dark; --bg:#0c0f14; --surface:#131820; --surface-raised:#181e27; --surface-soft:#10151c; --text:#e8ebf0; --text-soft:#c1c7d0; --muted:#818a98; --accent:#71b7ff; --accent-soft:#16283d; --border:#29313d; --border-strong:#3a4655; --selected:#e4b45c; --code:#090c11; --diagram:#f7f9fc; --diagram-text:#17202b; --shadow:0 18px 50px rgba(0,0,0,.22); }}
* {{ box-sizing:border-box; }}
html {{ scroll-behavior:smooth; }}
body {{ margin:0; background:var(--bg); color:var(--text); font:16px/1.72 ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; text-rendering:optimizeLegibility; }}
.page-header {{ position:sticky; top:0; z-index:20; border-bottom:1px solid var(--border); background:color-mix(in srgb,var(--bg) 88%,transparent); backdrop-filter:blur(16px); }}
.page-header-inner {{ width:min(100% - 36px, 920px); min-height:68px; margin:0 auto; display:flex; align-items:center; justify-content:space-between; gap:24px; }}
.eyebrow {{ color:var(--accent); font-size:11px; font-weight:750; letter-spacing:.13em; text-transform:uppercase; }}
.page-title {{ margin:2px 0 0; max-width:680px; overflow:hidden; color:var(--text); font-size:15px; font-weight:650; text-overflow:ellipsis; white-space:nowrap; }}
.page-meta {{ flex:none; color:var(--muted); font-size:12px; }}
main {{ width:min(100% - 36px, 920px); margin:0 auto; padding:38px 0 88px; }}
.message {{ position:relative; margin:0 0 26px; scroll-margin:110px 0; }}
.message.assistant {{ padding:3px 2px 24px; border-bottom:1px solid var(--border); }}
.message.user {{ margin-left:auto; padding:17px 20px; border:1px solid #29496a; border-radius:14px; background:linear-gradient(145deg,var(--accent-soft),#111d2a); box-shadow:0 10px 28px rgba(0,0,0,.12); }}
.message.selected {{ border-color:var(--selected); box-shadow:0 0 0 2px color-mix(in srgb,var(--selected) 38%,transparent); animation:selected 2.4s ease-out; }}
@keyframes selected {{ from {{ background-color:#47391d; }} }}
header,summary {{ display:flex; align-items:center; gap:9px; color:var(--muted); font-size:12px; font-weight:750; letter-spacing:.06em; text-transform:uppercase; }}
header {{ margin-bottom:12px; }}
.role-marker {{ display:inline-grid; width:23px; height:23px; place-items:center; border:1px solid var(--border-strong); border-radius:7px; color:var(--accent); background:var(--surface-raised); font-size:11px; letter-spacing:0; }}
.user .role-marker {{ border-color:#39658f; background:#1d3853; color:#d7ebff; }}
.markdown {{ color:var(--text); overflow-wrap:anywhere; }}
.markdown > :first-child {{ margin-top:0; }} .markdown > :last-child {{ margin-bottom:0; }}
.markdown p {{ margin:0 0 1em; }}
h1,h2,h3,h4,h5,h6 {{ margin:1.65em 0 .65em; line-height:1.25; letter-spacing:-.018em; }}
h1 {{ font-size:1.65rem; }} h2 {{ font-size:1.38rem; }} h3 {{ font-size:1.18rem; }}
ul,ol {{ padding-left:1.55em; }} li + li {{ margin-top:.35em; }}
a {{ color:var(--accent); text-underline-offset:3px; }}
blockquote {{ margin:1.2em 0; padding:2px 0 2px 16px; border-left:3px solid var(--border-strong); color:var(--text-soft); }}
code,pre {{ font-family:ui-monospace,SFMono-Regular,Consolas,"Liberation Mono",monospace; }}
code {{ padding:.14em .34em; border:1px solid var(--border); border-radius:5px; background:var(--surface-raised); font-size:.91em; }}
pre {{ overflow:auto; margin:1.25em 0; padding:16px 18px; border:1px solid var(--border); border-radius:10px; background:var(--code); font-size:13px; line-height:1.55; box-shadow:inset 0 1px rgba(255,255,255,.025); }}
pre code {{ padding:0; background:transparent; }}
table {{ display:block; max-width:100%; margin:1.35em 0; overflow-x:auto; border-spacing:0; border-collapse:separate; border:1px solid var(--border); border-radius:10px; }}
th,td {{ min-width:120px; padding:10px 13px; border-right:1px solid var(--border); border-bottom:1px solid var(--border); text-align:left; vertical-align:top; }}
th:last-child,td:last-child {{ border-right:0; }} tr:last-child td {{ border-bottom:0; }}
th {{ background:var(--surface-raised); color:var(--text-soft); font-size:13px; }}
.auxiliary {{ overflow:hidden; border:1px solid var(--border); border-radius:10px; background:var(--surface-soft); color:var(--text-soft); }}
.auxiliary summary {{ min-height:44px; padding:9px 13px; cursor:pointer; list-style:none; }}
.auxiliary summary::-webkit-details-marker {{ display:none; }}
.auxiliary summary::before {{ content:'›'; color:var(--muted); font-size:18px; transition:transform .16s ease; }}
.auxiliary[open] summary::before {{ transform:rotate(90deg); }}
.summary-kind {{ flex:none; }}
.summary-text {{ min-width:0; overflow:hidden; color:var(--text-soft); font-weight:550; letter-spacing:0; text-overflow:ellipsis; text-transform:none; white-space:nowrap; }}
.summary-text::before {{ content:'·'; margin-right:9px; color:var(--muted); }}
.summary-hint {{ margin-left:auto; color:var(--muted); font-size:10px; font-weight:500; letter-spacing:.04em; text-transform:none; }}
.auxiliary[open] .summary-hint {{ visibility:hidden; }}
.auxiliary-content {{ padding:0 16px 15px 45px; font-size:14px; }}
.diagram-card {{ margin:1.45em 0; overflow:hidden; border:1px solid var(--border-strong); border-radius:13px; background:var(--diagram); color:var(--diagram-text); box-shadow:var(--shadow); }}
.diagram-toolbar {{ display:flex; min-height:48px; align-items:center; gap:6px; padding:7px 9px 7px 14px; border-bottom:1px solid #d9dee6; background:#eef2f7; color:#455160; }}
.diagram-title {{ margin-right:auto; font-size:12px; font-weight:750; letter-spacing:.08em; text-transform:uppercase; }}
.diagram-status {{ color:#697586; font-size:11px; }}
.diagram-button {{ min-width:34px; height:32px; padding:0 9px; border:1px solid #c9d0da; border-radius:7px; background:#fff; color:#344052; cursor:pointer; font:600 12px/1 ui-sans-serif,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif; }}
.diagram-button:hover {{ border-color:#8d99a9; background:#f7f9fc; }}
.diagram-button:focus-visible {{ outline:2px solid #2979c9; outline-offset:2px; }}
.diagram-viewport {{ min-height:260px; max-height:72vh; overflow:auto; padding:26px; background:linear-gradient(#fff,#f8fafc); }}
.diagram-canvas {{ display:flex; width:100%; min-width:max-content; min-height:208px; align-items:center; justify-content:center; transition:width .12s ease; }}
.diagram-canvas svg {{ display:block; height:auto; max-width:none; filter:drop-shadow(0 5px 10px rgba(31,42,55,.08)); }}
.diagram-source {{ display:none; margin:0; border:0; border-top:1px solid #d9dee6; border-radius:0; background:#111720; color:#dce4ef; box-shadow:none; }}
.diagram-card.show-source .diagram-source {{ display:block; }}
.diagram-card.is-error .diagram-viewport {{ display:none; }}
.diagram-card.is-error .diagram-source {{ display:block; }}
.diagram-card:fullscreen {{ display:flex; flex-direction:column; border:0; border-radius:0; }}
.diagram-card:fullscreen .diagram-viewport {{ max-height:none; flex:1; }}
.diagram-card:fullscreen .diagram-canvas {{ min-height:calc(100vh - 110px); }}
hr {{ border:0; border-top:1px solid var(--border); }}
@media (max-width:680px) {{ body {{ font-size:15px; }} .page-header-inner,main {{ width:min(100% - 24px,920px); }} .page-meta,.diagram-status {{ display:none; }} .diagram-toolbar {{ padding-left:10px; }} .diagram-button {{ min-width:32px; padding:0 7px; }} .diagram-viewport {{ padding:14px; }} }}
@media (prefers-color-scheme:light) {{ :root {{ --bg:#f4f6f8; --surface:#fff; --surface-raised:#f1f4f7; --surface-soft:#f7f8fa; --text:#20262e; --text-soft:#4f5966; --muted:#6f7884; --accent:#0969da; --accent-soft:#e8f2ff; --border:#d8dde4; --border-strong:#c5ccd6; --selected:#9a6700; --code:#151a21; --shadow:0 18px 45px rgba(33,43,54,.12); }} .message.user {{ background:linear-gradient(145deg,#edf6ff,#e6f1fd); }} pre {{ color:#edf2f7; }} code {{ color:#27313d; }} pre code {{ color:inherit; }} }}
@media print {{ .page-header {{ position:static; }} .diagram-toolbar {{ display:none; }} .auxiliary:not([open]) {{ display:none; }} main {{ width:100%; }} }}
</style>
</head>
<body><div class="page-header"><div class="page-header-inner"><div><div class="eyebrow">Shikigami preview</div><div class="page-title">{title}</div></div><div class="page-meta">{visible_messages} messages</div></div></div><main>{body}</main>
<script src="{mermaid_script}"></script>
<script>
document.querySelectorAll('a').forEach(a => {{ a.target='_blank'; a.rel='noopener noreferrer'; }});
const diagrams=[];
document.querySelectorAll('pre > code.language-mermaid').forEach((code,index) => {{
  const source=code.textContent;
  const card=document.createElement('section'); card.className='diagram-card';
  const toolbar=document.createElement('div'); toolbar.className='diagram-toolbar';
  const heading=document.createElement('span'); heading.className='diagram-title'; heading.textContent=`Diagram ${{index+1}}`;
  const status=document.createElement('span'); status.className='diagram-status'; status.textContent='Fit';
  const viewport=document.createElement('div'); viewport.className='diagram-viewport';
  const canvas=document.createElement('div'); canvas.className='diagram-canvas'; canvas.textContent=source;
  const sourceView=document.createElement('pre'); sourceView.className='diagram-source';
  const sourceCode=document.createElement('code'); sourceCode.textContent=source; sourceView.append(sourceCode);
  const button=(label,title,action) => {{ const item=document.createElement('button'); item.type='button'; item.className='diagram-button'; item.textContent=label; item.title=title; item.setAttribute('aria-label',title); item.dataset.action=action; return item; }};
  toolbar.setAttribute('aria-label',`Diagram ${{index+1}} controls`);
  toolbar.append(heading,status,button('−','Zoom out','out'),button('Fit','Fit diagram','fit'),button('+','Zoom in','in'),button('Source','Show source','source'),button('Full','Full screen','full'),button('SVG','Download SVG','download'));
  viewport.append(canvas); card.append(toolbar,viewport,sourceView); code.parentElement.replaceWith(card);
  diagrams.push({{card,canvas,status,source,zoom:1}});
}});
const fitDiagram=(diagram) => {{ diagram.zoom=1; diagram.canvas.style.width='100%'; diagram.status.textContent='Fit'; diagram.card.querySelector('.diagram-viewport').scrollTo(0,0); }};
const setZoom=(diagram,next) => {{ diagram.zoom=Math.min(3,Math.max(.5,next)); diagram.canvas.style.width=`${{diagram.zoom*100}}%`; diagram.status.textContent=`${{Math.round(diagram.zoom*100)}}%`; }};
document.addEventListener('click',event => {{
  const control=event.target.closest('[data-action]'); if(!control) return;
  const card=control.closest('.diagram-card'); const diagram=diagrams.find(item => item.card===card); if(!diagram) return;
  const action=control.dataset.action;
  if(action==='in') setZoom(diagram,diagram.zoom+.25);
  if(action==='out') setZoom(diagram,diagram.zoom-.25);
  if(action==='fit') fitDiagram(diagram);
  if(action==='source') {{ card.classList.toggle('show-source'); control.textContent=card.classList.contains('show-source')?'Hide source':'Source'; }}
  if(action==='full') {{ if(document.fullscreenElement===card) document.exitFullscreen(); else card.requestFullscreen?.(); }}
  if(action==='download') {{ const svg=diagram.canvas.querySelector('svg'); if(!svg) return; const blob=new Blob([new XMLSerializer().serializeToString(svg)],{{type:'image/svg+xml'}}); const link=document.createElement('a'); link.href=URL.createObjectURL(blob); link.download=`shikigami-diagram-${{diagrams.indexOf(diagram)+1}}.svg`; link.click(); setTimeout(() => URL.revokeObjectURL(link.href),1000); }}
}});
if(window.mermaid) {{
  mermaid.initialize({{startOnLoad:false,securityLevel:'strict',theme:'default',flowchart:{{htmlLabels:true,useMaxWidth:true}},sequence:{{useMaxWidth:true}},themeVariables:{{fontFamily:'ui-sans-serif, -apple-system, BlinkMacSystemFont, Segoe UI, sans-serif',fontSize:'15px'}}}});
  Promise.all(diagrams.map(async (diagram,index) => {{ try {{ const rendered=await mermaid.render(`shi-diagram-${{index}}`,diagram.source); diagram.canvas.innerHTML=rendered.svg; rendered.bindFunctions?.(diagram.canvas); fitDiagram(diagram); }} catch(error) {{ diagram.card.classList.add('is-error'); diagram.status.textContent='Could not render'; }} }}));
}} else {{ diagrams.forEach(diagram => {{ diagram.card.classList.add('is-error'); diagram.status.textContent='Mermaid unavailable'; }}); }}
{selected_script}
</script>
</body></html>"#,
        title = escape_html(title),
        body = body,
        visible_messages = visible_messages,
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

fn message_summary(content: &str) -> String {
    let mut lines = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or("No details");
    let summary = if matches!(first, "Thought" | "Thinking…" | "Running command") {
        lines.next().unwrap_or(first)
    } else {
        first
    };
    truncate_summary(summary, 120)
}

fn truncate_summary(summary: &str, max_chars: usize) -> String {
    let mut chars = summary.chars();
    let prefix = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
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
        assert!(rendered.contains("button('Fit','Fit diagram','fit')"));
        assert!(rendered.contains("requestFullscreen"));
        assert!(rendered.contains("Download SVG"));
    }

    #[test]
    fn activity_and_changes_are_collapsed_for_readability() {
        let messages = vec![
            ChatMessage::for_preview_test(ChatRole::Activity, "Running: cargo test\noutput"),
            ChatMessage::for_preview_test(
                ChatRole::Diff,
                "Edited: src/ui.rs, src/chat.rs [completed]\ndiff --git a/a b/a",
            ),
        ];
        let rendered = render_document("Thread", &messages, None);

        assert!(rendered.contains("<details id=\"message-1\""));
        assert!(rendered.contains("class=\"message auxiliary activity\""));
        assert!(rendered.contains("class=\"message auxiliary diff\""));
        assert!(rendered.contains("class=\"summary-text\">Running: cargo test"));
        assert!(
            rendered.contains("class=\"summary-text\">Edited: src/ui.rs, src/chat.rs [completed]")
        );
        assert!(rendered.contains("2 messages"));
    }

    #[test]
    fn activity_summary_prefers_reasoning_content_and_escapes_html() {
        assert_eq!(
            message_summary("Thought\nChecking the <unsafe> edge case\nMore"),
            "Checking the <unsafe> edge case"
        );
        let rendered = render_document(
            "Thread",
            &[ChatMessage::for_preview_test(
                ChatRole::Activity,
                "Thought\nChecking the <unsafe> edge case",
            )],
            None,
        );
        assert!(rendered.contains("Checking the &lt;unsafe&gt; edge case"));
    }

    #[test]
    fn activity_summary_is_bounded() {
        let long = "x".repeat(140);
        let summary = message_summary(&long);

        assert_eq!(summary.chars().count(), 121);
        assert!(summary.ends_with('…'));
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
