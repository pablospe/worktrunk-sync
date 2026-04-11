# wt-sync

Rebase stacked worktree branches in dependency order.

An external subcommand for [worktrunk](https://github.com/max-sixty/worktrunk) — run it as `wt sync`.

## How it works

`wt sync` detects the branch dependency tree from git's commit graph using pairwise merge-base analysis, then rebases each branch onto its parent in topological order. All rebases use `--onto` with a stored fork-point, which correctly handles parent branches that have been rewritten (rebased, amended, or force-pushed).

Two files are persisted to `.git/wt/`:

- **`stack`** — the dependency tree in [git-machete](https://github.com/VirtusLab/git-machete) compatible format, used for parent tracking and integration detection.
- **`stack-forkpoints`** — each branch's parent SHA at last sync, used as the `--onto` base.

## Install

```bash
cargo install worktrunk-sync
```

The binary is named `wt-sync`, which worktrunk discovers automatically via [external subcommand dispatch](https://github.com/max-sixty/worktrunk/pull/2054).

## Usage

```bash
wt sync              # sync all stacks (default)
wt sync --stack      # sync only the current stack
wt sync --dry-run    # preview the plan without executing
```

### Flags

| Flag | Description |
|------|-------------|
| `--stack` | Only sync the stack containing the current branch |
| `--all` | Sync all stacks (default) |
| `--fetch` | Fetch from remote before syncing |
| `--push` | Push rebased branches after syncing |
| `--prune` | Remove integrated worktrees after syncing |
| `--dry-run` | Preview the sync plan without executing |

## Example

```
$ wt sync --dry-run
Dependency tree:
  main
    feature-a
      feature-b
    feature-c

Sync plan:
  1. Rebase feature-a onto main
  2. Rebase feature-b onto feature-a
  3. Rebase feature-c onto main
```

## Key behaviors

- **Zero configuration** — dependencies are inferred from git history
- **Stack file auto-managed** — created and updated on every sync
- **Conflict handling** — stops on first conflict; resolve and re-run `wt sync`
- **Integrated branch detection** — branches merged into their parent are detected and their children reparented

## Requirements

- [worktrunk](https://github.com/max-sixty/worktrunk) >= 0.35
- Git

## License

MIT
