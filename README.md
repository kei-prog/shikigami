# wyard

`wyard` is a fast, lightweight TUI for switching between Codex CLI threads
across repositories. It keeps a small list of repositories chosen by the user,
uses Codex App Server, and only lists threads created through wyard.

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
wyard
```

On first launch, wyard opens the repository picker. The initial background scan
checks common development directories and `ghq root` when available. Use `s`
for an explicit home-directory scan or `b` to browse to a repository. Scan
results are cached, while the main screen only shows repositories you register.

- `j` / `k`: move through the repository tree
- `h` / `l`: collapse or expand the selected repository
- `Enter`: expand a repository or open its selected thread in the chat pane
- `Tab`: focus the open chat
- `Esc`: select the parent repository or return from chat to the tree
- `a`: add repositories
- `n`: create a thread in the primary repository, a new worktree, or an existing worktree
- `x`: archive the selected thread, or restore it from the archived view
- `A`: switch between active and archived threads
- `d`: unregister a repository or remove a thread from wyard only
- `r`: reload registered repositories and threads
- `?`: show all keys
- `q`: quit

The left pane is one expandable tree: each registered repository keeps its
wyard-created threads directly underneath it, and multiple repositories can be
expanded at once. The right chat pane stays visible while navigating the tree.
Creating a thread with `n` always uses the selected repository, including when
the selection is one of that repository's child threads.

In the repository picker, `/` filters candidates, `Space` selects multiple
repositories, and `Enter` registers them. Repository discovery is read-only.
Existing worktrees from `git worktree list` are offered as locations when a new
thread is created.

Choosing `New worktree` creates an automatically named `wyard/<id>` branch and
worktree under wyard's local data directory, then immediately starts Codex. No
branch or directory name input is required. `Existing worktree` only lists
non-primary entries returned by `git worktree list`.

## Chat runtime

wyard starts one local `codex app-server` child and communicates over JSONL on
stdio. New threads are registered immediately from `thread/start`; existing
threads use `thread/resume` and `thread/read`. Messages, streamed reasoning
summaries, plans, command output tails, tool activity, and approvals stay inside
the wyard TUI instead of opening the Codex terminal UI.
User messages are shown as full-width colored input bands, while Codex responses
and activity use the normal chat background.

Chat starts in input mode. Press `Tab` for scroll mode, then use `j` / `k` for
one line, `Ctrl-u` / `Ctrl-d` for half a page, `PageUp` / `PageDown` for a full
page, and `g` / `G` for the beginning or latest message. `i`, `Enter`, `Tab`, or
`Esc` returns to input mode. In input mode, `Ctrl-u` clears the composer and
`Esc` returns focus to the repository tree. A blinking cursor follows the
composer input and is hidden outside input mode. New output follows the bottom
only while the view is already at the latest message.

Press `/` in an empty composer to open the command palette. Built-in wyard
commands and enabled Codex skills for the current workspace appear in one
fuzzy-searchable list, ranked by name and description matches. Selecting a
skill inserts its `$skill-name` mention and sends the corresponding App Server
skill input with the next message.

Existing Codex threads are not imported.

Archiving hides a thread without deleting its Codex history. If the thread uses
a clean worktree created by wyard, wyard offers to remove that worktree while
preserving its branch. Dirty worktrees, primary repositories, and worktrees not
created by wyard are always kept. Restoring an archived thread recreates a
removed wyard worktree from its preserved branch.

Repositories can also be listed non-interactively:

```bash
wyard repo list
```

## Scope

wyard is a lightweight App Server client. Codex still owns agent execution,
tools, sandboxing, conversation history, and authentication; wyard owns the TUI,
repository/worktree lifecycle, and its selected thread registry.
