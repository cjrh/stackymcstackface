//! `push` -- a drop-in replacement for `git push` that opens (stacked) PRs.
//!
//! See `stack.rs` for the orchestration logic. This file just defines the CLI
//! and dispatches to it.

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
    about = "Drop-in replacement for `git push` that opens a (stacked) PR on first push.",
    long_about = "A drop-in replacement for `git push`. Three cases, picked \
                  automatically:\n\n  \
                  * branch already has an open PR -- just pushes the new commits\n  \
                  * new branch off an open PR     -- pushes and opens a stacked PR\n  \
                  * new branch off the default    -- pushes and opens a regular PR\n\n\
                  The push goes to the merge-target remote (the repo where the \
                  PR is created). Stack topology is discovered from `git` and `gh`; \
                  rebase keeps only temporary recovery state. Also supports \
                  whole-stack rebase, guarded bottom-up merge, stack viewing, \
                  and branch navigation.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Push the current branch (drop-in `git push`); opens a (stacked) PR on first push.
    Push(PushArgs),
    /// Rebase every branch in the current PR stack, with conflict recovery.
    Rebase(rebase::Options),
    /// Merge a fully ready stack bottom-up, stopping safely if requirements change.
    Merge(merge::Options),
    /// Show the current PR stack and explicitly linked merged/closed history.
    View,
    /// Switch to the next (child) PR branch in the active stack.
    Up,
    /// Switch to the prior (parent) PR branch in the active stack.
    Down,
    /// Switch to a local branch by the PR number shown in `view`.
    Jump {
        #[arg(value_parser = clap::value_parser!(u64).range(1..))]
        number: u64,
    },
    /// Diagnose whether the environment is set up for `push` (read-only).
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

    /// Push with `--force-with-lease`. Use after a local rebase.
    #[arg(long)]
    force_with_lease: bool,

    /// Skip interactive prompts (assume "yes" to safe rescues).
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
