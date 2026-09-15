# stackymcstackface

A drop-in replacement for `git push` that also opens a pull request the
first time you push a branch, and stacks the PR correctly when your
branch descends from another open PR. Uses the official
[`gh`](https://cli.github.com/) CLI (must be installed).

Four subcommands: `push`, `rebase`, `merge`, and the read-only `doctor`.
Stacks are discovered from Git and GitHub, not a permanent local database.

`stackymcstackface push` figures out which of three things you meant:

1. **Branch already has an open PR**: just push the new commits.
2. **New branch on top of an open PR**: push and open a stacked PR
   with the parent branch as its base.
3. **New branch off the default branch**: push and open a regular PR.

The first case is the boring one, and that's the point: replace `git push`
with `stackymcstackface push` in your muscle memory and you never have to think about
which case applies.

You can make a shell alias like `alias sms=stackymcstackface` or whatever
you like for daily use. See [Install](#install).

## Goals

The manual stacked-PR workflow on GitHub is not complicated:
- push your branch to the merge-target repo (not your fork, if you are on a fork)
- open a PR with the parent branch (a separate PR) as the base (not `main`)
- when the parent PR is merged, and that branch is deleted, GitHub automatically retargets the next PR so its base becomes `main`

You can create a chain of these, which means you can keep working, doing new
fixes and features in new PRs stacked on top of previous PRs, without waiting for the earlier ones to merge.

But the process has two fiddly bits that are easy to get wrong:

1. The branches must live on the same remote as the PRs themselves
   (otherwise stacking silently breaks, which is particularly painful when
   working from a fork).
2. When you create the PR, you have to pick the right base branch by hand, in the UI:
   the parent branch in the stack, not `main`.

`stackymcstackface push` automates those two steps. `rebase` updates a whole
linear stack with Git-style conflict recovery; `merge` checks and merges a
ready stack bottom-up. Reviewing stays on GitHub.

There is one important prerequisite: when a stack PR merges, GitHub only
auto-retargets the next PR if the merged head branch is deleted through
GitHub's own merge flow. That means the post-merge "Delete branch"
button on the PR page, or the repo-level "Automatically delete head
branches" setting. Manual `git push --delete origin <branch>` (or the
equivalent low-level API call) bypasses the retarget logic and **closes**
the next PR instead. See [Repo setup](#repo-setup-one-time) and
[Merging a stack](#8-merging-a-stack).


## Demo

The same command in three situations:

**1. First push of a new branch:**

```sh
git checkout -b feat/parser
# ...commit...
sms push
```

Output:
```text
stackymcstackface

PLAN
  octocat/widgets · default main · remote origin
  push feat/parser and open a PR with base main

EXECUTE
  ✔ pushed feat/parser to origin
  ✔ opened pull request

  https://github.com/octocat/widgets/pull/421
```

Pushed, PR opened. The fetch, PR scan, and push run behind spinners; pass
`-v` to stream git's raw output instead (handy for troubleshooting).

*If your local clone is a fork of the merge target, `sms push` sends the branch to the **parent** repo (e.g. `upstream`), not your fork; this is handled automatically. See [Caveats](#caveats).*

**2. First push of a branch off that one:**

```sh
git checkout -b feat/parser-tests   # off feat/parser
# ...commit...
sms push
```

Output:
```text
stackymcstackface

PLAN
  octocat/widgets · default main · remote origin
  push feat/parser-tests and open a stacked PR with base feat/parser
    parent: #421 https://github.com/octocat/widgets/pull/421

EXECUTE
  ✔ pushed feat/parser-tests to origin
  ✔ opened pull request
  ✔ noted parent #421 in description

  https://github.com/octocat/widgets/pull/422
```

Pushed, *stacked* PR opened: base is `feat/parser` (not `main`), and a `Stacked on #421` line is appended to the PR body.

**3. Any later push to either branch:**

```sh
# ...more commits...
sms push
```

```text
stackymcstackface

PLAN
  octocat/widgets · default main · remote origin
  push new commits to feat/parser-tests
    PR #422 already open

EXECUTE
  ✔ pushed feat/parser-tests to origin

  https://github.com/octocat/widgets/pull/422
  base on GitHub: feat/parser
```

Just pushes. No prompts, no PR mutations.

### Caveats

**Stacking requires push access to the merge-target repo.** GitHub treats a PR as stacked only when its head and base branches both live on the *same* repository. Your stack branches therefore have to be pushable to the repo you're merging into. Three setups:

1. WORKS: **Direct clone of the merge target**: `origin` is the merge target; stacking works. This is the most common setup.
2. WORKS: **Fork of the merge target, *with* push access to the parent.** `sms push` automatically detects the fork, finds the parent's remote (typically `upstream`), and pushes your branches there instead of to your fork. Stacking works. For example, an internal fork of an employer repo where the team uses the OSS-style PR-from-fork flow but contributors retain push access to the parent. In this case you would no longer use your fork with `sms push`.
3. DOES NOT WORK: **Fork of the merge target, *without* push access to the parent.** This is the standard open-source contributor flow: fork the project, push topic branches to your fork, open PRs from `you/proj` against `upstream/proj`. Single PRs work fine; **stacking does not, and cannot.** GitHub does not support cross-repo stacked PRs, so no tool, `stackymcstackface` included, can make this work. `sms push` will surface it as a `git push` permission error against the parent remote.

If you're in bucket (3) and want a stack, the only options are out-of-band: get push access to the parent, or land your "stack" as independent, single-parent PRs in dependency order.

### Required repo settings

For the auto-retarget to work end to end:

1. GitHub setting **`delete_branch_on_merge` must be enabled.** `sms push`
   offers to do this for you when you have admin rights; to run it by hand,
   once per repo:

   ```sh
   gh api -X PATCH /repos/<owner>/<repo> -F delete_branch_on_merge=true
   ```

2. **Use merge commits for stack PRs.** GitHub retargets dependents for all
   three merge methods, but only merge commits preserve the original
   commits. Squash and GitHub rebase merges create new SHAs and can require
   subsequent restacking. `sms merge` deliberately supports merge commits only.

`sms push` checks (1) and offers to enable it when you have admin rights
(and does so under `--yes`). Without admin rights it prints the manual
command to hand to someone who has them. `sms merge` requires both settings
already enabled; it never changes repository settings.

See [Repo setup](#repo-setup-one-time) and [Merging a stack](#8-merging-a-stack)
for the details.

## Install

Whichever way you install, you also need `git` and a working,
authenticated `gh` (`gh auth status` should be green). `rebase` requires
Git 2.38 or newer. Building from source requires Rust 1.89 or newer.

The easiest install is with
[`cargo binstall`](https://github.com/cargo-bins/cargo-binstall): it
downloads a precompiled binary from the GitHub releases, so there is no
Rust toolchain or compile step. This crate is not published to crates.io,
so point `binstall` at the repository instead of a crate name:

```sh
cargo binstall --git https://github.com/cjrh/stackymcstackface stackymcstackface
```

`binstall` reads the version from the repo, finds the matching release,
and drops the `stackymcstackface` binary on your `PATH`.

No `cargo binstall`? Grab a precompiled binary by hand from the
[releases page](https://github.com/cjrh/stackymcstackface/releases) and
put it on your `PATH`, or [build from source](#building-from-source).

### Building from source

If you have a Rust toolchain and want to compile it yourself (for example
to hack on the tool), install from the working tree:

```sh
cargo install --path .
```

This builds and installs the binary as `stackymcstackface` on your `PATH`.

### Repo setup (one-time)

Two repository-level settings make stacking work end-to-end. Without
them, the tool will still open stacked PRs, but the *after-merge* part
of the workflow that GitHub handles will not work cleanly.

1. **Enable "Automatically delete head branches".** When a stack PR
   merges, GitHub deletes its head branch through the merge flow,
   which is the only deletion path that retargets dependent PRs to
   the merged PR's base.

   ```sh
   gh api -X PATCH /repos/<owner>/<repo> -F delete_branch_on_merge=true
   ```

   `sms push` offers to do this for you when you have admin rights on the
   repo. Web UI equivalent: Settings → General → Pull Requests →
   "Automatically delete head branches".

2. **Use merge commits for stack PRs.** All three GitHub merge methods
   support auto-retargeting, but only merge commits leave the original
   commits intact on `main`. Squash collapses them into a new commit;
   **GitHub rebase merges always create new SHAs too**, unlike some local
   `git rebase` operations. Rewritten history can make dependent PRs replay
   already-reviewed changes or require conflict resolution. `sms merge`
   uses merge commits exclusively and refuses required merge queues or
   rules requiring linear history.

3. Optional but recommended: set `git config stack.remote upstream` to
   disambiguate the merge-target remote name. The tool can usually figure
   it out by matching URLs, but this makes it explicit and avoids the
   fallback logic that otherwise tries to pick between multiple remotes.

4. Optional but recommended: In your GitHub settings, set "Allow merge commits"
   and for the "Default commit message" choose "Pull request title and description". 
   This way the merge commit gets a useful message by default, and you
   don't have to edit it every time, and using `git log --first-parent` on `main` 
   still shows the PR titles.

If you have admin access to the repo, settings (1) and (4) can be applied
in a single PATCH:

```sh
gh api -X PATCH /repos/<owner>/<repo> \
  -F delete_branch_on_merge=true \
  -F allow_merge_commit=true \
  -f merge_commit_title=PR_TITLE \
  -f merge_commit_message=PR_BODY
```

`-F` sends typed values (booleans here); `-f` sends strings. If `gh`
returns 403, you don't have admin permission on the repo; ask whoever
does, or apply the settings through Settings → General → Pull Requests
in the web UI.

### Suggested alias

The binary name is intentionally absurd. Pick a short alias for daily use.
`sms` is the obvious one:

```sh
# bash / zsh
alias sms='stackymcstackface'

# fish
alias --save sms='stackymcstackface'
```

From here on the examples use `sms`.

### Verifying setup

`sms doctor` runs a read-only check of the prerequisites for `push`:
`git` and `gh` are on `PATH`, `gh` is authenticated, the working
directory is a git clone of a GitHub repo, the merge-target remote is
unambiguous, `delete_branch_on_merge` is enabled, and the current branch
is in a state `push` would accept. Output is colour-coded
(`✔` / `⚠` / `✗`); the command exits non-zero if any check failed.
Honours `NO_COLOR` and disables colour automatically when stdout is not
a terminal. It never fetches, pushes, or mutates anything on GitHub.

## Design requirements

These are the design constraints behind the tool. If you do not share them,
this tool is probably not for you.

- **No permanent stack database.** Other stacking tools maintain a sidecar file
  describing the stack and its state. That file rots. Once it disagrees
  with reality on GitHub (which happens *most* once you have five or six
  PRs in a stack), the tooling becomes harder to fix than the manual
  workflow it replaced. Every invocation of `push` reconstructs the
  picture from authoritative sources only: `git fetch`,
  `git merge-base --is-ancestor`, and `gh pr list`. Nothing to keep in
  sync because there is nothing to sync. A running `rebase` is the narrow
  exception: a temporary recovery journal and private refs inside Git's
  metadata preserve original tips and progress, like Git's own rebase state.
  They are removed on completion or abort. An empty OS-lock file coordinates
  commands across worktrees; it stores no stack information.

- **Push to the merge-target remote, never anywhere else.** For non-fork
  repos that means `origin`. For forks, it almost always means `upstream`
  (or whatever you call the remote pointing at the parent). The tool
  figures this out by asking GitHub for `isFork`/`parent` and matching
  against your configured git remotes.

- **Detect a wrong-remote push and offer to fix it.** If you have already
  pushed your branch to your fork's `origin`, the tool notices and prompts
  before re-pushing to the correct remote and switching the upstream
  tracking ref.

- **Keep operations explicit.** `push` still pushes one branch without
  rewriting PR bases. `rebase` is local unless started with `--push`.
  `merge` requires confirmation (or `--yes`), honors GitHub's requirements,
  and never enables auto-merge, bypasses a queue, or deletes branches itself.

- **Refuse to act on a dirty repo state.** If the repo is mid-rebase,
  mid-merge, mid-cherry-pick, mid-revert, mid-bisect, or mid-`am`, `push`
  bails and tells you what it found. **Uncommitted changes are fine.** A
  common workflow is to peel one PR at a time off a large set of local
  changes. In contrast, `rebase` and `merge` require a clean index/worktree,
  including untracked files. An interrupted stack rebase blocks both `push`
  and `merge`, even between native Git rebases.

## Configuration

Optional. By default the tool picks the merge-target remote automatically:
`origin` if you are not on a fork, otherwise the local remote whose URL
matches the parent repo on GitHub.

If automatic detection picks wrong, or you want to be explicit:

```sh
git config stack.remote upstream
```

Per-repo (the default above) or `--global` if you want the same name
everywhere.

## Typical scenarios

### 1. First PR off `main`

```sh
git checkout -b feat/parser-cleanup
# ... edit, commit ...
sms push
```

```text
stackymcstackface

PLAN
  octocat/widgets · default main · remote origin
  push feat/parser-cleanup and open a PR with base main

EXECUTE
  ✔ pushed feat/parser-cleanup to origin
  ✔ opened pull request

  https://github.com/octocat/widgets/pull/421
```

### 2. Stacking on top of an existing PR

```sh
# while still on feat/parser-cleanup, with PR #421 open:
git checkout -b feat/parser-cleanup-tests
# ... edit, commit ...
sms push
```

```text
stackymcstackface

PLAN
  octocat/widgets · default main · remote origin
  push feat/parser-cleanup-tests and open a stacked PR with base feat/parser-cleanup
    parent: #421 https://github.com/octocat/widgets/pull/421

EXECUTE
  ✔ pushed feat/parser-cleanup-tests to origin
  ✔ opened pull request
  ✔ noted parent #421 in description

  https://github.com/octocat/widgets/pull/422
```

The parent is found by walking your branch's ancestry and picking the
**closest** open PR whose head SHA is an ancestor of your `HEAD`. The same
logic applies at any level of the stack.

### 3. Working in a fork

You forked `octocat/widgets` to `you/widgets` and have:

```text
origin    git@github.com:you/widgets.git       (fetch / push)
upstream  git@github.com:octocat/widgets.git   (fetch / push)
```

`sms push` will detect the fork, identify `upstream` as the merge-target
remote, and push your branches there, *not* to your fork's `origin`. That
is the only configuration that lets stacking work in a fork: both PR head
and base must live on the same repo.

### 4. Wrong-remote rescue

You forgot and ran `git push -u origin my-branch` first. Then:

```sh
sms push
```

```text
  ⚠ current branch tracks `origin/...` but the merge target is `upstream`.
    Stacked PRs only work when the branch lives on the merge-target remote.
Re-push to `upstream` and switch tracking? (Y/n)
```

Answer yes and the tool re-pushes to `upstream` with `--set-upstream`.
(`-y` skips the prompt.)

### 5. After a local rebase

Rebasing a stacked branch on top of its (also-rebased) parent is normal.
Re-run `sms push` with `--force-with-lease`:

```sh
sms push --force-with-lease
```

If a PR for the branch already exists, the tool just refreshes the push
and prints the existing PR URL. It will not try to recreate it.

### 6. Pushing updates to an existing PR (drop-in `git push`)

Once a branch has an open PR on the merge target, `sms push` is just
`git push`: it pushes any new commits to the existing remote branch and
reports the existing PR. No prompts, no PR mutations, no extra work.
This is what makes `sms push` safe to use as a blanket replacement for
`git push`.

```text
EXECUTE
  ✔ pushed feat/parser-cleanup to origin

  https://github.com/octocat/widgets/pull/421
  base on GitHub: main
```

It does **not** rewrite the PR's base. If a previously parent-less PR should
now be stacked under a new parent, change the base in the GitHub UI (or
delete the old PR). Keeping `push` from silently mutating PR bases is
deliberate.

### 7. Rebasing a whole stack

From **any branch in the stack**, including a middle branch:

```sh
sms rebase          # update every local stack branch onto fetched main
# Or choose publication at the start:
sms rebase --push   # rebase, then atomically publish every branch
```

The first PR is replayed onto the merge target's fetched default branch.
Each later PR replays only its own commits onto the rewritten parent.
Unrelated local branches are not moved. Your original checkout is restored
at the end. Plain `rebase` does not push; afterward you can publish each
branch with `sms push --force-with-lease` as before. Do not rerun
`rebase --push` on those unpublished local results: choose `--push` when
starting the operation, or publish the local-only results per branch.

On a conflict:

```sh
# edit conflicted files
git add <resolved-files>
sms rebase --continue
# repeat at later layers if necessary
# Or abandon the entire operation:
sms rebase --abort
```

`--continue` lets Git create the replacement commit. A valid manual
resolution commit is also accepted. Use the tool's `--abort` for whole-stack
rollback, not `git rebase --abort` (which only aborts the current layer).
There is no automatic conflict resolution or tool-level `--skip`.

During replay your original branches stay untouched. Private staging
branches hold the rewritten commits; all user branch tips are installed
in one expected-old-value ref transaction only after every layer succeeds.
Recovery refs retain the originals until cleanup. Git's automatic ref
updates, autostashing, autosquashing, and rebase hooks are disabled.

With `--push`, publication is **one atomic push with an explicit old-SHA
lease for each branch**. No branch is pushed during conflicts. If a lease
fails, none of the stack updates are published. After a rejected or
uncertain push, the local results and recovery record remain:

- Run `sms rebase --continue` to inspect authoritative remote heads. If all
  match the intended results, it finishes cleanup without pushing again;
  if all are still original, it retries the atomic push.
- `sms rebase --abort` restores original local branches only when the
  remote is still original. It cannot undo an already-published stack.
- If remote heads changed independently, coordinate with the other author
  before recovery. The command refuses to guess or overwrite those changes.

Resume/abort in the original worktree. Do not change stack refs, move to an
unrelated checkout, start other Git operations, or edit the recovery record
while it is active. The journal is
`$(git rev-parse --git-common-dir)/stackymcstackface-rebase.json`.
A corrupt record, changed refs, or an unprovable interrupted layer fails
closed and retains recovery data. If you manually aborted the native
rebase, use `sms rebase --abort`, then start again. Do not delete the journal
as a shortcut: preserve it and the private refs for manual recovery.

### 8. Merging a stack

```sh
sms merge --dry-run  # full initial preflight; no GitHub mutations
sms merge           # show plan, check all layers, ask for confirmation
sms merge --yes     # same checks, without the prompt
```

This selects the full linear stack, checks every PR against its current
base before merging anything, then merges bottom-up using merge commits.
Every local tip must match the fetched and GitHub PR heads. Drafts,
conflicts, unapproved required reviews, unknown/pending/failed checks,
non-clean GitHub merge states, and merge queues block the command. Checks
are conservative: even a failing optional reported check blocks it. The
command reads active branch rules and GitHub's aggregate merge readiness;
it fails if those APIs cannot establish readiness. GitHub remains the
final authority on permissions and branch requirements.

With "Automatically delete head branches" enabled, GitHub deletes the
merged head and retargets the next PR to the default branch. The command
briefly polls for retargeting/mergeability, then checks settings, rules,
heads, bases, and readiness again before the next merge. Each API merge
pins the expected head SHA, and the PR's merged state is confirmed afterward.
It never queues PRs, enables auto-merge, or attempts an admin override.

**This is not an atomic multi-PR transaction.** Retargeting can rerun CI,
dismiss approvals, or apply different default-branch rules. Initial checks
cannot guarantee those future requirements. If a later layer is blocked,
the command stops and reports confirmed merges and remaining/unconfirmed
PRs. Already-merged PRs are not rolled back. Inspect GitHub, wait for or
satisfy the remaining requirements, check out a remaining branch, then
rerun `sms merge`. Do not concurrently edit the stack while it runs.
GitHub has no expected-base-SHA merge transaction, so a target/rule change
between the final check and the server operation cannot be eliminated.

Merging only the top PR is deliberately not used as an optimization:
GitHub can mark ancestor PRs indirectly merged even when their own rules
were not satisfied. Manual bottom-up `gh pr merge <number> --merge` remains
available. If automatic deletion was disabled for a manual merge, use the
GitHub PR page's "Delete branch" button to trigger retargeting.

What you must **not** do: clean up merged head branches with
`git push --delete origin <branch>` or low-level `gh api` ref deletes.
Those bypass GitHub's retarget logic; the next PR in the stack will be
**closed**, not retargeted, and you will have to restore the deleted
branch and reopen the PR to recover.

### 9. Bail conditions

`push` refuses to run, with a clear message, in any of these states:

- mid-rebase, mid-merge, mid-cherry-pick, mid-revert, mid-bisect, mid-`am`
- detached `HEAD`
- current branch *is* the default branch (`main`/`master`/whatever)
- repository has no commits yet
- no GitHub remote, or `gh` not authenticated
- multiple local remotes match the merge target and none are named
  `origin` or `upstream` (set `git config stack.remote` to disambiguate)
- current branch shares no history with the default branch (so there is no
  sensible base to pick)

### Whole-stack limitations (first version)

`rebase` and `merge` require an unambiguous **linear chain of same-repo open
PRs rooted at the default branch**, with all stack branches already present
locally and synchronized with GitHub (publish local commits first). PR bases
identify the chain; every parent tip must be an ancestor of its child.
Cycles, duplicate heads, branching stacks, symbolic branch refs, shallow
history, replacement refs, grafts, missing refs, and empty layers are refused. `merge` also rejects
open ancestor PRs outside the selected chain, including equal-SHA aliases.
Correct their PR bases first; they must not bypass checks through an
indirect merge. Discovery fails closed at the 1000-open-PR cap instead of
silently omitting possible dependents.

New destructive commands support GitHub.com remotes with exactly one
fetch URL and one push URL, both identifying the merge-target repository.
`stack.remote` cannot bypass this check. Enterprise hosts and multiple push
URLs are not supported yet. `rebase` additionally refuses merge commits in
replayed history and any stack branch checked out in another worktree.
`merge` requires clean worktrees for all checked-out stack branches. It
requires merge commits and automatic head deletion already enabled, no
required merge queue, and access to the applicable-rules APIs. Large check
rollups that might be truncated are refused. Ordinary `push` retains its
existing single-branch behavior and tolerance of uncommitted changes.

## Flags

```text
sms push [OPTIONS]

  -t, --title <TITLE>     PR title. If omitted, `gh pr create --fill`
                          populates from commits.
  -b, --body <BODY>       PR body. Same fallback as --title.
      --draft             Open as a draft PR.
      --web               Open the new PR in a browser instead of just
                          printing its URL.
      --force-with-lease  Push with --force-with-lease. Use after a local
                          rebase.
  -y, --yes               Skip interactive prompts (assume "yes" for the
                          wrong-remote rescue).
  -v, --verbose           Show raw git output for fetch/push instead of
                          hiding it behind a spinner. Repeat (-vv) to also
                          pass git its own --verbose.
```

```text
sms rebase [--push]
sms rebase --continue
sms rebase --abort

      --push       Publish the entire completed stack atomically with leases.
      --continue   Resume the original operation (remembers its --push choice).
      --abort      Abort the entire operation, guarding original branch tips.

sms merge [OPTIONS]

  -y, --yes        Confirm the irreversible bottom-up merge without a prompt.
      --dry-run    Check the full stack and print its plan; never merge.
```

By default `push` groups its work into a `PLAN` section (what it will do)
and an `EXECUTE` section (pushing and opening the PR), with spinners over
the `git`/`gh` calls and the PR URL printed last. Output is colourised on
a terminal and plain when piped; `NO_COLOR` is honoured.

## How it works (the short version)

1. Read the working tree state via `git`. Bail on dirty operations or
   detached `HEAD`.
2. Ask `gh repo view` whether this is a fork and what the default branch is.
   If a fork, look up the parent repo's info too.
3. Pick the merge-target remote: `git config stack.remote` overrides;
   otherwise match local remote URLs against the merge-target repo (handles
   `https`, `git@`, and `ssh://` URLs, with or without `.git`).
4. `git fetch <merge-target-remote>`.
5. `gh pr list --repo <merge-target> --state open` and consider only PRs
   whose head lives on the merge-target repo.
6. For each such PR, ask `git merge-base --is-ancestor <pr-head> HEAD`.
   The closest ancestor (smallest `git rev-list --count`) is the parent.
   No ancestor means the base is the default branch.
7. Push the current branch to the merge-target remote with `--set-upstream`.
8. `gh pr create --base <parent> --head <branch> --repo <merge-target>`.
9. If the new PR is stacked on an existing PR, append a `Stacked on
   #<parent>` footer to its body via `gh pr edit`, separated by a
   `---` rule. Skipped under `--web` (the body is finalised in the
   browser there).

You can read the same algorithm directly in
[`src/stack.rs`](src/stack.rs).

## Non-goals

These are explicitly out of scope and unlikely to ever be added:

- A stack overview or visualisation command. `gh pr list` already shows it.
- A "submit the whole stack" command. `sms push` per branch is plenty.
- Background auto-merging/rebasing, merge-queue orchestration, or automatic
  conflict resolution. Explicit `rebase` and `merge` are supported.
- A permanent local stack database. Temporary rebase recovery metadata is
  the only stack-state exception. See "Design requirements".

## Further reading

Background on the GitHub mechanics this tool relies on:

- [Pull request retargeting](https://github.blog/changelog/2020-05-19-pull-request-retargeting/)
  — GitHub's 2020 changelog announcing the auto-retarget behaviour
  that makes stacking work after a merge. Triggers on
  *merged + deleted*, regardless of merge method.
- [About pull request merges](https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/incorporating-changes-from-a-pull-request/about-pull-request-merges)
  — official docs on merge commits, squash, and rebase, including the
  separate "indirect merge" feature (which *is* merge-method-specific
  and is what most "squash breaks stacks" claims actually conflate).
- [My workflow for stacked PRs on GitHub](https://www.davepacheco.net/blog/2025/stacked-prs-on-github/)
  — Dave Pacheco walks through stacked-PR mechanics end to end and
  explains, with worked examples, why squash merges produce spurious
  conflicts in the next PR even though retargeting itself succeeds.
- [Stacked pull requests with squash merge](https://echobind.com/post/stacked-pull-requests-with-squash-merge)
  — a complementary take on the same squash-vs-stacks problem and how
  to recover when you can't avoid squash.

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE) for the full text.

## Disambiguation

I thought I was very clever choosing a unique name, but it turns out other
projects have already used it:
- https://github.com/RickCarlino/stacky_mcstackface - A stack-based VM in js
- https://github.com/mp4096/smsf - Another stack-based VM
