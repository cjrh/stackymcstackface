//! The `stack` command: push the current branch to the merge-target remote
//! and open a PR whose base is either the default branch (regular PR) or the
//! closest ancestor branch with an open PR (stacked PR).
//!
//! Design notes:
//!
//! * No local state is kept. The "stack" is reconstructed each invocation by
//!   asking git ("which of these PR head SHAs is an ancestor of HEAD?") and
//!   asking GitHub ("which open PRs exist on the merge-target repo?"). This
//!   avoids the desync class of bugs that fragile sidecar tools suffer from.
//!
//! * The merge-target remote is the *only* remote we ever push to. PRs only
//!   stack correctly when both head and base live on the same repo; pushing
//!   to a fork's `origin` would silently break the stack.

use std::io::{self, Write};

use anyhow::{Context, Result, anyhow, bail};
use dialoguer::Confirm;

use crate::gh::{self, CreatePrOpts, OpenPr, RepoInfo};
use crate::git::{self, RepoState};

/// User-facing options for `stack`. Kept tiny on purpose; the goal is to
/// match the manual workflow, not to grow flags.
// Each bool is an independent CLI flag; collapsing them into a state-machine
// enum (clippy's suggestion) would obscure the clap mapping without simplifying
// anything.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct StackOpts {
    pub title: Option<String>,
    pub body: Option<String>,
    pub draft: bool,
    pub web: bool,
    /// Use `--force-with-lease` when pushing. Useful after a local rebase.
    pub force_with_lease: bool,
    /// Skip interactive prompts; assume "yes" for the wrong-remote rescue
    /// and bail (rather than prompt) for ambiguous situations.
    pub yes: bool,
}

pub fn run(opts: &StackOpts) -> Result<()> {
    let state = git::read_state().context("could not read git state")?;
    let branch = guard_state(&state)?;

    // Discover what we're pointed at on GitHub.
    let local_repo = gh::repo_info(None)
        .context("`gh repo view` failed; is this a GitHub repository and is `gh` authenticated?")?;
    let target_repo = resolve_merge_target(&local_repo)?;

    if branch == target_repo.default_branch {
        bail!(
            "current branch `{branch}` is the default branch of {target}; \
             create a feature branch first.",
            target = target_repo.name_with_owner
        );
    }

    let target_remote = resolve_merge_target_remote(&target_repo)?;
    println!(
        "→ merge target: {repo} (default branch `{default}`), remote `{remote}`",
        repo = target_repo.name_with_owner,
        default = target_repo.default_branch,
        remote = target_remote,
    );

    warn_if_no_auto_delete(&target_repo);

    // If a previous push went to the wrong remote, offer to rescue it.
    rescue_wrong_remote(&state, &target_remote, opts.yes)?;

    println!("→ fetching `{target_remote}` ...");
    git::fetch(&target_remote).with_context(|| format!("git fetch {target_remote} failed"))?;

    let prs = gh::list_open_prs(&target_repo.name_with_owner)
        .context("could not list open PRs on the merge target")?;

    let parent = pick_parent(&prs, &target_repo, &target_remote, &branch, &state.head_sha)?;
    let existing = prs
        .iter()
        .find(|p| p.head_ref_name == branch && !p.is_cross_repository)
        .cloned();

    print_plan(&parent, existing.as_ref(), &branch);

    println!("→ pushing `{branch}` to `{target_remote}` ...");
    git::push(&target_remote, &branch, opts.force_with_lease).with_context(|| {
        format!(
            "git push {target_remote} {branch} failed -- if you rebased, retry with --force-with-lease"
        )
    })?;

    if let Some(pr) = existing {
        println!(
            "✔ PR already exists: #{n} {url}\n  (base on GitHub: `{base}`)",
            n = pr.number,
            url = pr.url,
            base = pr.base_ref_name,
        );
        return Ok(());
    }

    let base_branch = parent.branch_name();
    println!(
        "→ creating PR: head=`{branch}` base=`{base_branch}` repo={repo}",
        repo = target_repo.name_with_owner,
    );
    let create_opts = CreatePrOpts {
        title: opts.title.as_deref(),
        body: opts.body.as_deref(),
        draft: opts.draft,
        web: opts.web,
    };
    let out = gh::create_pr(
        &target_repo.name_with_owner,
        base_branch,
        &branch,
        &create_opts,
    )
    .context("`gh pr create` failed")?;
    if !out.is_empty() {
        println!("{out}");
    }
    Ok(())
}

// --- repo-setting warnings --------------------------------------------------

/// Warn when the merge target lacks `delete_branch_on_merge`. Without it,
/// GitHub does not retarget dependent stack PRs after the parent merges,
/// so the stack effectively breaks at the first merge. The check happens
/// every run rather than once because settings change behind our back.
fn warn_if_no_auto_delete(target: &RepoInfo) {
    if target.delete_branch_on_merge {
        return;
    }
    eprintln!(
        "⚠  `delete_branch_on_merge` is OFF on {repo}. After this PR merges, \
         GitHub will not delete the head branch and the next stack PR will not \
         auto-retarget to `{default}`. Enable with:\n     \
         gh api -X PATCH /repos/{repo} -f delete_branch_on_merge=true",
        repo = target.name_with_owner,
        default = target.default_branch,
    );
}

// --- state guards -----------------------------------------------------------

/// Validate that the repo is in a state where stacking makes sense, and
/// return the current branch name.
fn guard_state(state: &RepoState) -> Result<String> {
    if let Some(op) = state.in_progress {
        bail!(
            "refusing to stack: a {} is in progress. Resolve it first \
             (`git {}`) and try again.",
            op.label(),
            match op {
                git::InProgress::Rebase => "rebase --abort | --continue",
                git::InProgress::Merge => "merge --abort | commit",
                git::InProgress::CherryPick => "cherry-pick --abort | --continue",
                git::InProgress::Revert => "revert --abort | --continue",
                git::InProgress::Bisect => "bisect reset",
                git::InProgress::ApplyMailbox => "am --abort | --continue",
            }
        );
    }
    state
        .current_branch
        .clone()
        .ok_or_else(|| anyhow!("HEAD is detached; check out a branch before stacking"))
}

// --- merge target resolution ------------------------------------------------

/// Decide which GitHub repo PRs should land on. For forks, that's the parent.
fn resolve_merge_target(local: &RepoInfo) -> Result<RepoInfo> {
    if !local.is_fork {
        return Ok(local.clone());
    }
    let parent = local
        .parent_name_with_owner
        .as_deref()
        .ok_or_else(|| anyhow!("repository is reported as a fork but has no parent"))?;
    gh::repo_info(Some(parent))
        .with_context(|| format!("could not load merge-target repo `{parent}`"))
}

/// Find the *local* git remote whose URL points at `target`. Honours the
/// `stack.remote` git config override; otherwise picks by URL match.
fn resolve_merge_target_remote(target: &RepoInfo) -> Result<String> {
    if let Some(name) = git::config_get("stack.remote")? {
        return Ok(name);
    }
    let remotes = git::list_remotes()?;
    let matches: Vec<_> = remotes
        .iter()
        .filter(|r| {
            url_points_at(&r.fetch_url, &target.name_with_owner)
                || url_points_at(&r.push_url, &target.name_with_owner)
        })
        .collect();
    match matches.as_slice() {
        [] => Err(anyhow!(
            "no local git remote points at `{target}`. Add one with:\n  \
             git remote add upstream https://github.com/{target}.git\n\
             ...or set `git config stack.remote <remote-name>`.",
            target = target.name_with_owner,
        )),
        [only] => Ok(only.name.clone()),
        many => {
            // Prefer well-known names if they appear; otherwise refuse to guess.
            for preferred in ["upstream", "origin"] {
                if let Some(m) = many.iter().find(|r| r.name == preferred) {
                    return Ok(m.name.clone());
                }
            }
            let names: Vec<_> = many.iter().map(|r| r.name.as_str()).collect();
            Err(anyhow!(
                "multiple remotes point at `{}`: {}. Disambiguate with \
                 `git config stack.remote <name>`.",
                target.name_with_owner,
                names.join(", "),
            ))
        }
    }
}

/// Loose URL match: strip a trailing `.git` and check whether the URL ends
/// with the canonical `owner/name`. Handles https, ssh, and git protocol.
fn url_points_at(url: &str, name_with_owner: &str) -> bool {
    let trimmed = url.trim().trim_end_matches('/').trim_end_matches(".git");
    let needle = name_with_owner.to_ascii_lowercase();
    let hay = trimmed.to_ascii_lowercase();
    if !hay.ends_with(&needle) {
        return false;
    }
    // Make sure the match is on a path boundary (so `oo/bar` doesn't match
    // `foo/bar`'s last 6 chars).
    matches!(
        hay.as_bytes().get(hay.len() - needle.len() - 1),
        Some(b'/' | b':')
    )
}

// --- wrong-remote rescue ----------------------------------------------------

/// If the current branch tracks a different remote than the merge target,
/// the user has likely pushed to (e.g.) `origin` of a fork. Offer to fix it.
fn rescue_wrong_remote(state: &RepoState, target_remote: &str, assume_yes: bool) -> Result<()> {
    let Some((upstream_remote, _)) = state.upstream.as_ref() else {
        return Ok(());
    };
    if upstream_remote == target_remote {
        return Ok(());
    }
    eprintln!();
    eprintln!(
        "⚠  current branch tracks `{upstream_remote}/...` but the merge target is `{target_remote}`."
    );
    eprintln!("   Stacked PRs only work when the branch lives on the merge-target remote.");
    eprintln!();
    let ok = if assume_yes {
        true
    } else {
        // Force-flush so the prompt prints in line order even when stdout is a pipe.
        let _ = io::stderr().flush();
        Confirm::new()
            .with_prompt(format!("Re-push to `{target_remote}` and switch tracking?"))
            .default(true)
            .interact()
            .unwrap_or(false)
    };
    if !ok {
        bail!("aborting: branch must live on `{target_remote}` for stacking");
    }
    // The actual re-push happens later in `run`; we just confirmed intent.
    Ok(())
}

// --- parent resolution ------------------------------------------------------

/// Where a stacked PR should be based.
#[derive(Debug, Clone)]
enum Parent {
    /// Stack on the default branch (a "regular" PR).
    Default { branch: String },
    /// Stack on top of an existing PR.
    Pr {
        branch: String,
        number: u64,
        url: String,
        distance: usize,
    },
}

impl Parent {
    fn branch_name(&self) -> &str {
        match self {
            Parent::Default { branch } | Parent::Pr { branch, .. } => branch,
        }
    }
}

/// Among the open PRs on the merge-target repo, find the one whose head SHA
/// is the *closest* ancestor of the current HEAD. Falls back to the default
/// branch when no PR qualifies. Errors when neither qualifies (which would
/// mean the current branch shares no history with anything on the target).
fn pick_parent(
    prs: &[OpenPr],
    target: &RepoInfo,
    target_remote: &str,
    current_branch: &str,
    head_sha: &str,
) -> Result<Parent> {
    let target_owner = target
        .name_with_owner
        .split('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();

    let mut best: Option<Parent> = None;
    for pr in prs {
        // Only consider PRs whose head lives on the merge-target repo. A
        // cross-repo PR's head SHA may not even exist locally.
        if pr.is_cross_repository || pr.head_repository_owner.to_ascii_lowercase() != target_owner {
            continue;
        }
        // Ignore the PR for the branch we're currently stacking (that's us,
        // not our parent).
        if pr.head_ref_name == current_branch {
            continue;
        }
        // The head SHA must exist locally to compare ancestry. If it's
        // missing, the user hasn't fetched recently enough -- but we just
        // fetched, so this typically means the PR head was force-pushed
        // *after* our fetch. Skip rather than crash.
        if git::resolve(&pr.head_ref_oid)?.is_none() {
            continue;
        }
        if pr.head_ref_oid == head_sha {
            // Exact match: someone else's PR points at our HEAD. Don't stack
            // on top of ourselves; skip.
            continue;
        }
        if !git::is_ancestor(&pr.head_ref_oid, head_sha)? {
            continue;
        }
        let distance = git::commits_between(&pr.head_ref_oid, head_sha)?;
        let candidate = Parent::Pr {
            branch: pr.head_ref_name.clone(),
            number: pr.number,
            url: pr.url.clone(),
            distance,
        };
        best = Some(match best {
            None => candidate,
            Some(prev) => closer(prev, candidate),
        });
    }

    if let Some(p) = best {
        return Ok(p);
    }

    // No PR qualifies. The default branch must at least share history with
    // HEAD; otherwise we have nothing sensible to base the new PR on.
    let default_branch = &target.default_branch;
    let remote_ref = format!("refs/remotes/{target_remote}/{default_branch}");
    let resolved = git::resolve(&remote_ref)?.ok_or_else(|| {
        anyhow!(
            "could not resolve `{remote_ref}` after fetching; is `{default_branch}` \
             really the default branch on the merge target?"
        )
    })?;
    if !git::is_ancestor(&resolved, head_sha)? {
        bail!(
            "current branch shares no history with `{default_branch}`; cannot \
             determine a PR base"
        );
    }
    Ok(Parent::Default {
        branch: default_branch.clone(),
    })
}

fn closer(a: Parent, b: Parent) -> Parent {
    let dist = |p: &Parent| match p {
        Parent::Pr { distance, .. } => *distance,
        Parent::Default { .. } => usize::MAX,
    };
    if dist(&b) < dist(&a) { b } else { a }
}

fn print_plan(parent: &Parent, existing: Option<&OpenPr>, branch: &str) {
    match (existing, parent) {
        (Some(pr), _) => {
            println!(
                "→ plan: push `{branch}` (PR #{n} already open: {url})",
                n = pr.number,
                url = pr.url
            );
        }
        (None, Parent::Default { branch: base }) => {
            println!("→ plan: push `{branch}` and open a PR with base `{base}`");
        }
        (
            None,
            Parent::Pr {
                branch: base,
                number,
                url,
                ..
            },
        ) => {
            println!(
                "→ plan: push `{branch}` and open a STACKED PR on top of #{number} \
                 (base `{base}`)\n        parent: {url}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_match_https() {
        assert!(url_points_at("https://github.com/foo/bar", "foo/bar"));
        assert!(url_points_at("https://github.com/foo/bar.git", "foo/bar"));
        assert!(url_points_at("https://github.com/Foo/Bar.git", "foo/bar"));
    }

    #[test]
    fn url_match_ssh() {
        assert!(url_points_at("git@github.com:foo/bar.git", "foo/bar"));
        assert!(url_points_at("ssh://git@github.com/foo/bar", "foo/bar"));
    }

    #[test]
    fn url_match_path_boundary() {
        // 'oo/bar' must not match the suffix of 'foo/bar'.
        assert!(!url_points_at("https://github.com/foo/bar", "oo/bar"));
        assert!(!url_points_at("https://github.com/zfoo/bar", "foo/bar"));
    }

    #[test]
    fn url_match_negative() {
        assert!(!url_points_at("https://github.com/other/repo", "foo/bar"));
        assert!(!url_points_at("", "foo/bar"));
    }
}
