//! Dump-first vs pairwise parent inference on synthetic stacks.
//!
//! Not wired into CI. Run with `cargo bench --bench parent_infer`.
//! `cargo bench --bench parent_infer -- --quick` is a smoke pass.
//!
//! Shapes:
//! - `linear`: one commit per branch on a shallow `main`.
//! - `deep_main`: a short stack on a `main` with a long history.
//! - `long_branches`: a stack whose branches each carry many commits.
//! - `main_ahead`: a stack that forked from a `main` that has since moved on.
//!
//! Every iteration gets its own copy of the fixture repository. worktrunk keeps
//! merge-bases in memory and under `.git`, so reusing one repository would let
//! pairwise answer from cache after the first iteration.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use tempfile::TempDir;
use worktrunk::git::Repository;
use worktrunk_sync::parent_infer::{
    infer_parents, infer_parents_pairwise, InferredParents, MemoryBudget,
};

const DEFAULT_BRANCH: &str = "main";
const SAMPLE_SIZE: usize = 10;
const WARM_UP: Duration = Duration::from_secs(1);
const MEASURE: Duration = Duration::from_secs(2);

/// Commit counts of one fixture: `main ← b1 ← … ← b{stack}`.
#[derive(Clone, Copy)]
struct Shape {
    /// Commits on `main` before the stack forks.
    main_commits: usize,
    stack: usize,
    /// Commits on each stack branch.
    branch_commits: usize,
    /// Commits `main` gained after the stack forked.
    main_ahead: usize,
}

/// Generates the fixture with one `git fast-import`.
struct Fixture {
    template: TempDir,
    names: Vec<String>,
}

impl Fixture {
    fn new(shape: Shape) -> Self {
        let template = TempDir::new().expect("temp dir");
        run(template.path(), &["init", "-q", "-b", DEFAULT_BRANCH]);

        let mut stream = String::new();
        let mut mark = 0usize;
        let mut chain = |stream: &mut String, name: &str, from: Option<usize>, count: usize| {
            let mut tip = from;
            for _ in 0..count {
                mark += 1;
                let _ = write!(
                    stream,
                    "commit refs/heads/{name}\nmark :{mark}\ncommitter T <t@t> {} +0000\ndata 2\nc\n",
                    1_600_000_000 + mark
                );
                if let Some(parent) = tip {
                    let _ = writeln!(stream, "from :{parent}");
                }
                stream.push('\n');
                tip = Some(mark);
            }
            tip
        };

        let fork = chain(&mut stream, DEFAULT_BRANCH, None, shape.main_commits.max(1));
        let mut names = vec![DEFAULT_BRANCH.to_string()];
        let mut tip = fork;
        for i in 1..=shape.stack {
            let name = format!("b{i}");
            tip = chain(&mut stream, &name, tip, shape.branch_commits.max(1));
            names.push(name);
        }
        chain(&mut stream, DEFAULT_BRANCH, fork, shape.main_ahead);

        let mut child = Command::new("git")
            .args(["fast-import", "--quiet"])
            .current_dir(template.path())
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn git fast-import");
        child
            .stdin
            .take()
            .expect("fast-import stdin")
            .write_all(stream.as_bytes())
            .expect("feed fast-import");
        assert!(child.wait().expect("fast-import").success());
        run(template.path(), &["checkout", "-q", "-f", DEFAULT_BRANCH]);

        Self { template, names }
    }

    /// A private copy of the fixture, opened fresh so no cache carries over.
    fn fresh(&self) -> (TempDir, Repository) {
        let copy = TempDir::new().expect("temp dir");
        copy_dir(self.template.path(), copy.path());
        let repo = Repository::at(copy.path()).expect("open fixture copy");
        (copy, repo)
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

fn bench_shape(c: &mut Criterion, group_name: &str, cases: &[(String, Shape)]) {
    let mut group = c.benchmark_group(group_name);
    group.sample_size(SAMPLE_SIZE);
    group.warm_up_time(WARM_UP);
    group.measurement_time(MEASURE);

    let skip = HashSet::new();
    for (label, shape) in cases {
        let fixture = Fixture::new(*shape);
        let branches = fixture.branches();
        // Different answers would make the timings incomparable.
        let (_keep, repo) = fixture.fresh();
        assert_eq!(
            dump_first(&repo, &branches, &skip),
            pairwise(&repo, &branches, &skip),
            "{group_name}/{label}: dump-first and pairwise disagree"
        );

        group.bench_with_input(BenchmarkId::new("dump_first", label), label, |b, _| {
            b.iter_batched(
                || fixture.fresh(),
                |(_dir, repo)| dump_first(&repo, &branches, &skip),
                BatchSize::PerIteration,
            );
        });
        group.bench_with_input(BenchmarkId::new("pairwise", label), label, |b, _| {
            b.iter_batched(
                || fixture.fresh(),
                |(_dir, repo)| pairwise(&repo, &branches, &skip),
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

fn bench_parent_infer(c: &mut Criterion) {
    let linear: Vec<_> = [8, 16, 24, 32]
        .into_iter()
        .map(|n| {
            (
                n.to_string(),
                Shape {
                    main_commits: 1,
                    stack: n,
                    branch_commits: 1,
                    main_ahead: 0,
                },
            )
        })
        .collect();
    bench_shape(c, "linear", &linear);

    let deep_main = [(
        "20000x8".to_string(),
        Shape {
            main_commits: 20_000,
            stack: 8,
            branch_commits: 1,
            main_ahead: 0,
        },
    )];
    bench_shape(c, "deep_main", &deep_main);

    let long_branches = [(
        "16x500".to_string(),
        Shape {
            main_commits: 1,
            stack: 16,
            branch_commits: 500,
            main_ahead: 0,
        },
    )];
    bench_shape(c, "long_branches", &long_branches);

    let main_ahead = [(
        "20000+2000x8".to_string(),
        Shape {
            main_commits: 20_000,
            stack: 8,
            branch_commits: 1,
            main_ahead: 2_000,
        },
    )];
    bench_shape(c, "main_ahead", &main_ahead);
}

fn run(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn copy_dir(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read fixture dir") {
        let entry = entry.expect("fixture entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            std::fs::create_dir_all(&target).expect("create dir");
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy file");
        }
    }
}

criterion_group!(benches, bench_parent_infer);
criterion_main!(benches);
