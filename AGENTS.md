# Repository Guidelines

## Project Structure & Module Organization

`shikigami` is a Rust 2024 TUI for managing Codex CLI threads across Git repositories. The entry point is `src/main.rs`; application state lives in `src/app.rs`, rendering in `src/ui.rs`, and App Server JSONL communication in `src/app_server.rs`. Repository discovery, thread data, Git worktrees, chat state, and clipboard support use correspondingly named modules. Tests are colocated in `#[cfg(test)] mod tests` blocks. Never commit generated `target/` contents.

## Product Scope & Feature Decisions

Shikigami enables powerful multi-threaded Codex use that the Codex CLI alone cannot provide. Keep responsibilities that Codex can perform through natural-language instructions, tools, or skills in Codex; do not reimplement them as built-in Shikigami workflows by default. Concentrate Shikigami on persistent client capabilities such as coordinating and navigating multiple threads, repositories, and worktrees, and making their state clear to the user.

Before adding a feature, determine whether Codex or a skill can already provide it sufficiently and whether it creates value unavailable in the Codex CLI itself. If both answers favor existing Codex capabilities, leave the feature to Codex. Prefer changes that reduce the human interaction and coordination cost of operating multiple concurrent Codex threads.

## Build, Test, and Development Commands

Run shell commands through `rtk` in this workspace:

- `rtk cargo run` launches the TUI; Codex CLI and Git must be installed.
- `rtk cargo build` compiles a debug binary.
- `rtk cargo test` runs the full unit-test suite.
- `rtk cargo fmt --check` verifies standard Rust formatting.
- `rtk cargo clippy --all-targets --all-features -- -D warnings` catches lint issues and treats warnings as failures.
- `rtk cargo install --path .` installs the local `shi` executable.

Prefer lightweight implementations. Verify performance-sensitive changes with before/after measurements.

## Coding Style & Naming Conventions

Use `rustfmt` defaults (four-space indentation). Use `snake_case` for functions and variables, `PascalCase` for types and traits, and `SCREAMING_SNAKE_CASE` for constants. Keep modules focused, return recoverable failures with `Result`, and add `anyhow::Context` at I/O and process boundaries. Do not block the async UI path.

## Testing Guidelines

Add focused unit tests beside the changed module. Name tests after behavior, such as `removes_clean_managed_worktree`. Use `tempfile` for filesystem scenarios; do not depend on user configuration or live Codex state. No coverage threshold is configured.

## Commit & Pull Request Guidelines

Recent history uses concise, imperative subjects such as `Redraw the TUI when the terminal is resized`. Keep each commit scoped to one coherent change. Before committing, inspect `rtk git status`, stage only intended files, and review the staged diff. Pull requests should explain user-visible behavior, list validation commands, and note performance or safety implications. Include a terminal screenshot or recording for material TUI changes and link relevant issues when available.

## Safety & Workspace Handling

Check `rtk git status` and `rtk git worktree list` before editing. Preserve unrelated user changes. Never remove a worktree unless explicitly requested and verified clean. Be especially careful around the `danger-full-access` App Server policy documented in `README.md`.
