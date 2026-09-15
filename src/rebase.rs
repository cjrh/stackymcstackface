//! Recoverable stack rebase. User branches are not moved during replay: private
//! branches hold results, then one compare-and-swap transaction installs them.

#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{gh, git, topology, ui::Console};

#[derive(clap::Args, Debug)]
pub struct Options {
    /// Resume the recorded stack rebase after resolving and staging conflicts.
    #[arg(long = "continue", conflicts_with_all = ["abort", "push"])]
    pub resume: bool,
    /// Abort the whole stack rebase, restoring original local branch tips.
    #[arg(long, conflicts_with = "push")]
    pub abort: bool,
    /// Atomically publish all branches with explicit leases after replay succeeds.
    #[arg(long)]
    pub push: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    layer: topology::Layer,
    new: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum Phase {
    Preparing,
    Ready(usize),
    Replaying { index: usize, onto: String },
    Applying,
    Publishing,
    Finishing,
    Aborting,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    id: String,
    worktree_git_dir: PathBuf,
    original_branch: String,
    default_branch: String,
    repo: String,
    remote: String,
    base: String,
    push: bool,
    phase: Phase,
    entries: Vec<Entry>,
}

impl Journal {
    fn stage(&self, i: usize) -> String {
        format!("refs/heads/sms-rebase-{}/{i}", self.id)
    }
    fn checkpoint(&self, i: usize) -> String {
        format!("refs/stackymcstackface/{}/checkpoint/{i}", self.id)
    }
    fn backup(&self, i: usize) -> String {
        format!("refs/stackymcstackface/{}/original/{i}", self.id)
    }
    fn base_ref(&self) -> String {
        format!("refs/stackymcstackface/{}/base", self.id)
    }

    fn save(&self, path: &Path) -> Result<()> {
        let temp = path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        fs::rename(temp, path)?;
        #[cfg(unix)]
        File::open(path.parent().context("journal has no parent")?)?.sync_all()?;
        Ok(())
    }

    fn load(path: &Path) -> Result<Self> {
        let j: Self = serde_json::from_slice(
            &fs::read(path).context("no stack rebase recovery record; start with `rebase`")?,
        )
        .context("corrupt stack rebase record; refusing to mutate refs")?;
        j.validate()?;
        Ok(j)
    }

    fn validate(&self) -> Result<()> {
        let sha =
            |s: &str| (s.len() == 40 || s.len() == 64) && s.bytes().all(|c| c.is_ascii_hexdigit());
        if self.version != 1
            || self.id.is_empty()
            || !self.id.bytes().all(|c| c.is_ascii_digit() || c == b'-')
            || self.entries.is_empty()
            || !sha(&self.base)
        {
            bail!("invalid stack rebase recovery record; refusing to mutate refs");
        }
        if self.worktree_git_dir != git::git_dir()? {
            bail!(
                "resume/abort the stack rebase in its original worktree: {}",
                self.worktree_git_dir.display()
            );
        }
        git::check_branch(&self.default_branch)?;
        let mut names = std::collections::BTreeSet::new();
        for (i, entry) in self.entries.iter().enumerate() {
            git::check_branch(&entry.layer.branch)?;
            if !names.insert(&entry.layer.branch)
                || !sha(&entry.layer.old)
                || !sha(&entry.layer.boundary)
                || entry.new.as_deref().is_some_and(|s| !sha(s))
                || (i > 0 && entry.layer.boundary != self.entries[i - 1].layer.old)
            {
                bail!("invalid branch/boundary in stack rebase record");
            }
        }
        if !names.contains(&self.original_branch) {
            bail!("invalid original branch in rebase record");
        }
        let done = match &self.phase {
            Phase::Preparing => 0,
            Phase::Ready(i) | Phase::Replaying { index: i, .. } => *i,
            Phase::Applying | Phase::Publishing | Phase::Finishing => self.entries.len(),
            Phase::Aborting => return Ok(()),
        };
        if done > self.entries.len()
            || (matches!(self.phase, Phase::Replaying { .. }) && done == self.entries.len())
            || self
                .entries
                .iter()
                .enumerate()
                .any(|(i, e)| e.new.is_some() != (i < done))
        {
            bail!("invalid progress in stack rebase record");
        }
        if let Phase::Replaying { index, onto } = &self.phase {
            let expected = if *index == 0 {
                &self.base
            } else {
                self.entries[index - 1]
                    .new
                    .as_ref()
                    .context("missing parent result")?
            };
            if onto != expected {
                bail!("invalid rebase destination in recovery record");
            }
        }
        Ok(())
    }

    fn check_checkout(&self) -> Result<()> {
        let state = git::read_state()?;
        if matches!(self.phase, Phase::Replaying { .. }) {
            return Ok(()); // Native metadata and the result checkpoint guard this phase.
        }
        if state.current_branch.as_deref() != Some(&self.original_branch)
            && !(0..self.entries.len()).any(|i| {
                state.current_branch.as_deref() == self.stage(i).strip_prefix("refs/heads/")
            })
        {
            bail!(
                "checkout changed outside the stack rebase; restore the recorded checkout before continuing"
            );
        }
        Ok(())
    }

    fn check_worktrees(&self) -> Result<()> {
        topology::check_worktrees(
            &self
                .entries
                .iter()
                .map(|e| e.layer.clone())
                .collect::<Vec<_>>(),
            true,
        )?;
        for wt in git::worktrees()? {
            if (0..self.entries.len()).any(|i| wt.branch.as_deref() == Some(&self.stage(i)))
                && git::worktree_git_dir(&wt.path)? != self.worktree_git_dir
            {
                bail!(
                    "private rebase branch is checked out in another worktree: {}",
                    wt.path.display()
                );
            }
        }
        Ok(())
    }

    fn check_originals(&self) -> Result<bool> {
        let mut all_old = true;
        let mut all_new = true;
        for entry in &self.entries {
            git::require_direct_ref(&format!("refs/heads/{}", entry.layer.branch))?;
            let actual = git::resolve(&format!("refs/heads/{}", entry.layer.branch))?;
            all_old &= actual.as_deref() == Some(&entry.layer.old);
            all_new &= entry.new.is_some() && actual == entry.new;
        }
        let valid = match self.phase {
            Phase::Applying | Phase::Aborting => all_old || all_new,
            Phase::Publishing | Phase::Finishing => all_new,
            _ => all_old,
        };
        if !valid {
            bail!(
                "stack branch refs changed outside this operation; refusing to overwrite user work (recovery refs are retained)"
            );
        }
        Ok(all_new)
    }

    fn check_private_refs(&self) -> Result<()> {
        let cleanup = matches!(
            self.phase,
            Phase::Finishing | Phase::Aborting | Phase::Preparing
        );
        for (i, entry) in self.entries.iter().enumerate() {
            for name in [self.backup(i), self.stage(i), self.checkpoint(i)] {
                git::require_direct_ref(&name)?;
            }
            let backup = git::resolve(&self.backup(i))?;
            if backup.as_deref() != Some(&entry.layer.old) && !(cleanup && backup.is_none()) {
                bail!("original recovery ref changed or disappeared");
            }
            let stage = git::resolve(&self.stage(i))?;
            if cleanup && stage.is_none() {
                continue;
            }
            let expected = entry.new.as_ref().unwrap_or(&entry.layer.old);
            if stage.as_ref() != Some(expected) {
                let active = matches!(self.phase, Phase::Replaying { index, .. } if index == i);
                if !active || stage.is_none() || stage != git::resolve(&self.checkpoint(i))? {
                    bail!("private rebase branch changed unexpectedly; recovery refs retained");
                }
            }
        }
        git::require_direct_ref(&self.base_ref())?;
        let base = git::resolve(&self.base_ref())?;
        if base.as_deref() != Some(&self.base) && !(cleanup && base.is_none()) {
            bail!("rebase base recovery ref changed");
        }
        Ok(())
    }

    /// Git updates the staging branch only when the complete rebase succeeds.
    /// The exec checkpoint independently identifies the last replayed commit.
    fn accept_result(&mut self, index: usize, onto: &str, path: &Path) -> Result<()> {
        let state = git::read_state()?;
        if state.in_progress.is_some() {
            bail!("Git still has an operation in progress");
        }
        let result = git::resolve(&self.stage(index))?.context("staging branch disappeared")?;
        if git::resolve(&self.checkpoint(index))?.as_ref() != Some(&result)
            || result == self.entries[index].layer.old
            || state.current_branch.as_deref() != self.stage(index).strip_prefix("refs/heads/")
            || state.head_sha != result
            || !git::is_ancestor(onto, &result)?
        {
            bail!(
                "cannot prove this layer completed (Git may have been manually aborted or interrupted before starting); use `rebase --abort` and restart; user branch refs have not moved"
            );
        }
        if !git::is_clean(Path::new("."))? {
            bail!("worktree is dirty after replay; preserve your changes before continuing");
        }
        self.entries[index].new = Some(result);
        self.phase = Phase::Ready(index + 1);
        self.save(path)
    }

    fn native_rebase(&self, index: usize, onto: &str) -> Result<bool> {
        let state = git::read_state()?;
        match state.in_progress {
            None => Ok(false),
            Some(git::InProgress::Rebase) => {
                let dir = self.worktree_git_dir.join("rebase-merge");
                for (name, expected) in [
                    ("head-name", self.stage(index)),
                    ("orig-head", self.entries[index].layer.old.clone()),
                    ("onto", onto.to_owned()),
                ] {
                    if fs::read_to_string(dir.join(name))
                        .unwrap_or_default()
                        .trim()
                        != expected
                    {
                        bail!(
                            "native rebase does not match the recorded stack operation; refusing to continue/abort it"
                        );
                    }
                }
                Ok(true)
            }
            Some(op) => bail!(
                "unrelated {} in progress; refusing to continue/abort it",
                op.label()
            ),
        }
    }

    fn local_updates(&self, rollback: bool) -> Result<Vec<(String, String, String)>> {
        self.entries
            .iter()
            .map(|entry| {
                let new = entry
                    .new
                    .clone()
                    .context("missing completed branch result")?;
                let old = entry.layer.old.clone();
                let (new, old) = if rollback { (old, new) } else { (new, old) };
                Ok((format!("refs/heads/{}", entry.layer.branch), new, old))
            })
            .collect()
    }

    fn check_remote_topology(&self) -> Result<()> {
        let repo = format!("https://github.com/{}", self.repo);
        let target = gh::repo_info(Some(&repo))?;
        if target.name_with_owner != self.repo || target.default_branch != self.default_branch {
            bail!(
                "merge-target repository/default branch changed during rebase; no push attempted"
            );
        }
        let prs = gh::list_open_prs(&repo)?;
        let actual = topology::linear_stack(&prs, &self.original_branch, &self.default_branch)?;
        if actual.len() != self.entries.len()
            || actual.iter().zip(&self.entries).any(|(pr, e)| {
                pr.number != e.layer.number
                    || pr.head_ref_name != e.layer.branch
                    || pr.head_ref_oid != e.layer.old
            })
        {
            bail!(
                "GitHub stack topology/heads changed during rebase; no push attempted; recovery record retained"
            );
        }
        Ok(())
    }

    fn remote_status(&self) -> Result<(bool, bool)> {
        topology::validate_remote(&self.remote, &self.repo)?;
        let heads = git::remote_heads(&self.remote)?;
        let all_old = self
            .entries
            .iter()
            .all(|e| heads.get(&format!("refs/heads/{}", e.layer.branch)) == Some(&e.layer.old));
        let all_new = self
            .entries
            .iter()
            .all(|e| heads.get(&format!("refs/heads/{}", e.layer.branch)) == e.new.as_ref());
        Ok((all_old, all_new))
    }

    fn cleanup(&self, path: &Path) -> Result<()> {
        let mut refs = vec![self.base_ref()];
        for i in 0..self.entries.len() {
            refs.extend([self.stage(i), self.backup(i), self.checkpoint(i)]);
        }
        let mut updates = Vec::new();
        for name in refs {
            if let Some(old) = git::resolve(&name)? {
                updates.push((name, String::new(), old));
            }
        }
        if !updates.is_empty() {
            git::update_refs(&updates)?;
        }
        fs::remove_file(path)?;
        Ok(())
    }
}

fn prepare(j: &mut Journal, path: &Path) -> Result<()> {
    let mut updates = Vec::new();
    for (i, e) in j.entries.iter().enumerate() {
        for name in [j.stage(i), j.backup(i)] {
            match git::resolve(&name)? {
                None => updates.push((name, e.layer.old.clone(), String::new())),
                Some(tip) if tip == e.layer.old => (),
                _ => bail!("private recovery ref already exists with a different value"),
            }
        }
    }
    if git::resolve(&j.base_ref())?.is_none() {
        updates.push((j.base_ref(), j.base.clone(), String::new()));
    }
    if !updates.is_empty() {
        git::update_refs(&updates)?;
    }
    j.phase = Phase::Ready(0);
    j.save(path)
}

fn resume(j: &mut Journal, path: &Path, console: &Console) -> Result<()> {
    loop {
        j.check_originals()?;
        j.check_private_refs()?;
        j.check_worktrees()?;
        j.check_checkout()?;
        match &j.phase {
            Phase::Preparing => {
                topology::clean_state()?;
                prepare(j, path)?;
            }
            Phase::Ready(index) => {
                topology::clean_state()?;
                let index = *index;
                if index == j.entries.len() {
                    j.phase = Phase::Applying;
                    j.save(path)?;
                    continue;
                }
                let onto = if index == 0 {
                    j.base.clone()
                } else {
                    j.entries[index - 1]
                        .new
                        .clone()
                        .context("missing parent result")?
                };
                if onto == j.entries[index].layer.boundary {
                    j.entries[index].new = Some(j.entries[index].layer.old.clone());
                    j.phase = Phase::Ready(index + 1);
                    j.save(path)?;
                    continue;
                }
                let branch = j.stage(index).trim_start_matches("refs/heads/").to_owned();
                git::checkout(&branch)?;
                j.phase = Phase::Replaying {
                    index,
                    onto: onto.clone(),
                };
                j.save(path)?;
                console.field(&format!(
                    "rebasing #{} {}",
                    j.entries[index].layer.number, j.entries[index].layer.branch
                ));
                git::rebase_layer(&branch, &j.entries[index].layer.boundary, &onto, &j.checkpoint(index))
                    .context("rebase stopped; fix conflicts, `git add` the resolutions, then run `stackymcstackface rebase --continue` (or --abort)")?;
                j.accept_result(index, &onto, path)?;
            }
            Phase::Replaying { index, onto } => {
                let (index, onto) = (*index, onto.clone());
                if j.native_rebase(index, &onto)? {
                    git::rebase_continue().context("resolve conflicts and stage with `git add`, then retry `rebase --continue`; or use `rebase --abort`")?;
                }
                j.accept_result(index, &onto, path)?;
            }
            Phase::Applying => {
                topology::clean_state()?;
                // Recovery also accepts the original checkout. Never change
                // its ref underneath an index that still has the old tree.
                let stage = j.stage(j.entries.len() - 1);
                git::checkout(stage.trim_start_matches("refs/heads/"))?;
                if !j.check_originals()? {
                    git::update_refs(&j.local_updates(false)?)?;
                }
                j.phase = if j.push {
                    Phase::Publishing
                } else {
                    Phase::Finishing
                };
                j.save(path)?;
            }
            Phase::Publishing => {
                topology::clean_state()?;
                let (all_old, all_new) = j.remote_status()?;
                if !all_new {
                    if !all_old {
                        bail!(
                            "remote stack changed; no push attempted. Local rebase is retained. Restore/coordinate remote heads before retrying `rebase --continue`; `--abort` is refused while publication is uncertain"
                        );
                    }
                    j.check_remote_topology()?;
                    let branches = j
                        .entries
                        .iter()
                        .map(|e| {
                            Ok((
                                e.layer.branch.clone(),
                                e.new.clone().context("missing result")?,
                                e.layer.old.clone(),
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    git::push_stack(&j.remote, &branches).context("atomic push failed or its result is uncertain; local results and recovery record retained. Run `rebase --continue` to check remote heads and safely retry, or `--abort` if no heads were published")?;
                    if !j.remote_status()?.1 {
                        bail!(
                            "cannot confirm publication; recovery record retained, run `rebase --continue`"
                        );
                    }
                }
                console.done("published the entire stack with explicit leases");
                j.phase = Phase::Finishing;
                j.save(path)?;
            }
            Phase::Finishing => {
                topology::clean_state()?;
                git::checkout(&j.original_branch)?;
                j.cleanup(path)?;
                console.done("rebased the entire stack; restored original checkout");
                return Ok(());
            }
            Phase::Aborting => bail!("abort was interrupted; run `rebase --abort` again"),
        }
    }
}

fn abort(j: &mut Journal, path: &Path, console: &Console) -> Result<()> {
    let applied = j.check_originals()?;
    j.check_private_refs()?;
    j.check_worktrees()?;
    if j.push
        && matches!(
            j.phase,
            Phase::Publishing | Phase::Finishing | Phase::Aborting
        )
    {
        let (all_old, all_new) = j.remote_status()?;
        if all_new && !all_old {
            bail!(
                "the rebased stack is already published; cannot abort remote changes. Run `rebase --continue` to finish cleanup"
            );
        }
        if !all_old {
            bail!(
                "remote publication is uncertain or heads changed; refusing rollback. Recovery refs retained"
            );
        }
    }
    if let Phase::Replaying { index, onto } = &j.phase {
        if j.native_rebase(*index, onto)? {
            git::rebase_abort()?;
        }
    } else if git::read_state()?.in_progress.is_some() {
        bail!("unrelated Git operation in progress; refusing to abort it");
    }
    if !git::is_clean(Path::new("."))? {
        bail!("worktree is dirty; refusing to discard unrelated work");
    }
    let state = git::read_state()?;
    let allowed = state.current_branch.as_deref() == Some(&j.original_branch)
        || (0..j.entries.len())
            .any(|i| state.current_branch.as_deref() == j.stage(i).strip_prefix("refs/heads/"))
        || (matches!(j.phase, Phase::Aborting)
            && state.current_branch.is_none()
            && j.entries.iter().any(|e| {
                e.new.as_deref() == Some(&state.head_sha) || e.layer.old == state.head_sha
            }));
    if !allowed {
        bail!(
            "checkout changed outside the stack rebase; refusing to switch away from unrelated work"
        );
    }
    // If Git completed outside the tool, retain the proven staging result in
    // the abort record as well, so a crash after detaching remains recoverable.
    if let Phase::Replaying { index, .. } = j.phase {
        let tip = git::resolve(&j.stage(index))?.context("missing staging branch")?;
        if tip != j.entries[index].layer.old {
            if git::resolve(&j.checkpoint(index))?.as_ref() != Some(&tip) {
                bail!("unproven staging result");
            }
            j.entries[index].new = Some(tip);
        }
    }
    j.phase = Phase::Aborting;
    j.save(path)?;
    // Detaching avoids leaving a checked-out branch's index at the new tree
    // when rolling that branch back via update-ref.
    git::detach()?;
    if applied {
        git::update_refs(&j.local_updates(true)?)?;
    }
    git::checkout(&j.original_branch)?;
    j.cleanup(path)?;
    console.done("aborted the whole stack rebase; original branches restored");
    Ok(())
}

pub fn run(opts: &Options) -> Result<()> {
    let _lock = topology::lock()?;
    let path = topology::journal_path()?;
    let console = Console::detect();
    console.banner();
    if opts.resume || opts.abort {
        git::require_full_history()?;
        let mut j = Journal::load(&path)?;
        return if opts.abort {
            abort(&mut j, &path, &console)
        } else {
            resume(&mut j, &path, &console)
        };
    }
    let stack = topology::discover(true)?;
    console.section("PLAN");
    console.field(&format!(
        "{} · remote {} · onto {}",
        stack.target.name_with_owner, stack.remote, stack.target.default_branch
    ));
    for e in &stack.layers {
        console.field(&format!("#{} {}", e.number, e.branch));
    }
    console.field(if opts.push {
        "rebase locally, then atomically publish the entire stack"
    } else {
        "rebase local branches only (no push)"
    });
    let id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let mut j = Journal {
        version: 1,
        id,
        worktree_git_dir: git::git_dir()?,
        original_branch: stack.original_branch,
        default_branch: stack.target.default_branch,
        repo: stack.target.name_with_owner,
        remote: stack.remote,
        base: stack.base,
        push: opts.push,
        phase: Phase::Preparing,
        entries: stack
            .layers
            .into_iter()
            .map(|layer| Entry { layer, new: None })
            .collect(),
    };
    j.save(&path)?;
    console.section("EXECUTE");
    resume(&mut j, &path, &console)
}
