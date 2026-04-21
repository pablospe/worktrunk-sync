//! Integration test for `wt-sync --prune`: verifies that integrated worktrees
//! are removed using `remove_worktree_with_cleanup`.

use std::path::Path;
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

fn wt_sync_bin() -> std::path::PathBuf {
    env!("CARGO_BIN_EXE_wt-sync").into()
}

/// Sets up a repo with:
///   main (initial commit + merge commit)
///   └── feature (one commit, merged into main)
///
/// Returns the feature worktree path. Repo lives at `tmp/repo`.
fn setup_repo_with_integrated_branch(tmp: &Path) -> std::path::PathBuf {
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("file.txt"), "initial").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial commit"]);

    // Create a worktree for the feature branch
    let wt_path = tmp.join("worktrees").join("feature");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            &wt_path.to_string_lossy(),
        ],
    );

    // Add a commit on the feature branch
    std::fs::write(wt_path.join("feature.txt"), "feature work").unwrap();
    git(&wt_path, &["add", "."]);
    git(&wt_path, &["commit", "-m", "feature work"]);

    // Merge the feature branch into main (making it "integrated")
    git(
        &repo,
        &["merge", "feature", "--no-ff", "-m", "Merge feature"],
    );

    wt_path
}

#[test]
fn test_prune_removes_integrated_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let wt_path = setup_repo_with_integrated_branch(tmp.path());

    // Verify the worktree exists before prune
    assert!(wt_path.exists(), "worktree should exist before prune");

    // Verify the branch exists
    let branches = git(&repo, &["branch"]);
    assert!(
        branches.contains("feature"),
        "feature branch should exist before prune"
    );

    // Run wt-sync --prune --all from the main worktree
    let output = Command::new(wt_sync_bin())
        .args(["--prune", "--all"])
        .current_dir(&repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("failed to run wt-sync");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "wt-sync --prune --all failed:\nstdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&output.stdout),
    );

    // Verify the worktree directory is gone
    assert!(
        !wt_path.exists(),
        "worktree directory should be removed after prune"
    );

    // Verify the branch is gone
    let branches_after = git(&repo, &["branch"]);
    assert!(
        !branches_after.contains("feature"),
        "feature branch should be deleted after prune, got: {branches_after}"
    );

    // Verify worktree is no longer listed
    let worktrees = git(&repo, &["worktree", "list"]);
    assert!(
        !worktrees.contains("feature"),
        "worktree should not appear in git worktree list"
    );
}

#[test]
fn test_prune_only_removes_integrated_worktree_not_wip() {
    // Exercises the prune loop with both an integrated branch (should be removed)
    // and a non-integrated branch (should survive). This ensures the skip behaviour
    // is tested within an active prune pass, not just by short-circuiting on an
    // empty `integrated` list.
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();

    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("file.txt"), "initial").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial commit"]);

    // Create a worktree for a branch that will be integrated
    let done_path = tmp.path().join("worktrees").join("done");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "done",
            &done_path.to_string_lossy(),
        ],
    );
    std::fs::write(done_path.join("done.txt"), "done work").unwrap();
    git(&done_path, &["add", "."]);
    git(&done_path, &["commit", "-m", "done work"]);

    // Merge `done` into main (making it integrated)
    git(&repo, &["merge", "done", "--no-ff", "-m", "Merge done"]);

    // Create a worktree for a branch that is NOT integrated.
    // Created after the merge so wip is based on main's current tip —
    // no rebase needed, which isolates the test to prune behaviour only.
    let wip_path = tmp.path().join("worktrees").join("wip");
    git(
        &repo,
        &["worktree", "add", "-b", "wip", &wip_path.to_string_lossy()],
    );
    std::fs::write(wip_path.join("wip.txt"), "work in progress").unwrap();
    git(&wip_path, &["add", "."]);
    git(&wip_path, &["commit", "-m", "wip commit"]);

    // Run wt-sync --prune --all
    let output = Command::new(wt_sync_bin())
        .args(["--prune", "--all"])
        .current_dir(&repo)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("failed to run wt-sync");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "wt-sync should succeed: {stderr}",);

    // The integrated worktree should be gone
    assert!(!done_path.exists(), "integrated worktree should be removed");
    let branches = git(&repo, &["branch"]);
    assert!(
        !branches.contains("done"),
        "integrated branch should be deleted"
    );

    // The non-integrated worktree should still exist
    assert!(wip_path.exists(), "non-integrated worktree should remain");
    assert!(
        branches.contains("wip"),
        "non-integrated branch should remain"
    );
}
