# worktrunk-sync

Rust CLI tool that rebases stacked worktree branches in dependency order.

## Project structure

- `src/main.rs` — CLI entry point (clap)
- `src/sync.rs` — Core sync logic (dependency detection, rebase, push, prune)
- `test-sync-e2e.sh` — End-to-end shell tests

## Build & test

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

## Pre-commit hooks

```bash
pre-commit install
pre-commit run --all-files
```

## Conventions

- Binary name: `wt-sync` (also invoked as `wt sync` via worktrunk)
- Uses `worktrunk` crate for git repository utilities
- Uses `color-print` for colored terminal output
- Error handling via `anyhow`
