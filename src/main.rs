//! CLI definitions and dispatch for the stacked PR workflow.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod doctor;
mod gh;
mod git;
mod merge;
mod rebase;
mod stack;
mod topology;
mod ui;
mod view;

#[derive(Parser, Debug)]
#[command(
    name = "stackymcstackface",
    about = "Push, inspect, restack, and merge stacked GitHub pull requests.",
    long_about = "Manage stacked GitHub pull requests with Git and the GitHub CLI (`gh`).\n\n\
                  Use `push` to publish one branch and open a PR if needed. Use `view` \
                  to inspect PR bases, `rebase` to restack branches after a parent or the \
                  default branch changes, and `merge --dry-run` to check readiness.\n\n\
                  Run `stackymcstackface <command> --help` for details and safety checks.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Push the current branch and open a PR if needed.
    ///
    /// Pushes to the merge-target remote, including when working from a fork.
    /// For a new PR, the closest open PR in your ancestry becomes its base;
    /// otherwise the default branch is used. Existing PR bases are not changed.
    ///
    /// If PLAN reports a non-fast-forward update, review the remote changes
    /// first. Use `--force-with-lease` only to publish an intentional rewrite.
    Push(PushArgs),
    /// Restack every branch in the current PR stack, with conflict recovery.
    ///
    /// Start on any branch in a linear stack of open, same-repository PRs.
    /// Local tips must match the fetched and GitHub heads. The first layer
    /// goes onto the fetched default branch; each child replays only its own
    /// commits onto the rewritten parent.
    ///
    /// PLAN shows each fork point and commit count. If a child lacks the
    /// current parent tip, rebase uses parent reflogs to establish the fork
    /// point. If it cannot establish one, it stops without moving branches.
    ///
    /// Local-only by default. Choose `--push` at the start to publish the
    /// completed stack atomically. If other stack branches are checked out
    /// in clean worktrees, opt in with `--update-worktrees`.
    ///
    /// On conflict, resolve and stage with `git add`, then run `rebase
    /// --continue` in the original worktree. Use `rebase --abort` to roll back
    /// the whole local operation; it cannot undo completed publication.
    #[command(
        after_long_help = "Examples:\n  stackymcstackface rebase --push\n  stackymcstackface rebase --push --update-worktrees\n  stackymcstackface rebase --continue\n  stackymcstackface rebase --abort"
    )]
    Rebase(rebase::Options),
    /// Merge a ready stack bottom-up.
    ///
    /// Start with `merge --dry-run` to check every PR without merging.
    /// Requires a linear stack with synchronized heads and clean worktrees,
    /// merge commits enabled, and automatic head branch deletion so GitHub
    /// retargets each child. Checks, reviews, rules, and merge readiness must
    /// allow each merge.
    ///
    /// This is not atomic across PRs. If requirements change after a merge,
    /// the command stops; already merged PRs stay merged.
    Merge(merge::Options),
    /// Show active PR bases and explicitly linked merged or closed history.
    ///
    /// From a PR branch, shows its stack. From the default branch, lists all
    /// active stacks. PR numbers can be passed to `jump`. This is read-only;
    /// displayed checks and review states do not replace merge preflight.
    View,
    /// Switch to the child PR branch in the active stack.
    ///
    /// Follows current GitHub PR bases and switches only to an existing local
    /// branch. If there is more than one child, use `jump <PR_NUMBER>`.
    Up,
    /// Switch to the parent PR branch in the active stack.
    ///
    /// Follows the current GitHub PR base, not historical links. The parent
    /// must be an open PR with an existing local branch.
    Down,
    /// Switch to a local branch by the PR number shown in `view`.
    ///
    /// Takes a GitHub PR number, not a row number. The branch must already
    /// exist locally. A historical PR branch must still point to its recorded
    /// head and must not be reused by an open PR.
    Jump {
        /// GitHub pull request number.
        #[arg(value_name = "PR_NUMBER", value_parser = clap::value_parser!(u64).range(1..))]
        number: u64,
    },
    /// Check whether the local environment is ready for `push`.
    ///
    /// Reports Git and GitHub CLI availability, authentication, merge-target
    /// remote, automatic branch deletion, and current branch state. Does not
    /// fetch, push, or change GitHub settings.
    Doctor,
}

#[allow(clippy::struct_excessive_bools)] // each bool is its own clap flag
#[derive(clap::Args, Debug)]
struct PushArgs {
    /// PR title. If omitted, `gh pr create --fill` populates from commits.
    #[arg(short = 't', long)]
    title: Option<String>,

    /// PR body. If omitted, `gh pr create --fill` populates from commits.
    #[arg(short = 'b', long)]
    body: Option<String>,

    /// Open as a draft PR.
    #[arg(long)]
    draft: bool,

    /// Open the new PR in a browser instead of returning the URL.
    #[arg(long)]
    web: bool,

    /// Publish an intentional rewrite using an explicit SHA lease.
    ///
    /// Uses the remote-tracking tip recorded before this command fetches.
    /// If the remote moved, inspect and reconcile the changes before retrying.
    #[arg(long)]
    force_with_lease: bool,

    /// Accept the wrong-remote rescue prompt and, if offered, enable automatic head branch deletion.
    #[arg(short = 'y', long)]
    yes: bool,

    /// Show raw git output for troubleshooting. Repeat (`-vv`) to also pass
    /// git its own `--verbose`. Without it, fetch/push progress is hidden
    /// behind a spinner.
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    verbose: u8,
}

impl From<PushArgs> for stack::StackOpts {
    fn from(a: PushArgs) -> Self {
        Self {
            title: a.title,
            body: a.body,
            draft: a.draft,
            web: a.web,
            force_with_lease: a.force_with_lease,
            yes: a.yes,
            verbose: a.verbose,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::Push(args) => stack::run(&args.into()),
        Cmd::Rebase(opts) => rebase::run(&opts),
        Cmd::Merge(opts) => merge::run(&opts),
        Cmd::View => view::run(),
        Cmd::Up => view::navigate(&view::Direction::Up),
        Cmd::Down => view::navigate(&view::Direction::Down),
        Cmd::Jump { number } => view::navigate(&view::Direction::Jump(number)),
        Cmd::Doctor => return doctor::run(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            ui::print_error(&format!("{e:#}"));
            ExitCode::from(1)
        }
    }
}
