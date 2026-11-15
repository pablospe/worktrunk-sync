//! Integration tests for gh-stack parent hints. A fake `gh` on PATH stands in
//! for the extension, so these never need the real one.

use std::os::unix::fs::PermissionsExt;
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

fn commit_file(dir: &Path, name: &str) {
    std::fs::write(dir.join(name), name).unwrap();
    git(dir, &["add", name]);
    git(dir, &["commit", "-m", name]);
}

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    /// Directory holding the fake `gh`, put first on PATH.
    bin: PathBuf,
}

impl Fixture {
    /// main with two branches forked from it: `a` and `b`. Git history alone
    /// makes both children of main; `b` also gets `a`'s commit when `stacked`.
    fn new(stacked: bool) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        commit_file(&repo, "initial.txt");

        let a = tmp.path().join("wt-a");
        git(&repo, &["worktree", "add", "-b", "a", &a.to_string_lossy()]);
        commit_file(&a, "a.txt");

        let b = tmp.path().join("wt-b");
        let start = if stacked { "a" } else { "main" };
        git(
            &repo,
            &["worktree", "add", "-b", "b", &b.to_string_lossy(), start],
        );
        commit_file(&b, "b.txt");
        commit_file(&repo, "m2.txt");

        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fixture = Self {
            _tmp: tmp,
            repo,
            bin,
        };
        fixture.set_gh_installed(true);
        fixture
    }

    /// Fake `gh`: succeeds only when installed, and logs its arguments.
    fn set_gh_installed(&self, installed: bool) {
        let path = self.bin.join("gh");
        let code = if installed { 0 } else { 1 };
        let log = self.gh_log();
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nexit {code}\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn gh_log(&self) -> PathBuf {
        self.bin.join("gh.log")
    }

    /// The `gh` calls that ran something other than the installation check.
    fn gh_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.gh_log())
            .unwrap_or_default()
            .lines()
            .filter(|line| *line != "stack --version")
            .map(String::from)
            .collect()
    }

    /// Write gh-stack's catalog declaring the stack `main <- a <- b`.
    fn declare_a_then_b(&self) {
        std::fs::write(
            self.repo.join(".git/gh-stack"),
            r#"{"schemaVersion":1,"repository":"o/r","stacks":[{"trunk":{"branch":"main"},
               "branches":[{"branch":"a"},{"branch":"b"}]}]}"#,
        )
        .unwrap();
    }

    fn wt_sync(&self, args: &[&str]) -> (Output, String) {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let output = Command::new(env!("CARGO_BIN_EXE_wt-sync"))
            .args(args)
            .current_dir(&self.repo)
            .env("PATH", path)
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

    fn plan(&self, args: &[&str]) -> String {
        let (output, text) = self.wt_sync(args);
        assert!(output.status.success(), "wt-sync {args:?} failed:\n{text}");
        text
    }
}

#[test]
fn declared_parent_is_used_and_labelled() {
    let f = Fixture::new(true);
    f.declare_a_then_b();
    let plan = f.plan(&["--all", "-n"]);
    assert!(plan.contains("└── b (gh-stack)"), "{plan}");
    assert!(plan.contains("rebase a onto main"), "{plan}");
    // b really is built on a, so there is nothing to warn about.
    assert!(!plan.contains("none of its commits"), "{plan}");
}

#[test]
fn declared_parent_beats_history_with_a_warning() {
    // History says b forked from main; gh-stack says it sits on a.
    let f = Fixture::new(false);
    f.declare_a_then_b();
    let plan = f.plan(&["--all", "-n"]);
    assert!(plan.contains("rebase b onto a"), "{plan}");
    assert!(
        plan.contains("but b has none of its commits"),
        "expected a warning that b was never built on a:\n{plan}"
    );

    let plan = f.plan(&["--all", "-n", "--no-gh-stack"]);
    assert!(plan.contains("rebase b onto main"), "{plan}");
    assert!(!plan.contains("(gh-stack)"), "{plan}");
}

#[test]
fn declared_parent_beats_the_stack_file() {
    let f = Fixture::new(false);
    // A real sync records b under main in .git/wt/stack.
    f.plan(&["--all"]);
    f.declare_a_then_b();
    let plan = f.plan(&["--all", "-n"]);
    assert!(plan.contains("rebase b onto a"), "{plan}");
}

#[test]
fn missing_gh_stack_is_ignored_unless_requested() {
    let f = Fixture::new(false);
    f.declare_a_then_b();
    f.set_gh_installed(false);

    let plan = f.plan(&["--all", "-n", "-v"]);
    assert!(plan.contains("rebase b onto main"), "{plan}");
    assert!(plan.contains("gh-stack is not installed"), "{plan}");

    let (output, text) = f.wt_sync(&["--all", "-n", "--gh-stack"]);
    assert!(!output.status.success(), "{text}");
    assert!(
        text.contains("gh extension install github/gh-stack"),
        "{text}"
    );
}

#[test]
fn unsupported_catalog_is_ignored() {
    let f = Fixture::new(false);
    std::fs::write(
        f.repo.join(".git/gh-stack"),
        r#"{"schemaVersion":2,"stacks":[]}"#,
    )
    .unwrap();
    let plan = f.plan(&["--all", "-n", "-v"]);
    assert!(plan.contains("rebase b onto main"), "{plan}");
    assert!(plan.contains("schema version 2"), "{plan}");
}

#[test]
fn paused_gh_stack_rebase_blocks_the_sync() {
    let f = Fixture::new(true);
    f.declare_a_then_b();
    std::fs::write(f.repo.join(".git/gh-stack-rebase-state"), "{}").unwrap();

    let (output, text) = f.wt_sync(&["--all"]);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains("gh stack rebase --continue"), "{text}");

    // --no-gh-stack opts out of the guard along with the hints.
    f.plan(&["--all", "-n", "--no-gh-stack"]);
}

#[test]
fn gh_stack_init_prints_the_commands() {
    let f = Fixture::new(true);
    let (output, text) = f.wt_sync(&["--all", "--gh-stack-init"]);
    assert!(output.status.success(), "{text}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "gh stack init --base main a b"
    );
    assert!(f.gh_calls().is_empty(), "{:?}", f.gh_calls());
    // Printing never syncs: no stack file is written.
    assert!(!f.repo.join(".git/wt/stack").exists());
}

#[test]
fn gh_stack_init_run_runs_them() {
    let f = Fixture::new(true);
    let (output, text) = f.wt_sync(&["--all", "--gh-stack-init=run", "-n"]);
    assert!(output.status.success(), "{text}");
    assert!(
        f.gh_calls().is_empty(),
        "dry run ran gh: {:?}",
        f.gh_calls()
    );

    let (output, text) = f.wt_sync(&["--all", "--gh-stack-init=run"]);
    assert!(output.status.success(), "{text}");
    assert_eq!(f.gh_calls(), ["stack init --base main a b"]);
}

#[test]
fn gh_stack_init_skips_registered_and_fan_out_stacks() {
    let f = Fixture::new(true);
    f.declare_a_then_b();
    let (output, text) = f.wt_sync(&["--all", "--gh-stack-init"]);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("a is already in a gh-stack stack"), "{text}");
    assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());

    // main -> x -> {y, z}: gh-stack can't hold it.
    let f = Fixture::new(false);
    let tmp = f.repo.parent().unwrap();
    let x = tmp.join("wt-x");
    git(
        &f.repo,
        &["worktree", "add", "-b", "x", &x.to_string_lossy()],
    );
    commit_file(&x, "x.txt");
    for name in ["y", "z"] {
        let wt = tmp.join(format!("wt-{name}"));
        git(
            &f.repo,
            &["worktree", "add", "-b", name, &wt.to_string_lossy(), "x"],
        );
        commit_file(&wt, &format!("{name}.txt"));
    }
    let (output, text) = f.wt_sync(&["--all", "--gh-stack-init"]);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("x has 2 branches on top of it"), "{text}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        ["gh stack init --base main a", "gh stack init --base main b"]
    );
}
