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
    /// The authenticated user's permission level on this repo, as GitHub's
    /// `RepositoryPermission` enum (`ADMIN`, `MAINTAIN`, `WRITE`, `TRIAGE`,
    /// `READ`). `None` when GitHub does not report one. Used to decide whether
    /// the viewer may change repo settings -- see [`RepoInfo::viewer_is_admin`].
    pub viewer_permission: Option<String>,
}

impl RepoInfo {
    /// True when the authenticated user can change repository settings -- i.e.
    /// run the `PATCH /repos/{owner}/{repo}` that toggles
    /// `delete_branch_on_merge`. GitHub gates repo administration on the
    /// `ADMIN` permission level, so anything weaker cannot apply the change.
    pub fn viewer_is_admin(&self) -> bool {
        self.viewer_permission.as_deref() == Some("ADMIN")
    }
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
/// - `viewerPermission`     — caller's permission level; gates offering to
///   change repo settings (see [`RepoInfo::viewer_is_admin`])
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
        #[serde(rename = "viewerPermission")]
        viewer_permission: Option<String>,
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
        "nameWithOwner,defaultBranchRef,isFork,parent,deleteBranchOnMerge,viewerPermission",
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
        viewer_permission: raw.viewer_permission,
    })
}

/// List up to 1000 open PRs on `repo` (`owner/name`). Whole-stack callers
/// must reject a full result because it may be truncated.
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

/// Fetch the markdown body of an existing PR. `pr` may be a URL, number, or
/// branch -- whatever `gh pr view` accepts.
///
/// `--json` fields used:
/// - `body` -- markdown body of the PR (may be empty or null)
pub fn pr_body(pr: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Raw {
        body: Option<String>,
    }
    let stdout = run_capture(&["pr", "view", pr, "--json", "body"])?;
    let raw: Raw = serde_json::from_str(&stdout)
        .with_context(|| format!("could not parse `gh pr view` output: {stdout}"))?;
    Ok(raw.body.unwrap_or_default())
}

/// Replace the body of an existing PR. `pr` accepts the same identifiers as
/// `pr_body`.
pub fn set_pr_body(pr: &str, body: &str) -> Result<()> {
    run_capture(&["pr", "edit", pr, "--body", body])?;
    Ok(())
}

/// Enable "Automatically delete head branches" on `repo` (`owner/name`) via
/// `PATCH /repos/{repo}`. This is the setting stacked PRs need so GitHub
/// retargets dependents after a parent merges. Requires admin rights on the
/// repo; callers should gate on [`RepoInfo::viewer_is_admin`] first, since the
/// API returns 403 otherwise. `-F` (not `-f`) so the value is sent as a real
/// boolean rather than the string `"true"`.
pub fn set_delete_branch_on_merge(repo: &str) -> Result<()> {
    let path = format!("/repos/{repo}");
    run_capture(&[
        "api",
        "-X",
        "PATCH",
        &path,
        "-F",
        "delete_branch_on_merge=true",
    ])?;
    Ok(())
}

// --- guarded stack merging -------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct MergeSettings {
    pub allow_merge_commit: bool,
    pub delete_branch_on_merge: bool,
    pub default_branch: String,
}

/// REST repo settings, scoped explicitly to GitHub.com for destructive commands.
pub fn merge_settings(repo: &str) -> Result<MergeSettings> {
    serde_json::from_str(&run_capture(&[
        "api",
        "--hostname",
        "github.com",
        &format!("repos/{repo}"),
    ])?)
    .context("could not read merge settings")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergePr {
    pub number: u64,
    pub state: String,
    pub is_draft: bool,
    pub is_cross_repository: bool,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub base_ref_name: String,
    pub base_ref_oid: String,
    pub mergeable: String,
    pub merge_state_status: String,
    pub review_decision: Option<String>,
    pub status_check_rollup: Option<Vec<serde_json::Value>>,
}

/// `--json`: number,state,isDraft,isCrossRepository,headRefName,headRefOid,
/// baseRefName,baseRefOid,mergeable,mergeStateStatus,reviewDecision,statusCheckRollup.
pub fn merge_pr(repo: &str, number: u64) -> Result<MergePr> {
    let json = run_capture(&[
        "pr",
        "view",
        &number.to_string(),
        "--repo",
        &format!("https://github.com/{repo}"),
        "--json",
        "number,state,isDraft,isCrossRepository,headRefName,headRefOid,baseRefName,baseRefOid,mergeable,mergeStateStatus,reviewDecision,statusCheckRollup",
    ])?;
    serde_json::from_str(&json).context("could not parse merge preflight PR data")
}

/// Query the same GraphQL queue flags used internally by `gh pr merge`.
/// These include legacy branch-protection queues as well as ruleset queues.
pub fn merge_queue(repo: &str, number: u64) -> Result<bool> {
    #[derive(Deserialize)]
    struct Response {
        data: Data,
    }
    #[derive(Deserialize)]
    struct Data {
        repository: Repository,
    }
    #[derive(Deserialize)]
    struct Repository {
        #[serde(rename = "pullRequest")]
        pull_request: Queue,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Queue {
        is_merge_queue_enabled: bool,
        is_in_merge_queue: bool,
    }
    let (owner, name) = repo
        .split_once('/')
        .context("invalid GitHub repository name")?;
    let json = run_capture(&[
        "api",
        "--hostname",
        "github.com",
        "graphql",
        "-f",
        "query=query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){isMergeQueueEnabled isInMergeQueue}}}",
        "-f",
        &format!("owner={owner}"),
        "-f",
        &format!("name={name}"),
        "-F",
        &format!("number={number}"),
    ])?;
    let response: Response =
        serde_json::from_str(&json).context("could not verify merge queue policy")?;
    let queue = response.data.repository.pull_request;
    Ok(queue.is_merge_queue_enabled || queue.is_in_merge_queue)
}

/// Percent-encode a single REST path component, including slashes in branch names.
fn path_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[derive(Debug, Deserialize)]
pub struct BranchRule {
    #[serde(rename = "type")]
    pub kind: String,
}

/// Read all active rules for this branch, not just rulesets owned by this repo.
/// `--paginate --slurp` retains all pages; errors/unknown JSON fail closed.
pub fn branch_rules(repo: &str, branch: &str) -> Result<Vec<BranchRule>> {
    let json = run_capture(&[
        "api",
        "--hostname",
        "github.com",
        "--paginate",
        "--slurp",
        &format!(
            "repos/{repo}/rules/branches/{}?per_page=100",
            path_component(branch)
        ),
    ])?;
    let pages: Vec<Vec<BranchRule>> =
        serde_json::from_str(&json).context("could not inspect applicable branch rules")?;
    Ok(pages.into_iter().flatten().collect())
}

/// REST merge is immediate or rejected; unlike `gh pr merge`, it never enables
/// auto-merge or enqueues a PR. GitHub also checks the exact expected head SHA.
pub fn merge_commit(repo: &str, number: u64, sha: &str) -> Result<()> {
    #[derive(Deserialize)]
    struct Response {
        merged: bool,
        message: String,
    }
    let json = run_capture(&[
        "api",
        "--hostname",
        "github.com",
        "--method",
        "PUT",
        &format!("repos/{repo}/pulls/{number}/merge"),
        "-f",
        "merge_method=merge",
        "-f",
        &format!("sha={sha}"),
    ])?;
    let response: Response = serde_json::from_str(&json).context("uncertain merge response")?;
    if !response.merged {
        return Err(anyhow!("GitHub refused merge: {}", response.message));
    }
    Ok(())
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
