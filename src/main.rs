use clap::{CommandFactory, Parser};
use clap_complete::env::CompleteEnv;

mod sync;

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

    /// Push rebased branches after syncing
    #[arg(short, long, overrides_with = "no_push")]
    push: bool,

    /// Don't push after syncing
    #[arg(long = "no-push", overrides_with = "push", hide = true)]
    no_push: bool,

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

    let opts = sync::SyncOptions {
        fetch: flag_pair(args.fetch, args.no_fetch).unwrap_or(false),
        all,
        push: flag_pair(args.push, args.no_push).unwrap_or(false),
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
