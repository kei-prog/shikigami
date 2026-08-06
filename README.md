# wyard

`wyard` is a fast, lightweight TUI for managing development workspaces. It uses
Git worktrees internally and opens the native Codex CLI in the selected workspace.

## Requirements

- Rust 1.85 or later
- Git
- ghq
- Codex CLI (`codex`)

## Install

```bash
cargo install --path .
```

## Use

Start the TUI:

```bash
wyard
```

wyard automatically discovers Git repositories under `ghq root`. Select a
repository, move to its Workspace pane, then:

- `j` / `k`: move within the focused pane
- `h` / `l`: move between Repository and Workspace panes
- `Enter`: move forward or open Codex CLI for the selected workspace
- `Esc`: move back or cancel
- `n`: create a workspace and branch using `git worktree add`
- `d`: remove a clean workspace directory; its branch remains
- `r`: rescan repositories under `ghq root`
- `?`: show all keys
- `q`: quit

Discovered repositories can also be listed non-interactively:

```bash
wyard repo list
```

New workspace directories are stored in wyard's platform data directory.
Workspace state is always read from `git worktree list`; wyard does not maintain
a second workspace database or repository registry. Removing a workspace uses
`git worktree remove`, which refuses to remove a dirty workspace and preserves
its branch.

## Scope

The first version intentionally does not embed a PTY or use Codex App Server.
When a workspace is opened, wyard temporarily leaves the TUI and starts the
native `codex` command with that workspace as its current directory. Exiting
Codex returns to wyard.
