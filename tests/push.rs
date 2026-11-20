//! `--push` pushes where `git push` would, and nowhere else.

use std::path::Path;
use std::process::Command;

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

fn commit_file(dir: &Path, name: &str) {
    std::fs::write(dir.join(name), name).unwrap();
    git(dir, &["add", name]);
    git(dir, &["commit", "-m", name]);
}

fn wt_sync(dir: &Path, args: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_wt-sync"))
        .args(args)
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
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

#[test]
fn branch_tracking_another_branch_is_not_pushed_to_its_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "--bare", "origin.git"]);
    git(root, &["init", "-q", "--bare", "upstream.git"]);
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "push.default", "simple"]);
    commit_file(&repo, "initial.txt");
    git(&repo, &["remote", "add", "origin", "../origin.git"]);
    git(&repo, &["remote", "add", "upstream", "../upstream.git"]);
    git(&repo, &["push", "-q", "-u", "origin", "main"]);
    git(&repo, &["push", "-q", "upstream", "main"]);
    git(&repo, &["fetch", "-q", "upstream"]);

    // mine tracks origin/mine: pushed. theirs tracks upstream/main, like a
    // branch started from someone else's repo: git has nowhere to push it.
    let mine = root.join("wt-mine");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "mine",
            &mine.to_string_lossy(),
        ],
    );
    commit_file(&mine, "mine.txt");
    git(&mine, &["push", "-q", "-u", "origin", "mine"]);
    commit_file(&mine, "mine2.txt");
    let theirs = root.join("wt-theirs");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--track",
            "-b",
            "theirs",
            &theirs.to_string_lossy(),
            "upstream/main",
        ],
    );
    commit_file(&theirs, "theirs.txt");

    let plan = wt_sync(&repo, &["--all", "--push", "-n"]);
    assert!(plan.contains("push mine to origin"), "{plan}");
    assert!(!plan.contains("push theirs"), "{plan}");
    assert!(
        plan.contains("Not pushing theirs: it tracks upstream/main"),
        "{plan}"
    );

    let out = wt_sync(&repo, &["--all", "--push"]);
    assert!(out.contains("Pushed mine"), "{out}");
    assert!(out.contains("Not pushing theirs"), "{out}");
    let upstream_heads = git(&root.join("upstream.git"), &["branch", "--list"]);
    assert!(
        !upstream_heads.contains("theirs"),
        "pushed to upstream: {upstream_heads}"
    );
    assert_eq!(
        git(&root.join("origin.git"), &["rev-parse", "mine"]),
        git(&repo, &["rev-parse", "mine"])
    );
}

#[test]
fn branch_whose_remote_branch_was_deleted_is_still_pushed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "--bare", "origin.git"]);
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    commit_file(&repo, "initial.txt");
    git(&repo, &["remote", "add", "origin", "../origin.git"]);
    git(&repo, &["push", "-q", "-u", "origin", "main"]);
    let a = root.join("wt-a");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "a", &a.to_string_lossy()],
    );
    commit_file(&a, "a.txt");
    git(&a, &["push", "-q", "-u", "origin", "a"]);
    git(&repo, &["push", "-q", "origin", "--delete", "a"]);
    git(&repo, &["fetch", "-q", "--prune"]);

    let out = wt_sync(&repo, &["--all", "--push"]);
    assert!(out.contains("Pushed a"), "{out}");
}

/// A fork: main tracks upstream/main, a is pushed to origin, b (on a) and c
/// (on main) were never pushed.
fn fork_with_unpushed_branches(root: &Path) -> std::path::PathBuf {
    git(root, &["init", "-q", "--bare", "origin.git"]);
    git(root, &["init", "-q", "--bare", "upstream.git"]);
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "push.default", "simple"]);
    commit_file(&repo, "initial.txt");
    git(&repo, &["remote", "add", "origin", "../origin.git"]);
    git(&repo, &["remote", "add", "upstream", "../upstream.git"]);
    git(&repo, &["push", "-q", "origin", "main"]);
    git(&repo, &["push", "-q", "-u", "upstream", "main"]);

    let add = |name: &str, base: &str| {
        let dir = root.join(format!("wt-{name}"));
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                name,
                &dir.to_string_lossy(),
                base,
            ],
        );
        commit_file(&dir, &format!("{name}.txt"));
        dir
    };
    let a = add("a", "main");
    git(&a, &["push", "-q", "-u", "origin", "a"]);
    add("b", "a");
    add("c", "main");
    repo
}

#[test]
fn push_names_the_branches_it_leaves_local() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = fork_with_unpushed_branches(tmp.path());

    let plan = wt_sync(&repo, &["--all", "--push", "-n"]);
    assert!(plan.contains("push a to origin"), "{plan}");
    assert!(plan.contains("no upstream yet"), "{plan}");
    assert!(plan.contains("wt sync --publish"), "{plan}");

    let out = wt_sync(&repo, &["--all", "--push"]);
    assert!(out.contains("Pushed a"), "{out}");
    assert!(out.contains("no upstream yet"), "{out}");
    let heads = git(&tmp.path().join("origin.git"), &["branch", "--list"]);
    assert!(!heads.contains('b') && !heads.contains('c'), "{heads}");
}

#[test]
fn publish_pushes_new_branches_to_the_stack_remote_not_the_forks_upstream() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = fork_with_unpushed_branches(root);

    let plan = wt_sync(&repo, &["--all", "--publish", "-n", "-v"]);
    assert!(plan.contains("publish b to origin"), "{plan}");
    assert!(plan.contains("$ git push -u origin b"), "{plan}");
    assert!(plan.contains("publish c to origin"), "{plan}");

    let out = wt_sync(&repo, &["--all", "--publish"]);
    assert!(out.contains("Published b"), "{out}");
    assert!(out.contains("Published c"), "{out}");
    for branch in ["b", "c"] {
        assert_eq!(
            git(&root.join("origin.git"), &["rev-parse", branch]),
            git(&repo, &["rev-parse", branch])
        );
        assert_eq!(
            git(
                &repo,
                &[
                    "rev-parse",
                    "--abbrev-ref",
                    &format!("{branch}@{{upstream}}")
                ]
            ),
            format!("origin/{branch}")
        );
    }
    let upstream_heads = git(&root.join("upstream.git"), &["branch", "--list"]);
    assert_eq!(upstream_heads, "* main", "published to upstream");
}

#[test]
fn publish_follows_the_parents_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = fork_with_unpushed_branches(root);
    // a lives on a third remote: b, stacked on it, goes there too.
    git(root, &["init", "-q", "--bare", "team.git"]);
    git(&repo, &["remote", "add", "team", "../team.git"]);
    git(&root.join("wt-a"), &["push", "-q", "-u", "team", "a"]);

    let plan = wt_sync(&repo, &["--all", "--publish", "-n"]);
    assert!(plan.contains("publish b to team"), "{plan}");
    assert!(plan.contains("publish c to origin"), "{plan}");
}
