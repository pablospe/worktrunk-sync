//! Branches that would conflict are skipped before their worktree is touched.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write_commit(dir: &Path, file: &str, content: &str) {
    std::fs::write(dir.join(file), content).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-m", &format!("{file}: {content}")]);
}

fn wt_sync(dir: &Path, args: &[&str]) -> (Output, String) {
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
    (output, text)
}

fn add_worktree(repo: &Path, name: &str, start: &str) -> PathBuf {
    let path = repo.parent().unwrap().join(format!("wt-{name}"));
    git(
        repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            name,
            &path.to_string_lossy(),
            start,
        ],
    );
    path
}

/// main <- stale (edits shared.txt) <- on-stale; main <- fine.
/// main then rewrites shared.txt, so only stale's stack conflicts.
fn setup(tmp: &Path) -> (PathBuf, PathBuf) {
    let repo = tmp.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    write_commit(&repo, "shared.txt", "original");
    let stale = add_worktree(&repo, "stale", "main");
    write_commit(&stale, "shared.txt", "stale's version");
    let on_stale = add_worktree(&repo, "on-stale", "stale");
    write_commit(&on_stale, "more.txt", "on top");
    let fine = add_worktree(&repo, "fine", "main");
    write_commit(&fine, "fine.txt", "fine");
    write_commit(&repo, "shared.txt", "main's version");
    (repo, stale)
}

#[test]
fn conflicting_branch_is_skipped_and_the_rest_is_synced() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, stale) = setup(tmp.path());
    let stale_before = git(&repo, &["rev-parse", "stale"]);

    let (output, plan) = wt_sync(&repo, &["--all", "-n"]);
    assert!(output.status.success(), "{plan}");
    assert!(
        plan.contains("Skipping stale: rebasing it onto main would conflict in shared.txt"),
        "{plan}"
    );
    assert!(
        plan.contains("Skipping on-stale: its parent stale would be skipped"),
        "{plan}"
    );
    assert!(plan.contains("rebase fine onto main"), "{plan}");

    let (output, text) = wt_sync(&repo, &["--all"]);
    assert!(
        !output.status.success(),
        "skipped branches must fail the run:\n{text}"
    );
    assert!(text.contains("Rebased fine onto main"), "{text}");
    assert!(
        text.contains("not rebased because of conflicts: on-stale, stale"),
        "{text}"
    );
    // stale's worktree was never touched: same commit, no rebase in progress.
    assert_eq!(git(&repo, &["rev-parse", "stale"]), stale_before);
    assert_eq!(git(&stale, &["status", "--porcelain"]), "");
    let git_dir = PathBuf::from(git(&stale, &["rev-parse", "--absolute-git-dir"]));
    assert!(
        !git_dir.join("rebase-merge").exists(),
        "stale is mid-rebase"
    );
}

#[test]
fn running_from_main_says_it_syncs_every_stack() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, _) = setup(tmp.path());
    let (_, plan) = wt_sync(&repo, &["-n"]);
    assert!(
        plan.contains("On main, so syncing all 2 stacks: fine, stale."),
        "{plan}"
    );
    let (_, plan) = wt_sync(&repo, &["-n", "--all"]);
    assert!(!plan.contains("syncing all"), "--all asked for it:\n{plan}");
}
