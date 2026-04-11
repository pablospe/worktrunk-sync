# wt-sync

Rebase stacked worktree branches in dependency order.

An external subcommand for [worktrunk](https://github.com/max-sixty/worktrunk) — run it as `wt sync`.

## How it works

`wt sync` detects the branch dependency tree from git's commit graph using pairwise merge-base analysis, then rebases each branch onto its parent in topological order.

All rebases use `--onto` with a stored fork-point, so parent branches that have been rewritten (rebased, amended, or force-pushed) are handled correctly — no duplicate commits or spurious conflicts.

### Persisted state

Two files are stored in `.git/wt/` (shared across all worktrees):

- **`stack`** — the dependency tree in [git-machete](https://github.com/VirtusLab/git-machete) compatible format, used for parent tracking and integration detection.
- **`stack-forkpoints`** — each branch's parent SHA at last sync, used as the `--onto` base for the next rebase.

Both files are auto-created and updated on every sync.

## Install

```bash
cargo install worktrunk-sync
```

The binary is named `wt-sync`, which worktrunk discovers automatically via [external subcommand dispatch](https://github.com/max-sixty/worktrunk/pull/2054).

## Usage

```bash
wt sync              # sync the current stack (default)
wt sync --all        # sync all stacks
wt sync --dry-run    # preview the plan without executing
```

### Full workflow

```bash
wt sync --fetch --push --prune
```

This fetches from the remote, rebases branches in the current stack, pushes the rebased branches, and removes any worktrees whose branches have been merged.

### Flags

| Flag | Description |
|------|-------------|
| `--stack` | Sync only the current stack (default) |
| `--all` | Sync all stacks |
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
- **Fork-point tracking** — stores the parent SHA at each sync so `--onto` rebases work correctly even after the parent is rewritten
- **Integrated branch detection** — branches merged into their parent (or into the default branch) are detected; their children are reparented automatically
- **Conflict handling** — stops on first conflict with instructions to resolve; fork-points for already-rebased branches are saved so re-running `wt sync` picks up where it left off
- **Non-zero exit on conflict** — so `wt sync && wt sync --push` won't silently push past a conflict

## Requirements

- [worktrunk](https://github.com/max-sixty/worktrunk) >= 0.35
- Git

## License

MIT
