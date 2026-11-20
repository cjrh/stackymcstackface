//! Read-only PR topology and local branch navigation. Current bases define
//! active edges; explicit body footers supply a separate, bounded history.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write;

use anyhow::{Context, Result, bail};

use crate::gh::{PrState, ViewPr};
use crate::ui::{self, Palette};
use crate::{gh, git, stack, topology};

const HISTORY_LIMIT: usize = 100;

pub enum Direction {
    Up,
    Down,
    Jump(u64),
}

/// Accept a standalone link or the footer emitted by push, not incidental
/// mentions, quoted text, or references to other repositories.
fn footer_parent(body: &str) -> Result<Option<u64>> {
    let mut parents = BTreeSet::new();
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("Stacked on #") else {
            continue;
        };
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let suffix = rest[end..].trim();
        if !suffix.is_empty()
            && !(suffix.starts_with("(`")
                && suffix.ends_with(
                    ") by [stackymcstackface](https://github.com/cjrh/stackymcstackface) ❤️",
                ))
        {
            continue;
        }
        let number = rest[..end]
            .parse::<u64>()
            .context("invalid parent footer")?;
        if number == 0 {
            bail!("invalid parent footer #0");
        }
        parents.insert(number);
    }
    if parents.len() > 1 {
        bail!("multiple parent footers");
    }
    Ok(parents.first().copied())
}

#[derive(Default)]
struct ActiveGraph {
    by_head: BTreeMap<String, Vec<u64>>,
    parents: BTreeMap<u64, Vec<u64>>,
    children: BTreeMap<u64, Vec<u64>>,
}

impl ActiveGraph {
    fn new(prs: &BTreeMap<u64, ViewPr>, default: &str) -> Self {
        let mut graph = Self::default();
        for pr in prs.values().filter(|p| p.state == PrState::Open) {
            graph
                .by_head
                .entry(pr.head_ref_name.clone())
                .or_default()
                .push(pr.number);
        }
        for pr in prs.values().filter(|p| p.state == PrState::Open) {
            if pr.base_ref_name == default {
                continue;
            }
            if let Some(parents) = graph.by_head.get(&pr.base_ref_name) {
                graph.parents.insert(pr.number, parents.clone());
                for parent in parents {
                    graph.children.entry(*parent).or_default().push(pr.number);
                }
            }
        }
        graph
    }

    /// Select a connected active component, including siblings. Reject only
    /// ambiguities in this component, not unrelated stacks elsewhere.
    fn component(&self, seed: u64, prs: &BTreeMap<u64, ViewPr>) -> Result<BTreeSet<u64>> {
        let mut selected = BTreeSet::new();
        let mut pending = vec![seed];
        while let Some(number) = pending.pop() {
            if !selected.insert(number) {
                continue;
            }
            let pr = &prs[&number];
            if self
                .by_head
                .get(&pr.head_ref_name)
                .is_some_and(|ids| ids.len() > 1)
            {
                bail!("multiple open PRs have head `{}`", pr.head_ref_name);
            }
            pending.extend(self.parents.get(&number).into_iter().flatten());
            pending.extend(self.children.get(&number).into_iter().flatten());
        }
        let mut checked = BTreeSet::new();
        for &number in &selected {
            let mut seen = BTreeSet::new();
            let mut cursor = number;
            while !checked.contains(&cursor) {
                if !seen.insert(cursor) {
                    bail!("cycle in active PR stack");
                }
                let Some(parent) = self.parents.get(&cursor).and_then(|p| p.first()) else {
                    break;
                };
                cursor = *parent;
            }
            checked.extend(seen);
        }
        Ok(selected)
    }
}

struct Overview {
    repo: String,
    default: String,
    branch: String,
    current: Option<u64>,
    prs: BTreeMap<u64, ViewPr>,
    graph: ActiveGraph,
    selected: BTreeSet<u64>,
    history: BTreeMap<u64, u64>,
    warnings: Vec<String>,
}

fn check_limit(prs: &[ViewPr]) -> Result<()> {
    if prs.len() >= 1000 {
        bail!("PR discovery reached its 1000-PR limit; refusing a possibly truncated stack");
    }
    Ok(())
}

fn discover(history: bool, status: bool) -> Result<Overview> {
    let branch = git::read_state()?
        .current_branch
        .context("HEAD is detached; check out a PR branch first")?;
    let target = stack::resolve_merge_target(&gh::repo_info(None)?)?;
    let listed = gh::list_view_prs(&target.name_with_owner, None, history, status)?;
    check_limit(&listed)?;
    let mut prs: BTreeMap<_, _> = listed
        .into_iter()
        .filter(|p| !p.is_cross_repository)
        .map(|p| (p.number, p))
        .collect();
    let graph = ActiveGraph::new(&prs, &target.default_branch);
    let current = if branch == target.default_branch {
        None
    } else {
        match graph.by_head.get(&branch).map(Vec::as_slice) {
            Some([number]) => Some(*number),
            Some(_) => bail!("multiple open PRs have head `{branch}`"),
            None if history => {
                let found = gh::list_view_prs(&target.name_with_owner, Some(&branch), true, false)?;
                check_limit(&found)?;
                let found: Vec<_> = found
                    .into_iter()
                    .filter(|p| !p.is_cross_repository && p.head_ref_name == branch)
                    .collect();
                match found.as_slice() {
                    [pr] if pr.state != PrState::Open => {
                        prs.insert(pr.number, pr.clone());
                        Some(pr.number)
                    }
                    [] => bail!(
                        "no same-repository PR for `{branch}`; check out a PR branch or the default branch"
                    ),
                    _ => bail!(
                        "ambiguous or changed PR history for `{branch}`; check out an open PR branch and retry"
                    ),
                }
            }
            None => bail!(
                "no same-repository open PR for `{branch}`; use `view` and `jump` for historical branches"
            ),
        }
    };
    let mut selected = BTreeSet::new();
    if let Some(number) = current {
        selected.extend(graph.component(number, &prs)?);
    } else {
        for &number in prs.keys() {
            if !selected.contains(&number) {
                selected.extend(graph.component(number, &prs)?);
            }
        }
    }
    let mut overview = Overview {
        repo: target.name_with_owner,
        default: target.default_branch,
        branch,
        current,
        prs,
        graph,
        selected,
        history: BTreeMap::new(),
        warnings: Vec::new(),
    };
    if history {
        overview.load_history()?;
    }
    Ok(overview)
}

impl Overview {
    /// Bodies arrive with the active list. Only referenced, absent PRs require
    /// extra requests, once per number. No age cutoff or repository history scan.
    fn load_history(&mut self) -> Result<()> {
        let mut referring: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for pr in self.prs.values() {
            if let Ok(Some(parent)) = footer_parent(pr.body.as_deref().unwrap_or_default()) {
                referring.entry(parent).or_default().push(pr.number);
            }
        }
        let mut pending: VecDeque<_> = self.selected.iter().copied().collect();
        let mut processed = BTreeSet::new();
        let mut attempted = BTreeSet::new();
        while let Some(number) = pending.pop_front() {
            if !processed.insert(number) {
                continue;
            }
            if self.prs[&number].state != PrState::Open {
                // After retargeting, multiple live roots can still explicitly
                // name the same historical parent. Keep them in the same view.
                for child in referring.get(&number).into_iter().flatten() {
                    for member in self.graph.component(*child, &self.prs)? {
                        if self.selected.insert(member) {
                            pending.push_back(member);
                        }
                    }
                }
            }
            let parent = match footer_parent(self.prs[&number].body.as_deref().unwrap_or_default())
            {
                Ok(Some(parent)) => parent,
                Ok(None) => continue,
                Err(error) => {
                    self.warnings
                        .push(format!("#{number}: {error}; history omitted"));
                    continue;
                }
            };
            if !self.prs.contains_key(&parent) {
                if attempted.len() >= HISTORY_LIMIT {
                    self.warnings.push(format!(
                        "history lookup limit ({HISTORY_LIMIT}) reached; remaining links omitted"
                    ));
                    break;
                }
                if !attempted.insert(parent) {
                    continue;
                }
                match gh::view_pr(&self.repo, parent) {
                    Ok(pr)
                        if pr.number == parent
                            && !pr.is_cross_repository
                            && pr.state != PrState::Open =>
                    {
                        self.prs.insert(parent, pr);
                    }
                    Ok(_) => {
                        self.warnings.push(format!("#{number}: linked #{parent} is cross-repository or changed; history omitted"));
                        continue;
                    }
                    Err(_) => {
                        self.warnings.push(format!(
                            "#{number}: linked #{parent} is unavailable; history omitted"
                        ));
                        continue;
                    }
                }
            }
            // A footer must never override a live base or connect unrelated
            // open PRs after somebody manually changes their bases.
            if self.prs[&parent].state == PrState::Open {
                continue;
            }
            let mut cursor = parent;
            let mut cycle = cursor == number;
            while !cycle {
                let Some(next) = self.history.get(&cursor) else {
                    break;
                };
                cursor = *next;
                cycle = cursor == number;
            }
            if cycle {
                self.warnings.push(format!(
                    "#{number}: cyclic history link to #{parent} omitted"
                ));
                continue;
            }
            self.history.insert(number, parent);
            if self.selected.insert(parent) {
                pending.push_back(parent);
            }
        }
        Ok(())
    }

    fn row(&self, number: u64, palette: &Palette) -> String {
        let pr = &self.prs[&number];
        let (state, color) = match pr.state {
            PrState::Open if pr.is_draft => ("DRAFT", palette.yellow()),
            PrState::Open => ("OPEN", palette.green()),
            PrState::Closed => ("CLOSED", palette.red()),
            PrState::Merged => ("MERGED", palette.green()),
        };
        format!(
            "{} [{}] {} — {}{}",
            ui::styled(&format!("#{number}"), palette.pr_number()),
            ui::styled(state, color),
            ui::styled(&plain(&pr.head_ref_name), palette.branch()),
            plain(&pr.title),
            if self.current == Some(number) {
                ui::styled("  <- current", palette.bold())
            } else {
                String::new()
            }
        )
    }

    fn render(&self, palette: &Palette) -> String {
        let mut out = format!(
            "{} · current {} · default {}\n\nACTIVE STACK (current PR bases)\n",
            plain(&self.repo),
            plain(&self.branch),
            plain(&self.default)
        );
        let roots: Vec<_> = self
            .selected
            .iter()
            .copied()
            .filter(|n| self.prs[n].state == PrState::Open && !self.graph.parents.contains_key(n))
            .collect();
        if roots.is_empty() {
            out.push_str("No active PRs in this view.\n");
        }
        for root in roots {
            let mut pending = vec![root];
            while let Some(number) = pending.pop() {
                let pr = &self.prs[&number];
                let base = ui::styled(&plain(&pr.base_ref_name), palette.branch());
                let base = match self.graph.parents.get(&number).and_then(|p| p.first()) {
                    Some(parent) => format!(
                        "{} ({base})",
                        ui::styled(&format!("#{parent}"), palette.pr_number())
                    ),
                    None => base,
                };
                let _ = writeln!(out, "\n{}", self.row(number, palette));
                let _ = writeln!(out, "base: {base} · {}", pr_status(pr, palette));
                if let Some(children) = self.graph.children.get(&number) {
                    pending.extend(children.iter().rev());
                }
            }
        }
        let historical: Vec<_> = self
            .selected
            .iter()
            .filter(|n| self.prs[n].state != PrState::Open)
            .collect();
        if !historical.is_empty() {
            out.push_str("\nHISTORY (explicit footers, not current bases)\n");
            for number in historical {
                let _ = writeln!(out, "{}", self.row(*number, palette));
            }
            for (child, parent) in &self.history {
                let _ = writeln!(
                    out,
                    "{} previously stacked on {}",
                    ui::styled(&format!("#{child}"), palette.pr_number()),
                    ui::styled(&format!("#{parent}"), palette.pr_number())
                );
            }
        }
        for warning in &self.warnings {
            let _ = writeln!(out, "\nWarning: {}", plain(warning));
        }
        out.push_str("\nUse `jump <PR number>` to switch to an existing local branch.\n");
        out
    }

    fn destination(&self, direction: &Direction) -> Result<u64> {
        match direction {
            Direction::Jump(number) => {
                if !self.selected.contains(number) {
                    bail!("PR #{number} is not in this view; run `stackymcstackface view` again");
                }
                Ok(*number)
            }
            Direction::Up | Direction::Down => {
                let current = self
                    .current
                    .context("the default branch is not a PR; use `view` and `jump`")?;
                let neighbors = if matches!(direction, Direction::Up) {
                    self.graph.children.get(&current)
                } else {
                    self.graph.parents.get(&current)
                };
                match neighbors.map(Vec::as_slice).unwrap_or_default() {
                    [number] => Ok(*number),
                    [] => bail!(
                        "no {} PR in the active stack",
                        if matches!(direction, Direction::Up) {
                            "next"
                        } else {
                            "prior"
                        }
                    ),
                    choices => bail!(
                        "multiple next PRs: {}; use `jump <PR number>`",
                        choices
                            .iter()
                            .map(|n| format!("#{n}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
        }
    }
}

/// GitHub's aggregate signals are independent. In particular, MERGEABLE
/// means no conflicts, not that reviews, checks, or base freshness are clear.
fn pr_status(pr: &ViewPr, palette: &Palette) -> String {
    let (merge, merge_color) = match pr.mergeable.as_deref() {
        Some("MERGEABLE") => ("no conflicts", palette.green()),
        Some("CONFLICTING") => ("conflicts", palette.red()),
        _ => ("unknown", palette.yellow()),
    };
    let (state, state_color) = match pr.merge_state_status.as_deref() {
        Some("BEHIND") => ("behind base", palette.yellow()),
        Some("BLOCKED") => ("blocked", palette.yellow()),
        Some("CLEAN") => ("clean", palette.green()),
        Some("DIRTY") => ("conflicts", palette.red()),
        Some("DRAFT") => ("draft", palette.yellow()),
        Some("HAS_HOOKS") => ("hooks pending", palette.yellow()),
        Some("UNSTABLE") => ("checks not passing", palette.yellow()),
        _ => ("unknown", palette.yellow()),
    };
    let (reviews, review_color) = match pr.review_decision.as_deref() {
        Some("APPROVED") => ("approved", palette.green()),
        Some("CHANGES_REQUESTED") => ("changes requested", palette.red()),
        Some("REVIEW_REQUIRED") => ("required", palette.yellow()),
        None | Some("") => ("no decision", palette.dim()),
        _ => ("unknown", palette.yellow()),
    };
    format!(
        "merge: {} · GitHub: {} · reviews: {}",
        ui::styled(merge, merge_color),
        ui::styled(state, state_color),
        ui::styled(reviews, review_color)
    )
}

/// GitHub titles and bodies are untrusted terminal text.
fn plain(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

pub fn run() -> Result<()> {
    print!("{}", discover(true, true)?.render(&Palette::detect()));
    Ok(())
}

pub fn navigate(direction: &Direction) -> Result<()> {
    let _lock = topology::lock()?;
    topology::no_rebase()?;
    let state = git::read_state()?;
    if let Some(op) = state.in_progress {
        bail!(
            "a {} is already in progress; finish it before navigating",
            op.label()
        );
    }
    let overview = discover(matches!(direction, Direction::Jump(_)), false)?;
    for warning in &overview.warnings {
        eprintln!("Warning: {}", plain(warning));
    }
    let number = overview.destination(direction)?;
    let pr = &overview.prs[&number];
    if pr.state != PrState::Open {
        let local = git::resolve(&format!("refs/heads/{}", pr.head_ref_name))?;
        if overview.graph.by_head.contains_key(&pr.head_ref_name)
            || local.as_deref() != Some(&pr.head_ref_oid)
        {
            bail!(
                "historical branch `{}` is missing, changed, or reused; refusing to switch to a different PR's work",
                plain(&pr.head_ref_name)
            );
        }
    }
    // Git itself protects uncommitted files and other worktrees. No force,
    // autostash, fetch, branch creation, or history rewriting occurs here.
    git::switch_local(&pr.head_ref_name)?;
    println!("Switched to {} (PR #{number})", plain(&pr.head_ref_name));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, head: &str, base: &str) -> ViewPr {
        ViewPr {
            number,
            head_ref_name: head.into(),
            base_ref_name: base.into(),
            title: format!("PR {number}"),
            state: PrState::Open,
            is_draft: false,
            is_cross_repository: false,
            head_ref_oid: "abc".into(),
            body: None,
            mergeable: None,
            merge_state_status: None,
            review_decision: None,
        }
    }

    #[test]
    fn selects_tree_from_middle_not_unrelated_roots() {
        let prs = [
            pr(1, "a", "main"),
            pr(2, "b", "a"),
            pr(3, "c", "a"),
            pr(4, "other", "main"),
        ]
        .into_iter()
        .map(|p| (p.number, p))
        .collect();
        let graph = ActiveGraph::new(&prs, "main");
        assert_eq!(graph.component(2, &prs).unwrap(), BTreeSet::from([1, 2, 3]));
        assert_eq!(graph.children[&1], vec![2, 3]);
    }

    #[test]
    fn refuses_cycles_and_duplicate_heads() {
        for items in [
            vec![pr(1, "a", "b"), pr(2, "b", "a")],
            vec![pr(1, "a", "main"), pr(2, "a", "main")],
            vec![pr(1, "a", "a")],
        ] {
            let prs = items.into_iter().map(|p| (p.number, p)).collect();
            assert!(ActiveGraph::new(&prs, "main").component(1, &prs).is_err());
        }
    }

    #[test]
    fn parses_only_explicit_parent_lines() {
        assert_eq!(footer_parent("Body\n\n---\n\nStacked on #42 (`feat/a`) by [stackymcstackface](https://github.com/cjrh/stackymcstackface) ❤️").unwrap(), Some(42));
        assert_eq!(footer_parent("Stacked on #42").unwrap(), Some(42));
        for body in [
            "text Stacked on #42",
            "> Stacked on #42",
            "Stacked on #42 elsewhere",
            "Stacked on #42/repo",
        ] {
            assert_eq!(footer_parent(body).unwrap(), None);
        }
        assert!(footer_parent("Stacked on #1\nStacked on #2").is_err());
        assert!(footer_parent("Stacked on #0").is_err());
    }

    #[test]
    fn deep_stacks_stay_flat_and_color_is_optional() {
        let prs: BTreeMap<_, _> = (1..=128)
            .map(|n| {
                let base = if n == 1 {
                    "main".into()
                } else {
                    format!("layer-{}", n - 1)
                };
                (n, pr(n, &format!("layer-{n}"), &base))
            })
            .collect();
        let graph = ActiveGraph::new(&prs, "main");
        let overview = Overview {
            repo: "owner/repo".into(),
            default: "main".into(),
            branch: "layer-128".into(),
            current: Some(128),
            selected: graph.component(128, &prs).unwrap(),
            prs,
            graph,
            history: BTreeMap::new(),
            warnings: Vec::new(),
        };
        let plain = overview.render(&Palette::for_test(false));
        assert!(!plain.contains('\x1b'));
        assert_eq!(plain.lines().filter(|l| l.starts_with('#')).count(), 128);
        assert!(plain.lines().all(|l| !l.starts_with(' ')));
        assert!(plain.contains("base: #127 (layer-127)"));
        assert!(plain.contains("#128 [OPEN] layer-128 — PR 128  <- current"));
        assert!(plain.find("#1 [").unwrap() < plain.find("#128 [").unwrap());
        let palette = Palette::for_test(true);
        let colored = overview.render(&palette);
        assert!(colored.contains('\x1b'));
        assert!(colored.contains(&ui::styled("#128", palette.pr_number())));
        assert!(colored.contains(&ui::styled("layer-128", palette.branch())));
        assert_ne!(ui::styled("#128", palette.pr_number()), "#128");
        assert_ne!(ui::styled("layer-128", palette.branch()), "layer-128");
    }

    #[test]
    fn status_signals_remain_independent_and_unknown_is_not_success() {
        let palette = Palette::for_test(false);
        let mut pr = pr(1, "a", "main");
        assert_eq!(
            pr_status(&pr, &palette),
            "merge: unknown · GitHub: unknown · reviews: no decision"
        );
        for (value, label) in [
            ("MERGEABLE", "no conflicts"),
            ("CONFLICTING", "conflicts"),
            ("UNKNOWN", "unknown"),
            ("NEW_VALUE", "unknown"),
        ] {
            pr.mergeable = Some(value.into());
            assert!(pr_status(&pr, &palette).starts_with(&format!("merge: {label} ·")));
        }
        pr.mergeable = Some("MERGEABLE".into());
        for (value, label) in [
            ("BEHIND", "behind base"),
            ("BLOCKED", "blocked"),
            ("CLEAN", "clean"),
            ("DIRTY", "conflicts"),
            ("DRAFT", "draft"),
            ("HAS_HOOKS", "hooks pending"),
            ("UNSTABLE", "checks not passing"),
            ("UNKNOWN", "unknown"),
            ("NEW_VALUE", "unknown"),
        ] {
            pr.merge_state_status = Some(value.into());
            let status = pr_status(&pr, &palette);
            assert!(status.contains(&format!("GitHub: {label} ·")), "{status}");
            assert!(status.starts_with("merge: no conflicts"));
        }
        for (value, label) in [
            ("APPROVED", "approved"),
            ("CHANGES_REQUESTED", "changes requested"),
            ("REVIEW_REQUIRED", "required"),
            ("", "no decision"),
            ("NEW_VALUE", "unknown"),
        ] {
            pr.review_decision = Some(value.into());
            assert!(pr_status(&pr, &palette).ends_with(&format!("reviews: {label}")));
        }
    }

    #[test]
    fn terminal_text_cannot_add_control_sequences() {
        assert_eq!(plain("a\n\x1b[2J\tb"), "a  [2J b");
    }
}
