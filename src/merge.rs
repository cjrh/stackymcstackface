//! Non-atomic, fail-closed bottom-up merge orchestration. Each mutation has a
//! fresh preflight and expected head SHA; completed merges are never rolled back.

use std::collections::BTreeSet;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use dialoguer::Confirm;

use crate::{gh, git, topology, ui::Console};

#[derive(clap::Args, Debug)]
pub struct Options {
    /// Confirm merging the entire stack without an interactive prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Perform all initial checks and print the plan without merging.
    #[arg(long)]
    pub dry_run: bool,
}

fn check_settings(stack: &topology::Stack) -> Result<()> {
    topology::validate_remote(&stack.remote, &stack.target.name_with_owner)?;
    let settings = gh::merge_settings(&stack.target.name_with_owner)?;
    if !settings.allow_merge_commit {
        bail!("repository must allow merge commits; squash/rebase merges are not supported");
    }
    if !settings.delete_branch_on_merge {
        bail!(
            "enable delete_branch_on_merge before merging a stack; branches are never deleted manually"
        );
    }
    if settings.default_branch != stack.target.default_branch {
        bail!("repository default branch changed; rerun from a remaining branch");
    }
    Ok(())
}

fn check_rules(repo: &str, branches: impl IntoIterator<Item = String>) -> Result<()> {
    for branch in branches.into_iter().collect::<BTreeSet<_>>() {
        for rule in gh::branch_rules(repo, &branch)? {
            if rule.kind == "merge_queue" {
                bail!(
                    "branch `{branch}` requires a merge queue, which this command does not support"
                );
            }
            if rule.kind == "required_linear_history" {
                bail!("branch `{branch}` requires linear history, incompatible with merge commits");
            }
        }
    }
    Ok(())
}

fn validate_ready(pr: &gh::MergePr, layer: &topology::Layer, base: &str) -> Result<()> {
    if pr.number != layer.number
        || pr.is_cross_repository
        || pr.head_ref_name != layer.branch
        || pr.head_ref_oid != layer.old
    {
        bail!(
            "PR #{} head/identity changed; rerun preflight",
            layer.number
        );
    }
    if pr.state != "OPEN" || pr.is_draft {
        bail!("PR #{} is not open and non-draft", pr.number);
    }
    if pr.base_ref_name != base {
        bail!(
            "PR #{} has base `{}`, expected `{base}`; wait for GitHub retargeting or inspect it manually",
            pr.number,
            pr.base_ref_name
        );
    }
    if pr.mergeable != "MERGEABLE" || pr.merge_state_status != "CLEAN" {
        bail!(
            "PR #{} is not ready: mergeable={}, merge state={} (conflicts, pending checks, or branch requirements); no bypass will be attempted",
            pr.number,
            pr.mergeable,
            pr.merge_state_status
        );
    }
    if !matches!(
        pr.review_decision.as_deref(),
        None | Some("") | Some("APPROVED")
    ) {
        bail!("PR #{} has unmet review requirements", pr.number);
    }
    if let Some(checks) = &pr.status_check_rollup {
        // gh bounds GraphQL connection results. Refuse a full page rather than
        // treating an incomplete list of checks as evidence of readiness.
        if checks.len() >= 100 {
            bail!("PR #{} check rollup may be truncated", pr.number);
        }
        for check in checks {
            let ready = match check.get("__typename").and_then(|v| v.as_str()) {
                Some("CheckRun") => {
                    check["status"] == "COMPLETED"
                        && matches!(
                            check["conclusion"].as_str(),
                            Some("SUCCESS" | "NEUTRAL" | "SKIPPED")
                        )
                }
                Some("StatusContext") => check["state"] == "SUCCESS",
                _ => false,
            };
            if !ready {
                bail!(
                    "PR #{} has a failed, pending, or unknown check; waiting is left to the user",
                    pr.number
                );
            }
        }
    }
    Ok(())
}

fn ready_pr(
    repo: &str,
    layer: &topology::Layer,
    base: &str,
    previous_base: Option<&str>,
) -> Result<gh::MergePr> {
    for attempt in 0..3 {
        let pr = gh::merge_pr(repo, layer.number)?;
        // Only transient GitHub computation/expected retargeting is polled.
        // Changed heads, failed CI and blocked requirements are never retried.
        let retargeting =
            previous_base == Some(pr.base_ref_name.as_str()) && pr.base_ref_name != base;
        if attempt < 2
            && pr.head_ref_oid == layer.old
            && pr.state == "OPEN"
            && !pr.is_draft
            && (retargeting || pr.mergeable == "UNKNOWN" || pr.merge_state_status == "UNKNOWN")
        {
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        validate_ready(&pr, layer, base)?;
        if gh::merge_queue(repo, layer.number)? {
            bail!(
                "PR #{} requires/is in a merge queue; refusing direct merge",
                layer.number
            );
        }
        return Ok(pr);
    }
    unreachable!("bounded polling returns on its last iteration")
}

fn check_local(stack: &topology::Stack) -> Result<()> {
    topology::no_rebase()?;
    topology::clean_state()?;
    git::require_full_history()?;
    topology::check_worktrees(&stack.layers, false)?;
    for layer in &stack.layers {
        git::require_direct_ref(&format!("refs/heads/{}", layer.branch))?;
        if git::resolve(&format!("refs/heads/{}", layer.branch))?.as_deref() != Some(&layer.old) {
            bail!(
                "local branch `{}` changed after planning; refusing merge",
                layer.branch
            );
        }
    }
    Ok(())
}

fn check_remaining(stack: &topology::Stack, first: usize) -> Result<()> {
    let repo = &stack.target.name_with_owner;
    let prs = gh::list_open_prs(&format!("https://github.com/{repo}"))?;
    let actual = topology::linear_stack(
        &prs,
        &stack.layers[first].branch,
        &stack.target.default_branch,
    )?;
    let remaining = &stack.layers[first..];
    if actual.len() != remaining.len()
        || actual.iter().zip(remaining).any(|(a, e)| {
            a.number != e.number || a.head_ref_oid != e.old || a.head_ref_name != e.branch
        })
    {
        bail!("remaining stack topology or heads changed; rerun from a remaining branch");
    }
    topology::check_indirect_merges(&prs, remaining)?;
    Ok(())
}

fn confirmed_merged(repo: &str, layer: &topology::Layer) -> Result<()> {
    for attempt in 0..3 {
        let pr = gh::merge_pr(repo, layer.number)?;
        if pr.state == "MERGED" && pr.head_ref_oid == layer.old && pr.number == layer.number {
            return Ok(());
        }
        if attempt < 2 {
            thread::sleep(Duration::from_millis(500));
        }
    }
    bail!(
        "PR #{} merge outcome is uncertain; inspect GitHub before rerunning",
        layer.number
    )
}

fn execute(stack: &topology::Stack, console: &Console, merged: &mut Vec<u64>) -> Result<()> {
    let repo = &stack.target.name_with_owner;
    for (i, layer) in stack.layers.iter().enumerate() {
        check_local(stack)?;
        check_settings(stack)?;
        let previous = i.checked_sub(1).map(|p| stack.layers[p].branch.as_str());
        let ready = ready_pr(repo, layer, &stack.target.default_branch, previous)?;
        check_remaining(stack, i)?;
        // Retargeting may apply different default-branch rules. Recheck those,
        // and every still-active layer's current base, before each mutation.
        check_rules(
            repo,
            std::iter::once(stack.target.default_branch.clone()).chain(
                stack.layers[i..stack.layers.len() - 1]
                    .iter()
                    .map(|e| e.branch.clone()),
            ),
        )?;
        // Re-read the immediate PR after the other preflight calls. The API
        // pins its head, but GitHub has no conditional expected-base merge API.
        let fresh = ready_pr(repo, layer, &stack.target.default_branch, None)?;
        if fresh.base_ref_oid != ready.base_ref_oid {
            bail!("merge target advanced during preflight; rerun to recheck the stack");
        }
        let mutation = gh::merge_commit(repo, layer.number, &layer.old);
        // A transport failure can still mean the server merged. Always inspect
        // the actual PR, record confirmed progress, then stop on uncertainty.
        match confirmed_merged(repo, layer) {
            Ok(()) => {
                merged.push(layer.number);
                console.done(&format!("merged #{} {}", layer.number, layer.branch));
                if let Err(error) = mutation {
                    return Err(error).context(
                        "merge was confirmed despite an API error; stopping before the next PR",
                    );
                }
            }
            Err(error) => {
                if let Err(mutation) = mutation {
                    return Err(mutation).context(format!("{error:#}"));
                }
                return Err(error);
            }
        }
    }
    Ok(())
}

pub fn run(opts: &Options) -> Result<()> {
    let _lock = topology::lock()?;
    let console = Console::detect();
    console.banner();
    let stack = topology::discover(false)?;
    let repo = &stack.target.name_with_owner;
    console.section("PLAN");
    console.field(&format!(
        "{repo} · merge commits into {}",
        stack.target.default_branch
    ));
    for layer in &stack.layers {
        console.field(&format!("#{} {}", layer.number, layer.branch));
    }
    console.warn("merges are irreversible and NOT atomic across PRs; later retargeting/checks may stop the operation after partial completion");
    check_settings(&stack)?;
    check_rules(
        repo,
        std::iter::once(stack.target.default_branch.clone()).chain(
            stack.layers[..stack.layers.len() - 1]
                .iter()
                .map(|e| e.branch.clone()),
        ),
    )?;
    // All layers must be ready against their current bases before ANY merge.
    for (i, layer) in stack.layers.iter().enumerate() {
        let base = if i == 0 {
            &stack.target.default_branch
        } else {
            &stack.layers[i - 1].branch
        };
        let pr = ready_pr(repo, layer, base, None)?;
        let expected_base = if i == 0 {
            &stack.base
        } else {
            &stack.layers[i - 1].old
        };
        if &pr.base_ref_oid != expected_base {
            bail!(
                "PR #{} base head changed since fetch; rerun preflight",
                layer.number
            );
        }
    }
    console.done("all PRs passed initial preflight against their current bases");
    if opts.dry_run {
        console.field("dry run: nothing merged; future retargeted requirements are not guaranteed");
        return Ok(());
    }
    if !opts.yes
        && !Confirm::new()
            .with_prompt("Merge the entire stack bottom-up?")
            .default(false)
            .interact()
            .context("confirmation unavailable; use --yes for unattended merging")?
    {
        console.field("cancelled; nothing merged");
        return Ok(());
    }
    console.section("EXECUTE");
    let mut merged = Vec::new();
    if let Err(error) = execute(&stack, &console, &mut merged) {
        let remaining: Vec<_> = stack
            .layers
            .iter()
            .filter(|e| !merged.contains(&e.number))
            .map(|e| format!("#{} ({})", e.number, e.branch))
            .collect();
        bail!(
            "merge stopped: {error:#}\nconfirmed merged: {}\nremaining/unconfirmed: {}\ninspect GitHub, then rerun from a remaining branch; completed merges are not rolled back",
            if merged.is_empty() {
                "none".into()
            } else {
                merged
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            remaining.join(", ")
        );
    }
    console.done("merged the entire stack");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ready() -> gh::MergePr {
        serde_json::from_value(serde_json::json!({"number":1,"state":"OPEN","isDraft":false,"isCrossRepository":false,
            "headRefName":"a","headRefOid":"abc","baseRefName":"main","baseRefOid":"def","mergeable":"MERGEABLE",
            "mergeStateStatus":"CLEAN","reviewDecision":"APPROVED","statusCheckRollup":[]})).unwrap()
    }
    #[test]
    fn readiness_fails_closed() {
        let layer = topology::Layer {
            branch: "a".into(),
            number: 1,
            old: "abc".into(),
            boundary: "def".into(),
        };
        assert!(validate_ready(&ready(), &layer, "main").is_ok());
        for edit in [
            "draft", "unknown", "blocked", "head", "review", "check", "closed",
        ] {
            let mut pr = ready();
            match edit {
                "draft" => pr.is_draft = true,
                "unknown" => pr.mergeable = "UNKNOWN".into(),
                "blocked" => pr.merge_state_status = "BLOCKED".into(),
                "head" => pr.head_ref_oid = "changed".into(),
                "review" => pr.review_decision = Some("REVIEW_REQUIRED".into()),
                "check" => {
                    pr.status_check_rollup = Some(vec![
                        serde_json::json!({"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE"}),
                    ])
                }
                _ => pr.state = "CLOSED".into(),
            }
            assert!(validate_ready(&pr, &layer, "main").is_err(), "{edit}");
        }
    }
}
