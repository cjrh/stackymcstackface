//! `stack` -- automate stacked PR creation on GitHub.
//!
//! See `stack.rs` for the orchestration logic. This file just defines the CLI
//! and dispatches to it.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod gh;
mod git;
mod stack;

#[derive(Parser, Debug)]
#[command(
    name = "stackymcstackface",
    about = "Push the current branch and open a (stacked) PR on GitHub.",
    long_about = "Pushes the current branch to the merge-target remote (the repo \
                  where the PR is created), then opens a PR. If the branch \
                  descends from another open PR's head, the new PR is stacked \
                  on top of it; otherwise it targets the default branch.\n\n\
                  No local stack state is kept: every invocation reconstructs \
                  the picture from `git` and `gh`.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Push the current branch and open a (stacked) PR.
    Stack(StackArgs),
}

#[allow(clippy::struct_excessive_bools)] // each bool is its own clap flag
#[derive(clap::Args, Debug)]
struct StackArgs {
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
}

impl From<StackArgs> for stack::StackOpts {
    fn from(a: StackArgs) -> Self {
        Self {
            title: a.title,
            body: a.body,
            draft: a.draft,
            web: a.web,
            force_with_lease: a.force_with_lease,
            yes: a.yes,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::Stack(args) => stack::run(&args.into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}
