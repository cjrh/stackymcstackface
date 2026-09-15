//! Thin wrapper over the `git` CLI.
//!
//! The interface is deliberately small: callers describe *what* they need
//! (branch name, ancestry, push, etc.) and never see process spawning or
//! stdout parsing. This keeps the orchestration layer in `stack.rs` readable
//! and makes it possible to swap implementations later (e.g., libgit2) without
//! touching callers.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;

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
    let upstream = run(&[
        "rev-parse",
        "--abbrev-ref",
        "--symbolic-full-name",
        "@{upstream}",
    ])
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

/// How a fetch/push should treat git's own chatter. The caller picks this from
/// its verbosity setting; the quiet default hides progress behind the UI's
/// spinner, while the streaming modes hand git the terminal so the raw output
/// is there to troubleshoot with.
#[derive(Debug, Clone, Copy)]
pub enum GitOutput {
    /// Capture and discard output on success; failures still carry git's
    /// stderr in the returned error. The quiet default.
    Quiet,
    /// Inherit the user's terminal so progress is visible. `verbose` adds
    /// git's own `--verbose` for extra detail.
    Stream { verbose: bool },
}

/// `git fetch <remote>`, with output handled per `output`.
pub fn fetch(remote: &str, output: GitOutput) -> Result<()> {
    let mut args = vec!["fetch"];
    if matches!(output, GitOutput::Stream { verbose: true }) {
        args.push("--verbose");
    }
    args.push(remote);
    run_git(&args, output)
}

/// Push `branch` to `remote`, setting upstream tracking. Optionally use
/// `--force-with-lease` for safe re-pushes after a local rebase. Output is
/// handled per `output`.
pub fn push(remote: &str, branch: &str, force_with_lease: bool, output: GitOutput) -> Result<()> {
    let mut args = vec!["push", "--set-upstream"];
    if force_with_lease {
        args.push("--force-with-lease");
    }
    if matches!(output, GitOutput::Stream { verbose: true }) {
        args.push("--verbose");
    }
    args.push(remote);
    args.push(branch);
    run_git(&args, output)
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

/// Resolve an existing commit, not just a syntactically valid SHA.
pub fn resolve(rev: &str) -> Result<Option<String>> {
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .output()
        .context("failed to invoke git rev-parse")?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
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

// --- whole-stack operations -------------------------------------------------

/// Absolute, worktree-specific Git metadata directory.
pub fn git_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(run(&["rev-parse", "--absolute-git-dir"])?))
}

pub fn worktree_in_progress(path: &Path) -> Result<Option<InProgress>> {
    Ok(detect_in_progress(&worktree_git_dir(path)?))
}

pub fn worktree_git_dir(path: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(run(&[
        "-C",
        path.to_str().context("non-UTF-8 worktree path")?,
        "rev-parse",
        "--absolute-git-dir",
    ])?))
}

/// Absolute shared metadata directory (also shared by linked worktrees).
pub fn common_dir() -> Result<PathBuf> {
    Ok(PathBuf::from(run(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ])?))
}

/// All effective fetch or push URLs, including Git's URL rewrites.
pub fn remote_urls(remote: &str, push: bool) -> Result<Vec<String>> {
    let mut args = vec!["remote", "get-url", "--all"];
    if push {
        args.push("--push");
    }
    args.push(remote);
    Ok(run(&args)?.lines().map(str::to_owned).collect())
}

/// Refuse fetch destinations that redirect updates outside their named refs.
pub fn check_fetch_refs(remote: &str) -> Result<()> {
    let prefix = format!("refs/remotes/{remote}/");
    let refs = run(&["for-each-ref", "--format=%(refname) %(symref)", &prefix])?;
    for line in refs.lines() {
        if let Some((name, target)) = line.split_once(' ')
            && !target.is_empty()
            && name != format!("{prefix}HEAD")
        {
            bail!(
                "symbolic fetch destination `{name}` is not supported; no referent will be updated"
            );
        }
    }
    Ok(())
}

/// Fetch only remote-tracking heads, regardless of configured ref/tag mappings.
/// HEAD is a conventional symbolic remote alias, never a writable destination.
pub fn fetch_heads(remote: &str) -> Result<()> {
    run(&[
        "fetch",
        "--prune",
        "--no-prune-tags",
        "--no-tags",
        "--refmap=",
        "--recurse-submodules=no",
        remote,
        &format!("+refs/heads/*:refs/remotes/{remote}/*"),
        "^refs/heads/HEAD",
    ])
    .map(|_| ())
}

/// True when tracked, staged, and untracked files are all clean.
pub fn is_clean(path: &Path) -> Result<bool> {
    Ok(run(&[
        "-C",
        path.to_str().context("non-UTF-8 worktree path")?,
        "status",
        "--porcelain",
        "--untracked-files=all",
    ])?
    .is_empty())
}

#[derive(Debug)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// `git worktree list --porcelain -z`, preserving paths containing spaces.
pub fn worktrees() -> Result<Vec<Worktree>> {
    let output = run(&["worktree", "list", "--porcelain", "-z"])?;
    let mut result = Vec::new();
    for record in output.split("\0\0") {
        let mut path = None;
        let mut branch = None;
        for field in record.split('\0') {
            if let Some(p) = field.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            }
            if let Some(b) = field.strip_prefix("branch ") {
                branch = Some(b.to_owned());
            }
        }
        if let Some(path) = path {
            result.push(Worktree { path, branch });
        }
    }
    Ok(result)
}

pub fn merge_base(a: &str, b: &str) -> Result<String> {
    let bases = run(&["merge-base", "--all", a, b])?;
    if bases.lines().count() != 1 {
        bail!("history has no unique merge base");
    }
    Ok(bases)
}

pub fn has_merges(base: &str, tip: &str) -> Result<bool> {
    Ok(!run(&["rev-list", "--merges", &format!("{base}..{tip}")])?.is_empty())
}

pub fn check_branch(branch: &str) -> Result<()> {
    run(&["check-ref-format", "--branch", branch]).map(|_| ())
}

/// Symbolic refs can resolve to the expected SHA but name a different branch.
pub fn require_direct_ref(name: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["symbolic-ref", "--quiet", name])
        .output()
        .context("could not inspect ref identity")?;
    match output.status.code() {
        Some(0) => bail!("symbolic ref `{name}` is not supported; no referent will be updated"),
        Some(1) => Ok(()),
        _ => bail!("could not verify direct ref `{name}`"),
    }
}

/// An incomplete or locally replaced graph cannot prove safe ancestry.
pub fn require_full_history() -> Result<()> {
    if run(&["rev-parse", "--is-shallow-repository"])? != "false" {
        bail!("shallow history is not supported; fetch the full history before rebase/merge");
    }
    if !run(&["for-each-ref", "--format=%(refname)", "refs/replace/"])?.is_empty()
        || common_dir()?.join("info/grafts").exists()
    {
        bail!(
            "replacement refs or grafts are not supported; use the original commit graph for rebase/merge"
        );
    }
    Ok(())
}

/// Update refs together with expected old values. Each tuple is (ref, new, old).
/// An empty new value deletes the ref; an empty old value requires it absent.
pub fn update_refs(updates: &[(String, String, String)]) -> Result<()> {
    for (name, _, _) in updates {
        require_direct_ref(name)?;
    }
    let mut child = Command::new("git")
        .args(["update-ref", "--no-deref", "--create-reflog", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = String::from("start\n");
    for (name, new, old) in updates {
        if new.is_empty() {
            input.push_str(&format!("delete {name} {old}\n"));
        } else if old.is_empty() {
            input.push_str(&format!("create {name} {new}\n"));
        } else {
            input.push_str(&format!("update {name} {new} {old}\n"));
        }
    }
    input.push_str("prepare\ncommit\n");
    child
        .stdin
        .take()
        .context("git stdin unavailable")?
        .write_all(input.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!(
            "ref transaction failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub fn checkout(branch: &str) -> Result<()> {
    run(&["checkout", "--no-guess", branch]).map(|_| ())
}

pub fn detach() -> Result<()> {
    run(&["checkout", "--detach"]).map(|_| ())
}

/// Rebase a private branch, retaining empty commits and recording a checkpoint
/// after every replayed commit. No unrelated refs or autostashes are allowed.
pub fn rebase_layer(branch: &str, boundary: &str, onto: &str, checkpoint: &str) -> Result<()> {
    run_inherit(&[
        "-c",
        "rebase.updateRefs=false",
        "-c",
        "rebase.autoStash=false",
        "-c",
        "rebase.autoSquash=false",
        "-c",
        "rebase.abbreviateCommands=false",
        "-c",
        "rebase.instructionFormat=%s",
        "-c",
        "core.hooksPath=/dev/null",
        "rebase",
        "--merge",
        "--no-update-refs",
        "--no-autostash",
        "--no-autosquash",
        "--no-rebase-merges",
        "--no-fork-point",
        "--force-rebase",
        "--reapply-cherry-picks",
        "--empty=keep",
        "--keep-empty",
        "--onto",
        onto,
        "--exec",
        &format!("git update-ref --no-deref {checkpoint} HEAD"),
        boundary,
        branch,
    ])
}

pub fn rebase_continue() -> Result<()> {
    let status = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "rebase.updateRefs=false",
            "-c",
            "rebase.autoStash=false",
            "rebase",
            "--continue",
        ])
        .env("GIT_EDITOR", "true")
        .status()?;
    if !status.success() {
        bail!("git rebase --continue stopped");
    }
    Ok(())
}

pub fn rebase_abort() -> Result<()> {
    run_inherit(&["-c", "core.hooksPath=/dev/null", "rebase", "--abort"])
}

/// Publish all heads or none; explicit leases cannot be weakened by background fetches.
pub fn push_stack(remote: &str, branches: &[(String, String, String)]) -> Result<()> {
    let mut args = vec![
        "push".to_owned(),
        "--atomic".to_owned(),
        "--no-follow-tags".to_owned(),
        "--recurse-submodules=no".to_owned(),
    ];
    for (branch, _, old) in branches {
        args.push(format!("--force-with-lease=refs/heads/{branch}:{old}"));
    }
    args.push(remote.to_owned());
    for (branch, new, _) in branches {
        args.push(format!("{new}:refs/heads/{branch}"));
    }
    run(&args.iter().map(String::as_str).collect::<Vec<_>>()).map(|_| ())
}

/// Read authoritative remote heads, including after an uncertain push result.
pub fn remote_heads(remote: &str) -> Result<std::collections::BTreeMap<String, String>> {
    let text = run(&["ls-remote", "--heads", remote])?;
    text.lines()
        .map(|line| {
            let (sha, name) = line.split_once('\t').context("invalid ls-remote output")?;
            Ok((name.to_owned(), sha.to_owned()))
        })
        .collect()
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
        return Err(anyhow!("git {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run `git <args>`, dispatching on the requested output handling. `Quiet`
/// captures (and discards) stdout; the `Stream` variants inherit the terminal.
fn run_git(args: &[&str], output: GitOutput) -> Result<()> {
    match output {
        GitOutput::Quiet => run(args).map(|_| ()),
        GitOutput::Stream { .. } => run_inherit(args),
    }
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
