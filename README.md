# Shikigami

Shikigami is a fast, lightweight TUI for switching between Codex CLI threads
across repositories. It keeps a small list of repositories chosen by the user,
uses Codex App Server, and only lists threads created through Shikigami.

## Requirements

- Rust 1.88 or later
- Git
- Codex CLI (`codex`)

## Install

Install the latest stable release from GitHub:

```bash
cargo install --git https://github.com/kei-prog/shikigami --tag v0.1.0
```

To try the latest development version from `main` instead:

```bash
cargo install --git https://github.com/kei-prog/shikigami
```

Or install from a local checkout:

```bash
cargo install --path .
```

## Use

```bash
shi
```

On first launch, Shikigami asks you to choose a projects folder or a single Git
repository. Projects folders are saved and scanned only when you request it, and
scan results are cached. Use `r` to rescan saved folders, `s` for an explicit
home-directory scan, or `b` to choose another folder or repository. The main
screen only shows repositories you register.

- `j` / `k`: move through the repository tree and preview the selected thread
- `h` / `l`: collapse or expand the selected repository
- `H` / `L`: collapse or expand all repositories
- `Enter`: expand a repository or focus the selected thread's chat input
- `Tab`: focus the open chat
- `Esc`: select the parent repository or return from chat to the tree
- `/`: fuzzy-search active threads from the repository tree
- `R`: open thread-name actions for one thread, one repository, or all repositories
- `y` / `Y`: copy the selected thread ID or its `codex resume` command
- `a`: add repositories
- `n`: create a thread in the primary repository, a new worktree, or an existing worktree
- `x`: archive the selected thread, or restore it from the archived view
- `A`: switch between active and archived threads
- `!`: show threads that completed, failed, or need approval
- `d`: unregister a repository, or permanently delete a thread from the archived view
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

A chat can also create an independent thread when asked in natural language.
Shikigami gives it a single `shikigami.start_thread` tool that starts the first
turn in either the current workspace or a new managed worktree. The new thread
does not inherit the current conversation and appears in the repository tree.

A background turn that completes or fails stays in the attention list until its
chat is viewed or the item is dismissed, including across Shikigami restarts.
Entries for threads no longer registered with Shikigami are removed on startup.
The header and repository rows show pending counts; `!` opens the list, `j` / `k`
selects an item, `Enter` opens it, and `d` dismisses a completed or failed item.
Pending approvals remain session-local because their App Server request cannot
be answered after a restart. An approval is shown only inside its requesting
chat; approvals from background threads add attention markers without
interrupting the visible thread. The popup explains the command, reason, working
directory, or requested access without exposing App Server JSON. Use `j` / `k`
or the arrow keys and `Enter` to choose from the decisions offered by Codex,
including one-time approval and a proposed persistent command rule when
available. The list is updated directly from App Server events without polling.

In the repository picker, `/` filters candidates, `Space` selects multiple
repositories, and `Enter` registers them. Opening the picker shows cached
candidates immediately without starting a scan. Repository discovery is read-only.
Existing worktrees from `git worktree list` are offered as locations when a new
thread is created.

Choosing `New worktree` creates an automatically named `shi/<id>` branch and
worktree under Shikigami's local data directory, then immediately starts Codex. No
branch or directory name input is required. `Existing worktree` only lists
non-primary entries returned by `git worktree list`.

## Chat runtime

Shikigami allows one interactive `shi` process at a time. It holds an OS file
lock while running, so a second process exits with a clear already-running error.
The active process starts a dedicated Codex App Server over stdio and stops that
child process when Shikigami exits.
New threads are registered immediately from `thread/start`; existing threads use
`thread/resume` and `thread/read`. Each connection unsubscribes from its resumed
threads on exit. Pressing `q` asks for confirmation when responses started by that
Shikigami process are still running or temporary side chats remain. Confirming
interrupts those responses and deletes the temporary chats before exit. Messages,
streamed reasoning summaries, plans, command output
tails, tool activity, and approvals stay inside the Shikigami TUI instead of
opening the Codex terminal UI. App Server stderr is captured instead of being
written over the terminal interface.

Thread names are owned by Codex App Server, not Shikigami's local registry.
Renaming uses `thread/name/set`; `thread/name/updated` refreshes the repository
tree, thread picker and search, visible or side chat titles, attention entries,
and archived/restored views from one in-memory value. Shikigami caches the last
observed display titles separately so they appear immediately on the next launch,
then reconciles only its registered thread IDs with bounded concurrent
`thread/read` requests. Live App Server values always replace cached values, and
the cache is never written back to Codex. Opening a thread also reads its current
name, so changes made by another client cannot leave the cache as a competing
source. The rename dialog
trims outer whitespace, rejects empty names, and limits input to 100 displayed
characters to keep the TUI compact. A failed App Server request leaves the old
name and the rename input intact. Archived threads must be restored before they
can be renamed.

Pressing `R` in the repository tree or thread picker always opens the same explicit
action menu: manually rename the selected thread, suggest names in its repository,
or suggest names across all registered repositories. Unavailable actions remain
visible but disabled. Both suggestion actions then open a multi-select list of active
threads; the all-repositories scope includes each repository name in its rows. After
choosing the target threads, Shikigami reads only their recent conversation content
and asks a temporary read-only Codex thread to propose concise names. Each proposal
follows the natural language used by the user in that conversation while preserving
conventional technical names. The review screen keeps every existing name unchanged
until the user includes, edits, and confirms its proposal. Manual editing stays
docked below the proposal list so the selected thread and nearby names remain visible
for comparison. Failed
updates remain in the review screen and can be retried without reapplying successful
renames. An animated progress dialog distinguishes conversation loading, waiting for
Codex, and applying the selected names, with real item counts and elapsed time.

Codex does not persist a new thread until its first turn. When an untitled chat
has no messages, leaving it removes the temporary thread from Shikigami. A clean
Shikigami-managed worktree created for that chat is removed too; a thread whose
managed worktree has changes is kept. If an empty thread remains after an
interrupted session and its temporary App Server thread has expired, opening it
starts a replacement in the same worktree and updates the stored thread id.

If another Codex process already owns a thread, Shikigami keeps the
history visible using `thread/read` and marks the chat `READ ONLY`. Close the
other Codex session and select the thread again through `/threads` to retry
`thread/resume`; a successful retry restores normal input.
Execution defaults to `Auto`: Codex can write inside the thread workspace and
asks for approval before elevated actions. Choose `/permissions` to switch
between `Auto` and `Dangerous`; the selection is saved for future Shikigami
sessions and applies to subsequent turns across all threads. `Dangerous` uses
`danger-full-access` with approval prompts disabled and requires confirmation
when enabled. The green `AUTO · WORKSPACE` or red `DANGEROUS` label in the
header keeps the current policy visible.
User messages are shown as full-width colored input bands, while Codex responses
use the normal chat background. Commands, reasoning, file changes, and other
activity use compact full-width gray bands separated by one line. Their heading
is yellow while running, green when completed, and red when failed.
Codex responses render common Markdown formatting, including headings, emphasis,
lists, quotes, inline code, horizontal rules, and fenced code blocks. Copying a
message or conversation preserves the original Markdown source.
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
`Esc` returns focus to the repository tree. `Shift-Enter` inserts a newline;
the arrow keys move the input cursor, while `Home` / `End` and `Ctrl-a` /
`Ctrl-e` move to the start or end of the current input line. Press `Ctrl-c` in
either mode to stop the current response. While a response is streaming, type
another message and press `Enter` to steer the active turn with the additional
instruction. A yellow message-box status remains visible until App Server
reports the steer as a user message. Pending follow-up contents are listed
between the chat history and message box, then removed one by one as App Server
reports them. After `Ctrl-c`, the message box shows `Stopping response…`; once
App Server confirms the interruption, `Response interrupted` remains in the
chat activity.
A blinking cursor follows the composer input and is hidden outside input mode.
New output follows the bottom only while the view is already at the latest
message. Entering scroll mode always starts from the latest message at the
bottom.

Press `Ctrl-v` (or `Ctrl-Alt-v` in terminals that reserve `Ctrl-v`) to attach
an image from the system clipboard. Pasting a single PNG, JPEG, GIF, or WebP
file path also attaches that file; other pasted content remains composer text.
Attachments appear as numbered image rows in the message box. `Backspace` in an
empty composer or `Ctrl-x` removes the last attachment before sending, while
`Ctrl-u` clears the full draft.
Image-only messages are supported. Shikigami sends attachments as App Server
`localImage` inputs for both `turn/start` and active-turn `turn/steer`, and keeps
their labels aligned in history, pending follow-ups, main chats, and side chats.
The live App Server model catalog controls image availability: a model whose
`inputModalities` excludes `image` cannot accept or send image attachments, and
the draft remains available so it can be removed or sent after switching models.

In scroll mode, `J` / `K` selects the next or previous raw chat message and
keeps it visible. Press `y` to copy that message without its rendered wrapping
or borders, or `Y` to copy the full current chat with role labels. Main and side
chats keep independent selections. Clipboard commands are launched only when a
copy is requested (`pbcopy` on macOS, with native command fallbacks elsewhere).
When a diff hunk is visible, `e` copies a command that opens the hunk's new-file
line in the configured Git editor. Shikigami resolves the editor with
`git var GIT_EDITOR`, recognizes the line-number syntax of common editors, and
falls back to opening the file without a line number for unknown editors. Paths
are resolved inside the thread workspace; deleted files and paths outside that
workspace are rejected.

Press `/` in the repository tree to open the thread search directly. Press `/`
in an empty composer to open the command palette. Built-in Shikigami
commands and enabled Codex skills for the current workspace appear in one
fuzzy-searchable list, ranked by name and description matches. Selecting a
skill inserts its `$skill-name` mention and sends the corresponding App Server
skill input with the next message. Choose `/threads` to fuzzy-search active
threads across registered repositories by title, repository, location, or path;
the picker also shows current, working, and attention state. `Enter` opens the
selected thread and `Esc` returns to the current chat without changing it.
Press `R` in the picker to rename its selected thread, `y` to copy its thread ID,
or `Y` to copy its `codex resume` command. Choose `/attention` to open the
attention list or `/model` to select a model and its
reasoning effort from the live App Server model catalog. The current selection
appears in the header and applies to subsequent turns in that thread. Press
`Ctrl-r` in chat input to open the current model's reasoning-effort slider;
`j` / `k` changes the effort, `Enter` applies it, and `Esc` cancels.
New chats default to `medium` reasoning when the selected model supports it.

Choose `/permissions` to select the global execution mode. Choose `/sidechat` to
create a temporary fork of the current thread. Each main
thread can keep multiple side chats for the current Shikigami session. The chat
area splits into main and the selected side pane, and both can stream independently.
Press `Ctrl-g` to switch focus. While the side pane is focused, `Ctrl-n` and
`Ctrl-p` cycle through its forks. `/sides` opens the full list; moving with
`j` / `k` previews immediately, `Enter` confirms, and `Esc` restores the
previous selection. Use `/sideclose` to close the selected fork without
confirmation. Use `/sidepromote` while the side pane is focused to move that
fork into the repository tree as a persistent thread, preserving its conversation,
workspace, model, and active turn. Moving to another main thread hides its remaining
side chats; returning restores them. `/sideclose` and confirmed app exit delete
unpromoted side chats from Codex. Shikigami records temporary side chats locally
and cleans them up on the next launch after a crash. A thread created immediately
before an unrecoverable registry-write failure is left untouched to avoid deleting
an unrelated thread.

Existing Codex threads are not imported.

Archiving hides a thread while preserving its Codex history, worktree, and branch.
An active response in the main thread or one of its side chats must be stopped
before the thread can be archived.
Restoring an archived thread only returns it to the active view.
Press `d` in the archived view and confirm to delete the Codex history and local
Shikigami record. Shikigami shows the deletion progress while it waits for Codex.
A clean
Shikigami-managed worktree is removed; deletion is refused while it is dirty.
User-owned worktrees and the managed branch are preserved.

Repositories can also be listed non-interactively:

```bash
shi repo list
```

## Scope

Shikigami is a lightweight App Server client. Codex still owns agent execution,
tools, sandboxing, conversation history, and authentication; Shikigami owns the TUI,
repository/worktree lifecycle, and its selected thread registry.
