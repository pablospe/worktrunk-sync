//! Integration tests for rebase base selection: the stored fork-point must
//! only win over the merge-base when the parent was rebased, and the dry-run
//! plan must agree with what the real run does.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Run a git command in the given directory, panicking on failure.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
    if !output.status.success() {
        panic!(
            "git {args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(dir: &Path, file: &str, content: &str, msg: &str) -> String {
    std::fs::write(dir.join(file), content).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

/// Run wt-sync with `args`, returning (success, stderr).
fn wt_sync(repo: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_wt-sync"))
        .args(args)
        .current_dir(repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        // The rebase wt-sync runs needs a committer identity; CI has none.
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .expect("failed to run wt-sync");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

fn wt_sync_ok(repo: &Path, args: &[&str]) -> String {
    let (ok, stderr) = wt_sync(repo, args);
    assert!(ok, "wt-sync {args:?} failed:\n{stderr}");
    stderr
}

/// main (initial) ← feature (one commit, in its own worktree).
/// Returns (repo path, feature worktree path).
fn setup_stack(tmp: &Path) -> (PathBuf, PathBuf) {
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    commit(&repo, "file.txt", "initial\n", "initial commit");

    let wt = tmp.join("worktrees").join("feature");
    git(
        &repo,
        &["worktree", "add", "-b", "feature", &wt.to_string_lossy()],
    );
    commit(&wt, "feature.txt", "feature work\n", "feature work");
    (repo, wt)
}

fn rev(repo: &Path, r: &str) -> String {
    git(repo, &["rev-parse", r])
}

fn commits_ahead(repo: &Path, base: &str, branch: &str) -> usize {
    git(repo, &["rev-list", "--count", &format!("{base}..{branch}")])
        .parse()
        .unwrap()
}

fn stored_fork_point(repo: &Path, branch: &str) -> Option<String> {
    let content = std::fs::read_to_string(repo.join(".git/wt/stack-forkpoints")).ok()?;
    content
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{branch}=")).map(str::to_string))
}

#[test]
fn stored_fork_point_wins_when_parent_was_rebased() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _wt) = setup_stack(tmp.path());

    // main gets a commit A; feature is rebased onto it and the fork-point recorded.
    let a = commit(&repo, "main.txt", "a\n", "A");
    wt_sync_ok(&repo, &["--all"]);
    assert_eq!(
        stored_fork_point(&repo, "feature").as_deref(),
        Some(a.as_str())
    );

    // Rewrite A as A' (same change, new SHA): merge-base slides back to initial.
    git(&repo, &["reset", "--hard", "HEAD~1"]);
    let a2 = commit(&repo, "main.txt", "a\n", "A rewritten");
    assert_ne!(a, a2);

    let stderr = wt_sync_ok(&repo, &["--all", "-v"]);
    assert!(
        stderr.contains("using stored fork-point: main was rebased"),
        "expected stored fork-point reason, got:\n{stderr}"
    );
    // Only the feature commit was replayed, not the old A.
    assert_eq!(commits_ahead(&repo, "main", "feature"), 1);
    assert_eq!(
        stored_fork_point(&repo, "feature").as_deref(),
        Some(a2.as_str())
    );
}

#[test]
fn merge_base_wins_when_branch_was_rebased_by_hand_onto_newer_parent() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, wt) = setup_stack(tmp.path());

    // First sync records fork-point = initial.
    wt_sync_ok(&repo, &["--all"]);
    let initial = rev(&repo, "main");
    assert_eq!(
        stored_fork_point(&repo, "feature").as_deref(),
        Some(initial.as_str())
    );

    // main gets a squash commit S; feature is rebased onto it by hand.
    let squash = commit(&repo, "file.txt", "squash\n", "squash of some PR");
    git(&wt, &["rebase", "main"]);
    assert_eq!(rev(&repo, "feature~1"), squash);

    // main advances to D, touching the same file S touched. Replaying S on
    // top of D (what the stale fork-point would do) conflicts.
    commit(&repo, "file.txt", "after squash\n", "D");

    let stderr = wt_sync_ok(&repo, &["--all", "-v"]);
    assert!(
        stderr.contains("using merge-base: stored fork-point is stale"),
        "expected stale fork-point reason, got:\n{stderr}"
    );
    assert_eq!(commits_ahead(&repo, "main", "feature"), 1);
    assert_eq!(
        git(&repo, &["log", "-1", "--format=%s", "feature"]),
        "feature work"
    );
    // No rebase left in progress in the feature worktree.
    assert!(!repo.join(".git/worktrees/feature/rebase-merge").exists());
    assert!(!repo.join(".git/worktrees/feature/rebase-apply").exists());
}

#[test]
fn stored_fork_point_outside_branch_history_falls_back_to_merge_base() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _wt) = setup_stack(tmp.path());
    wt_sync_ok(&repo, &["--all"]);

    // A commit that is not in feature's history at all.
    git(&repo, &["checkout", "-b", "unrelated"]);
    let unrelated = commit(&repo, "unrelated.txt", "x\n", "unrelated");
    git(&repo, &["checkout", "main"]);
    git(&repo, &["branch", "-D", "unrelated"]);
    std::fs::write(
        repo.join(".git/wt/stack-forkpoints"),
        format!("feature={unrelated}\n"),
    )
    .unwrap();

    commit(&repo, "main.txt", "b\n", "B");
    let stderr = wt_sync_ok(&repo, &["--all", "-v"]);
    assert!(
        stderr.contains("not in its history"),
        "expected warning about stored fork-point, got:\n{stderr}"
    );
    assert!(
        stderr.contains("using merge-base: stored fork-point is not in the branch's history"),
        "expected merge-base reason, got:\n{stderr}"
    );
    assert_eq!(commits_ahead(&repo, "main", "feature"), 1);
}

#[test]
fn dry_run_matches_real_run() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, wt) = setup_stack(tmp.path());
    wt_sync_ok(&repo, &["--all"]);

    // Up to date: the plan must say so instead of printing a rebase.
    let plan = wt_sync_ok(&repo, &["--all", "-n"]);
    assert!(
        plan.contains("feature is up to date with main"),
        "expected up-to-date line, got:\n{plan}"
    );
    assert!(!plan.contains("rebase feature onto main"), "{plan}");

    // Hand-rebase onto a newer main, then advance main again: the plan must
    // pick the merge-base (S), not the stale stored fork-point (initial).
    let initial = rev(&repo, "main");
    let squash = commit(&repo, "file.txt", "squash\n", "S");
    git(&wt, &["rebase", "main"]);
    commit(&repo, "other.txt", "d\n", "D");

    let plan = wt_sync_ok(&repo, &["--all", "-nv"]);
    assert!(plan.contains("rebase feature onto main"), "{plan}");
    assert!(
        plan.contains(&format!("$ git rebase --onto main {squash} feature")),
        "expected merge-base as --onto base, got:\n{plan}"
    );
    assert!(
        !plan.contains(&initial),
        "stale fork-point in plan:\n{plan}"
    );
    assert!(
        plan.contains("# using merge-base: stored fork-point is stale"),
        "{plan}"
    );
    assert!(
        plan.contains("Note: branches whose parent is rebased"),
        "{plan}"
    );
    // Dry-run must not touch anything.
    assert_eq!(rev(&repo, "feature~1"), squash);
    assert_eq!(
        stored_fork_point(&repo, "feature").as_deref(),
        Some(initial.as_str())
    );
}
