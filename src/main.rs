//! `push` -- a drop-in replacement for `git push` that opens (stacked) PRs.
//!
//! See `stack.rs` for the orchestration logic. This file just defines the CLI
//! and dispatches to it.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod doctor;
mod gh;
mod git;
mod stack;

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
                  PR is created). No local stack state is kept: every invocation \
                  reconstructs the picture from `git` and `gh`.",
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
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Push(args) => match stack::run(&args.into()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e:#}");
                ExitCode::from(1)
            }
        },
        Cmd::Doctor => doctor::run(),
    }
}
