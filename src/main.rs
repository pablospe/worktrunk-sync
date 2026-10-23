use clap::{CommandFactory, Parser};
use clap_complete::env::CompleteEnv;
use worktrunk_sync::sync;

/// Rebase stacked worktree branches in dependency order.
///
/// Detects branch dependencies from git history (or an explicit stack file),
/// then rebases each branch onto its parent in topological order.
#[derive(Parser)]
#[command(name = "wt-sync", version)]
struct Cli {
    /// Only sync the current stack (default)
    #[arg(short, long, overrides_with = "all")]
    stack: bool,

    /// Sync all stacks
    ///
    /// By default, only the stack containing the current branch is synced.
    /// With `--all`, every worktree branch is synced.
    #[arg(short, long, overrides_with = "stack")]
    all: bool,

    /// Fetch from remote before syncing
    #[arg(short, long, overrides_with = "no_fetch")]
    fetch: bool,

    /// Don't fetch from remote
    #[arg(long = "no-fetch", overrides_with = "fetch", hide = true)]
    no_fetch: bool,

    /// Fetch, then fast-forward the root branch to its upstream
    ///
    /// Like `git pull --ff-only` for the default branch: implies `--fetch`,
    /// then moves the local root (e.g. `main`) up to `origin/main` before the
    /// stack is rebased onto it. Stacked branches are never touched. A root
    /// that has diverged, has no upstream, or is checked out in a dirty
    /// worktree is left alone with a warning.
    #[arg(
        short = 'u',
        long,
        overrides_with = "no_pull",
        conflicts_with = "no_fetch"
    )]
    pull: bool,

    /// Don't fast-forward the root branch
    #[arg(long = "no-pull", overrides_with = "pull", hide = true)]
    no_pull: bool,

    /// Push rebased branches after syncing
    #[arg(short, long, overrides_with = "no_push")]
    push: bool,

    /// Don't push after syncing
    #[arg(long = "no-push", overrides_with = "push", hide = true)]
    no_push: bool,

    /// Push the stack as-is, without fetching or rebasing
    ///
    /// Pushes every branch in the selected scope (current stack, or all with
    /// `--all`) that has an upstream, in dependency order.
    #[arg(long, conflicts_with_all = ["fetch", "pull", "push", "no_push", "prune", "force"])]
    push_only: bool,

    /// Remove integrated worktrees after syncing
    #[arg(short = 'P', long, overrides_with = "no_prune")]
    prune: bool,

    /// Don't remove integrated worktrees
    #[arg(long = "no-prune", overrides_with = "prune", hide = true)]
    no_prune: bool,

    /// Force removal of dirty integrated worktrees (with --prune)
    #[arg(short = 'F', long, requires = "prune")]
    force: bool,

    /// Show git commands that would be executed
    #[arg(short, long)]
    verbose: bool,

    /// Preview the sync plan without executing
    #[arg(short = 'n', long)]
    dry_run: bool,
}

fn flag_pair(positive: bool, negative: bool) -> Option<bool> {
    match (positive, negative) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

fn main() {
    CompleteEnv::with_factory(Cli::command).complete();

    let args = Cli::parse();

    let all = if args.stack {
        false
    } else if args.all {
        true
    } else {
        // Default: sync current stack only
        // (may be overridden by config in the future)
        false
    };

    let pull = flag_pair(args.pull, args.no_pull).unwrap_or(false);

    let opts = sync::SyncOptions {
        // --pull implies --fetch: fast-forwarding to a stale remote-tracking
        // ref would be pointless.
        fetch: flag_pair(args.fetch, args.no_fetch).unwrap_or(false) || pull,
        pull,
        all,
        push: flag_pair(args.push, args.no_push).unwrap_or(false),
        push_only: args.push_only,
        prune: flag_pair(args.prune, args.no_prune).unwrap_or(false),
        force: args.force,
        verbose: args.verbose,
        dry_run: args.dry_run,
    };

    if let Err(e) = sync::handle_sync(opts) {
        // Print the error chain
        eprintln!("Error: {e}");
        for cause in e.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_only_parses() {
        let args = Cli::try_parse_from(["wt-sync", "--push-only", "--all", "-nv"]).unwrap();
        assert!(args.push_only);
        assert!(args.all);
    }

    #[test]
    fn test_pull_parses() {
        let args = Cli::try_parse_from(["wt-sync", "--pull"]).unwrap();
        assert!(args.pull);
        let args = Cli::try_parse_from(["wt-sync", "-u"]).unwrap();
        assert!(args.pull);
        // Hidden --no-pull wins when given later, like the other flag pairs
        let args = Cli::try_parse_from(["wt-sync", "--pull", "--no-pull"]).unwrap();
        assert_eq!(flag_pair(args.pull, args.no_pull), Some(false));
        let args = Cli::try_parse_from(["wt-sync", "-upP"]).unwrap();
        assert!(args.pull && args.push && args.prune);
    }

    #[test]
    fn test_pull_conflicts() {
        assert!(
            Cli::try_parse_from(["wt-sync", "--pull", "--no-fetch"]).is_err(),
            "--pull should conflict with --no-fetch"
        );
        assert!(
            Cli::try_parse_from(["wt-sync", "--push-only", "--pull"]).is_err(),
            "--push-only should conflict with --pull"
        );
        // --pull and --fetch are compatible: pull implies fetch
        assert!(Cli::try_parse_from(["wt-sync", "--pull", "--fetch"]).is_ok());
    }

    #[test]
    fn test_push_only_conflicts() {
        for flag in [
            "--fetch",
            "--pull",
            "--push",
            "--prune",
            "--no-push",
            "--force",
        ] {
            assert!(
                Cli::try_parse_from(["wt-sync", "--push-only", flag]).is_err(),
                "--push-only should conflict with {flag}"
            );
        }
    }

    #[test]
    fn test_cli_debug_assert() {
        Cli::command().debug_assert();
    }
}
