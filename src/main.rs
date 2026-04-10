use clap::Parser;

mod sync;

/// Rebase stacked worktree branches in dependency order.
///
/// Detects branch dependencies from git history (or an explicit stack file),
/// then rebases each branch onto its parent in topological order.
#[derive(Parser)]
#[command(name = "wt-sync", version)]
struct Cli {
    /// Only sync the current stack
    ///
    /// Without this flag, all worktree branches are synced. With `--stack`, only
    /// the stack containing the current branch is synced.
    #[arg(long)]
    stack: bool,

    /// Sync all stacks
    #[arg(long, overrides_with = "stack")]
    all: bool,

    /// Fetch from remote before syncing
    #[arg(long, overrides_with = "no_fetch")]
    fetch: bool,

    /// Don't fetch from remote
    #[arg(long = "no-fetch", overrides_with = "fetch", hide = true)]
    no_fetch: bool,

    /// Push rebased branches after syncing
    #[arg(long, overrides_with = "no_push")]
    push: bool,

    /// Don't push after syncing
    #[arg(long = "no-push", overrides_with = "push", hide = true)]
    no_push: bool,

    /// Remove integrated worktrees after syncing
    #[arg(long, overrides_with = "no_prune")]
    prune: bool,

    /// Don't remove integrated worktrees
    #[arg(long = "no-prune", overrides_with = "prune", hide = true)]
    no_prune: bool,

    /// Preview the sync plan without executing
    #[arg(long)]
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
    let args = Cli::parse();

    let all = if args.stack {
        false
    } else if args.all {
        true
    } else {
        // Default: sync all
        true
    };

    let opts = sync::SyncOptions {
        fetch: flag_pair(args.fetch, args.no_fetch).unwrap_or(false),
        all,
        push: flag_pair(args.push, args.no_push).unwrap_or(false),
        prune: flag_pair(args.prune, args.no_prune).unwrap_or(false),
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
