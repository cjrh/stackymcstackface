//! Whole-stack discovery and shared safety guards. GitHub defines edges; Git
//! validates every ref and boundary before either destructive command starts.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{gh, git, stack};

pub const JOURNAL: &str = "stackymcstackface-rebase.json";

/// An OS lock is released even if the process crashes. The empty lock file is
/// only a coordination primitive, never a source of stack topology.
pub fn lock() -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(git::common_dir()?.join("stackymcstackface.lock"))?;
    file.try_lock()
        .context("another stackymcstackface command is running in this repository")?;
    Ok(file)
}

pub fn journal_path() -> Result<PathBuf> {
    Ok(git::common_dir()?.join(JOURNAL))
}

pub fn no_rebase() -> Result<()> {
    if journal_path()?.exists() {
        bail!(
            "a stack rebase is in progress; use `stackymcstackface rebase --continue` or `--abort` in its original worktree"
        );
    }
    Ok(())
}

pub fn clean_state() -> Result<String> {
    let state = git::read_state()?;
    if let Some(op) = state.in_progress {
        bail!("a {} is already in progress", op.label());
    }
    if !git::is_clean(Path::new("."))? {
        bail!("worktree/index is dirty (including untracked files); commit or stash first");
    }
    state
        .current_branch
        .context("HEAD is detached; check out a stack branch first")
}

/// New destructive commands require both effective destinations to be the same
/// GitHub.com repo. Do not trust stack.remote or a matching fetch URL alone.
pub fn validate_remote(remote: &str, repo: &str) -> Result<()> {
    if !git::list_remotes()?.iter().any(|r| r.name == remote) {
        bail!("merge-target remote `{remote}` does not exist");
    }
    for push in [false, true] {
        let urls = git::remote_urls(remote, push)?;
        if urls.len() != 1 || !url_matches(&urls[0], repo) {
            bail!(
                "remote `{remote}` must have exactly one fetch and one push URL, both pointing to https://github.com/{repo}; check stack.remote and pushurl (only GitHub.com is supported for rebase/merge)"
            );
        }
    }
    Ok(())
}

fn url_matches(url: &str, repo: &str) -> bool {
    let url = url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase();
    let repo = repo.to_ascii_lowercase();
    [
        format!("https://github.com/{repo}"),
        format!("git@github.com:{repo}"),
        format!("ssh://git@github.com/{repo}"),
    ]
    .contains(&url)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layer {
    pub branch: String,
    pub number: u64,
    pub old: String,
    pub boundary: String,
}

pub struct Stack {
    pub target: gh::RepoInfo,
    pub remote: String,
    pub original_branch: String,
    pub base: String,
    pub layers: Vec<Layer>,
}

/// Follow both directions so invocation from a middle branch still selects the
/// entire chain. Reject forks instead of silently excluding sibling PRs.
pub fn linear_stack(prs: &[gh::OpenPr], current: &str, default: &str) -> Result<Vec<gh::OpenPr>> {
    if prs.len() >= 1000 {
        bail!("PR discovery reached its 1000-PR limit; refusing a possibly truncated stack");
    }
    let mut by_head: BTreeMap<&str, Vec<&gh::OpenPr>> = BTreeMap::new();
    for pr in prs.iter().filter(|p| !p.is_cross_repository) {
        by_head.entry(&pr.head_ref_name).or_default().push(pr);
    }
    let unique = |name: &str| -> Result<&gh::OpenPr> {
        match by_head.get(name).map(Vec::as_slice) {
            Some([pr]) => Ok(pr),
            Some(_) => bail!("multiple open PRs have head `{name}`"),
            None => {
                bail!("no same-repository open PR for `{name}`; check out a remaining stack branch")
            }
        }
    };
    let mut root = unique(current)?;
    let mut seen = BTreeSet::new();
    while root.base_ref_name != default {
        if !seen.insert(root.number) {
            bail!("cycle in PR stack");
        }
        root = unique(&root.base_ref_name)?;
    }
    let mut result = Vec::new();
    seen.clear();
    loop {
        if !seen.insert(root.number) {
            bail!("cycle in PR stack");
        }
        unique(&root.head_ref_name)?;
        result.push(root.clone());
        // Include cross-repo children in the ambiguity check, rather than
        // merging/deleting a base that would strand an unsupported dependent.
        let children: Vec<_> = prs
            .iter()
            .filter(|p| p.base_ref_name == root.head_ref_name)
            .collect();
        match children.as_slice() {
            [] => break,
            [child] if !child.is_cross_repository => root = child,
            _ => bail!(
                "branching or cross-repository stack at `{}` is not supported",
                root.head_ref_name
            ),
        }
    }
    Ok(result)
}

pub fn check_worktrees(layers: &[Layer], rebase: bool) -> Result<()> {
    let owner = git::git_dir()?;
    for wt in git::worktrees()? {
        if !layers
            .iter()
            .any(|l| wt.branch.as_deref() == Some(&format!("refs/heads/{}", l.branch)))
        {
            continue;
        }
        if rebase {
            // The current worktree is allowed; all other checkouts are refused,
            // even clean ones, because changing their branch ref stales the index.
            let current = git::worktree_git_dir(&wt.path)?;
            if current != owner {
                bail!(
                    "stack branch is checked out in another worktree: {}",
                    wt.path.display()
                );
            }
        } else if !git::is_clean(&wt.path)? || git::worktree_in_progress(&wt.path)?.is_some() {
            bail!(
                "stack branch has a dirty worktree or in-progress operation: {}",
                wt.path.display()
            );
        }
    }
    Ok(())
}

/// A PR with the wrong base can still be merged indirectly through ancestry.
/// Refuse it rather than expanding the user's selected stack without consent.
pub fn check_indirect_merges(prs: &[gh::OpenPr], layers: &[Layer]) -> Result<()> {
    let top = layers.last().context("empty stack")?;
    for pr in prs {
        if layers.iter().any(|layer| layer.number == pr.number) {
            continue;
        }
        // With complete history, an unknown object cannot be an ancestor of
        // the fetched top. This also handles fork PRs not fetched as heads.
        if git::resolve(&pr.head_ref_oid)?.is_some()
            && git::is_ancestor(&pr.head_ref_oid, &top.old)?
        {
            bail!(
                "PR #{} ({}) is an unchecked ancestor outside the selected stack; correct the PR bases before merging",
                pr.number,
                pr.head_ref_name
            );
        }
    }
    Ok(())
}

pub fn discover(rebase: bool) -> Result<Stack> {
    no_rebase()?;
    let original_branch = clean_state()?;
    git::require_full_history()?;
    let target = stack::resolve_merge_target(&gh::repo_info(None)?)?;
    let remote = stack::resolve_merge_target_remote(&target)?;
    validate_remote(&remote, &target.name_with_owner)?;
    // Do not let GH_HOST/GH_REPO redirect authoritative stack discovery to
    // another host with the same owner/name after validating GitHub.com URLs.
    let target = gh::repo_info(Some(&format!(
        "https://github.com/{}",
        target.name_with_owner
    )))?;
    validate_remote(&remote, &target.name_with_owner)?;
    if original_branch == target.default_branch {
        bail!("check out an open PR branch, not the default branch");
    }
    git::check_fetch_refs(&remote)?;
    git::fetch_heads(&remote)?;
    let prs = gh::list_open_prs(&format!("https://github.com/{}", target.name_with_owner))?;
    let ordered = linear_stack(&prs, &original_branch, &target.default_branch)?;
    let base = git::resolve(&format!("refs/remotes/{remote}/{}", target.default_branch))?
        .context("default branch was not fetched")?;
    let mut layers: Vec<Layer> = Vec::new();
    for pr in ordered {
        git::check_branch(&pr.head_ref_name)?;
        git::require_direct_ref(&format!("refs/heads/{}", pr.head_ref_name))?;
        let local = git::resolve(&format!("refs/heads/{}", pr.head_ref_name))?
            .with_context(|| format!("stack branch `{}` must exist locally", pr.head_ref_name))?;
        let fetched = git::resolve(&format!("refs/remotes/{remote}/{}", pr.head_ref_name))?;
        if local != pr.head_ref_oid || fetched.as_deref() != Some(&local) {
            bail!(
                "branch `{}` differs between local, fetched, and GitHub heads; publish/synchronize it first and retry",
                pr.head_ref_name
            );
        }
        let boundary = match layers.last() {
            Some(parent) => {
                if !git::is_ancestor(&parent.old, &local)? {
                    bail!(
                        "parent `{}` is not an ancestor of `{}`; restack manually first",
                        parent.branch,
                        pr.head_ref_name
                    );
                }
                parent.old.clone()
            }
            None => git::merge_base(&base, &local)?,
        };
        if boundary == local {
            bail!("branch `{}` has no commits of its own", pr.head_ref_name);
        }
        if rebase && git::has_merges(&boundary, &local)? {
            bail!(
                "branch `{}` contains merge commits; automatic stack rebase supports linear commit histories only",
                pr.head_ref_name
            );
        }
        layers.push(Layer {
            branch: pr.head_ref_name,
            number: pr.number,
            old: local,
            boundary,
        });
    }
    check_worktrees(&layers, rebase)?;
    if !rebase {
        check_indirect_merges(&prs, &layers)?;
    }
    Ok(Stack {
        target,
        remote,
        original_branch,
        base,
        layers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pr(n: u64, head: &str, base: &str) -> gh::OpenPr {
        gh::OpenPr {
            number: n,
            head_ref_name: head.into(),
            head_ref_oid: "abc".into(),
            base_ref_name: base.into(),
            head_repository_owner: "owner".into(),
            url: String::new(),
            is_cross_repository: false,
        }
    }
    #[test]
    fn whole_chain_from_middle() {
        let prs = vec![pr(3, "c", "b"), pr(1, "a", "main"), pr(2, "b", "a")];
        assert_eq!(
            linear_stack(&prs, "b", "main")
                .unwrap()
                .iter()
                .map(|p| p.number)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
    #[test]
    fn ambiguous_topologies_fail_closed() {
        for prs in [
            vec![pr(1, "a", "b"), pr(2, "b", "a")],
            vec![pr(1, "a", "main"), pr(2, "b", "a"), pr(3, "c", "a")],
            vec![pr(1, "a", "main"), pr(2, "a", "main")],
            vec![pr(1, "a", "missing")],
        ] {
            assert!(linear_stack(&prs, "a", "main").is_err());
        }
        assert!(linear_stack(&vec![pr(1, "a", "main"); 1000], "a", "main").is_err());
    }
    #[test]
    fn remote_identity_is_exact() {
        assert!(url_matches("git@github.com:Owner/Repo.git", "owner/repo"));
        assert!(url_matches("https://github.com/owner/repo", "owner/repo"));
        assert!(!url_matches(
            "https://evil.example/owner/repo",
            "owner/repo"
        ));
        assert!(!url_matches("https://github.com/other/repo", "owner/repo"));
    }
}
