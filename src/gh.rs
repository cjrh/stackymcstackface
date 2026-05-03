//! Thin wrapper over the `gh` CLI for the bits `stack` needs.
//!
//! Like `git.rs`, this module exposes a tiny strongly-typed surface so the
//! orchestration layer never has to deal with subprocess plumbing or JSON
//! shape. Each public call corresponds to exactly one `gh` invocation.

use std::process::Command;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

/// Information about a repository hosted on GitHub, scoped to what `stack`
/// needs to pick the right merge target.
#[derive(Debug, Clone)]
pub struct RepoInfo {
    /// `owner/name`, e.g. `octocat/hello-world`.
    pub name_with_owner: String,
    /// The repo's default branch name (`main`, `master`, ...).
    pub default_branch: String,
    /// True when this repo is a fork of another.
    pub is_fork: bool,
    /// `owner/name` of the parent when `is_fork` is true.
    pub parent_name_with_owner: Option<String>,
    /// True if "Automatically delete head branches" is enabled on the repo.
    /// When false, dependent stack PRs do not auto-retarget after the
    /// parent merges, because GitHub only retargets when the head branch
    /// is deleted through the merge flow.
    pub delete_branch_on_merge: bool,
}

/// One open pull request, as far as `stack` cares.
#[derive(Debug, Clone)]
pub struct OpenPr {
    pub number: u64,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub base_ref_name: String,
    pub head_repository_owner: String,
    pub url: String,
    pub is_cross_repository: bool,
}

/// Fetch info for a repo. Pass `None` to use whatever `gh` infers from the
/// current working directory's git remotes.
///
/// `--json` fields used (run `gh repo view --json '' 2>&1` to see the full
/// list `gh` accepts):
/// - `nameWithOwner`        — `owner/name`, the canonical id used everywhere
/// - `defaultBranchRef`     — has a `name` field; the repo's default branch
/// - `isFork`               — true when this repo was forked from another
/// - `parent`               — only populated when `isFork`; has `nameWithOwner`
/// - `deleteBranchOnMerge`  — controls whether stacked-PR auto-retargeting works
pub fn repo_info(name_with_owner: Option<&str>) -> Result<RepoInfo> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
        #[serde(rename = "defaultBranchRef")]
        default_branch_ref: DefaultBranchRef,
        #[serde(rename = "isFork")]
        is_fork: bool,
        parent: Option<ParentRef>,
        #[serde(rename = "deleteBranchOnMerge")]
        delete_branch_on_merge: bool,
    }
    #[derive(Deserialize)]
    struct DefaultBranchRef {
        name: String,
    }
    #[derive(Deserialize)]
    struct ParentRef {
        #[serde(rename = "nameWithOwner")]
        name_with_owner: String,
    }

    let mut args = vec![
        "repo",
        "view",
        "--json",
        "nameWithOwner,defaultBranchRef,isFork,parent,deleteBranchOnMerge",
    ];
    if let Some(target) = name_with_owner {
        args.insert(2, target);
    }
    let stdout = run_capture(&args)?;
    let raw: Raw = serde_json::from_str(&stdout)
        .with_context(|| format!("could not parse `gh repo view` output: {stdout}"))?;
    Ok(RepoInfo {
        name_with_owner: raw.name_with_owner,
        default_branch: raw.default_branch_ref.name,
        is_fork: raw.is_fork,
        parent_name_with_owner: raw.parent.map(|p| p.name_with_owner),
        delete_branch_on_merge: raw.delete_branch_on_merge,
    })
}

/// List open PRs on `repo` (`owner/name`). The list is unbounded: GitHub's
/// default page size is small enough to surprise us, so request a high cap.
///
/// `--json` fields used:
/// - `number`              — PR number on the repo
/// - `headRefName`         — branch name of the PR head
/// - `headRefOid`          — commit SHA at the PR head (for ancestry checks)
/// - `baseRefName`         — branch the PR is targeting
/// - `headRepositoryOwner` — has `login`; tells us if the head lives on a fork
/// - `url`                 — html URL to print to the user
/// - `isCrossRepository`   — true when head is in a fork rather than `repo`
pub fn list_open_prs(repo: &str) -> Result<Vec<OpenPr>> {
    #[derive(Deserialize)]
    struct Raw {
        number: u64,
        #[serde(rename = "headRefName")]
        head_ref_name: String,
        #[serde(rename = "headRefOid")]
        head_ref_oid: String,
        #[serde(rename = "baseRefName")]
        base_ref_name: String,
        #[serde(rename = "headRepositoryOwner")]
        head_repository_owner: Owner,
        url: String,
        #[serde(rename = "isCrossRepository")]
        is_cross_repository: bool,
    }
    #[derive(Deserialize)]
    struct Owner {
        login: String,
    }

    let stdout = run_capture(&[
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "open",
        "--limit",
        "1000",
        "--json",
        "number,headRefName,headRefOid,baseRefName,headRepositoryOwner,url,isCrossRepository",
    ])?;
    let raws: Vec<Raw> = serde_json::from_str(&stdout)
        .with_context(|| format!("could not parse `gh pr list` output: {stdout}"))?;
    Ok(raws
        .into_iter()
        .map(|r| OpenPr {
            number: r.number,
            head_ref_name: r.head_ref_name,
            head_ref_oid: r.head_ref_oid,
            base_ref_name: r.base_ref_name,
            head_repository_owner: r.head_repository_owner.login,
            url: r.url,
            is_cross_repository: r.is_cross_repository,
        })
        .collect())
}

/// Options for creating a PR. Title/body default to gh's `--fill` behaviour
/// when both are absent.
#[derive(Debug, Default)]
pub struct CreatePrOpts<'a> {
    pub title: Option<&'a str>,
    pub body: Option<&'a str>,
    pub draft: bool,
    pub web: bool,
}

/// Create a PR on `repo` from `head` into `base`, returning whatever `gh`
/// printed (typically the new PR's URL).
pub fn create_pr(repo: &str, base: &str, head: &str, opts: &CreatePrOpts<'_>) -> Result<String> {
    let mut args: Vec<String> = vec![
        "pr".into(),
        "create".into(),
        "--repo".into(),
        repo.into(),
        "--base".into(),
        base.into(),
        "--head".into(),
        head.into(),
    ];
    match (opts.title, opts.body) {
        (Some(t), Some(b)) => {
            args.push("--title".into());
            args.push(t.into());
            args.push("--body".into());
            args.push(b.into());
        }
        (Some(t), None) => {
            args.push("--title".into());
            args.push(t.into());
            args.push("--fill".into());
        }
        (None, Some(b)) => {
            args.push("--body".into());
            args.push(b.into());
            args.push("--fill".into());
        }
        (None, None) => {
            args.push("--fill".into());
        }
    }
    if opts.draft {
        args.push("--draft".into());
    }
    if opts.web {
        args.push("--web".into());
    }
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_capture(&arg_refs)
}

// --- internals --------------------------------------------------------------

fn run_capture(args: &[&str]) -> Result<String> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .with_context(|| format!("failed to invoke gh {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("gh {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
