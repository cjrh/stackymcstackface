//! The `push` command: push the current branch to the merge-target remote
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
use crate::git::{self, GitOutput, RepoState};
use crate::ui::Console;

/// User-facing options for `push`. Kept tiny on purpose; the goal is to
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
    /// Verbosity for git output: 0 hides fetch/push behind a spinner, 1
    /// streams raw git output, 2+ also passes git its own `--verbose`.
    pub verbose: u8,
}

pub fn run(opts: &StackOpts) -> Result<()> {
    let console = Console::detect();
    console.banner();

    let state = git::read_state().context("could not read git state")?;
    let branch = guard_state(&state)?;

    console.section("PLAN");

    // Discover what we're pointed at on GitHub.
    let target_repo = console.spinner("resolving merge target", || {
        let local = gh::repo_info(None).context(
            "`gh repo view` failed; is this a GitHub repository and is `gh` authenticated?",
        )?;
        resolve_merge_target(&local)
    })?;

    if branch == target_repo.default_branch {
        bail!(
            "current branch `{branch}` is the default branch of {target}; \
             create a feature branch first.",
            target = target_repo.name_with_owner
        );
    }

    let target_remote = resolve_merge_target_remote(&target_repo)?;
    console.field(&format!(
        "{repo} · default {default} · remote {remote}",
        repo = target_repo.name_with_owner,
        default = target_repo.default_branch,
        remote = target_remote,
    ));

    let enable_auto_delete = plan_auto_delete(&console, &target_repo, opts.yes);

    // If a previous push went to the wrong remote, offer to rescue it.
    rescue_wrong_remote(&console, &state, &target_remote, opts.yes)?;

    git_step(
        &console,
        opts.verbose,
        &format!("fetching {target_remote}"),
        |out| {
            git::fetch(&target_remote, out)
                .with_context(|| format!("git fetch {target_remote} failed"))
        },
    )?;

    let prs = console.spinner("scanning open pull requests", || {
        gh::list_open_prs(&target_repo.name_with_owner)
            .context("could not list open PRs on the merge target")
    })?;

    let parent = pick_parent(&prs, &target_repo, &target_remote, &branch, &state.head_sha)?;
    let existing = prs
        .iter()
        .find(|p| p.head_ref_name == branch && !p.is_cross_repository)
        .cloned();

    let (headline, note) = plan_sentence(&parent, existing.as_ref(), &branch);
    console.plan(&headline, note.as_deref());

    console.section("EXECUTE");

    if enable_auto_delete {
        match console.spinner("enabling delete_branch_on_merge", || {
            gh::set_delete_branch_on_merge(&target_repo.name_with_owner)
        }) {
            Ok(()) => console.done("enabled delete_branch_on_merge"),
            // Non-fatal: the push and PR are the user's actual goal, and the
            // setting only matters at the *next* merge. Warn and carry on.
            Err(e) => console.warn(&format!("could not enable delete_branch_on_merge: {e:#}")),
        }
    }

    git_step(
        &console,
        opts.verbose,
        &format!("pushing {branch} → {target_remote}"),
        |out| {
            git::push(&target_remote, &branch, opts.force_with_lease, out).with_context(|| {
                format!(
                    "git push {target_remote} {branch} failed -- if you rebased, retry with --force-with-lease"
                )
            })
        },
    )?;
    console.done(&format!("pushed {branch} to {target_remote}"));

    if let Some(pr) = existing {
        console.url(&pr.url);
        console.dim_line(&format!("base on GitHub: {}", pr.base_ref_name));
        return Ok(());
    }

    let base_branch = parent.branch_name();
    let create_opts = CreatePrOpts {
        title: opts.title.as_deref(),
        body: opts.body.as_deref(),
        draft: opts.draft,
        web: opts.web,
    };
    let out = console.spinner("creating pull request", || {
        gh::create_pr(
            &target_repo.name_with_owner,
            base_branch,
            &branch,
            &create_opts,
        )
        .context("`gh pr create` failed")
    })?;
    console.done("opened pull request");

    // For stacked PRs, point the new PR's body at the parent. `--web` skips
    // create-from-CLI, so there's no URL to edit yet; let the user handle the
    // body in the browser in that case.
    if !opts.web
        && let Parent::Pr {
            number: parent_number,
            branch: parent_branch,
            ..
        } = &parent
    {
        let pr_url = out.trim();
        if pr_url.starts_with("https://") {
            append_stack_footer(&console, pr_url, *parent_number, parent_branch);
        }
    }

    if out.is_empty() {
        console.dim_line("opened in your browser");
    } else {
        console.url(out.trim());
    }
    Ok(())
}

// --- git step framing -------------------------------------------------------

/// Run a git step, honouring verbosity. Quiet (verbose 0) hides the work
/// behind a spinner labelled `lead`; verbose streams git's raw output to the
/// terminal under a dim lead-in so it is available for troubleshooting. The
/// `work` closure receives the matching [`GitOutput`] to pass down to git.
fn git_step(
    console: &Console,
    verbose: u8,
    lead: &str,
    work: impl FnOnce(GitOutput) -> Result<()>,
) -> Result<()> {
    if verbose >= 1 {
        console.dim_line(lead);
        work(GitOutput::Stream {
            verbose: verbose >= 2,
        })
    } else {
        console.spinner(lead, || work(GitOutput::Quiet))
    }
}

// --- repo-setting offer -----------------------------------------------------

/// Decide what to do about a merge target that lacks `delete_branch_on_merge`,
/// and return `true` when the caller should enable it in the EXECUTE phase.
///
/// The setting matters because GitHub only retargets dependent stack PRs after
/// the parent merges when the parent's head branch is deleted through the merge
/// flow -- which only happens with this on. So the stack breaks at the first
/// merge without it. Checked every run rather than once because the setting can
/// change behind our back.
///
/// Branches on whether we *can* fix it. With admin rights we offer to enable
/// it (auto-confirmed under `--yes`); otherwise -- or if the user declines --
/// we fall back to a warning with the manual command, since someone with admin
/// may hold the rights we lack. The interaction lives here in PLAN, mirroring
/// the wrong-remote rescue; the mutation itself is deferred to EXECUTE.
fn plan_auto_delete(console: &Console, target: &RepoInfo, assume_yes: bool) -> bool {
    if target.delete_branch_on_merge {
        return false;
    }
    if !target.viewer_is_admin() {
        warn_no_auto_delete(console, target);
        return false;
    }
    if assume_yes {
        return true;
    }
    console.warn(&format!(
        "`delete_branch_on_merge` is OFF on {repo}. After a stack PR merges, \
         GitHub will not delete the head branch and the next PR will not \
         auto-retarget to `{default}`.",
        repo = target.name_with_owner,
        default = target.default_branch,
    ));
    // Flush the warning (printed to stdout) so it lands before the prompt.
    let _ = io::stdout().flush();
    let yes = Confirm::new()
        .with_prompt("Enable `delete_branch_on_merge` now?")
        .default(true)
        .interact()
        .unwrap_or(false);
    if !yes {
        console.dim_line(&format!(
            "leaving it off — enable later with: \
             gh api -X PATCH /repos/{repo} -F delete_branch_on_merge=true",
            repo = target.name_with_owner,
        ));
    }
    yes
}

/// Warn that `delete_branch_on_merge` is off and the viewer cannot change it,
/// handing over the command for someone who can. Used when the viewer lacks
/// admin rights -- the only path that still just warns, since we have no action
/// to offer.
fn warn_no_auto_delete(console: &Console, target: &RepoInfo) {
    console.warn(&format!(
        "`delete_branch_on_merge` is OFF on {repo} and you lack admin rights to \
         change it. After a stack PR merges, GitHub will not delete the head \
         branch and the next PR will not auto-retarget to `{default}`. Ask an \
         admin to enable it:\n\
         gh api -X PATCH /repos/{repo} -F delete_branch_on_merge=true",
        repo = target.name_with_owner,
        default = target.default_branch,
    ));
}

// --- state guards -----------------------------------------------------------

/// Validate that the repo is in a state where stacking makes sense, and
/// return the current branch name.
fn guard_state(state: &RepoState) -> Result<String> {
    if let Some(op) = state.in_progress {
        bail!(
            "refusing to push: a {} is in progress. Resolve it first \
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
        .ok_or_else(|| anyhow!("HEAD is detached; check out a branch before pushing"))
}

// --- merge target resolution ------------------------------------------------

/// Decide which GitHub repo PRs should land on. For forks, that's the parent.
pub(crate) fn resolve_merge_target(local: &RepoInfo) -> Result<RepoInfo> {
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
pub(crate) fn resolve_merge_target_remote(target: &RepoInfo) -> Result<String> {
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
fn rescue_wrong_remote(
    console: &Console,
    state: &RepoState,
    target_remote: &str,
    assume_yes: bool,
) -> Result<()> {
    let Some((upstream_remote, _)) = state.upstream.as_ref() else {
        return Ok(());
    };
    if upstream_remote == target_remote {
        return Ok(());
    }
    console.warn(&format!(
        "current branch tracks `{upstream_remote}/...` but the merge target is `{target_remote}`.\n\
         Stacked PRs only work when the branch lives on the merge-target remote."
    ));
    let ok = if assume_yes {
        true
    } else {
        // Flush the warning (printed to stdout) so it lands before the prompt.
        let _ = io::stdout().flush();
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

// --- stack footer -----------------------------------------------------------

/// Append a "Stacked on #N" footer to a freshly-created stacked PR's body,
/// separated from whatever `--fill` (or the user) put there by a `---`
/// horizontal rule. Failures are non-fatal: the PR is already created with
/// the correct base, so the worst case is a missing footer.
fn append_stack_footer(console: &Console, pr_url: &str, parent_number: u64, parent_branch: &str) {
    let footer = format!(
        "Stacked on #{parent_number} (`{parent_branch}`) by \
         [stackymcstackface](https://github.com/cjrh/stackymcstackface) ❤️"
    );
    let result = console.spinner("noting parent in PR description", || {
        let current = gh::pr_body(pr_url)?;
        gh::set_pr_body(pr_url, &merge_body_with_footer(&current, &footer))
    });
    match result {
        Ok(()) => console.done(&format!("noted parent #{parent_number} in description")),
        Err(e) => console.warn(&format!("could not update PR description: {e:#}")),
    }
}

/// Combine an existing PR body with a new footer. Trims trailing whitespace
/// from the body so the `---` separator lands on its own line, and skips the
/// separator entirely when the body is empty.
fn merge_body_with_footer(current: &str, footer: &str) -> String {
    let trimmed = current.trim_end();
    if trimmed.is_empty() {
        footer.to_string()
    } else {
        format!("{trimmed}\n\n---\n\n{footer}")
    }
}

// --- plan wording -----------------------------------------------------------

/// Build the PLAN section's wording for the situation we resolved to: a
/// headline sentence plus an optional dim follow-on (the existing PR number,
/// or the parent a stacked PR will sit on). Pure so it can be unit-tested.
fn plan_sentence(
    parent: &Parent,
    existing: Option<&OpenPr>,
    branch: &str,
) -> (String, Option<String>) {
    match (existing, parent) {
        (Some(pr), _) => (
            format!("push new commits to {branch}"),
            Some(format!("PR #{} already open", pr.number)),
        ),
        (None, Parent::Default { branch: base }) => (
            format!("push {branch} and open a PR with base {base}"),
            None,
        ),
        (
            None,
            Parent::Pr {
                branch: base,
                number,
                url,
                ..
            },
        ) => (
            format!("push {branch} and open a stacked PR with base {base}"),
            Some(format!("parent: #{number} {url}")),
        ),
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

    #[test]
    fn footer_appends_with_separator() {
        let merged = merge_body_with_footer("Some PR body.", "Stacked on #5.");
        assert_eq!(merged, "Some PR body.\n\n---\n\nStacked on #5.");
    }

    #[test]
    fn footer_trims_trailing_whitespace_before_separator() {
        let merged = merge_body_with_footer("Body with trailing newline.\n\n", "Stacked on #5.");
        assert_eq!(
            merged,
            "Body with trailing newline.\n\n---\n\nStacked on #5."
        );
    }

    #[test]
    fn footer_replaces_empty_body() {
        assert_eq!(
            merge_body_with_footer("", "Stacked on #5."),
            "Stacked on #5."
        );
        assert_eq!(
            merge_body_with_footer("   \n\n", "Stacked on #5."),
            "Stacked on #5."
        );
    }

    fn open_pr(number: u64, base: &str) -> OpenPr {
        OpenPr {
            number,
            head_ref_name: "feat".into(),
            head_ref_oid: "deadbeef".into(),
            base_ref_name: base.into(),
            head_repository_owner: "octocat".into(),
            url: format!("https://example/pull/{number}"),
            is_cross_repository: false,
        }
    }

    #[test]
    fn plan_existing_pr_takes_precedence_over_parent() {
        let parent = Parent::Default {
            branch: "main".into(),
        };
        let existing = open_pr(7, "main");
        let (headline, note) = plan_sentence(&parent, Some(&existing), "feat");
        assert_eq!(headline, "push new commits to feat");
        assert_eq!(note.as_deref(), Some("PR #7 already open"));
    }

    #[test]
    fn plan_regular_pr_has_no_note() {
        let parent = Parent::Default {
            branch: "main".into(),
        };
        let (headline, note) = plan_sentence(&parent, None, "feat");
        assert_eq!(headline, "push feat and open a PR with base main");
        assert_eq!(note, None);
    }

    #[test]
    fn plan_stacked_pr_names_parent() {
        let parent = Parent::Pr {
            branch: "feat-base".into(),
            number: 3,
            url: "https://example/pull/3".into(),
            distance: 1,
        };
        let (headline, note) = plan_sentence(&parent, None, "feat");
        assert_eq!(
            headline,
            "push feat and open a stacked PR with base feat-base"
        );
        assert_eq!(note.as_deref(), Some("parent: #3 https://example/pull/3"));
    }
}
