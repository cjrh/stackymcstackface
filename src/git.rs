//! Thin wrapper over the `git` CLI.
//!
//! The interface is deliberately small: callers describe *what* they need
//! (branch name, ancestry, push, etc.) and never see process spawning or
//! stdout parsing. This keeps the orchestration layer in `stack.rs` readable
//! and makes it possible to swap implementations later (e.g., libgit2) without
//! touching callers.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

/// One remote configured in the repository.
#[derive(Debug, Clone)]
pub struct Remote {
    pub name: String,
    pub fetch_url: String,
    pub push_url: String,
}

/// A long-running operation that prevents safe stacking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProgress {
    Rebase,
    Merge,
    CherryPick,
    Revert,
    Bisect,
    ApplyMailbox,
}

impl InProgress {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rebase => "rebase",
            Self::Merge => "merge",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Bisect => "bisect",
            Self::ApplyMailbox => "am (mailbox apply)",
        }
    }
}

/// A snapshot of the things `stack` needs to know about the working repo.
#[derive(Debug, Clone)]
pub struct RepoState {
    /// Current branch, or `None` if HEAD is detached.
    pub current_branch: Option<String>,
    /// In-progress operation, if any. Stacking should refuse to proceed.
    pub in_progress: Option<InProgress>,
    /// SHA at HEAD.
    pub head_sha: String,
    /// Configured upstream of the current branch as `(remote, branch)`,
    /// when one is set.
    pub upstream: Option<(String, String)>,
}

/// Read everything `stack` needs from the working tree in one shot.
///
/// Returns an error only when git itself fails (e.g., not a repo).
/// Detached HEAD is *not* an error here; callers decide policy.
pub fn read_state() -> Result<RepoState> {
    let git_dir = run(&["rev-parse", "--git-dir"])?;
    let head_sha = run(&["rev-parse", "HEAD"])
        .map_err(|_| anyhow!("repository has no commits yet (HEAD is unborn)"))?;
    let branch = run(&["symbolic-ref", "--quiet", "--short", "HEAD"]).ok();
    let upstream = run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"])
        .ok()
        .and_then(|s| split_upstream(&s));
    let in_progress = detect_in_progress(Path::new(&git_dir));
    Ok(RepoState {
        current_branch: branch,
        in_progress,
        head_sha,
        upstream,
    })
}

fn split_upstream(s: &str) -> Option<(String, String)> {
    let (remote, branch) = s.split_once('/')?;
    if remote.is_empty() || branch.is_empty() {
        return None;
    }
    Some((remote.to_string(), branch.to_string()))
}

fn detect_in_progress(git_dir: &Path) -> Option<InProgress> {
    let exists = |p: &str| git_dir.join(p).exists();
    if exists("rebase-merge") || exists("rebase-apply") {
        // rebase-apply is also used by `git am`; treat both as rebase-ish.
        // Distinguish only if useful.
        if git_dir.join("rebase-apply/applying").exists() {
            return Some(InProgress::ApplyMailbox);
        }
        return Some(InProgress::Rebase);
    }
    if exists("MERGE_HEAD") {
        return Some(InProgress::Merge);
    }
    if exists("CHERRY_PICK_HEAD") {
        return Some(InProgress::CherryPick);
    }
    if exists("REVERT_HEAD") {
        return Some(InProgress::Revert);
    }
    if exists("BISECT_LOG") {
        return Some(InProgress::Bisect);
    }
    None
}

/// `git fetch <remote>` -- streamed straight to the user's terminal so they
/// see progress on big fetches.
pub fn fetch(remote: &str) -> Result<()> {
    run_inherit(&["fetch", remote])
}

/// Push `branch` to `remote`, setting upstream tracking. Optionally use
/// `--force-with-lease` for safe re-pushes after a local rebase.
pub fn push(remote: &str, branch: &str, force_with_lease: bool) -> Result<()> {
    let mut args = vec!["push", "--set-upstream"];
    if force_with_lease {
        args.push("--force-with-lease");
    }
    args.push(remote);
    args.push(branch);
    run_inherit(&args)
}

/// True iff `ancestor` is an ancestor of (or equal to) `descendant`.
pub fn is_ancestor(ancestor: &str, descendant: &str) -> Result<bool> {
    let status = Command::new("git")
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()
        .context("failed to invoke git merge-base")?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        Some(c) => bail!("git merge-base --is-ancestor exited with code {c}"),
        None => bail!("git merge-base --is-ancestor was killed by a signal"),
    }
}

/// Number of commits in `from..to` (commits reachable from `to` but not `from`).
pub fn commits_between(from: &str, to: &str) -> Result<usize> {
    let out = run(&["rev-list", "--count", &format!("{from}..{to}")])?;
    out.parse::<usize>()
        .with_context(|| format!("could not parse commit count from {out:?}"))
}

/// Resolve a revision to a SHA, returning `None` if it doesn't exist locally.
pub fn resolve(rev: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", rev])
        .output()
        .context("failed to invoke git rev-parse")?;
    if output.status.success() {
        Ok(Some(String::from_utf8_lossy(&output.stdout).trim().to_string()))
    } else {
        Ok(None)
    }
}

/// All configured remotes with their fetch and push URLs.
pub fn list_remotes() -> Result<Vec<Remote>> {
    let raw = run(&["remote", "-v"])?;
    let mut by_name: std::collections::BTreeMap<String, (Option<String>, Option<String>)> =
        std::collections::BTreeMap::default();
    for line in raw.lines() {
        // Format: "<name>\t<url> (<fetch|push>)"
        let mut parts = line.split_whitespace();
        let name = match parts.next() {
            Some(n) => n.to_string(),
            None => continue,
        };
        let url = match parts.next() {
            Some(u) => u.to_string(),
            None => continue,
        };
        let kind = parts.next().unwrap_or("");
        let entry = by_name.entry(name).or_insert((None, None));
        if kind.contains("push") {
            entry.1 = Some(url);
        } else {
            entry.0 = Some(url);
        }
    }
    Ok(by_name
        .into_iter()
        .map(|(name, (fetch, push))| {
            let fetch = fetch.unwrap_or_default();
            let push = push.unwrap_or_else(|| fetch.clone());
            Remote {
                name,
                fetch_url: fetch,
                push_url: push,
            }
        })
        .collect())
}

/// Look up a single git config value. Returns `None` if unset.
pub fn config_get(key: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args(["config", "--get", key])
        .output()
        .context("failed to invoke git config")?;
    match output.status.code() {
        Some(0) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Some(1) => Ok(None), // key not present
        Some(c) => bail!("git config --get exited with code {c}"),
        None => bail!("git config --get was killed by a signal"),
    }
}

// --- internals --------------------------------------------------------------

/// Capture stdout of `git <args>` as a trimmed string, erroring on non-zero.
fn run(args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to invoke git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run `git <args>` and inherit the user's stdout/stderr. Used for commands
/// like `fetch` and `push` where the user wants to see progress.
fn run_inherit(args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .status()
        .with_context(|| format!("failed to invoke git {}", args.join(" ")))?;
    if !status.success() {
        return Err(anyhow!("git {} failed", args.join(" ")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_split() {
        assert_eq!(
            split_upstream("origin/main"),
            Some(("origin".into(), "main".into()))
        );
        // Branch names may contain slashes; only the first split matters.
        assert_eq!(
            split_upstream("origin/feature/foo"),
            Some(("origin".into(), "feature/foo".into()))
        );
        assert_eq!(split_upstream("noslash"), None);
        assert_eq!(split_upstream(""), None);
    }
}
