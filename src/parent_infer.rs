//! Infer stacked-branch parents from one `git rev-list --parents` dump.
//!
//! Cold discovery used to call `git merge-base` and two `rev-list --count`s per
//! pair. This module dumps the parent graph of the worktree tips plus the
//! default branch once, then applies the same two-tier pick in memory.
//!
//! `git merge-base` is used only for a pair whose dump has more than one best
//! merge-base (criss-cross). The whole tree falls back to pairwise git only
//! when the dump would exceed [`MemoryBudget`].

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use worktrunk::git::Repository;

/// Commits allowed in the dump before falling back to pairwise git.
pub const MAX_COMMITS: usize = 100_000;

/// Tip×commit bitset budget (32 MiB).
pub const MAX_BITSET_BYTES: usize = 32 * 1024 * 1024;

/// Memory ceiling for the in-memory dump. Over either cap, every pair uses git.
#[derive(Debug, Clone, Copy)]
pub struct MemoryBudget {
    pub max_commits: usize,
    pub max_bitset_bytes: usize,
}

impl Default for MemoryBudget {
    fn default() -> Self {
        Self {
            max_commits: MAX_COMMITS,
            max_bitset_bytes: MAX_BITSET_BYTES,
        }
    }
}

/// Bytes for `tips` bitsets over `commits` (one bit per commit).
pub fn bitset_bytes(tips: usize, commits: usize) -> usize {
    tips.saturating_mul(commits.div_ceil(8))
}

/// Git history needed to infer parents. Facade over [`Repository`].
pub trait History {
    fn resolve_shas(&self, revs: &[&str]) -> Result<Vec<String>>;
    fn rev_list_parents(&self, tips: &[&str]) -> Result<String>;
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>>;
    fn count_commits(&self, base: &str, head: &str) -> Result<usize>;
    fn commit_details_many(&self, shas: &[&str]) -> Result<HashMap<String, (String, i64, String)>>;
}

impl History for Repository {
    fn resolve_shas(&self, revs: &[&str]) -> Result<Vec<String>> {
        if revs.is_empty() {
            return Ok(Vec::new());
        }
        // `git rev-parse --end-of-options` echoes the marker on stdout, so
        // the multi-ref form stays unguarded (same as worktrunk).
        let mut args = Vec::with_capacity(revs.len() + 1);
        args.push("rev-parse");
        args.extend(revs.iter().copied());
        let stdout = self.run_command(&args)?;
        let shas: Vec<String> = stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if shas.len() != revs.len() {
            bail!(
                "git rev-parse returned {} SHAs for {} refs",
                shas.len(),
                revs.len()
            );
        }
        Ok(shas)
    }

    fn rev_list_parents(&self, tips: &[&str]) -> Result<String> {
        if tips.is_empty() {
            return Ok(String::new());
        }
        let mut args = Vec::with_capacity(tips.len() + 3);
        args.push("rev-list");
        args.push("--parents");
        args.push("--end-of-options");
        args.extend(tips.iter().copied());
        self.run_command(&args)
            .context("git rev-list --parents failed")
    }

    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
        Repository::merge_base(self, a, b)
    }

    fn count_commits(&self, base: &str, head: &str) -> Result<usize> {
        Repository::count_commits(self, base, head)
    }

    fn commit_details_many(&self, shas: &[&str]) -> Result<HashMap<String, (String, i64, String)>> {
        Repository::commit_details_many(self, shas)
    }
}

/// Parent chosen for one branch, plus equidistant-parent warnings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferredParents {
    /// Child branch → parent branch. Missing children default to the default branch.
    pub parents: HashMap<String, String>,
    pub equidistant: Vec<EquidistantParents>,
}

/// Two or more candidates shared the same two-tier rank for `branch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EquidistantParents {
    pub branch: String,
    pub candidates: Vec<String>,
    pub picked: String,
}

/// Infer a parent for each branch that is not `default_branch` and not in `skip`.
///
/// `branches` must include `default_branch` when it is a valid candidate (it
/// always is). Candidates include every name in `branches`; `skip` only
/// suppresses assigning a parent (integrated branches stay candidates).
pub fn infer_parents<H: History>(
    history: &H,
    branches: &[&str],
    default_branch: &str,
    skip: &HashSet<&str>,
    budget: MemoryBudget,
) -> Result<InferredParents> {
    match infer_from_dump(history, branches, default_branch, skip, budget)? {
        Some(inferred) => Ok(inferred),
        None => infer_parents_pairwise(history, branches, default_branch, skip),
    }
}

/// Pairwise `git merge-base` + two `rev-list --count`s per pair. Used when the
/// dump is over budget, and as the identity baseline in tests.
pub fn infer_parents_pairwise<H: History>(
    history: &H,
    branches: &[&str],
    default_branch: &str,
    skip: &HashSet<&str>,
) -> Result<InferredParents> {
    let mut parents = HashMap::new();
    let mut equidistant = Vec::new();

    for branch in branches {
        if *branch == default_branch || skip.contains(branch) {
            continue;
        }

        let mut ranked = Vec::new();
        for candidate in branches {
            if *candidate == *branch {
                continue;
            }
            let Some(mb) = history.merge_base(candidate, branch)? else {
                continue;
            };
            let branch_depth = history.count_commits(&mb, branch)?;
            if branch_depth == 0 {
                continue;
            }
            let candidate_depth = history.count_commits(&mb, candidate)?;
            ranked.push(RankedParent {
                name: (*candidate).to_string(),
                merge_base: mb,
                branch_depth,
                candidate_depth,
            });
        }

        if let Some(pick) = select_parent(&ranked, history)? {
            record_pick(branch, pick, &mut parents, &mut equidistant);
        }
    }

    Ok(InferredParents {
        parents,
        equidistant,
    })
}

/// `None` means the dump is over budget (or unusable); caller falls back.
fn infer_from_dump<H: History>(
    history: &H,
    branches: &[&str],
    default_branch: &str,
    skip: &HashSet<&str>,
    budget: MemoryBudget,
) -> Result<Option<InferredParents>> {
    let dump_refs = dump_refs(branches, default_branch);
    let tip_shas = history.resolve_shas(&dump_refs)?;
    let dump =
        history.rev_list_parents(&tip_shas.iter().map(String::as_str).collect::<Vec<_>>())?;
    let graph = CommitGraph::parse(&dump);

    let tip_count = dump_refs.len();
    if graph.len() > budget.max_commits
        || bitset_bytes(tip_count, graph.len()) > budget.max_bitset_bytes
    {
        return Ok(None);
    }

    let mut tip_index = Vec::with_capacity(branches.len());
    let mut branch_shas = Vec::with_capacity(branches.len());
    for branch in branches {
        let pos = dump_refs
            .iter()
            .position(|r| *r == *branch)
            .expect("dump_refs contains every branch");
        let sha = &tip_shas[pos];
        let Some(idx) = graph.index.get(sha).copied() else {
            return Ok(None);
        };
        tip_index.push(idx);
        branch_shas.push(sha.clone());
    }

    let bits: Vec<Bitset> = tip_index
        .iter()
        .map(|&idx| graph.ancestor_bits(idx))
        .collect();

    let mut parents = HashMap::new();
    let mut equidistant = Vec::new();

    for (b_i, branch) in branches.iter().enumerate() {
        if *branch == default_branch || skip.contains(branch) {
            continue;
        }

        let mut ranked = Vec::new();
        for (c_i, candidate) in branches.iter().enumerate() {
            if c_i == b_i {
                continue;
            }
            let Some(mb) = merge_base_for_pair(
                history,
                &graph,
                &bits[b_i],
                &bits[c_i],
                &branch_shas[b_i],
                &branch_shas[c_i],
            )?
            else {
                continue;
            };
            let Some((branch_depth, candidate_depth)) = pair_depths(
                history,
                &graph,
                tip_index[b_i],
                tip_index[c_i],
                &mb,
                branch,
                candidate,
            )?
            else {
                continue;
            };
            if branch_depth == 0 {
                continue;
            }
            ranked.push(RankedParent {
                name: (*candidate).to_string(),
                merge_base: mb,
                branch_depth,
                candidate_depth,
            });
        }

        if let Some(pick) = select_parent(&ranked, history)? {
            record_pick(branch, pick, &mut parents, &mut equidistant);
        }
    }

    Ok(Some(InferredParents {
        parents,
        equidistant,
    }))
}

fn dump_refs<'a>(branches: &'a [&'a str], default_branch: &'a str) -> Vec<&'a str> {
    let mut refs = branches.to_vec();
    if !refs.contains(&default_branch) {
        refs.push(default_branch);
    }
    refs
}

fn merge_base_for_pair<H: History>(
    history: &H,
    graph: &CommitGraph,
    bits_b: &Bitset,
    bits_c: &Bitset,
    sha_b: &str,
    sha_c: &str,
) -> Result<Option<String>> {
    if sha_b == sha_c {
        return Ok(Some(sha_b.to_string()));
    }
    let bases = graph.maximal_common_ancestors(bits_b, bits_c);
    match bases.as_slice() {
        [] => Ok(None),
        [one] => Ok(Some(graph.shas[*one].clone())),
        _ => history.merge_base(sha_c, sha_b),
    }
}

fn pair_depths<H: History>(
    history: &H,
    graph: &CommitGraph,
    head_idx: usize,
    candidate_idx: usize,
    mb_sha: &str,
    head: &str,
    candidate: &str,
) -> Result<Option<(usize, usize)>> {
    if let Some(mb_idx) = graph.index.get(mb_sha).copied() {
        return Ok(Some((
            graph.count_ahead(head_idx, mb_idx),
            graph.count_ahead(candidate_idx, mb_idx),
        )));
    }
    Ok(Some((
        history.count_commits(mb_sha, head)?,
        history.count_commits(mb_sha, candidate)?,
    )))
}

struct RankedParent {
    name: String,
    merge_base: String,
    branch_depth: usize,
    candidate_depth: usize,
}

struct ParentPick {
    parent: String,
    equidistant: Vec<String>,
}

/// Two-tier pick: true ancestors (candidate_depth == 0) win by smallest
/// branch_depth; else diverged by (branch_depth, candidate_depth). Ties break
/// on merge-base committer timestamp (newest wins).
fn select_parent<H: History>(ranked: &[RankedParent], history: &H) -> Result<Option<ParentPick>> {
    let mut ancestors: Vec<&RankedParent> =
        ranked.iter().filter(|r| r.candidate_depth == 0).collect();
    let mut diverged: Vec<&RankedParent> =
        ranked.iter().filter(|r| r.candidate_depth != 0).collect();

    let ties: Vec<&RankedParent> = if !ancestors.is_empty() {
        ancestors.sort_by_key(|r| r.branch_depth);
        let best_bd = ancestors[0].branch_depth;
        ancestors
            .into_iter()
            .filter(|r| r.branch_depth == best_bd)
            .collect()
    } else if !diverged.is_empty() {
        diverged.sort_by_key(|r| (r.branch_depth, r.candidate_depth));
        let best = (diverged[0].branch_depth, diverged[0].candidate_depth);
        diverged
            .into_iter()
            .filter(|r| (r.branch_depth, r.candidate_depth) == best)
            .collect()
    } else {
        return Ok(None);
    };

    let mut parent = ties[0].name.clone();
    if ties.len() == 1 {
        return Ok(Some(ParentPick {
            parent,
            equidistant: Vec::new(),
        }));
    }

    let mb_shas: Vec<&str> = ties.iter().map(|t| t.merge_base.as_str()).collect();
    let commits = history.commit_details_many(&mb_shas)?;
    let mut best_ts = i64::MIN;
    for t in &ties {
        if let Some(&(_, ts, _)) = commits.get(t.merge_base.as_str()) {
            if ts > best_ts {
                best_ts = ts;
                parent = t.name.clone();
            }
        }
    }

    Ok(Some(ParentPick {
        parent,
        equidistant: ties.iter().map(|t| t.name.clone()).collect(),
    }))
}

fn record_pick(
    branch: &str,
    pick: ParentPick,
    parents: &mut HashMap<String, String>,
    equidistant: &mut Vec<EquidistantParents>,
) {
    if !pick.equidistant.is_empty() {
        equidistant.push(EquidistantParents {
            branch: branch.to_string(),
            candidates: pick.equidistant,
            picked: pick.parent.clone(),
        });
    }
    parents.insert(branch.to_string(), pick.parent);
}

struct Bitset {
    words: Vec<u64>,
}

impl Bitset {
    fn new(bits: usize) -> Self {
        Self {
            words: vec![0; bits.div_ceil(64)],
        }
    }

    fn set(&mut self, i: usize) {
        self.words[i / 64] |= 1 << (i % 64);
    }

    fn get(&self, i: usize) -> bool {
        self.words
            .get(i / 64)
            .is_some_and(|word| (word >> (i % 64)) & 1 == 1)
    }
}

struct CommitGraph {
    index: HashMap<String, usize>,
    shas: Vec<String>,
    parents: Vec<Vec<usize>>,
}

impl CommitGraph {
    fn parse(dump: &str) -> Self {
        let mut index = HashMap::new();
        let mut shas = Vec::new();
        let mut parent_names: Vec<Vec<String>> = Vec::new();

        for line in dump.lines() {
            let mut parts = line.split_whitespace();
            let Some(sha) = parts.next() else {
                continue;
            };
            if !index.contains_key(sha) {
                index.insert(sha.to_string(), shas.len());
                shas.push(sha.to_string());
                parent_names.push(Vec::new());
            }
            let idx = index[sha];
            let listed: Vec<String> = parts.map(ToOwned::to_owned).collect();
            for parent in &listed {
                if !index.contains_key(parent) {
                    index.insert(parent.clone(), shas.len());
                    shas.push(parent.clone());
                    parent_names.push(Vec::new());
                }
            }
            parent_names[idx] = listed;
        }

        let parents = parent_names
            .into_iter()
            .map(|names| names.into_iter().map(|p| index[&p]).collect())
            .collect();

        Self {
            index,
            shas,
            parents,
        }
    }

    fn len(&self) -> usize {
        self.shas.len()
    }

    fn ancestor_bits(&self, tip: usize) -> Bitset {
        let mut bits = Bitset::new(self.shas.len());
        let mut stack = vec![tip];
        while let Some(i) = stack.pop() {
            if bits.get(i) {
                continue;
            }
            bits.set(i);
            stack.extend_from_slice(&self.parents[i]);
        }
        bits
    }

    fn mark_reachable(&self, start: usize, seen: &mut [bool]) {
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            if seen[i] {
                continue;
            }
            seen[i] = true;
            stack.extend_from_slice(&self.parents[i]);
        }
    }

    fn maximal_common_ancestors(&self, a: &Bitset, b: &Bitset) -> Vec<usize> {
        let n = self.shas.len();
        let mut common = Vec::new();
        for i in 0..n {
            if a.get(i) && b.get(i) {
                common.push(i);
            }
        }
        if common.len() <= 1 {
            return common;
        }
        let mut dominated = vec![false; n];
        for &c in &common {
            for &p in &self.parents[c] {
                self.mark_reachable(p, &mut dominated);
            }
        }
        common.into_iter().filter(|&c| !dominated[c]).collect()
    }

    fn count_ahead(&self, head: usize, base: usize) -> usize {
        let n = self.shas.len();
        let mut from_base = vec![false; n];
        self.mark_reachable(base, &mut from_base);
        let mut seen = vec![false; n];
        let mut stack = vec![head];
        let mut count = 0;
        while let Some(i) = stack.pop() {
            if seen[i] {
                continue;
            }
            seen[i] = true;
            if !from_base[i] {
                count += 1;
            }
            stack.extend_from_slice(&self.parents[i]);
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-b", "main"]);
    }

    fn commit(dir: &Path, file: &str, msg: &str) {
        std::fs::write(dir.join(file), msg).unwrap();
        git(dir, &["add", file]);
        git(dir, &["commit", "-m", msg]);
    }

    /// main ← pr1 ← pr2 ← pr3, plus feature off main.
    fn stacked_repo(tmp: &Path) -> PathBuf {
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        commit(&repo, "main.txt", "main");
        git(&repo, &["checkout", "-b", "pr1"]);
        commit(&repo, "pr1.txt", "pr1");
        git(&repo, &["checkout", "-b", "pr2"]);
        commit(&repo, "pr2.txt", "pr2");
        git(&repo, &["checkout", "-b", "pr3"]);
        commit(&repo, "pr3.txt", "pr3");
        git(&repo, &["checkout", "main"]);
        git(&repo, &["checkout", "-b", "feature"]);
        commit(&repo, "feat.txt", "feature");
        repo
    }

    #[test]
    fn dump_first_matches_pairwise_on_synthetic_stack() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = stacked_repo(tmp.path());
        let repo = Repository::at(&repo_path).unwrap();
        let branches = ["main", "pr1", "pr2", "pr3", "feature"];
        let skip = HashSet::new();

        let dump = infer_parents(&repo, &branches, "main", &skip, MemoryBudget::default()).unwrap();
        let pair = infer_parents_pairwise(&repo, &branches, "main", &skip).unwrap();

        let dump_map: BTreeMap<_, _> = dump.parents.iter().collect();
        let pair_map: BTreeMap<_, _> = pair.parents.iter().collect();
        println!("dump-first: {dump_map:?}");
        println!("pairwise:   {pair_map:?}");

        assert_eq!(dump.parents, pair.parents);
        assert_eq!(dump.parents.get("pr1").map(String::as_str), Some("main"));
        assert_eq!(dump.parents.get("pr2").map(String::as_str), Some("pr1"));
        assert_eq!(dump.parents.get("pr3").map(String::as_str), Some("pr2"));
        assert_eq!(
            dump.parents.get("feature").map(String::as_str),
            Some("main")
        );
    }

    #[test]
    fn over_budget_dump_matches_pairwise() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = stacked_repo(tmp.path());
        let repo = Repository::at(&repo_path).unwrap();
        let branches = ["main", "pr1", "pr2", "pr3", "feature"];
        let skip = HashSet::new();
        let budget = MemoryBudget {
            max_commits: 1,
            max_bitset_bytes: MAX_BITSET_BYTES,
        };

        let over = infer_parents(&repo, &branches, "main", &skip, budget).unwrap();
        let pair = infer_parents_pairwise(&repo, &branches, "main", &skip).unwrap();
        assert_eq!(over.parents, pair.parents);
    }

    #[test]
    fn skip_does_not_assign_parent_but_keeps_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = stacked_repo(tmp.path());
        let repo = Repository::at(&repo_path).unwrap();
        let branches = ["main", "pr1", "pr2", "pr3", "feature"];
        let skip = HashSet::from(["pr1"]);

        let inferred =
            infer_parents(&repo, &branches, "main", &skip, MemoryBudget::default()).unwrap();
        assert!(!inferred.parents.contains_key("pr1"));
        assert_eq!(inferred.parents.get("pr2").map(String::as_str), Some("pr1"));
    }

    fn commit_dated(dir: &Path, file: &str, msg: &str, date: &str) {
        std::fs::write(dir.join(file), format!("{msg}\n")).unwrap();
        git(dir, &["add", file]);
        let output = Command::new("git")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                msg,
            ])
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("HK", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn timestamp_tie_picks_newer_merge_base() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        commit_dated(&repo, "main.txt", "main", "2020-01-01T00:00:00 +0000");
        git(&repo, &["checkout", "-b", "pr1"]);
        commit_dated(&repo, "pr1.txt", "pr1", "2020-01-01T00:00:01 +0000");
        git(&repo, &["checkout", "main"]);
        git(&repo, &["checkout", "-b", "pr2"]);
        commit_dated(&repo, "pr2.txt", "pr2", "2020-01-01T00:00:02 +0000");
        git(&repo, &["checkout", "-b", "pr3"]);
        git(&repo, &["merge", "--no-ff", "--no-edit", "pr1"]);

        let git_repo = Repository::at(&repo).unwrap();
        let branches = ["main", "pr1", "pr2", "pr3"];
        let skip = HashSet::new();
        let dump =
            infer_parents(&git_repo, &branches, "main", &skip, MemoryBudget::default()).unwrap();
        let pair = infer_parents_pairwise(&git_repo, &branches, "main", &skip).unwrap();

        assert_eq!(dump.parents, pair.parents);
        assert_eq!(dump.parents.get("pr3").map(String::as_str), Some("pr2"));
        assert_eq!(dump.equidistant.len(), 1);
        assert_eq!(dump.equidistant[0].branch, "pr3");
        assert_eq!(dump.equidistant[0].picked, "pr2");
    }

    #[test]
    fn bitset_bytes_is_tips_times_commit_bytes() {
        assert_eq!(bitset_bytes(2, 8), 2);
        assert_eq!(bitset_bytes(2, 9), 4);
        assert_eq!(bitset_bytes(0, 100_000), 0);
        assert_eq!(bitset_bytes(10, 0), 0);
    }

    #[test]
    fn parse_dump_counts_unique_commits_and_dangling_parents() {
        let graph = CommitGraph::parse("c1 p1 p2\np1 root\nroot\n");
        assert_eq!(graph.len(), 4); // c1, p1, p2 (dangling), root
        assert!(graph.index.contains_key("p2"));
        assert!(graph.parents[graph.index["p2"]].is_empty());
    }

    struct FakeHistory {
        names: HashMap<String, String>,
        dump: String,
        merge_bases: HashMap<(String, String), Option<String>>,
        timestamps: HashMap<String, i64>,
        merge_base_calls: std::cell::Cell<usize>,
    }

    impl History for FakeHistory {
        fn resolve_shas(&self, revs: &[&str]) -> Result<Vec<String>> {
            revs.iter()
                .map(|rev| {
                    self.names
                        .get(*rev)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("unknown rev {rev}"))
                })
                .collect()
        }

        fn rev_list_parents(&self, _tips: &[&str]) -> Result<String> {
            Ok(self.dump.clone())
        }

        fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
            self.merge_base_calls.set(self.merge_base_calls.get() + 1);
            let key = if a <= b {
                (a.to_string(), b.to_string())
            } else {
                (b.to_string(), a.to_string())
            };
            Ok(self.merge_bases.get(&key).cloned().flatten())
        }

        fn count_commits(&self, _base: &str, _head: &str) -> Result<usize> {
            bail!("count_commits should not run on the dump path");
        }

        fn commit_details_many(
            &self,
            shas: &[&str],
        ) -> Result<HashMap<String, (String, i64, String)>> {
            Ok(shas
                .iter()
                .filter_map(|sha| {
                    self.timestamps
                        .get(*sha)
                        .map(|ts| ((*sha).to_string(), (sha.to_string(), *ts, String::new())))
                })
                .collect())
        }
    }

    #[test]
    fn criss_cross_calls_merge_base_for_that_pair_only() {
        // root ← a, root ← b; left and right both merge a+b (incomparable bases).
        let dump = "\
leftsha asha bsha
rightsha bsha asha
asha root
bsha root
root
";
        let history = FakeHistory {
            names: HashMap::from([
                ("main".into(), "root".into()),
                ("left".into(), "leftsha".into()),
                ("right".into(), "rightsha".into()),
            ]),
            dump: dump.to_string(),
            merge_bases: HashMap::from([(
                ("leftsha".into(), "rightsha".into()),
                Some("asha".into()),
            )]),
            timestamps: HashMap::new(),
            merge_base_calls: std::cell::Cell::new(0),
        };
        let branches = ["main", "left", "right"];
        let skip = HashSet::new();
        let inferred =
            infer_parents(&history, &branches, "main", &skip, MemoryBudget::default()).unwrap();

        assert_eq!(history.merge_base_calls.get(), 2);
        assert_eq!(
            inferred.parents.get("left").map(String::as_str),
            Some("main")
        );
        assert_eq!(
            inferred.parents.get("right").map(String::as_str),
            Some("main")
        );
    }
}
