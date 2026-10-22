//! Integration tests for `wt-sync --pull`: fetch, then fast-forward the root
//! branch to its upstream before the stack is rebased.
//!
//! Every test uses a throwaway repo with a local bare "origin" under a
//! tempdir; nothing here touches a real remote.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn commit_file(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
    git(dir, &["add", name]);
    git(dir, &["commit", "-m", &format!("add {name}")]);
}

/// Run wt-sync in `dir` and return its output plus captured stderr.
fn wt_sync(dir: &Path, args: &[&str]) -> (Output, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_wt-sync"))
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("NO_COLOR", "1")
        // The rebase commits need an identity; CI runners have none configured.
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .expect("failed to run wt-sync");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output, stderr)
}

/// A clone (`repo`) of a local bare `origin` whose `main` has moved on by
/// `remote_commits` commits that the clone hasn't fetched yet.
struct Fixture {
    repo: PathBuf,
    /// Second clone used to push commits to origin behind the first's back.
    publisher: PathBuf,
}

fn setup(tmp: &Path, remote_commits: usize) -> Fixture {
    let origin = tmp.join("origin.git");
    git(
        tmp,
        &["init", "--bare", "-b", "main", &origin.to_string_lossy()],
    );

    let publisher = tmp.join("publisher");
    git(
        tmp,
        &[
            "clone",
            "-q",
            &origin.to_string_lossy(),
            &publisher.to_string_lossy(),
        ],
    );
    git(&publisher, &["checkout", "-b", "main"]);
    commit_file(&publisher, "file.txt", "initial");
    git(&publisher, &["push", "-u", "origin", "main"]);
    git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let repo = tmp.join("repo");
    git(
        tmp,
        &[
            "clone",
            "-q",
            &origin.to_string_lossy(),
            &repo.to_string_lossy(),
        ],
    );

    for i in 0..remote_commits {
        commit_file(&publisher, &format!("remote-{i}.txt"), "upstream work");
    }
    if remote_commits > 0 {
        git(&publisher, &["push", "-q", "origin", "main"]);
    }

    Fixture { repo, publisher }
}

fn sha(dir: &Path, rev: &str) -> String {
    git(dir, &["rev-parse", rev])
}

fn assert_success(output: &Output, stderr: &str) {
    assert!(
        output.status.success(),
        "wt-sync failed:\nstdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&output.stdout),
    );
}

#[test]
fn pull_fast_forwards_checked_out_root() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = setup(tmp.path(), 2);
    let stale = sha(&fx.repo, "main");
    let target = sha(&fx.publisher, "main");
    assert_ne!(stale, target);

    let (output, stderr) = wt_sync(&fx.repo, &["--pull"]);
    assert_success(&output, &stderr);

    assert_eq!(
        sha(&fx.repo, "main"),
        target,
        "main should be fast-forwarded"
    );
    assert_eq!(sha(&fx.repo, "HEAD"), target, "the checkout should follow");
    assert!(
        stderr.contains("Fast-forwarded main") && stderr.contains("(2 commits)"),
        "expected fast-forward message, got:\n{stderr}"
    );
    assert!(
        std::fs::read_to_string(fx.repo.join("remote-1.txt")).is_ok(),
        "working tree should contain the pulled file"
    );
    // Clean worktree afterwards
    assert_eq!(git(&fx.repo, &["status", "--porcelain"]), "");
}

#[test]
fn pull_rebases_stack_onto_updated_root() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = setup(tmp.path(), 1);

    // A stacked branch forked from the stale main.
    let wt_path = tmp.path().join("worktrees").join("feature");
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            &wt_path.to_string_lossy(),
        ],
    );
    commit_file(&wt_path, "feature.txt", "feature work");

    let (output, stderr) = wt_sync(&wt_path, &["--pull"]);
    assert_success(&output, &stderr);

    let target = sha(&fx.publisher, "main");
    assert_eq!(sha(&fx.repo, "main"), target);
    assert!(stderr.contains("Fast-forwarded main"), "{stderr}");
    assert!(stderr.contains("Rebased feature onto main"), "{stderr}");
    // feature now sits on the new main tip
    assert_eq!(sha(&fx.repo, "feature~1"), target);
}

#[test]
fn pull_fails_before_fetching_when_root_not_checked_out() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = setup(tmp.path(), 1);
    let stale = sha(&fx.repo, "main");
    let stale_remote = sha(&fx.repo, "origin/main");

    // Detach the main checkout so `main` isn't checked out anywhere.
    git(&fx.repo, &["checkout", "-q", "--detach"]);

    let (output, stderr) = wt_sync(&fx.repo, &["--pull"]);

    assert!(!output.status.success(), "expected failure, got:\n{stderr}");
    assert!(stderr.contains("has no worktree"), "{stderr}");
    assert!(
        !stderr.contains("Fetching"),
        "must fail before fetching:\n{stderr}"
    );
    assert_eq!(sha(&fx.repo, "main"), stale, "main must not move");
    assert_eq!(sha(&fx.repo, "origin/main"), stale_remote, "no fetch");
}

#[test]
fn pull_warns_and_continues_when_root_diverged() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = setup(tmp.path(), 1);
    commit_file(&fx.repo, "local.txt", "local-only work");
    let local = sha(&fx.repo, "main");

    let (output, stderr) = wt_sync(&fx.repo, &["--pull"]);
    assert_success(&output, &stderr);

    assert_eq!(sha(&fx.repo, "main"), local, "diverged main must not move");
    assert!(
        stderr.contains("main has diverged from origin/main (1 ahead, 1 behind)"),
        "{stderr}"
    );
    assert!(!stderr.contains("Fast-forwarded"), "{stderr}");
}

#[test]
fn pull_warns_when_root_worktree_is_dirty() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = setup(tmp.path(), 1);
    let stale = sha(&fx.repo, "main");
    std::fs::write(fx.repo.join("file.txt"), "edited but not committed").unwrap();

    let (output, stderr) = wt_sync(&fx.repo, &["--pull"]);
    assert_success(&output, &stderr);

    assert_eq!(sha(&fx.repo, "main"), stale, "dirty root must not move");
    assert!(
        stderr.contains("Could not fast-forward main") && stderr.contains("uncommitted changes"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(fx.repo.join("file.txt")).unwrap(),
        "edited but not committed"
    );
}

#[test]
fn pull_warns_when_root_has_no_upstream() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = setup(tmp.path(), 0);
    git(&fx.repo, &["branch", "--unset-upstream", "main"]);

    let (output, stderr) = wt_sync(&fx.repo, &["--pull"]);
    assert_success(&output, &stderr);
    assert!(stderr.contains("main has no upstream"), "{stderr}");
}

#[test]
fn behind_root_warns_without_pull() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = setup(tmp.path(), 3);
    let stale = sha(&fx.repo, "main");

    // --fetch alone updates origin/main but leaves main where it was.
    let (output, stderr) = wt_sync(&fx.repo, &["--fetch"]);
    assert_success(&output, &stderr);
    assert_eq!(sha(&fx.repo, "main"), stale);
    assert!(
        stderr.contains("main is 3 commits behind origin/main; run with --pull to update it"),
        "{stderr}"
    );

    // Plain `wt-sync` sees the same stale state from the previous fetch.
    let (output, stderr) = wt_sync(&fx.repo, &[]);
    assert_success(&output, &stderr);
    assert_eq!(sha(&fx.repo, "main"), stale);
    assert!(stderr.contains("run with --pull to update it"), "{stderr}");
}

#[test]
fn up_to_date_root_is_quiet_without_pull() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = setup(tmp.path(), 0);

    let (output, stderr) = wt_sync(&fx.repo, &["--fetch"]);
    assert_success(&output, &stderr);
    assert!(!stderr.contains("behind"), "{stderr}");
    assert!(!stderr.contains("--pull"), "{stderr}");
}

#[test]
fn pull_dry_run_previews_without_touching_anything() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = setup(tmp.path(), 2);
    let stale = sha(&fx.repo, "main");

    // A stacked branch, so the plan has something to show.
    let wt_path = tmp.path().join("worktrees").join("feature");
    git(
        &fx.repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            &wt_path.to_string_lossy(),
        ],
    );
    commit_file(&wt_path, "feature.txt", "feature work");

    // Dry run skips the fetch, so without one it only sees the old origin/main.
    let (output, stderr) = wt_sync(&fx.repo, &["--pull", "-n"]);
    assert_success(&output, &stderr);
    assert!(
        !stderr.contains("fast-forward"),
        "nothing to preview before a fetch:\n{stderr}"
    );

    git(&fx.repo, &["fetch", "-q"]);
    let (output, stderr) = wt_sync(&fx.repo, &["--pull", "-nv"]);
    assert_success(&output, &stderr);

    assert_eq!(sha(&fx.repo, "main"), stale, "dry run must not move main");
    let short_old = git(&fx.repo, &["rev-parse", "--short", "main"]);
    let short_new = git(&fx.repo, &["rev-parse", "--short", "origin/main"]);
    assert!(
        stderr.contains(&format!(
            "Would fast-forward main {short_old}..{short_new} (2 commits)"
        )),
        "{stderr}"
    );
    assert!(stderr.contains("may be stale"), "{stderr}");
    assert!(stderr.contains("$ git fetch"), "{stderr}");
    assert!(stderr.contains("merge --ff-only origin/main"), "{stderr}");
    // The plan must account for the pending fast-forward: feature is up to
    // date with the stale local main, but not with where main would move.
    assert!(stderr.contains("rebase feature onto main"), "{stderr}");
    assert!(!stderr.contains("feature is up to date"), "{stderr}");
    let mb = sha(&fx.repo, "main");
    assert!(
        stderr.contains(&format!("$ git rebase --onto main {mb} feature")),
        "{stderr}"
    );
}
