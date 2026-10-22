//! Infer stacked-branch parents from one `git rev-list --parents` dump.
//!
//! Cold discovery used to call `git merge-base` and two `rev-list --count`s per
//! pair. This module dumps the parent graph of the worktree tips plus the
//! default branch once, then applies the same two-tier pick in memory.
//!
//! The dump stops at the best common ancestor of every tip (the *floor*), so a
//! long history below the stack is never walked. Merge-bases come from the
//! dump, and commits-ahead counts are bitset popcounts. `git merge-base` runs
//! only for a pair whose dump has more than one best merge-base (criss-cross).
//! The whole tree falls back to pairwise git only when the dump would exceed
//! [`MemoryBudget`].

use std::collections::hash_map::Entry;
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
    /// Tip commit SHA of each local branch, in order.
    fn resolve_branch_tips(&self, branches: &[&str]) -> Result<Vec<String>>;
    /// Best common ancestor of every commit in `tips`; `None` when they share none.
    fn octopus_merge_base(&self, tips: &[&str]) -> Result<Option<String>>;
    /// `rev-list --parents` of `tips` without anything reachable from the
    /// parents of `floor` (so `floor` is the last commit listed), cut off after
    /// `max_lines` commits.
    fn rev_list_parents(
        &self,
        tips: &[&str],
        floor: Option<&str>,
        max_lines: usize,
    ) -> Result<String>;
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>>;
    fn count_commits(&self, base: &str, head: &str) -> Result<usize>;
    fn commit_details_many(&self, shas: &[&str]) -> Result<HashMap<String, (String, i64, String)>>;
}

impl History for Repository {
    fn resolve_branch_tips(&self, branches: &[&str]) -> Result<Vec<String>> {
        if branches.is_empty() {
            return Ok(Vec::new());
        }
        // Fully qualified: a tag named like a branch would otherwise win the
        // `rev-parse` lookup and return a tag object that no dump contains.
        // `git rev-parse --end-of-options` echoes the marker on stdout, so the
        // multi-ref form stays unguarded (same as worktrunk).
        let qualified: Vec<String> = branches
            .iter()
            .map(|branch| format!("refs/heads/{branch}"))
            .collect();
        let mut args = Vec::with_capacity(qualified.len() + 1);
        args.push("rev-parse");
        args.extend(qualified.iter().map(String::as_str));
        let stdout = self.run_command(&args)?;
        let shas: Vec<String> = stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if shas.len() != branches.len() {
            bail!(
                "git rev-parse returned {} SHAs for {} branches",
                shas.len(),
                branches.len()
            );
        }
        Ok(shas)
    }

    fn octopus_merge_base(&self, tips: &[&str]) -> Result<Option<String>> {
        if tips.is_empty() {
            return Ok(None);
        }
        let mut args = Vec::with_capacity(tips.len() + 3);
        args.extend(["merge-base", "--octopus", "--end-of-options"]);
        args.extend(tips.iter().copied());
        // Exit 1 (no common ancestor, e.g. an orphan branch) and a real failure
        // both read as "no floor". Only the size of the walk depends on it, so
        // the capped, unbounded dump is the safe answer to either.
        Ok(self
            .run_command(&args)
            .ok()
            .map(|stdout| stdout.trim().to_owned())
            .filter(|sha| !sha.is_empty()))
    }

    fn rev_list_parents(
        &self,
        tips: &[&str],
        floor: Option<&str>,
        max_lines: usize,
    ) -> Result<String> {
        if tips.is_empty() {
            return Ok(String::new());
        }
        let max_count = format!("--max-count={max_lines}");
        let below_floor = floor.map(|sha| format!("^{sha}^@"));
        let mut args = Vec::with_capacity(tips.len() + 5);
        args.extend([
            "rev-list",
            "--parents",
            max_count.as_str(),
            "--end-of-options",
        ]);
        args.extend(tips.iter().copied());
        args.extend(below_floor.as_deref());
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
    // SHAs, not names: a tag sharing a branch's name must not be measured.
    let tips = history.resolve_branch_tips(branches)?;
    rank_parents(
        history,
        branches,
        default_branch,
        skip,
        |branch, candidate| {
            let Some(merge_base) = history.merge_base(&tips[candidate], &tips[branch])? else {
                return Ok(None);
            };
            Ok(Some(Measure {
                head_depth: history.count_commits(&merge_base, &tips[branch])?,
                other_depth: history.count_commits(&merge_base, &tips[candidate])?,
                merge_base,
            }))
        },
    )
}

/// `None` means the dump is over budget (or unusable); caller falls back.
fn infer_from_dump<H: History>(
    history: &H,
    branches: &[&str],
    default_branch: &str,
    skip: &HashSet<&str>,
    budget: MemoryBudget,
) -> Result<Option<InferredParents>> {
    // `branches` come first, so branch `i` is `dump_refs[i]`.
    let dump_refs = dump_refs(branches, default_branch);
    let tip_shas = history.resolve_branch_tips(&dump_refs)?;
    let tips: Vec<&str> = tip_shas.iter().map(String::as_str).collect();
    let floor = history.octopus_merge_base(&tips)?;
    let dump = history.rev_list_parents(
        &tips,
        floor.as_deref(),
        budget.max_commits.saturating_add(1),
    )?;
    let Some(graph) = CommitGraph::parse(&dump, dump_refs.len(), budget) else {
        return Ok(None);
    };
    let floor_idx = match &floor {
        Some(sha) => match graph.index.get(sha.as_str()) {
            Some(&idx) => Some(idx),
            None => return Ok(None),
        },
        None => None,
    };
    let mut tip_idx = Vec::with_capacity(branches.len());
    for sha in &tip_shas[..branches.len()] {
        let Some(&idx) = graph.index.get(sha.as_str()) else {
            return Ok(None);
        };
        tip_idx.push(idx);
    }

    let mut pairs = DumpPairs::new(history, graph, floor_idx, tip_idx, &tip_shas);
    let inferred = rank_parents(
        history,
        branches,
        default_branch,
        skip,
        |branch, candidate| pairs.measure(branch, candidate),
    )?;
    Ok(Some(inferred))
}

/// How far `head` and `other` sit from their merge-base.
#[derive(Clone)]
struct Measure {
    merge_base: String,
    head_depth: usize,
    other_depth: usize,
}

impl Measure {
    fn flipped(&self) -> Self {
        Self {
            merge_base: self.merge_base.clone(),
            head_depth: self.other_depth,
            other_depth: self.head_depth,
        }
    }
}

/// Rank every candidate for every non-skipped branch. `measure(head, other)`
/// takes positions in `branches`; each unordered pair is measured once.
fn rank_parents<H, F>(
    history: &H,
    branches: &[&str],
    default_branch: &str,
    skip: &HashSet<&str>,
    mut measure: F,
) -> Result<InferredParents>
where
    H: History,
    F: FnMut(usize, usize) -> Result<Option<Measure>>,
{
    let mut parents = HashMap::new();
    let mut equidistant = Vec::new();
    // Keyed by (low, high) position, stored from the low branch's side.
    let mut measured: HashMap<(usize, usize), Option<Measure>> = HashMap::new();

    for (branch_pos, branch) in branches.iter().enumerate() {
        if *branch == default_branch || skip.contains(branch) {
            continue;
        }

        let mut ranked = Vec::new();
        for (candidate_pos, candidate) in branches.iter().enumerate() {
            if candidate_pos == branch_pos {
                continue;
            }
            let key = (branch_pos.min(candidate_pos), branch_pos.max(candidate_pos));
            let low_side = match measured.entry(key) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let result = measure(branch_pos, candidate_pos)?;
                    entry.insert(if branch_pos < candidate_pos {
                        result
                    } else {
                        result.map(|m| m.flipped())
                    })
                }
            };
            let Some(low_side) = low_side else {
                continue;
            };
            let m = if branch_pos < candidate_pos {
                low_side.clone()
            } else {
                low_side.flipped()
            };
            if m.head_depth == 0 {
                continue;
            }
            ranked.push(RankedParent {
                name: (*candidate).to_string(),
                merge_base: m.merge_base,
                branch_depth: m.head_depth,
                candidate_depth: m.other_depth,
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

fn dump_refs<'a>(branches: &'a [&'a str], default_branch: &'a str) -> Vec<&'a str> {
    let mut refs = branches.to_vec();
    if !refs.contains(&default_branch) {
        refs.push(default_branch);
    }
    refs
}

/// Merge-base and commits-ahead for branch pairs, read off the dump.
struct DumpPairs<'h, 'd, H> {
    history: &'h H,
    graph: CommitGraph<'d>,
    /// Dump position of the floor commit, when the dump was cut at one.
    floor: Option<usize>,
    /// Per branch: dump position, tip SHA, ancestor bitset, bitset popcount.
    tips: Vec<BranchTip>,
    /// Per merge-base chosen by git: (commits reachable from it, floor among them).
    reach: HashMap<usize, (usize, bool)>,
}

struct BranchTip {
    idx: usize,
    sha: String,
    ancestors: Bitset,
    ancestor_count: usize,
}

impl<'h, 'd, H: History> DumpPairs<'h, 'd, H> {
    fn new(
        history: &'h H,
        graph: CommitGraph<'d>,
        floor: Option<usize>,
        tip_idx: Vec<usize>,
        tip_shas: &[String],
    ) -> Self {
        let tips = tip_idx
            .into_iter()
            .zip(tip_shas)
            .map(|(idx, sha)| {
                let ancestors = graph.ancestor_bits(idx);
                BranchTip {
                    idx,
                    sha: sha.clone(),
                    ancestor_count: ancestors.count(),
                    ancestors,
                }
            })
            .collect();
        Self {
            history,
            graph,
            floor,
            tips,
            reach: HashMap::new(),
        }
    }

    fn measure(&mut self, head: usize, other: usize) -> Result<Option<Measure>> {
        let (h, o) = (&self.tips[head], &self.tips[other]);
        if h.idx == o.idx {
            return Ok(Some(Measure {
                merge_base: h.sha.clone(),
                head_depth: 0,
                other_depth: 0,
            }));
        }
        let bases = self
            .graph
            .maximal_common_ancestors(h.idx, &h.ancestors, o.idx, &o.ancestors);
        match bases.as_slice() {
            [] => Ok(None),
            [only] => {
                // One best base means every common ancestor sits under it, so
                // the common set *is* its history and each side's distance is
                // what the other side's bitset does not cover.
                let common = h.ancestors.and_count(&o.ancestors);
                Ok(Some(Measure {
                    merge_base: self.graph.shas[*only].to_string(),
                    head_depth: h.ancestor_count - common,
                    other_depth: o.ancestor_count - common,
                }))
            }
            _ => self.measure_criss_cross(head, other).map(Some),
        }
    }

    /// Several best bases: git names the one the old pairwise code used, and
    /// the depths are measured from that base.
    fn measure_criss_cross(&mut self, head: usize, other: usize) -> Result<Measure> {
        let (h_sha, o_sha) = (self.tips[head].sha.clone(), self.tips[other].sha.clone());
        let Some(merge_base) = self.history.merge_base(&o_sha, &h_sha)? else {
            bail!("git found no merge-base for {h_sha} and {o_sha}, but the dump has common ancestors");
        };
        let (h, o) = (&self.tips[head], &self.tips[other]);
        let base = self.graph.index.get(merge_base.as_str()).copied();
        let reach = match base {
            Some(base) => Some(
                *self
                    .reach
                    .entry(base)
                    .or_insert_with(|| self.graph.reach_from(base, self.floor)),
            ),
            None => None,
        };
        match reach {
            // Everything below the floor is under this base too, so counting
            // only the dump's commits misses nothing.
            Some((reachable, covers_floor)) if self.floor.is_none() || covers_floor => {
                Ok(Measure {
                    merge_base,
                    head_depth: h.ancestor_count - reachable,
                    other_depth: o.ancestor_count - reachable,
                })
            }
            // This base does not sit on the floor, so commits below the floor
            // can be ahead of it and the dump cannot see them.
            _ => Ok(Measure {
                head_depth: self.history.count_commits(&merge_base, &h_sha)?,
                other_depth: self.history.count_commits(&merge_base, &o_sha)?,
                merge_base,
            }),
        }
    }
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

/// Pick a branch's parent in two tiers.
///
/// 1. True ancestors (`candidate_depth == 0`, so the merge-base is the
///    candidate's tip): branches whose tip is reachable from the branch. The
///    closest one (smallest `branch_depth`) wins.
/// 2. Diverged candidates, only when there is no true ancestor: smallest
///    `branch_depth`, then smallest `candidate_depth`.
///
/// Tier 1 always beats tier 2. That is what prevents parent cycles: if B
/// descends from C, C is a true ancestor of B and wins over a sibling that only
/// shares a fork point. It also means a sibling forked from the parent's tip
/// (same `branch_depth`, same merge-base commit) cannot steal the child on list
/// order; only `candidate_depth == 0` keeps the parent. Equal ranks keep the
/// candidate whose merge-base committer timestamp is newest.
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

    fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    fn and_count(&self, other: &Self) -> usize {
        self.words
            .iter()
            .zip(&other.words)
            .map(|(a, b)| (a & b).count_ones() as usize)
            .sum()
    }
}

/// Parent/child edges of the dumped commits, borrowing SHAs from the dump text.
struct CommitGraph<'d> {
    index: HashMap<&'d str, usize>,
    shas: Vec<&'d str>,
    parents: Vec<Vec<usize>>,
    children: Vec<Vec<usize>>,
    /// Whether the dump gave the commit its own line. A commit that only ever
    /// appears as someone's parent is *dangling*: `^floor^@` cut it off, so it
    /// is an ancestor of the floor and common to every tip. It never counts as
    /// a commit ahead and never takes part in a bitset.
    listed: Vec<bool>,
}

/// Position of `sha`, adding it when new. `None` once the next commit would
/// exceed [`MemoryBudget`].
fn intern<'d>(
    index: &mut HashMap<&'d str, usize>,
    shas: &mut Vec<&'d str>,
    parents: &mut Vec<Vec<usize>>,
    sha: &'d str,
    tips: usize,
    budget: MemoryBudget,
) -> Option<usize> {
    if let Some(&existing) = index.get(sha) {
        return Some(existing);
    }
    let next = shas.len() + 1;
    if next > budget.max_commits || bitset_bytes(tips, next) > budget.max_bitset_bytes {
        return None;
    }
    let idx = shas.len();
    index.insert(sha, idx);
    shas.push(sha);
    parents.push(Vec::new());
    Some(idx)
}

impl<'d> CommitGraph<'d> {
    fn parse(dump: &'d str, tips: usize, budget: MemoryBudget) -> Option<Self> {
        let mut index = HashMap::new();
        let mut shas = Vec::new();
        let mut parents: Vec<Vec<usize>> = Vec::new();
        let mut listed: Vec<bool> = Vec::new();

        for line in dump.lines() {
            let mut parts = line.split_whitespace();
            let Some(sha) = parts.next() else {
                continue;
            };
            let idx = intern(&mut index, &mut shas, &mut parents, sha, tips, budget)?;
            listed.resize(shas.len(), false);
            listed[idx] = true;
            for parent in parts {
                let parent = intern(&mut index, &mut shas, &mut parents, parent, tips, budget)?;
                parents[idx].push(parent);
            }
        }
        listed.resize(shas.len(), false);

        let mut children = vec![Vec::new(); parents.len()];
        for (child, listed) in parents.iter().enumerate() {
            for &parent in listed {
                children[parent].push(child);
            }
        }

        Some(Self {
            index,
            shas,
            parents,
            children,
            listed,
        })
    }

    fn ancestor_bits(&self, tip: usize) -> Bitset {
        let mut bits = Bitset::new(self.shas.len());
        let mut stack = vec![tip];
        while let Some(i) = stack.pop() {
            if bits.get(i) || !self.listed[i] {
                continue;
            }
            bits.set(i);
            stack.extend_from_slice(&self.parents[i]);
        }
        bits
    }

    /// Listed commits reachable from `start` (itself included), and whether
    /// `target` is one of them.
    fn reach_from(&self, start: usize, target: Option<usize>) -> (usize, bool) {
        let mut seen = vec![false; self.shas.len()];
        let mut stack = vec![start];
        let mut count = 0;
        while let Some(i) = stack.pop() {
            if seen[i] || !self.listed[i] {
                continue;
            }
            seen[i] = true;
            count += 1;
            stack.extend_from_slice(&self.parents[i]);
        }
        (count, target.is_some_and(|t| seen[t]))
    }

    /// Best merge-bases of two tips: the common ancestors with no common
    /// descendant. Work scales with the commits unique to tip `a`, not with
    /// the history both share.
    fn maximal_common_ancestors(
        &self,
        a: usize,
        a_bits: &Bitset,
        b: usize,
        b_bits: &Bitset,
    ) -> Vec<usize> {
        // One tip is an ancestor of the other: it is the only best base.
        if b_bits.get(a) {
            return vec![a];
        }
        if a_bits.get(b) {
            return vec![b];
        }
        // The first common commit on every path down from `a` is a candidate,
        // and every best base is the first common commit on some path.
        let mut seen = HashSet::new();
        let mut stack = vec![a];
        let mut candidates = Vec::new();
        while let Some(i) = stack.pop() {
            if !seen.insert(i) {
                continue;
            }
            for &parent in &self.parents[i] {
                if b_bits.get(parent) {
                    candidates.push(parent);
                } else {
                    stack.push(parent);
                }
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        // The common set is closed under ancestry, so a candidate has a common
        // descendant exactly when it has a common child.
        candidates.retain(|&c| {
            !self.children[c]
                .iter()
                .any(|&child| a_bits.get(child) && b_bits.get(child))
        });
        candidates
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let graph = CommitGraph::parse("c1 p1 p2\np1 root\nroot\n", 1, MemoryBudget::default())
            .expect("dump fits the budget");
        assert_eq!(graph.shas.len(), 4); // c1, p1, p2 (dangling), root
        assert!(graph.index.contains_key("p2"));
        assert!(graph.parents[graph.index["p2"]].is_empty());
        assert!(!graph.listed[graph.index["p2"]]);
        assert!(graph.listed[graph.index["c1"]] && graph.listed[graph.index["root"]]);
    }

    #[test]
    fn parse_returns_none_once_the_next_commit_exceeds_budget() {
        let dump = "c1\nroot\n";
        let over = MemoryBudget {
            max_commits: 1,
            max_bitset_bytes: MAX_BITSET_BYTES,
        };
        assert!(CommitGraph::parse(dump, 2, over).is_none());
        let graph = CommitGraph::parse(dump, 2, MemoryBudget::default()).expect("within budget");
        assert_eq!(graph.shas.len(), 2);
    }

    struct FakeHistory {
        names: HashMap<String, String>,
        floor: Option<String>,
        dump: String,
        /// Looked up as `(other, head)`, the order `DumpPairs` asks git in.
        merge_bases: HashMap<(String, String), Option<String>>,
        timestamps: HashMap<String, i64>,
        counts: HashMap<(String, String), usize>,
        merge_base_calls: std::cell::Cell<usize>,
    }

    impl FakeHistory {
        fn new(names: &[(&str, &str)], dump: &str) -> Self {
            Self {
                names: names
                    .iter()
                    .map(|(name, sha)| ((*name).into(), (*sha).into()))
                    .collect(),
                floor: None,
                dump: dump.to_string(),
                merge_bases: HashMap::new(),
                timestamps: HashMap::new(),
                counts: HashMap::new(),
                merge_base_calls: std::cell::Cell::new(0),
            }
        }
    }

    impl History for FakeHistory {
        fn resolve_branch_tips(&self, branches: &[&str]) -> Result<Vec<String>> {
            branches
                .iter()
                .map(|branch| {
                    self.names
                        .get(*branch)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("unknown branch {branch}"))
                })
                .collect()
        }

        fn octopus_merge_base(&self, _tips: &[&str]) -> Result<Option<String>> {
            Ok(self.floor.clone())
        }

        fn rev_list_parents(
            &self,
            _tips: &[&str],
            floor: Option<&str>,
            _max_lines: usize,
        ) -> Result<String> {
            assert_eq!(floor, self.floor.as_deref());
            Ok(self.dump.clone())
        }

        fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
            self.merge_base_calls.set(self.merge_base_calls.get() + 1);
            match self.merge_bases.get(&(a.to_string(), b.to_string())) {
                Some(found) => Ok(found.clone()),
                None => bail!("no merge-base scripted for ({a}, {b})"),
            }
        }

        fn count_commits(&self, base: &str, head: &str) -> Result<usize> {
            match self.counts.get(&(base.to_string(), head.to_string())) {
                Some(count) => Ok(*count),
                None => bail!("count_commits({base}, {head}) should not run on the dump path"),
            }
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
    fn criss_cross_asks_git_once_for_that_pair_only() {
        // root ← a, root ← b; left and right both merge a+b (incomparable bases).
        let dump = "\
leftsha asha bsha
rightsha bsha asha
asha root
bsha root
root
";
        let mut history = FakeHistory::new(
            &[("main", "root"), ("left", "leftsha"), ("right", "rightsha")],
            dump,
        );
        history
            .merge_bases
            .insert(("rightsha".into(), "leftsha".into()), Some("asha".into()));
        let branches = ["main", "left", "right"];
        let skip = HashSet::new();
        let inferred =
            infer_parents(&history, &branches, "main", &skip, MemoryBudget::default()).unwrap();

        // (left, right) is measured once, not once per direction.
        assert_eq!(history.merge_base_calls.get(), 1);
        assert_eq!(
            inferred.parents.get("left").map(String::as_str),
            Some("main")
        );
        assert_eq!(
            inferred.parents.get("right").map(String::as_str),
            Some("main")
        );
    }

    fn assert_parent(repo: &Path, branches: &[&str], child: &str, parent: &str) {
        let git_repo = Repository::at(repo).unwrap();
        let skip = HashSet::new();
        let dump =
            infer_parents(&git_repo, branches, "main", &skip, MemoryBudget::default()).unwrap();
        let pair = infer_parents_pairwise(&git_repo, branches, "main", &skip).unwrap();
        assert_eq!(dump.parents, pair.parents);
        assert_eq!(dump.parents.get(child).map(String::as_str), Some(parent));
    }

    #[test]
    fn sibling_at_parent_tip_does_not_steal_child() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        commit(&repo, "main.txt", "main");
        git(&repo, &["checkout", "-b", "pr1"]);
        commit(&repo, "pr1.txt", "pr1");
        git(&repo, &["checkout", "-b", "pr2"]);
        commit(&repo, "pr2.txt", "pr2");
        git(&repo, &["checkout", "pr1"]);
        git(&repo, &["checkout", "-b", "side"]);
        commit(&repo, "side.txt", "side");

        assert_parent(&repo, &["main", "side", "pr1", "pr2"], "pr2", "pr1");
    }

    #[test]
    fn closer_diverged_sibling_loses_to_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        commit(&repo, "root.txt", "root");
        commit(&repo, "main.txt", "main");
        git(&repo, &["checkout", "-b", "topic"]);
        commit(&repo, "t1.txt", "t1");
        commit(&repo, "t2.txt", "t2");
        commit(&repo, "t3.txt", "t3");
        git(&repo, &["checkout", "-b", "side", "HEAD~1"]);
        commit(&repo, "side.txt", "side");

        assert_parent(&repo, &["side", "main", "topic"], "topic", "main");
    }

    #[test]
    fn diverged_branch_prefers_closer_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        commit(&repo, "root.txt", "root");
        git(&repo, &["checkout", "-b", "a"]);
        commit(&repo, "a1.txt", "a1");
        commit(&repo, "a2.txt", "a2");
        git(&repo, &["checkout", "main"]);
        commit(&repo, "main.txt", "main");
        git(&repo, &["checkout", "-b", "b", "HEAD~1"]);
        commit(&repo, "b.txt", "b");

        assert_parent(&repo, &["a", "main", "b"], "b", "main");
    }

    fn criss_cross_history(child_cross_base: &str) -> FakeHistory {
        let dump = "\
child basenear basefar
cross basefar basenear
basenear extra root
basefar root
extra root
rival r1
r1 basefar
main m1
m1 root
root
";
        let mut history = FakeHistory::new(
            &[
                ("main", "main"),
                ("child", "child"),
                ("cross", "cross"),
                ("rival", "rival"),
            ],
            dump,
        );
        history.merge_bases.insert(
            ("cross".into(), "child".into()),
            Some(child_cross_base.into()),
        );
        history
    }

    #[test]
    fn criss_cross_merge_base_sha_changes_winner() {
        let branches = ["main", "child", "cross", "rival"];
        let skip = HashSet::new();

        let basenear = criss_cross_history("basenear");
        let near =
            infer_parents(&basenear, &branches, "main", &skip, MemoryBudget::default()).unwrap();
        assert!(basenear.merge_base_calls.get() > 0);
        assert_eq!(near.parents.get("child").map(String::as_str), Some("cross"));

        let basefar = criss_cross_history("basefar");
        let far =
            infer_parents(&basefar, &branches, "main", &skip, MemoryBudget::default()).unwrap();
        assert!(basefar.merge_base_calls.get() > 0);
        assert_eq!(far.parents.get("child").map(String::as_str), Some("rival"));
    }

    /// X forks below the floor and merges main back in, so its fork commit's
    /// parent sits under the floor in X's history only. That parent is common
    /// to every tip and must not be counted as one of X's own commits.
    #[test]
    fn fork_below_the_floor_is_not_counted_ahead() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        commit(&repo, "root.txt", "root");
        commit(&repo, "m1.txt", "m1");
        let m1 = git(&repo, &["rev-parse", "HEAD"]);
        commit(&repo, "m2.txt", "m2");
        let m2 = git(&repo, &["rev-parse", "HEAD"]);
        commit(&repo, "m3.txt", "m3");
        git(&repo, &["checkout", "-b", "X", &m1]);
        commit(&repo, "x1.txt", "x1");
        git(&repo, &["branch", "xside", &m2]);
        git(&repo, &["merge", "--no-ff", "--no-edit", "xside"]);
        git(&repo, &["branch", "-D", "xside"]);
        git(&repo, &["merge", "--no-ff", "--no-edit", "main"]);
        git(&repo, &["checkout", "-b", "B", "main"]);
        for i in 1..=5 {
            commit(&repo, &format!("b{i}.txt"), &format!("b{i}"));
        }
        git(&repo, &["checkout", "main"]);
        for i in 1..=4 {
            commit(&repo, &format!("a{i}.txt"), &format!("a{i}"));
        }

        // X is 3 ahead of main, main is 4 ahead of X's merge: X is B's parent.
        assert_parent(&repo, &["main", "X", "B"], "B", "X");

        let git_repo = Repository::at(&repo).unwrap();
        let recording = Recording::new(&git_repo);
        let skip = HashSet::new();
        infer_parents(
            &recording,
            &["main", "X", "B"],
            "main",
            &skip,
            MemoryBudget::default(),
        )
        .unwrap();
        // Dangling nodes under the floor are not best merge-bases either.
        assert_eq!(recording.pair_git_calls.get(), 0);
    }

    /// Delegates to a real repository and records what the inference asked of git.
    struct Recording<'a> {
        repo: &'a Repository,
        pair_git_calls: std::cell::Cell<usize>,
        dump_lines: std::cell::Cell<usize>,
        dump_cap: std::cell::Cell<usize>,
    }

    impl<'a> Recording<'a> {
        fn new(repo: &'a Repository) -> Self {
            Self {
                repo,
                pair_git_calls: std::cell::Cell::new(0),
                dump_lines: std::cell::Cell::new(0),
                dump_cap: std::cell::Cell::new(0),
            }
        }
    }

    impl History for Recording<'_> {
        fn resolve_branch_tips(&self, branches: &[&str]) -> Result<Vec<String>> {
            self.repo.resolve_branch_tips(branches)
        }

        fn octopus_merge_base(&self, tips: &[&str]) -> Result<Option<String>> {
            self.repo.octopus_merge_base(tips)
        }

        fn rev_list_parents(
            &self,
            tips: &[&str],
            floor: Option<&str>,
            max_lines: usize,
        ) -> Result<String> {
            let dump = self.repo.rev_list_parents(tips, floor, max_lines)?;
            self.dump_lines.set(dump.lines().count());
            self.dump_cap.set(max_lines);
            Ok(dump)
        }

        fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>> {
            self.pair_git_calls.set(self.pair_git_calls.get() + 1);
            History::merge_base(self.repo, a, b)
        }

        fn count_commits(&self, base: &str, head: &str) -> Result<usize> {
            self.pair_git_calls.set(self.pair_git_calls.get() + 1);
            History::count_commits(self.repo, base, head)
        }

        fn commit_details_many(
            &self,
            shas: &[&str],
        ) -> Result<HashMap<String, (String, i64, String)>> {
            self.repo.commit_details_many(shas)
        }
    }

    /// `n` linear commits on `main`, written with one `git fast-import`.
    fn deep_main(dir: &Path, n: usize) {
        use std::io::Write;
        let mut stream = String::new();
        for i in 0..n {
            let from = if i == 0 {
                String::new()
            } else {
                format!("from :{i}\n")
            };
            stream.push_str(&format!(
                "commit refs/heads/main\nmark :{}\ncommitter T <t@t> {} +0000\ndata 2\nc\n{from}\n",
                i + 1,
                1_600_000_000 + i
            ));
        }
        let mut child = Command::new("git")
            .args(["fast-import", "--quiet"])
            .current_dir(dir)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stream.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
        git(dir, &["checkout", "-f", "main"]);
    }

    #[test]
    fn dump_stops_at_the_stack_floor_however_deep_main_is() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        deep_main(&repo, 400);
        git(&repo, &["checkout", "-b", "pr1"]);
        commit(&repo, "pr1.txt", "pr1");
        git(&repo, &["checkout", "-b", "pr2"]);
        commit(&repo, "pr2.txt", "pr2");
        git(&repo, &["checkout", "main"]);
        git(&repo, &["checkout", "-b", "side"]);
        commit(&repo, "side.txt", "side");

        let git_repo = Repository::at(&repo).unwrap();
        let recording = Recording::new(&git_repo);
        let branches = ["main", "pr1", "pr2", "side"];
        let skip = HashSet::new();
        let inferred = infer_parents(
            &recording,
            &branches,
            "main",
            &skip,
            MemoryBudget::default(),
        )
        .unwrap();
        let pair = infer_parents_pairwise(&git_repo, &branches, "main", &skip).unwrap();

        assert_eq!(inferred.parents, pair.parents);
        assert_eq!(inferred.parents.get("pr2").map(String::as_str), Some("pr1"));
        // main's tip, three branch commits, and main's parent as the dangling edge.
        assert!(
            recording.dump_lines.get() <= 5,
            "{}",
            recording.dump_lines.get()
        );
        assert_eq!(recording.pair_git_calls.get(), 0);
    }

    #[test]
    fn main_ahead_of_the_stack_still_counts_its_new_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        deep_main(&repo, 50);
        git(&repo, &["checkout", "-b", "pr1"]);
        commit(&repo, "pr1.txt", "pr1");
        git(&repo, &["checkout", "-b", "pr2"]);
        commit(&repo, "pr2.txt", "pr2");
        git(&repo, &["checkout", "main"]);
        for i in 0..6 {
            commit(&repo, &format!("m{i}.txt"), "m");
        }

        assert_parent(&repo, &["main", "pr1", "pr2"], "pr1", "main");
        assert_parent(&repo, &["main", "pr1", "pr2"], "pr2", "pr1");
    }

    #[test]
    fn orphan_branch_leaves_no_floor_and_still_matches_pairwise() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        commit(&repo, "main.txt", "main");
        git(&repo, &["checkout", "-b", "pr1"]);
        commit(&repo, "pr1.txt", "pr1");
        git(&repo, &["checkout", "--orphan", "island"]);
        commit(&repo, "island.txt", "island");

        let git_repo = Repository::at(&repo).unwrap();
        let tips = git_repo
            .resolve_branch_tips(&["main", "pr1", "island"])
            .unwrap();
        let tips: Vec<&str> = tips.iter().map(String::as_str).collect();
        assert_eq!(git_repo.octopus_merge_base(&tips).unwrap(), None);
        assert_parent(&repo, &["main", "pr1", "island"], "pr1", "main");
    }

    #[test]
    fn tag_named_like_a_branch_does_not_force_the_pairwise_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = stacked_repo(tmp.path());
        // An annotated tag called `pr1` on main; `rev-parse pr1` would return it.
        git(&repo_path, &["tag", "-a", "-m", "tag", "pr1", "main"]);
        let repo = Repository::at(&repo_path).unwrap();
        let branches = ["main", "pr1", "pr2", "pr3", "feature"];
        let skip = HashSet::new();

        let recording = Recording::new(&repo);
        let dump = infer_parents(
            &recording,
            &branches,
            "main",
            &skip,
            MemoryBudget::default(),
        )
        .unwrap();
        assert_eq!(recording.pair_git_calls.get(), 0, "fell back to pairwise");
        assert_eq!(dump.parents.get("pr2").map(String::as_str), Some("pr1"));

        let pair = infer_parents_pairwise(&repo, &branches, "main", &skip).unwrap();
        assert_eq!(pair.parents, dump.parents);
    }

    #[test]
    fn in_budget_dump_never_asks_git_about_pairs_and_over_budget_always_does() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_path = stacked_repo(tmp.path());
        let repo = Repository::at(&repo_path).unwrap();
        let branches = ["main", "pr1", "pr2", "pr3", "feature"];
        let skip = HashSet::new();

        let within = Recording::new(&repo);
        infer_parents(&within, &branches, "main", &skip, MemoryBudget::default()).unwrap();
        assert_eq!(within.pair_git_calls.get(), 0);

        let tight = MemoryBudget {
            max_commits: 3,
            max_bitset_bytes: MAX_BITSET_BYTES,
        };
        let over = Recording::new(&repo);
        infer_parents(&over, &branches, "main", &skip, tight).unwrap();
        assert!(over.pair_git_calls.get() > 0);
        // The walk is cut one line past the cap, so it cannot run away.
        assert_eq!(over.dump_cap.get(), 4);
        assert!(over.dump_lines.get() <= 4);
    }

    /// Criss-cross whose best bases `a` and `b` differ in whether they sit on
    /// the floor `f`: only `a` descends from it. `b` is a separate root.
    fn floor_history() -> FakeHistory {
        let dump = "\
l a b
r b a
a f
b q
f p
q
";
        let mut history = FakeHistory::new(&[("main", "f"), ("left", "l"), ("right", "r")], dump);
        history.floor = Some("f".into());
        history
    }

    fn measure_left_right(history: &FakeHistory) -> Measure {
        let graph = CommitGraph::parse(&history.dump, 3, MemoryBudget::default()).unwrap();
        let floor = graph.index.get("f").copied();
        let tip_shas: Vec<String> = ["f", "l", "r"].map(String::from).to_vec();
        let tip_idx = tip_shas
            .iter()
            .map(|sha| graph.index[sha.as_str()])
            .collect();
        let mut pairs = DumpPairs::new(history, graph, floor, tip_idx, &tip_shas);
        pairs
            .measure(1, 2)
            .unwrap()
            .expect("left and right share ancestors")
    }

    #[test]
    fn criss_cross_base_on_the_floor_is_measured_from_the_dump() {
        let mut history = floor_history();
        history
            .merge_bases
            .insert(("r".into(), "l".into()), Some("a".into()));
        let m = measure_left_right(&history);
        // `l` reaches 6 dump commits; `a` reaches a, f and p.
        assert_eq!(
            (m.merge_base.as_str(), m.head_depth, m.other_depth),
            ("a", 3, 3)
        );
    }

    #[test]
    fn criss_cross_base_off_the_floor_is_counted_by_git() {
        let mut history = floor_history();
        history
            .merge_bases
            .insert(("r".into(), "l".into()), Some("b".into()));
        // Commits under `f` are ahead of `b` but not in the dump; git sees them.
        history.counts.insert(("b".into(), "l".into()), 5);
        history.counts.insert(("b".into(), "r".into()), 5);
        let m = measure_left_right(&history);
        assert_eq!(
            (m.merge_base.as_str(), m.head_depth, m.other_depth),
            ("b", 5, 5)
        );
    }
}
