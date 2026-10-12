//! Dump-first vs pairwise parent inference on synthetic linear stacks.
//!
//! Not wired into CI. Run with `cargo bench --bench parent_infer`.
//! `cargo bench --bench parent_infer -- --quick` is a smoke pass.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tempfile::TempDir;
use worktrunk::git::Repository;
use worktrunk_sync::parent_infer::{
    infer_parents, infer_parents_pairwise, InferredParents, MemoryBudget,
};

const DEFAULT_BRANCH: &str = "main";
/// Stack depths that still show pairwise's O(n²) wall without a long laptop run.
const STACK_LENGTHS: [usize; 4] = [8, 16, 24, 32];
const SAMPLE_SIZE: usize = 10;
const WARM_UP: Duration = Duration::from_secs(1);
const MEASURE: Duration = Duration::from_secs(2);

struct LinearStack {
    _tmp: TempDir,
    repo: Repository,
    /// `main`, then `b1`..`bn`.
    names: Vec<String>,
}

impl LinearStack {
    /// `main ← b1 ← b2 ← … ← bn`.
    fn new(stack_len: usize) -> Self {
        let tmp = TempDir::new().expect("temp dir");
        let dir = tmp.path();
        init_repo(dir);
        commit(dir, "main.txt", "main");

        let mut names = vec![DEFAULT_BRANCH.to_string()];
        for i in 1..=stack_len {
            let name = format!("b{i}");
            git(dir, &["checkout", "-b", &name]);
            commit(dir, &format!("{name}.txt"), &name);
            names.push(name);
        }

        let repo = Repository::at(dir).expect("open synthetic repo");
        Self {
            _tmp: tmp,
            repo,
            names,
        }
    }

    fn branches(&self) -> Vec<&str> {
        self.names.iter().map(String::as_str).collect()
    }
}

fn dump_first(repo: &Repository, branches: &[&str], skip: &HashSet<&str>) -> InferredParents {
    infer_parents(
        repo,
        branches,
        DEFAULT_BRANCH,
        skip,
        MemoryBudget::default(),
    )
    .expect("dump-first parent inference")
}

fn pairwise(repo: &Repository, branches: &[&str], skip: &HashSet<&str>) -> InferredParents {
    infer_parents_pairwise(repo, branches, DEFAULT_BRANCH, skip).expect("pairwise parent inference")
}

fn bench_parent_infer(c: &mut Criterion) {
    let mut group = c.benchmark_group("parent_infer");
    group.sample_size(SAMPLE_SIZE);
    group.warm_up_time(WARM_UP);
    group.measurement_time(MEASURE);

    let skip = HashSet::new();
    for n in STACK_LENGTHS {
        let stack = LinearStack::new(n);
        let branches = stack.branches();

        group.bench_with_input(BenchmarkId::new("dump_first", n), &n, |b, _| {
            b.iter(|| dump_first(&stack.repo, &branches, &skip));
        });
        group.bench_with_input(BenchmarkId::new("pairwise", n), &n, |b, _| {
            b.iter(|| pairwise(&stack.repo, &branches, &skip));
        });
    }

    group.finish();
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .env("HK", "0")
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
    if !output.status.success() {
        panic!(
            "git {args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-b", DEFAULT_BRANCH]);
}

fn commit(dir: &Path, file: &str, msg: &str) {
    std::fs::write(dir.join(file), msg).expect("write commit file");
    git(dir, &["add", file]);
    git(dir, &["commit", "-m", msg]);
}

criterion_group!(benches, bench_parent_infer);
criterion_main!(benches);
