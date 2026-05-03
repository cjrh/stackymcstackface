# stackymcstackface

A small helper for stacked pull requests on GitHub, driven by the official
[`gh`](https://cli.github.com/) CLI. One subcommand: `stack`. It pushes your
current branch to the right remote and opens the PR with the right base.

## Demo

You've already created a PR for the current branch.
Now you create a new branch off it for some follow-up work.
You commit the changes, but have not pushed yet.

Run this:

```text
$ stackymcstackface stack
→ plan: push `feat/parser-tests` and open a STACKED PR on top of #421
        (base `feat/parser-cleanup`)
→ pushing `feat/parser-tests` to `origin` ...
→ creating PR: head=`feat/parser-tests` base=`feat/parser-cleanup` ...
https://github.com/octocat/widgets/pull/422
```

A new PR will be created, stacked correctly on top of the previous
PR.

The new PR's base is the parent branch (not `main`), so GitHub treats it
as stacked. When #421 later merges, GitHub auto-retargets #422 to `main`,
**but only if the merge-target repo has the right settings**. Without
them, #422's base will keep pointing at the now-merged parent branch and
you will be stuck wondering why nothing moved.

### Required repo settings

For the auto-retarget to work end to end:

1. GitHub setting **`delete_branch_on_merge` must be enabled.** Run once per repo:

   ```sh
   gh api -X PATCH /repos/<owner>/<repo> -f delete_branch_on_merge=true
   ```

2. **Use merge commits or rebase merges, not squashes.** GitHub retargets
   dependents for all three merge methods, but squash creates a new
   commit on `main` whose content overlaps the original commits still
   living in the next PR's branch, so the retargeted PR comes back with
   conflicts you have to rebase out. See [Repo setup](#repo-setup-one-time).

`stackymcstackface` prints a warning at the top of every run if (1) is
off, so you find out before merging rather than after. (2) is per-merge
and not detectable from the CLI; pick the right button in the GitHub UI
or restrict the repo defaults under Settings → General → Pull Requests.

See [Repo setup](#repo-setup-one-time) and [Merging a stack](#8-merging-a-stack)
for the details.

## Goals

The manual stacked-PR workflow on GitHub is not complicated, but it has two
fiddly bits that are easy to get wrong:

1. The branches must live on the same remote as the PRs themselves
   (otherwise stacking silently breaks, which is particularly painful when
   working from a fork).
2. When you create the PR, you have to pick the right base branch by hand:
   the parent branch in the stack, not `main`.

`stackymcstackface` automates exactly those two steps. Reviewing and
rebasing stay the normal GitHub workflow you already know. So does
merging, with one prerequisite: when a stack PR merges, GitHub only
auto-retargets the next PR if the merged head branch is deleted through
GitHub's own merge flow. That means the post-merge "Delete branch"
button on the PR page, or the repo-level "Automatically delete head
branches" setting. Manual `git push --delete origin <branch>` (or the
equivalent low-level API call) bypasses the retarget logic and **closes**
the next PR instead. See [Repo setup](#repo-setup-one-time) and
[Merging a stack](#8-merging-a-stack).

## Design requirements

These are the design constraints behind the tool. If you do not share them,
this tool is probably not for you.

- **No local state.** Other stacking tools maintain a sidecar file
  describing the stack and its state. That file rots. Once it disagrees
  with reality on GitHub (which happens *most* once you have five or six
  PRs in a stack), the tooling becomes harder to fix than the manual
  workflow it replaced. Every invocation of `stack` reconstructs the
  picture from authoritative sources only: `git fetch`,
  `git merge-base --is-ancestor`, and `gh pr list`. Nothing to keep in
  sync because there is nothing to sync.

- **Push to the merge-target remote, never anywhere else.** For non-fork
  repos that means `origin`. For forks, it almost always means `upstream`
  (or whatever you call the remote pointing at the parent). The tool
  figures this out by asking GitHub for `isFork`/`parent` and matching
  against your configured git remotes.

- **Detect a wrong-remote push and offer to fix it.** If you have already
  pushed your branch to your fork's `origin`, the tool notices and prompts
  before re-pushing to the correct remote and switching the upstream
  tracking ref.

- **Do as little as possible.** One subcommand. Push the branch, open the
  PR, print the URL. That is the whole tool. There is no `submit-stack`,
  no `restack`, no `land`, and no merge orchestration; GitHub already does
  those.

- **Refuse to act on a dirty repo state.** If the repo is mid-rebase,
  mid-merge, mid-cherry-pick, mid-revert, mid-bisect, or mid-`am`, `stack`
  bails and tells you what it found. **Uncommitted changes are fine.** A
  common workflow is to peel one PR at a time off a large set of local
  changes.

## Install

You need `git` and a working, authenticated `gh` (`gh auth status` should be
green).

```sh
cargo install --path .
```

This installs the binary as `stackymcstackface` on your `PATH`.

### Repo setup (one-time)

Two repository-level settings make stacking work end-to-end. Without
them, the tool will still open stacked PRs, but the *after-merge* part
of the workflow that GitHub handles will not work cleanly.

1. **Enable "Automatically delete head branches".** When a stack PR
   merges, GitHub deletes its head branch through the merge flow,
   which is the only deletion path that retargets dependent PRs to
   the merged PR's base.

   ```sh
   gh api -X PATCH /repos/<owner>/<repo> -f delete_branch_on_merge=true
   ```

   Web UI equivalent: Settings → General → Pull Requests →
   "Automatically delete head branches".

2. **Use merge commits or rebase merges, not squash, for stack PRs.**
   GitHub auto-retargets dependent PRs the same way for all three
   merge methods — squash does not break the retarget itself. The
   problem is what lands on `main`: squash collapses the parent
   branch's commits into one new commit whose content is the same diff
   that the next PR's branch *also* still contains as its original,
   un-squashed commits. After retarget, the next PR's diff against
   `main` re-litigates those changes against the squash commit, so
   already-reviewed hunks come back as merge conflicts and you have to
   rebase to clear them. Merge commits and rebase merges leave the
   original commits intact on `main`, so the dependent branch lines up
   cleanly with no rework. Pick per-merge in the GitHub UI, or
   restrict the repo defaults under Settings → General → Pull Requests.

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
returns 403, you don't have admin permission on the repo — ask whoever
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
**closest** open PR whose head SHA is an ancestor of your `HEAD`. The same
logic applies at any level of the stack.

### 3. Working in a fork

You forked `octocat/widgets` to `you/widgets` and have:

```text
origin    git@github.com:you/widgets.git       (fetch / push)
upstream  git@github.com:octocat/widgets.git   (fetch / push)
```

`sms stack` will detect the fork, identify `upstream` as the merge-target
remote, and push your branches there, *not* to your fork's `origin`. That
is the only configuration that lets stacking work in a fork: both PR head
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

If a PR for the branch already exists, the tool just refreshes the push
and prints the existing PR URL. It will not try to recreate it.

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

### 8. Merging a stack

Merge from the bottom up, one PR at a time:

```sh
gh pr merge 1 --merge   # or use the web UI; do NOT use --squash
```

With "Automatically delete head branches" enabled (see
[Repo setup](#repo-setup-one-time)), GitHub deletes #1's head branch as
part of the merge and retargets the next PR (say, #2) so its base
becomes `main`. Repeat for #2, then #3, until the stack is empty.

If you forgot to enable the setting, click the "Delete branch" button
on the merged PR's page on GitHub. That deletion path also retargets
dependents.

What you must **not** do: clean up merged head branches with
`git push --delete origin <branch>` or low-level `gh api` ref deletes.
Those bypass GitHub's retarget logic; the next PR in the stack will be
**closed**, not retargeted, and you will have to restore the deleted
branch and reopen the PR to recover.

### 9. Bail conditions

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
   No ancestor means the base is the default branch.
7. Push the current branch to the merge-target remote with `--set-upstream`.
8. `gh pr create --base <parent> --head <branch> --repo <merge-target>`.

You can read the same algorithm directly in
[`src/stack.rs`](src/stack.rs).

## Non-goals

These are explicitly out of scope and unlikely to ever be added:

- A stack overview or visualisation command. `gh pr list` already shows it.
- A "submit the whole stack" command. `sms stack` per branch is plenty.
- Auto-merging, auto-rebasing, conflict resolution, or any other workflow
  orchestration.
- A local stack-state file of any kind. See "Design requirements".

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
