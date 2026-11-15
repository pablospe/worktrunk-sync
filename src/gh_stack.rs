//! Read-only interop with [gh-stack](https://github.com/github/gh-stack).
//!
//! gh-stack keeps the stacks a user registered (`gh stack init` / `add`) in a
//! JSON catalog at `<git-common-dir>/gh-stack`. Its declared order is used as
//! parent hints: a declared parent beats the stack file and inference. The
//! catalog is never written, and gh-stack itself is never run except for an
//! installation check.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

/// Catalog file name inside the git common directory.
const CATALOG_FILE: &str = "gh-stack";

/// The only catalog schema this reader understands.
const SCHEMA_VERSION: u32 = 1;

/// Journals gh-stack leaves behind while a rebase or modify is paused.
const PAUSED_STATE_FILES: [(&str, &str); 2] = [
    ("gh-stack-rebase-state", "rebase"),
    ("gh-stack-modify-state", "modify"),
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogFile {
    schema_version: u32,
    #[serde(default)]
    stacks: Vec<CatalogStack>,
}

#[derive(Deserialize)]
struct CatalogStack {
    trunk: CatalogBranch,
    #[serde(default)]
    branches: Vec<CatalogBranch>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogBranch {
    branch: String,
    pull_request: Option<CatalogPullRequest>,
}

#[derive(Deserialize)]
struct CatalogPullRequest {
    #[serde(default)]
    merged: bool,
}

/// What reading the catalog produced.
pub enum Catalog {
    /// No catalog file: gh-stack was never used in this clone.
    Absent,
    /// A catalog this reader can't use (unknown schema or invalid JSON).
    Unsupported(String),
    /// Declared parents: branch -> parent, merged branches already skipped.
    Parents(HashMap<String, String>),
}

/// Read the catalog from `common_dir`.
pub fn read_catalog(common_dir: &Path) -> Catalog {
    let path = common_dir.join(CATALOG_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => parse_catalog(&content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Catalog::Absent,
        Err(e) => Catalog::Unsupported(format!("cannot read {}: {e}", path.display())),
    }
}

fn parse_catalog(content: &str) -> Catalog {
    let file: CatalogFile = match serde_json::from_str(content) {
        Ok(file) => file,
        Err(e) => return Catalog::Unsupported(format!("invalid catalog: {e}")),
    };
    if file.schema_version != SCHEMA_VERSION {
        return Catalog::Unsupported(format!(
            "catalog schema version {} (only {SCHEMA_VERSION} is supported)",
            file.schema_version
        ));
    }

    // Each branch's parent is the nearest unmerged branch below it, or the
    // stack's trunk. Merged branches get no entry: they are integrated, and
    // their children skip over them.
    let mut parents = HashMap::new();
    for stack in file.stacks {
        let mut below = stack.trunk.branch;
        for branch in stack.branches {
            if branch.pull_request.is_some_and(|pr| pr.merged) {
                continue;
            }
            // gh-stack keeps a branch in one stack; if a hand-edited catalog
            // lists it twice, the first stack wins.
            parents
                .entry(branch.branch.clone())
                .or_insert_with(|| below.clone());
            below = branch.branch;
        }
    }
    Catalog::Parents(parents)
}

/// The paused gh-stack operation in `common_dir`, if any ("rebase" or "modify").
pub fn paused_operation(common_dir: &Path) -> Option<&'static str> {
    PAUSED_STATE_FILES
        .iter()
        .find(|(file, _)| common_dir.join(file).exists())
        .map(|&(_, op)| op)
}

/// Whether the `gh stack` extension is installed and runs.
pub fn is_installed() -> bool {
    Command::new("gh")
        .args(["stack", "--version"])
        // Don't start gh-stack's background release check.
        .env("GH_STACK_NO_UPDATE_NOTIFIER", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parents(json: &str) -> HashMap<String, String> {
        match parse_catalog(json) {
            Catalog::Parents(parents) => parents,
            Catalog::Unsupported(why) => panic!("unexpected unsupported catalog: {why}"),
            Catalog::Absent => unreachable!(),
        }
    }

    #[test]
    fn linear_stack_chains_onto_trunk() {
        let p = parents(
            r#"{"schemaVersion":1,"stacks":[{"trunk":{"branch":"main"},
                "branches":[{"branch":"a"},{"branch":"b"},{"branch":"c"}]}]}"#,
        );
        assert_eq!(p["a"], "main");
        assert_eq!(p["b"], "a");
        assert_eq!(p["c"], "b");
    }

    #[test]
    fn merged_branches_are_skipped() {
        let p = parents(
            r#"{"schemaVersion":1,"stacks":[{"trunk":{"branch":"main"},
                "branches":[{"branch":"a","pullRequest":{"number":1,"merged":true}},
                            {"branch":"b","pullRequest":{"number":2}},
                            {"branch":"c"}]}]}"#,
        );
        assert!(!p.contains_key("a"));
        assert_eq!(p["b"], "main");
        assert_eq!(p["c"], "b");
    }

    #[test]
    fn custom_trunk_is_kept() {
        let p = parents(
            r#"{"schemaVersion":1,"stacks":[{"trunk":{"branch":"develop"},
                "branches":[{"branch":"a"}]}]}"#,
        );
        assert_eq!(p["a"], "develop");
    }

    #[test]
    fn unknown_schema_or_bad_json_is_unsupported() {
        assert!(matches!(
            parse_catalog(r#"{"schemaVersion":2,"stacks":[]}"#),
            Catalog::Unsupported(_)
        ));
        assert!(matches!(parse_catalog("not json"), Catalog::Unsupported(_)));
    }

    #[test]
    fn paused_operation_detects_journals() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(paused_operation(dir.path()), None);
        std::fs::write(dir.path().join("gh-stack-modify-state"), "{}").unwrap();
        assert_eq!(paused_operation(dir.path()), Some("modify"));
    }
}
