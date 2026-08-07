# Shikigami

`shi` is a fast, lightweight TUI for switching between Codex CLI threads
across repositories. It keeps a small list of repositories chosen by the user,
uses Codex App Server, and only lists threads created through Shikigami.

## Requirements

- Rust 1.85 or later
- Git
- Codex CLI (`codex`)

ghq is optional. When available, its repositories are included in the fast
discovery pass.

## Install

```bash
cargo install --path .
```

## Use

```bash
shi
```

On first launch, Shikigami opens the repository picker. The initial background scan
checks common development directories and `ghq root` when available. Use `s`
for an explicit home-directory scan or `b` to browse to a repository. Scan
results are cached, while the main screen only shows repositories you register.

- `j` / `k`: move through the repository tree and preview the selected thread
- `h` / `l`: collapse or expand the selected repository
- `Enter`: expand a repository or focus the selected thread's chat input
- `Tab`: focus the open chat
- `Esc`: select the parent repository or return from chat to the tree
- `a`: add repositories
- `n`: create a thread in the primary repository, a new worktree, or an existing worktree
- `x`: archive the selected thread, or restore it from the archived view
- `A`: switch between active and archived threads
- `!`: show threads that completed, failed, or need approval
- `d`: unregister a repository or remove a thread from Shikigami only
- `r`: reload registered repositories and threads
- `?`: show all keys
- `q`: quit

The left pane is one expandable tree: each registered repository keeps its
Shikigami-created threads directly underneath it, and multiple repositories can be
expanded at once. Expansion state is restored on the next launch. The right
chat pane stays visible while navigating the tree.
Creating a thread with `n` always uses the selected repository, including when
the selection is one of that repository's child threads.

Moving onto a thread switches the right pane automatically. Uncached history is
loaded after a short debounce; previously viewed threads switch immediately.
Each thread keeps an independent chat state, so multiple turns can continue in
the background while another thread is displayed. The tree marks the visible,
working, and approval-waiting threads separately.

During a Shikigami session, a background turn that completes or fails stays in
the attention list until its chat is viewed or the item is dismissed. The header
and repository rows show pending counts; `!` opens the list, `j` / `k` selects an
item, `Enter` opens it, and `d` dismisses a completed or failed item. Pending
approvals must be accepted or declined. The list is updated directly from App
Server events without polling.

In the repository picker, `/` filters candidates, `Space` selects multiple
repositories, and `Enter` registers them. Repository discovery is read-only.
Existing worktrees from `git worktree list` are offered as locations when a new
thread is created.

Choosing `New worktree` creates an automatically named `shi/<id>` branch and
worktree under Shikigami's local data directory, then immediately starts Codex. No
branch or directory name input is required. `Existing worktree` only lists
non-primary entries returned by `git worktree list`.

## Chat runtime

Shikigami starts one local `codex app-server` child and communicates over JSONL on
stdio. New threads are registered immediately from `thread/start`; existing
threads use `thread/resume` and `thread/read`. Messages, streamed reasoning
summaries, plans, command output tails, tool activity, and approvals stay inside
the Shikigami TUI instead of opening the Codex terminal UI.
Every thread and turn runs with `danger-full-access` and approval prompts
disabled. The red `DANGEROUS` label in the header keeps that execution policy
visible.
User messages are shown as full-width colored input bands, while Codex responses
use the normal chat background. Commands, reasoning, file changes, and other
activity use compact full-width gray bands separated by one line. Their heading
is yellow while running, green when completed, and red when failed.
File-change activity automatically includes the unified diff supplied by App
Server. Added lines are green, removed lines are red, and hunk headers are cyan;
Shikigami does not run Git or an external diff formatter for this display.
After sending a message, an animated `Thinking…` indicator appears until the
first response, reasoning summary, or tool activity arrives. It is display-only
and is not added to the saved conversation history. The latest in-progress
reasoning, command, edit, or tool activity keeps animating for the full active
turn, including periods without incoming App Server events.

Chat starts in input mode. Press `Tab` for scroll mode, then use `j` / `k` for
one line, `u` / `d` for half a page, `PageUp` / `PageDown` for a full
page, and `g` / `G` for the beginning or latest message. `i`, `Enter`, `Tab`, or
`Esc` returns to input mode. In input mode, `Ctrl-u` clears the composer and
`Esc` returns focus to the repository tree. A blinking cursor follows the
composer input and is hidden outside input mode. New output follows the bottom
only while the view is already at the latest message. Entering scroll mode
always starts from the latest message at the bottom.

In scroll mode, `J` / `K` selects the next or previous raw chat message and
keeps it visible. Press `y` to copy that message without its rendered wrapping
or borders, or `Y` to copy the full current chat with role labels. Main and side
chats keep independent selections. Clipboard commands are launched only when a
copy is requested (`pbcopy` on macOS, with native command fallbacks elsewhere).
When a diff hunk is visible, `e` temporarily suspends Shikigami and opens Neovim at
the hunk's new-file line. Exiting Neovim restores the same Shikigami chat and scroll
position with a full redraw. Paths are resolved inside the thread workspace;
deleted files and paths outside that workspace are rejected.

Press `/` in an empty composer to open the command palette. Built-in Shikigami
commands and enabled Codex skills for the current workspace appear in one
fuzzy-searchable list, ranked by name and description matches. Selecting a
skill inserts its `$skill-name` mention and sends the corresponding App Server
skill input with the next message. Choose `/attention` to open the attention
list or `/model` to select a model and its
reasoning effort from the live App Server model catalog. The current selection
appears in the header and applies to subsequent turns in that thread. Press
`Ctrl-r` in chat input to open the current model's reasoning-effort slider;
`j` / `k` changes the effort, `Enter` applies it, and `Esc` cancels.
New chats default to `medium` reasoning when the selected model supports it.

Choose `/sidechat` to create an ephemeral fork of the current thread. Each main
thread can keep multiple side chats for the current Shikigami session. The chat area
splits into main and the selected side pane, and both can stream independently.
Press `Ctrl-g` to switch focus. While the side pane is focused, `Ctrl-n` and
`Ctrl-p` cycle through its forks. `/sides` opens the full list; moving with
`j` / `k` previews immediately, `Enter` confirms, and `Esc` restores the
previous selection. Use `/sideclose` to close the selected fork without
confirmation. Use `/sidepromote` while the side pane is focused to move that
fork into the repository tree as a persistent thread after its current turn has
finished, preserving its conversation, workspace, and model. Moving to another
main thread hides its remaining side chats; returning restores them. Unpromoted
side chats are not added to the repository tree and disappear when Shikigami
exits. Quitting asks for confirmation whenever at least one unpromoted side chat
remains open.

Existing Codex threads are not imported.

Archiving hides a thread without deleting its Codex history. If the thread uses
a clean worktree created by Shikigami, Shikigami offers to remove that worktree while
preserving its branch. Dirty worktrees, primary repositories, and worktrees not
created by Shikigami are always kept. Restoring an archived thread recreates a
removed Shikigami worktree from its preserved branch.

Shikigami copies repository, thread, and UI state from the legacy `wyard` data
directory on first launch. Existing `wyard/<id>` managed worktrees remain supported
in place; newly created managed worktrees use the `shi/<id>` branch prefix.

Repositories can also be listed non-interactively:

```bash
shi repo list
```

## Scope

Shikigami is a lightweight App Server client. Codex still owns agent execution,
tools, sandboxing, conversation history, and authentication; Shikigami owns the TUI,
repository/worktree lifecycle, and its selected thread registry.
