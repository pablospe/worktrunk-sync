# wt-sync

Rebase stacked worktree branches in dependency order.

Works with any git worktrees — no special setup required. Can also be used as an external subcommand for [worktrunk](https://github.com/max-sixty/worktrunk) via `wt sync`.

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

The binary is named `wt-sync` and can be run directly. If [worktrunk](https://github.com/max-sixty/worktrunk) is installed, it also works as `wt sync` via [external subcommand dispatch](https://github.com/max-sixty/worktrunk/pull/2054).

## Usage

```bash
wt-sync              # sync the current stack (default)
wt-sync --all        # sync all stacks
wt-sync -nv          # preview the plan with git commands

# Or via worktrunk:
wt sync
wt sync --all
```

### Full workflow

```bash
wt sync --fetch --push --prune
```

This fetches from the remote, rebases branches in the current stack, pushes the rebased branches, and removes any worktrees whose branches have been merged.

### Flags

| Flag | Short | Description |
|------|-------|-------------|
| `--stack` | `-s` | Sync only the current stack (default) |
| `--all` | `-a` | Sync all stacks |
| `--fetch` | `-f` | Fetch from remote before syncing |
| `--push` | `-p` | Push rebased branches after syncing |
| `--prune` | `-P` | Remove integrated worktrees after syncing |
| `--force` | `-F` | Force removal of dirty integrated worktrees (with `--prune`) |
| `--verbose` | `-v` | Show git commands that would be executed |
| `--dry-run` | `-n` | Preview the sync plan without executing |

## Example

```
$ wt-sync -nv
Dependency tree:
main
├── feature-a
│   └── feature-b
└── feature-c

Planned operations:
  rebase feature-a onto main
    $ git rebase --onto main abc1234 feature-a
  rebase feature-b onto feature-a
    $ git rebase --onto feature-a def5678 feature-b
  rebase feature-c onto main
    $ git rebase --onto main abc1234 feature-c
```

## Key behaviors

- **Zero configuration** — dependencies are inferred from git history
- **Fork-point tracking** — stores the parent SHA at each sync so `--onto` rebases work correctly even after the parent is rewritten
- **Integrated branch detection** — branches merged into their parent (or into the default branch) are detected; their children are reparented automatically
- **Conflict handling** — stops on first conflict with instructions to resolve; fork-points for already-rebased branches are saved so re-running `wt sync` picks up where it left off
- **Non-zero exit on conflict** — so `wt sync && wt sync --push` won't silently push past a conflict

## Requirements

- Git with worktrees (`git worktree add`)
- [worktrunk](https://github.com/max-sixty/worktrunk) >= 0.35 (runtime dependency for repository utilities)

## Development

### Pre-commit hooks

Install [pre-commit](https://pre-commit.com/) and enable hooks:

```bash
pip install pre-commit
pre-commit install
```

Hooks run automatically on each commit (rustfmt, clippy, typos, trailing whitespace, etc.). To run manually:

```bash
pre-commit run --all-files
```

## License

MIT
