# wyard

`wyard` is a fast, lightweight TUI for switching between Codex CLI threads
across repositories. It keeps a small list of repositories chosen by the user,
uses the native Codex CLI, and only lists threads created through wyard.

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

- `j` / `k`: move within the focused pane
- `h` / `l`: move between Repository and Thread panes
- `Enter`: move forward or resume the selected thread
- `Esc`: move back or cancel
- `a`: add repositories
- `n`: create a thread; choose the primary repository or an existing worktree
- `d`: unregister a repository or remove a thread from wyard only
- `r`: reload registered repositories and threads
- `?`: show all keys
- `q`: quit

In the repository picker, `/` filters candidates, `Space` selects multiple
repositories, and `Enter` registers them. Repository discovery is read-only.
wyard does not create or remove worktrees. Existing worktrees from
`git worktree list` are offered as locations when a new thread is created.

## Thread registration

wyard starts the native `codex` command with a per-process `notify` callback.
After the first completed turn, the callback records the Codex thread ID and its
working directory in wyard's local data directory. Selecting that thread later
runs `codex resume <thread-id>` from the same directory.

Existing Codex threads are not imported. If Codex exits before its first turn
completes, no notification is sent and the thread is not registered. The
per-process callback temporarily overrides any configured Codex `notify`
callback for Codex sessions launched by wyard.

Repositories can also be listed non-interactively:

```bash
wyard repo list
```

## Scope

wyard intentionally does not embed a terminal or use Codex App Server. It leaves
the TUI while the native Codex CLI is running and redraws itself after Codex
exits.
