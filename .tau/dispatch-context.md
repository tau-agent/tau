# Project: tau

Rust workspace with crates in `crates/` (tau, tau-cli, tau-tui).

## Build & Test

```bash
cargo build 2>&1 | tail -5
cargo clippy --workspace -- -D warnings 2>&1 | tail -20
cargo fmt --check 2>&1
cargo test --workspace 2>&1 | tail -20
```

## Git Workflow

- You are on a task branch. Commit your changes here.
- Do NOT merge into main — that is handled by `task_merge` after review.
- Write clear commit messages describing what changed and why.

## Coding Conventions

- Run `cargo fmt` before committing.
- All clippy warnings must be clean (`-D warnings`).
- Add tests for new functionality.
- Use `read` tool to examine files (not `cat`). Use `edit` tool for precise changes.
