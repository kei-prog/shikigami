# wyard

`wyard` is a fast, lightweight TUI for managing development workspaces. It treats
Jujutsu as the source of truth and opens the native Codex CLI in the selected
workspace.

## Requirements

- Rust 1.85 or later
- Jujutsu (`jj`)
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

Register a repository with `a`, enter its path, and press Enter. Select the
repository, move to its Workspace pane, then:

- `n`: create a JJ workspace
- `Enter`: open Codex CLI in the selected workspace
- `d`: remove the selected repository registration or forget a workspace; files remain on disk
- `?`: show all keys
- `q`: quit

Repositories can also be managed non-interactively:

```bash
wyard repo add /path/to/repository
wyard repo list
wyard repo remove repository-name
```

New workspace directories are stored in wyard's platform data directory. JJ
workspace state is always read from `jj workspace list`; wyard does not maintain
a second workspace database.

## Scope

The first version intentionally does not embed a PTY or use Codex App Server.
When a workspace is opened, wyard temporarily leaves the TUI and starts the
native `codex` command with that workspace as its current directory. Exiting
Codex returns to wyard.
