# stackymcstackface

A small, opinionated helper for **stacked pull requests on GitHub**, driven by
the official [`gh`](https://cli.github.com/) CLI. One subcommand: `stack`. It
pushes your current branch to the right remote and opens the PR with the right
base.

## Goals

The manual stacked-PR workflow on GitHub is not complicated, but it has two
fiddly bits that are easy to get wrong:

1. The branches must live on the **same remote** as the PRs themselves
   (otherwise stacking silently breaks — particularly painful when working
   from a fork).
2. When you create the PR, you have to **pick the right base branch by hand**
   — the parent branch in the stack, not `main`.

`stackymcstackface` automates exactly those two steps. Everything else
— reviewing, merging, rebasing — stays the normal GitHub workflow you already
know. Importantly, GitHub itself handles the part that matters most: when the
parent PR merges into the default branch, child PRs automatically retarget to
the default branch on their own.

## Design requirements

These are the design constraints behind the tool. They are not negotiable —
if you do not share them, this tool is not for you.

- **No local state.** Other stacking tools maintain a sidecar file describing
  the stack and its state. That file rots. When it disagrees with reality on
  GitHub — which happens *most* once you have five or six PRs in a stack —
  the tooling becomes harder to fix than the manual workflow it replaced.
  Every invocation of `stack` reconstructs the picture from authoritative
  sources only: `git fetch`, `git merge-base --is-ancestor`, and `gh pr list`.
  There is nothing to keep in sync because there is nothing to sync.

- **Push to the merge-target remote, never anywhere else.** For non-fork
  repos that means `origin`. For forks, that almost always means `upstream`
  (or whatever you call the remote pointing at the parent). The tool figures
  this out by asking GitHub `isFork`/`parent` and matching against your
  configured git remotes.

- **Detect a wrong-remote push and offer to fix it.** If you have already
  pushed your branch to your fork's `origin`, the tool notices and prompts
  before re-pushing to the correct remote and switching the upstream
  tracking ref.

- **Do as little as possible.** One subcommand. Push the branch. Open the
  PR. Print the URL. That is the whole tool. No `submit-stack`, no
  `restack`, no `land`, no merge orchestration — those are GitHub's job.

- **Refuse to act on a dirty repo state.** If the repo is mid-rebase,
  mid-merge, mid-cherry-pick, mid-revert, mid-bisect, or mid-`am`, `stack`
  bails and tells you what it found. **Uncommitted changes are fine** — a
  common workflow is to peel one PR at a time off a large set of local
  changes.

## Install

You need `git` and a working, authenticated `gh` (`gh auth status` should be
green).

```sh
cargo install --path .
```

This installs the binary as `stackymcstackface` on your `PATH`.

### Suggested alias

The binary name is intentionally absurd. Pick a short alias for daily use —
`sms` is the obvious one:

```sh
# bash / zsh
alias sms='stackymcstackface'

# fish
alias --save sms='stackymcstackface'
```

From here on the examples use `sms`.

## Configuration

Optional. By default the tool picks the merge-target remote automatically:
`origin` if you are not on a fork, otherwise the local remote whose URL
matches the parent repo on GitHub.

If automatic detection picks wrong, or you simply want to be explicit:

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
sms stack
```

```text
→ merge target: octocat/widgets (default branch `main`), remote `origin`
→ fetching `origin` ...
→ plan: push `feat/parser-cleanup` and open a PR with base `main`
→ pushing `feat/parser-cleanup` to `origin` ...
→ creating PR: head=`feat/parser-cleanup` base=`main` repo=octocat/widgets
https://github.com/octocat/widgets/pull/421
```

### 2. Stacking on top of an existing PR

```sh
# while still on feat/parser-cleanup, with PR #421 open:
git checkout -b feat/parser-cleanup-tests
# ... edit, commit ...
sms stack
```

```text
→ merge target: octocat/widgets (default branch `main`), remote `origin`
→ fetching `origin` ...
→ plan: push `feat/parser-cleanup-tests` and open a STACKED PR on top of #421
        (base `feat/parser-cleanup`)
        parent: https://github.com/octocat/widgets/pull/421
→ pushing `feat/parser-cleanup-tests` to `origin` ...
→ creating PR: head=`feat/parser-cleanup-tests` base=`feat/parser-cleanup` ...
https://github.com/octocat/widgets/pull/422
```

The parent is found by walking your branch's ancestry and picking the
**closest** open PR whose head SHA is an ancestor of your `HEAD`. Repeat for
as many levels as you like.

### 3. Working in a fork

You forked `octocat/widgets` to `you/widgets` and have:

```text
origin    git@github.com:you/widgets.git       (fetch / push)
upstream  git@github.com:octocat/widgets.git   (fetch / push)
```

`sms stack` will detect the fork, identify `upstream` as the merge-target
remote, and push your branches there — *not* to your fork's `origin`. This
is the only configuration that lets stacking work in a fork. Both PR head
and base must live on the same repo.

### 4. Wrong-remote rescue

You forgot and ran `git push -u origin my-branch` first. Then:

```sh
sms stack
```

```text
⚠  current branch tracks `origin/...` but the merge target is `upstream`.
   Stacked PRs only work when the branch lives on the merge-target remote.

Re-push to `upstream` and switch tracking? (Y/n)
```

Answer yes and the tool re-pushes to `upstream` with `--set-upstream`.
(`-y` skips the prompt.)

### 5. After a local rebase

Rebasing a stacked branch on top of its (also-rebased) parent is normal.
Re-run `sms stack` with `--force-with-lease`:

```sh
sms stack --force-with-lease
```

If a PR for the branch already exists, the tool just refreshes the push and
prints the existing PR URL — it does not try to recreate it.

### 6. Re-running on an existing PR

Safe. The tool detects an existing open PR with the same head branch on the
merge target, pushes any new commits, and reports:

```text
✔ PR already exists: #421 https://github.com/octocat/widgets/pull/421
  (base on GitHub: `main`)
```

It does **not** rewrite the PR's base. If a previously parent-less PR should
now be stacked under a new parent, change the base in the GitHub UI (or
delete the old PR). Keeping `stack` from silently mutating PR bases is
deliberate.

### 7. Bail conditions

`stack` refuses to run, with a clear message, in any of these states:

- mid-rebase, mid-merge, mid-cherry-pick, mid-revert, mid-bisect, mid-`am`
- detached `HEAD`
- current branch *is* the default branch (`main`/`master`/whatever)
- repository has no commits yet
- no GitHub remote, or `gh` not authenticated
- multiple local remotes match the merge target and none are named
  `origin` or `upstream` (set `git config stack.remote` to disambiguate)
- current branch shares no history with the default branch (so there is no
  sensible base to pick)

## Flags

```text
sms stack [OPTIONS]

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
```

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
   No ancestor → base is the default branch.
7. Push the current branch to the merge-target remote with `--set-upstream`.
8. `gh pr create --base <parent> --head <branch> --repo <merge-target>`.

That is the entire algorithm. You can read it directly in
[`src/stack.rs`](src/stack.rs).

## Non-goals

These are explicitly out of scope and unlikely to ever be added:

- A stack overview / visualisation command. `gh pr list` already shows it.
- A "submit the whole stack" command. `sms stack` per branch is plenty.
- Auto-merging, auto-rebasing, conflict resolution, or any other workflow
  orchestration.
- A local stack-state file of any kind. See "Design requirements".

## License

GPL-3.0-or-later. See [`LICENSE`](LICENSE) for the full text.
