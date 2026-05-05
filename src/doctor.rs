//! `doctor` -- read-only environment check for the `push` workflow.
//!
//! Walks the prerequisites that `push` would otherwise hit one at a time
//! during a real run, and reports them up front as Pass / Warn / Fail.
//! Does not fetch, push, or mutate anything on GitHub. Exit code is 1 if
//! any check failed; warnings are advisory.

use std::io::IsTerminal;
use std::process::{Command, ExitCode};

use anstyle::{AnsiColor, Color, Style};

use crate::{gh, git, stack};

/// Run all checks and print a coloured report to stdout.
pub fn run() -> ExitCode {
    let palette = Palette::detect();
    let mut report = Report::new(&palette);

    let title = palette.bold();
    println!(
        "{}stackymcstackface doctor{}",
        title.render(),
        title.render_reset()
    );

    let git_ok = run_tool_checks(&mut report);
    let state = git_ok.then(|| run_repo_checks(&mut report)).flatten();
    let target = if git_ok && state.is_some() {
        run_github_checks(&mut report)
    } else {
        None
    };
    if let (Some(state), Some(target)) = (state.as_ref(), target.as_ref())
        && state.current_branch.is_some()
    {
        run_branch_checks(&mut report, state, target);
    }

    println!();
    report.print_summary();
    if report.has_fail() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

// --- check groups ----------------------------------------------------------

/// Returns true iff `git` is on PATH (other tool checks always run regardless
/// because their failure mode is informative on its own).
fn run_tool_checks(report: &mut Report<'_>) -> bool {
    report.section("Tools");
    let git_ok = report.check("`git` on PATH", probe_tool("git"));
    let gh_ok = report.check("`gh` on PATH", probe_tool("gh"));
    if gh_ok {
        report.check("`gh` authenticated", probe_gh_auth());
    }
    git_ok
}

fn run_repo_checks(report: &mut Report<'_>) -> Option<git::RepoState> {
    report.section("Repository");
    if !report.check("inside a git repository", probe_in_repo()) {
        return None;
    }
    match git::read_state() {
        Ok(state) => {
            report.check("repository has commits", Outcome::pass());
            report.check("no long-running git operation", probe_in_progress(&state));
            report.check("HEAD is on a branch (not detached)", probe_branch(&state));
            Some(state)
        }
        Err(e) => {
            report.check("repository has commits", Outcome::fail(format!("{e:#}")));
            None
        }
    }
}

fn run_github_checks(report: &mut Report<'_>) -> Option<gh::RepoInfo> {
    report.section("GitHub");
    let local = match gh::repo_info(None) {
        Ok(info) => {
            report.check(
                "`gh repo view` succeeds (GitHub remote configured)",
                Outcome::pass_with(info.name_with_owner.clone()),
            );
            info
        }
        Err(e) => {
            report.check(
                "`gh repo view` succeeds (GitHub remote configured)",
                Outcome::fail(format!("{e:#}")),
            );
            return None;
        }
    };
    let target = match stack::resolve_merge_target(&local) {
        Ok(t) => {
            let inline = if local.is_fork {
                format!("{} (this clone is a fork)", t.name_with_owner)
            } else {
                t.name_with_owner.clone()
            };
            report.check("merge target identified", Outcome::pass_with(inline));
            t
        }
        Err(e) => {
            report.check("merge target identified", Outcome::fail(format!("{e:#}")));
            return None;
        }
    };
    report.check(
        "`delete_branch_on_merge` enabled on merge target",
        probe_delete_on_merge(&target),
    );
    report.check(
        "merge-target remote uniquely resolvable",
        probe_target_remote(&target),
    );
    Some(target)
}

fn run_branch_checks(report: &mut Report<'_>, state: &git::RepoState, target: &gh::RepoInfo) {
    report.section("Current branch");
    report.check(
        "on a feature branch (not the default branch)",
        probe_not_default(state, target),
    );
    report.check(
        "upstream tracks the merge-target remote",
        probe_upstream(state, target),
    );
}

// --- probes ---------------------------------------------------------------

fn probe_tool(name: &str) -> Outcome {
    match Command::new(name).arg("--version").output() {
        Ok(out) if out.status.success() => {
            let line = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            if line.is_empty() {
                Outcome::pass()
            } else {
                Outcome::pass_with(line)
            }
        }
        Ok(_) => Outcome::fail(format!("`{name} --version` exited non-zero")),
        Err(_) => Outcome::fail(format!("`{name}` not found on PATH. Install it and retry.")),
    }
}

fn probe_gh_auth() -> Outcome {
    let out = match Command::new("gh").args(["auth", "status"]).output() {
        Ok(o) => o,
        Err(e) => return Outcome::fail(format!("could not invoke `gh auth status`: {e}")),
    };
    if out.status.success() {
        Outcome::pass()
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let prefix = if stderr.is_empty() {
            String::from("not authenticated")
        } else {
            stderr
        };
        Outcome::fail(format!("{prefix}\nFix: gh auth login"))
    }
}

fn probe_in_repo() -> Outcome {
    match Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
    {
        Ok(o) if o.status.success() => Outcome::pass(),
        _ => Outcome::fail("not inside a git working tree (run from a clone)"),
    }
}

fn probe_in_progress(state: &git::RepoState) -> Outcome {
    match state.in_progress {
        None => Outcome::pass(),
        Some(op) => Outcome::fail(format!(
            "a {label} is in progress; resolve it before running `push`",
            label = op.label(),
        )),
    }
}

fn probe_branch(state: &git::RepoState) -> Outcome {
    match &state.current_branch {
        Some(b) => Outcome::pass_with(b.clone()),
        None => Outcome::fail(
            "HEAD is detached; `push` requires a named branch. \
             Check out (or create) a branch first.",
        ),
    }
}

fn probe_delete_on_merge(target: &gh::RepoInfo) -> Outcome {
    if target.delete_branch_on_merge {
        Outcome::pass()
    } else {
        Outcome::warn(format!(
            "off on {repo}; once a stack PR merges, the next PR will not \
             auto-retarget. Enable with:\n  \
             gh api -X PATCH /repos/{repo} -f delete_branch_on_merge=true",
            repo = target.name_with_owner,
        ))
    }
}

fn probe_target_remote(target: &gh::RepoInfo) -> Outcome {
    match stack::resolve_merge_target_remote(target) {
        Ok(name) => Outcome::pass_with(name),
        Err(e) => Outcome::fail(format!("{e:#}")),
    }
}

fn probe_not_default(state: &git::RepoState, target: &gh::RepoInfo) -> Outcome {
    let Some(branch) = state.current_branch.as_deref() else {
        // Already reported as Fail by the detached-HEAD check; suppress here.
        return Outcome::warn("HEAD is detached (see above)");
    };
    if branch == target.default_branch {
        Outcome::warn(format!(
            "current branch `{branch}` is the default branch of {repo}; \
             `push` would refuse to run here. Create a feature branch first.",
            repo = target.name_with_owner,
        ))
    } else {
        Outcome::pass_with(branch.to_string())
    }
}

fn probe_upstream(state: &git::RepoState, target: &gh::RepoInfo) -> Outcome {
    let target_remote = match stack::resolve_merge_target_remote(target) {
        Ok(n) => n,
        Err(_) => return Outcome::warn("merge-target remote unresolved (see above)"),
    };
    match &state.upstream {
        None => Outcome::pass_with("no upstream set yet (fresh branch)"),
        Some((remote, _)) if remote == &target_remote => {
            Outcome::pass_with(format!("tracks `{remote}`"))
        }
        Some((remote, _)) => Outcome::warn(format!(
            "branch tracks `{remote}` but merge target is `{target_remote}`. \
             `push` will offer a wrong-remote rescue."
        )),
    }
}

// --- report plumbing ------------------------------------------------------

#[derive(Clone, Copy)]
enum Status {
    Pass,
    Warn,
    Fail,
}

/// One check's verdict plus optional rendered detail. `inline` appears on the
/// same line after the label (used for short, positive context such as a
/// version string); `detail` is printed on subsequent indented lines (used
/// for warning/failure explanations).
struct Outcome {
    status: Status,
    inline: Option<String>,
    detail: Option<String>,
}

impl Outcome {
    fn pass() -> Self {
        Self {
            status: Status::Pass,
            inline: None,
            detail: None,
        }
    }
    fn pass_with(inline: impl Into<String>) -> Self {
        Self {
            status: Status::Pass,
            inline: Some(inline.into()),
            detail: None,
        }
    }
    fn warn(detail: impl Into<String>) -> Self {
        Self {
            status: Status::Warn,
            inline: None,
            detail: Some(detail.into()),
        }
    }
    fn fail(detail: impl Into<String>) -> Self {
        Self {
            status: Status::Fail,
            inline: None,
            detail: Some(detail.into()),
        }
    }
}

struct Report<'p> {
    palette: &'p Palette,
    pass: u32,
    warn: u32,
    fail: u32,
}

impl<'p> Report<'p> {
    fn new(palette: &'p Palette) -> Self {
        Self {
            palette,
            pass: 0,
            warn: 0,
            fail: 0,
        }
    }

    fn section(&self, name: &str) {
        let bold = self.palette.bold();
        println!("\n{}{name}{}", bold.render(), bold.render_reset());
    }

    /// Print the result line and bump tallies. Returns `true` for Pass so
    /// the caller can gate dependent checks (Warn/Fail returns `false`).
    fn check(&mut self, label: &str, outcome: Outcome) -> bool {
        let (style, glyph) = match outcome.status {
            Status::Pass => (self.palette.green(), "✔"),
            Status::Warn => (self.palette.yellow(), "⚠"),
            Status::Fail => (self.palette.red(), "✗"),
        };
        let dim = self.palette.dim();
        let inline = match &outcome.inline {
            Some(s) => format!(" {}({s}){}", dim.render(), dim.render_reset()),
            None => String::new(),
        };
        println!(
            "  {sty}{glyph}{rst} {label}{inline}",
            sty = style.render(),
            rst = style.render_reset(),
        );
        if let Some(detail) = &outcome.detail {
            for line in detail.lines() {
                println!(
                    "      {dim}{line}{rdim}",
                    dim = dim.render(),
                    rdim = dim.render_reset(),
                );
            }
        }
        match outcome.status {
            Status::Pass => self.pass += 1,
            Status::Warn => self.warn += 1,
            Status::Fail => self.fail += 1,
        }
        matches!(outcome.status, Status::Pass)
    }

    fn print_summary(&self) {
        let bold = self.palette.bold();
        let g = self.palette.green();
        let y = self.palette.yellow();
        let r = self.palette.red();
        println!(
            "{b}Summary:{rb} {g}{p} passed{rg}, {y}{w} warning(s){ry}, {r}{f} failed{rr}.",
            b = bold.render(),
            rb = bold.render_reset(),
            g = g.render(),
            rg = g.render_reset(),
            y = y.render(),
            ry = y.render_reset(),
            r = r.render(),
            rr = r.render_reset(),
            p = self.pass,
            w = self.warn,
            f = self.fail,
        );
    }

    fn has_fail(&self) -> bool {
        self.fail > 0
    }
}

// --- colour ---------------------------------------------------------------

/// Wraps anstyle's `Style` with a single on/off switch determined by tty
/// detection and the `NO_COLOR` env var. Disabled palettes hand back empty
/// `Style`s, whose `render*()` output is empty -- so callers never need to
/// branch on whether colour is on.
struct Palette {
    enabled: bool,
}

impl Palette {
    fn detect() -> Self {
        Self {
            enabled: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    fn maybe(&self, s: Style) -> Style {
        if self.enabled { s } else { Style::new() }
    }

    fn green(&self) -> Style {
        self.maybe(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Green))))
    }
    fn yellow(&self) -> Style {
        self.maybe(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Yellow))))
    }
    fn red(&self) -> Style {
        self.maybe(Style::new().fg_color(Some(Color::Ansi(AnsiColor::Red))))
    }
    fn bold(&self) -> Style {
        self.maybe(Style::new().bold())
    }
    fn dim(&self) -> Style {
        self.maybe(Style::new().dimmed())
    }
}
