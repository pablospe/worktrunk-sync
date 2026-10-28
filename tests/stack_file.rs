//! Integration tests for the auto-saved stack file (`.git/wt/stack`): branches
//! it lists keep their recorded parent, branches it doesn't list are inferred.

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

fn commit_file(dir: &Path, name: &str) {
    std::fs::write(dir.join(name), name).unwrap();
    git(dir, &["add", name]);
    git(dir, &["commit", "-m", name]);
}

/// Run wt-sync in `dir`, asserting success. Returns stdout + stderr.
fn wt_sync(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_wt-sync"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .expect("failed to run wt-sync");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "wt-sync {args:?} failed:\n{text}");
    text
}

/// Add a worktree for a new branch `name` started at `start`, with one commit.
fn add_branch(repo: &Path, tmp: &Path, name: &str, start: &str) -> PathBuf {
    let path = tmp.join(format!("wt-{name}"));
    git(
        repo,
        &[
            "worktree",
            "add",
            "-b",
            name,
            &path.to_string_lossy(),
            start,
        ],
    );
    commit_file(&path, &format!("{name}.txt"));
    path
}

#[test]
fn branch_created_after_first_sync_is_inferred() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    commit_file(&repo, "initial.txt");

    add_branch(&repo, tmp.path(), "a", "main");
    commit_file(&repo, "m2.txt");

    // The first real sync writes the stack file with only `a` in it.
    wt_sync(&repo, &["--all"]);
    let stack = std::fs::read_to_string(repo.join(".git/wt/stack")).unwrap();
    assert_eq!(stack.trim(), "a");

    // `b` is stacked on `a` after that sync, so the file doesn't list it.
    add_branch(&repo, tmp.path(), "b", "a");
    commit_file(&repo, "m3.txt");

    // The dry run shows `b` against `a`'s current tip, so it reads as up to
    // date with `a`; before the fix it was rebased onto main.
    let plan = wt_sync(&repo, &["--all", "-n"]);
    assert!(
        plan.contains("b is up to date with a") && !plan.contains("rebase b onto main"),
        "b should be inferred onto a, not defaulted to main:\n{plan}"
    );

    // The next real sync records it, so later runs keep the parent.
    wt_sync(&repo, &["--all"]);
    let stack = std::fs::read_to_string(repo.join(".git/wt/stack")).unwrap();
    assert_eq!(stack, "a\n\tb\n");
}
