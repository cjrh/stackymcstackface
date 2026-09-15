#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Repo {
    temp: PathBuf,
    root: PathBuf,
    bare: PathBuf,
    bin: PathBuf,
    real_git: String,
    old: Vec<String>,
}

fn text(out: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn ok(out: Output) -> String {
    assert!(out.status.success(), "{}", text(&out));
    String::from_utf8(out.stdout).unwrap().trim().into()
}

impl Repo {
    fn command(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env("NO_COLOR", "1");
        command
    }

    fn git(&self, args: &[&str]) -> String {
        ok(self.command(&self.real_git).args(args).output().unwrap())
    }

    fn commit(&self, file: &str, content: &str) {
        fs::write(self.root.join(file), content).unwrap();
        self.git(&["add", file]);
        self.git(&["commit", "-m", file]);
    }

    // conflict=1 changes shared in both lower layers; conflict=2 only in b.
    fn new(conflict: usize) -> Self {
        let temp = std::env::temp_dir().join(format!(
            "sms tests {}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let root = temp.join("repo with spaces");
        let bare = temp.join("bare remote.git");
        let bin = temp.join("mock bin");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let real_git = ok(Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap());
        let mut repo = Self {
            temp,
            root,
            bare,
            bin,
            real_git,
            old: Vec::new(),
        };
        repo.git(&["init", "-b", "main"]);
        repo.git(&["config", "user.name", "Stack Test"]);
        repo.git(&["config", "user.email", "stack@example.invalid"]);
        repo.git(&["config", "stack.remote", "origin"]);
        repo.commit("shared", "base\n");
        repo.git(&["checkout", "-b", "a"]);
        if conflict == 1 {
            repo.commit("shared", "a\n");
        } else {
            repo.commit("a.txt", "a\n");
        }
        repo.git(&["checkout", "-b", "b"]);
        if conflict > 0 {
            repo.commit("shared", "b\n");
        } else {
            repo.commit("b.txt", "b\n");
        }
        repo.git(&["checkout", "-b", "c"]);
        repo.commit("c.txt", "c\n");
        repo.old = ["a", "b", "c"]
            .iter()
            .map(|b| repo.git(&["rev-parse", b]))
            .collect();
        repo.git(&["init", "--bare", repo.bare.to_str().unwrap()]);
        repo.git(&["remote", "add", "origin", repo.bare.to_str().unwrap()]);
        repo.git(&["push", "origin", "main", "a", "b", "c"]);
        repo.git(&["checkout", "main"]);
        if conflict > 0 {
            repo.commit("shared", "upstream\n");
        } else {
            repo.commit("upstream.txt", "upstream\n");
        }
        repo.git(&["push", "origin", "main"]);
        repo.git(&["checkout", "c"]);
        let prs: Vec<_> = ["a","b","c"].iter().enumerate().map(|(i,b)| json!({
            "number":i+1,"headRefName":b,"headRefOid":repo.old[i],"baseRefName":if i==0 {"main"} else {["a","b","c"][i-1]},
            "headRepositoryOwner":{"login":"owner"},"url":format!("https://github.com/owner/repo/pull/{}",i+1),"isCrossRepository":false
        })).collect();
        let state =
            json!({"prs":prs,"merged":[],"base":repo.git(&["rev-parse","main"]),"bare":repo.bare});
        fs::write(
            repo.root.join(".git/mock-gh.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        for (name, source) in [
            ("gh", include_str!("mock_gh.py")),
            ("git", include_str!("mock_git.py")),
        ] {
            let path = repo.bin.join(name);
            fs::write(&path, source).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        repo
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root.join(".git/mock-gh.json")).unwrap()).unwrap()
    }

    fn set(&self, key: &str, value: Value) {
        let mut state = self.state();
        state[key] = value;
        fs::write(
            self.root.join(".git/mock-gh.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
    }

    fn sms_at(&self, args: &[&str], dir: &Path) -> Output {
        self.command(env!("CARGO_BIN_EXE_stackymcstackface"))
            .current_dir(dir)
            .args(args)
            .env(
                "PATH",
                format!("{}:{}", self.bin.display(), std::env::var("PATH").unwrap()),
            )
            .env("REAL_GIT", &self.real_git)
            .env("MOCK_REPO", &self.root)
            .output()
            .unwrap()
    }

    fn sms(&self, args: &[&str]) -> Output {
        self.sms_at(args, &self.root)
    }
    fn journal(&self) -> PathBuf {
        self.root.join(".git/stackymcstackface-rebase.json")
    }

    fn tips(&self) -> Vec<String> {
        ["a", "b", "c"]
            .iter()
            .map(|b| self.git(&["rev-parse", b]))
            .collect()
    }

    fn remote_tips(&self) -> Vec<String> {
        ["a", "b", "c"]
            .iter()
            .map(|b| self.git(&["--git-dir", self.bare.to_str().unwrap(), "rev-parse", b]))
            .collect()
    }

    fn mutations(&self) -> usize {
        let log = fs::read_to_string(self.root.join(".git/mock-gh.log")).unwrap_or_default();
        log.lines()
            .filter(|l| l.contains("merge_method=merge"))
            .count()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp);
    }
}

fn fails(out: Output, expected: &str) -> String {
    assert!(!out.status.success(), "unexpected success: {}", text(&out));
    let output = text(&out);
    assert!(output.contains(expected), "expected {expected:?}: {output}");
    output
}

#[test]
fn rebase_whole_chain_from_middle_preserves_layer_boundaries() {
    let repo = Repo::new(0);
    repo.git(&["checkout", "b"]);
    // A hostile global updateRefs setting must not move a non-stack alias.
    repo.git(&["branch", "unrelated", "a"]);
    repo.git(&["config", "rebase.updateRefs", "true"]);
    ok(repo.sms(&["rebase"]));
    assert_ne!(repo.tips(), repo.old);
    assert_eq!(repo.remote_tips(), repo.old);
    assert_eq!(repo.git(&["symbolic-ref", "--short", "HEAD"]), "b");
    assert_eq!(repo.git(&["rev-parse", "unrelated"]), repo.old[0]);
    for (parent, child) in [("main", "a"), ("a", "b"), ("b", "c")] {
        repo.git(&["merge-base", "--is-ancestor", parent, child]);
        assert_eq!(
            repo.git(&["rev-list", "--count", &format!("{parent}..{child}")]),
            "1"
        );
    }
    assert_eq!(repo.git(&["show", "c:a.txt"]), "a");
    assert_eq!(repo.git(&["show", "c:b.txt"]), "b");
    assert!(!repo.journal().exists());
    assert_eq!(
        repo.git(&[
            "for-each-ref",
            "--format=%(refname)",
            "refs/stackymcstackface",
            "refs/heads/sms-rebase-"
        ]),
        ""
    );
}

#[test]
fn conflicts_on_multiple_layers_continue_with_staged_and_committed_resolutions() {
    let repo = Repo::new(1);
    fails(repo.sms(&["rebase"]), "rebase stopped");
    assert_eq!(repo.tips(), repo.old);
    fails(repo.sms(&["push"]), "stack rebase is in progress");
    fails(repo.sms(&["merge", "--yes"]), "stack rebase is in progress");
    fs::write(repo.root.join("shared"), "resolved first\n").unwrap();
    repo.git(&["add", "shared"]);
    // A valid manual resolution commit is accepted by native git --continue.
    repo.git(&["commit", "-m", "resolve first"]);
    fails(repo.sms(&["rebase", "--continue"]), "rebase stopped");
    assert_eq!(repo.tips(), repo.old);
    fs::write(repo.root.join("shared"), "resolved second\n").unwrap();
    repo.git(&["add", "shared"]);
    ok(repo.sms(&["rebase", "--continue"]));
    assert_eq!(repo.git(&["show", "c:shared"]), "resolved second");
    assert_eq!(repo.git(&["symbolic-ref", "--short", "HEAD"]), "c");
    assert!(!repo.journal().exists());
}

#[test]
fn abort_after_later_conflict_restores_every_original() {
    let repo = Repo::new(2);
    fails(repo.sms(&["rebase"]), "rebase stopped");
    let journal: Value = serde_json::from_slice(&fs::read(repo.journal()).unwrap()).unwrap();
    assert_eq!(journal["phase"]["Replaying"]["index"], 1);
    ok(repo.sms(&["rebase", "--abort"]));
    assert_eq!(repo.tips(), repo.old);
    assert_eq!(repo.git(&["symbolic-ref", "--short", "HEAD"]), "c");
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
    assert!(!repo.journal().exists());
}

#[test]
fn manual_abort_is_not_mistaken_for_success() {
    let repo = Repo::new(2);
    fails(repo.sms(&["rebase"]), "rebase stopped");
    repo.git(&["rebase", "--abort"]);
    fails(
        repo.sms(&["rebase", "--continue"]),
        "cannot prove this layer completed",
    );
    assert_eq!(repo.tips(), repo.old);
    ok(repo.sms(&["rebase", "--abort"]));
    assert_eq!(repo.tips(), repo.old);
}

#[test]
fn external_branch_edit_is_never_overwritten() {
    let repo = Repo::new(2);
    fails(repo.sms(&["rebase"]), "rebase stopped");
    repo.git(&["update-ref", "refs/heads/a", &repo.old[1]]);
    fails(repo.sms(&["rebase", "--continue"]), "refs changed");
    fails(repo.sms(&["rebase", "--abort"]), "refs changed");
    assert_eq!(repo.git(&["rev-parse", "a"]), repo.old[1]);
    assert!(repo.journal().exists());
}

#[test]
fn dirty_unpublished_foreign_rebase_and_other_worktrees_are_rejected() {
    let repo = Repo::new(0);
    fs::write(repo.root.join("untracked"), "work").unwrap();
    fails(repo.sms(&["rebase"]), "dirty");
    fails(repo.sms(&["merge", "--yes"]), "dirty");
    fs::remove_file(repo.root.join("untracked")).unwrap();
    repo.commit("extra", "unpublished");
    fails(repo.sms(&["rebase"]), "publish/synchronize");
    fails(repo.sms(&["merge", "--yes"]), "publish/synchronize");
    repo.git(&["reset", "--hard", &repo.old[2]]);
    let other = repo.temp.join("other worktree");
    repo.git(&["worktree", "add", other.to_str().unwrap(), "a"]);
    fails(repo.sms(&["rebase"]), "another worktree");
    fs::write(other.join("untracked"), "dirty").unwrap();
    fails(repo.sms(&["merge", "--yes"]), "dirty worktree");
    let conflict = Repo::new(1);
    let native = conflict
        .command(&conflict.real_git)
        .args(["rebase", "main"])
        .output()
        .unwrap();
    assert!(!native.status.success());
    fails(conflict.sms(&["rebase"]), "already in progress");
    fails(
        conflict.sms(&["rebase", "--continue"]),
        "no stack rebase recovery record",
    );
    assert!(!conflict.journal().exists());
}

#[test]
fn wrong_worktree_and_corrupt_record_fail_closed() {
    let repo = Repo::new(2);
    fails(repo.sms(&["rebase"]), "rebase stopped");
    let other = repo.temp.join("linked");
    repo.git(&[
        "worktree",
        "add",
        "--detach",
        other.to_str().unwrap(),
        "main",
    ]);
    fails(
        repo.sms_at(&["rebase", "--continue"], &other),
        "original worktree",
    );
    fails(
        repo.sms_at(&["push"], &other),
        "stack rebase is in progress",
    );
    fs::write(repo.journal(), "not json").unwrap();
    fails(
        repo.sms(&["rebase", "--abort"]),
        "corrupt stack rebase record",
    );
    assert_eq!(repo.tips(), repo.old);
}

#[test]
fn atomic_push_has_explicit_leases_and_recovers_uncertain_success() {
    let repo = Repo::new(0);
    repo.set("uncertain_push", json!(true));
    fails(repo.sms(&["rebase", "--push"]), "result is uncertain");
    assert_ne!(repo.tips(), repo.old);
    assert_eq!(repo.remote_tips(), repo.tips());
    assert!(repo.journal().exists());
    fails(repo.sms(&["rebase", "--abort"]), "already published");
    ok(repo.sms(&["rebase", "--continue"]));
    assert!(!repo.journal().exists());
    let log = fs::read_to_string(repo.root.join(".git/mock-push.log")).unwrap();
    assert_eq!(log.lines().count(), 1);
    assert!(log.contains("--atomic"));
    for (i, b) in ["a", "b", "c"].iter().enumerate() {
        assert!(log.contains(&format!(
            "--force-with-lease=refs/heads/{b}:{}",
            repo.old[i]
        )));
    }
}

#[test]
fn push_lease_race_updates_no_other_branches() {
    let repo = Repo::new(0);
    repo.set("race_push_branch", json!("c"));
    fails(repo.sms(&["rebase", "--push"]), "atomic push failed");
    let remote = repo.remote_tips();
    assert_eq!(remote[0], repo.old[0]);
    assert_eq!(remote[1], repo.old[1]);
    assert_eq!(remote[2], repo.state()["base"].as_str().unwrap());
    fails(repo.sms(&["rebase", "--continue"]), "remote stack changed");
    assert!(repo.journal().exists());
}

#[test]
fn failed_push_can_abort_all_local_updates_or_retry() {
    let repo = Repo::new(0);
    repo.set("reject_push", json!(true));
    fails(repo.sms(&["rebase", "--push"]), "atomic push failed");
    assert_ne!(repo.tips(), repo.old);
    assert_eq!(repo.remote_tips(), repo.old);
    ok(repo.sms(&["rebase", "--abort"]));
    assert_eq!(repo.tips(), repo.old);
    assert_eq!(repo.git(&["status", "--porcelain"]), "");
    repo.set("reject_push", json!(false));
    ok(repo.sms(&["rebase", "--push"]));
    assert_eq!(repo.remote_tips(), repo.tips());
}

#[test]
fn blocked_top_and_other_unready_states_cause_zero_merges() {
    let repo = Repo::new(0);
    for (key, value, expected) in [
        ("blocked", json!(3), "not ready"),
        ("draft", json!(2), "non-draft"),
        ("unknown", json!(3), "not ready"),
        ("failed_check", json!(3), "failed, pending"),
        ("changed_head", json!(3), "head/identity changed"),
        ("queue", json!(2), "merge queue"),
        ("rule_queue", json!(true), "merge queue"),
        ("rules_error", json!(true), "rules unavailable"),
        ("no_delete", json!(true), "delete_branch_on_merge"),
        ("no_merge_commits", json!(true), "allow merge commits"),
    ] {
        repo.set(key, value);
        fails(repo.sms(&["merge", "--yes"]), expected);
        assert_eq!(repo.mutations(), 0, "{key}");
        repo.set(key, Value::Null);
    }
}

#[test]
fn merge_success_retargets_and_dry_run_never_mutates() {
    let repo = Repo::new(0);
    ok(repo.sms(&["merge", "--dry-run"]));
    assert_eq!(repo.mutations(), 0);
    ok(repo.sms(&["merge", "--yes"]));
    assert_eq!(repo.state()["merged"], json!([1, 2, 3]));
    assert_eq!(repo.mutations(), 3);
    assert_eq!(repo.tips(), repo.old);
}

#[test]
fn partial_merge_reports_progress_and_rerun_discovers_remaining_stack() {
    let repo = Repo::new(0);
    repo.set("partial_block", json!(2));
    let output = fails(repo.sms(&["merge", "--yes"]), "merge stopped");
    assert!(output.contains("confirmed merged: #1"));
    assert!(output.contains("#2 (b), #3 (c)"));
    assert_eq!(repo.mutations(), 1);
    repo.set("partial_block", Value::Null);
    // Mock retargeting keeps main's real fetched SHA stable; b's initial
    // ancestry remains valid when the root becomes b.
    ok(repo.sms(&["merge", "--yes"]));
    assert_eq!(repo.state()["merged"], json!([1, 2, 3]));
}

#[test]
fn wrong_push_destination_and_truncated_discovery_never_mutate() {
    let repo = Repo::new(0);
    repo.set("wrong_push_url", json!(true));
    fails(repo.sms(&["rebase"]), "exactly one fetch and one push URL");
    fails(
        repo.sms(&["merge", "--yes"]),
        "exactly one fetch and one push URL",
    );
    repo.set("wrong_push_url", Value::Null);
    repo.set("truncated", json!(true));
    fails(repo.sms(&["rebase"]), "possibly truncated stack");
    fails(repo.sms(&["merge", "--yes"]), "possibly truncated stack");
    assert!(!repo.journal().exists());
    assert_eq!(repo.mutations(), 0);
}

#[test]
fn concurrent_command_lock_is_fail_closed() {
    let repo = Repo::new(0);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(repo.root.join(".git/stackymcstackface.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    fails(repo.sms(&["rebase"]), "another stackymcstackface command");
    fails(
        repo.sms(&["merge", "--yes"]),
        "another stackymcstackface command",
    );
    fails(repo.sms(&["push"]), "another stackymcstackface command");
}

#[test]
fn native_completion_checkpoint_recovers_before_journal_save() {
    let repo = Repo::new(2);
    fails(repo.sms(&["rebase"]), "rebase stopped");
    fs::write(repo.root.join("shared"), "resolved\n").unwrap();
    repo.git(&["add", "shared"]);
    // Equivalent to a process crash after Git exits but before our journal save.
    repo.git(&["rebase", "--continue"]);
    assert_eq!(repo.tips(), repo.old);
    ok(repo.sms(&["rebase", "--continue"]));
    assert!(!repo.journal().exists());
    assert_eq!(repo.git(&["show", "c:shared"]), "resolved");
}

#[test]
fn applying_transaction_recovers_both_sides_of_atomic_ref_update() {
    for (installed, original_checkout) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let repo = Repo::new(0);
        repo.set("reject_push", json!(true));
        fails(repo.sms(&["rebase", "--push"]), "atomic push failed");
        let rewritten = repo.tips();
        let mut journal: Value =
            serde_json::from_slice(&fs::read(repo.journal()).unwrap()).unwrap();
        // Simulate the durable Applying phase on either side of the Git ref
        // transaction. No runtime failure injection is added to production code.
        journal["phase"] = json!("Applying");
        journal["push"] = json!(false);
        fs::write(repo.journal(), serde_json::to_vec(&journal).unwrap()).unwrap();
        if !installed {
            for (i, branch) in ["a", "b", "c"].iter().enumerate() {
                repo.git(&[
                    "update-ref",
                    &format!("refs/heads/{branch}"),
                    &repo.old[i],
                    &rewritten[i],
                ]);
            }
        }
        if original_checkout {
            repo.git(&["checkout", "c"]);
        }
        ok(repo.sms(&["rebase", "--continue"]));
        assert_eq!(repo.tips(), rewritten);
        assert_eq!(repo.git(&["status", "--porcelain"]), "");
        assert_eq!(repo.git(&["branch", "--show-current"]), "c");
        assert!(!repo.journal().exists());
    }
}

#[test]
fn foreign_native_rebase_is_never_aborted_by_stack_recovery() {
    let repo = Repo::new(2);
    fails(repo.sms(&["rebase"]), "rebase stopped");
    repo.git(&["rebase", "--abort"]);
    repo.git(&["checkout", "-b", "foreign", "b"]);
    let out = repo
        .command(&repo.real_git)
        .args(["rebase", "main"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    fails(repo.sms(&["rebase", "--continue"]), "does not match");
    fails(repo.sms(&["rebase", "--abort"]), "does not match");
    assert!(repo.root.join(".git/rebase-merge").exists());
    assert_eq!(repo.tips(), repo.old);
}

#[test]
fn abort_refuses_unrelated_dirty_work_after_a_failed_push() {
    let repo = Repo::new(0);
    repo.set("reject_push", json!(true));
    fails(repo.sms(&["rebase", "--push"]), "atomic push failed");
    fs::write(repo.root.join("unrelated-work"), "keep me").unwrap();
    fails(
        repo.sms(&["rebase", "--abort"]),
        "refusing to discard unrelated work",
    );
    assert_eq!(
        fs::read_to_string(repo.root.join("unrelated-work")).unwrap(),
        "keep me"
    );
    assert_ne!(repo.tips(), repo.old);
}

#[test]
fn merge_commit_histories_and_missing_local_heads_are_rejected() {
    let repo = Repo::new(0);
    repo.git(&["branch", "-D", "a"]);
    fails(repo.sms(&["rebase"]), "must exist locally");
    repo.git(&["branch", "a", &repo.old[0]]);
    repo.git(&["checkout", "-b", "side", "b"]);
    repo.commit("side.txt", "side");
    repo.git(&["checkout", "c"]);
    repo.git(&["merge", "--no-ff", "side", "-m", "merge side"]);
    repo.git(&["push", "origin", "c"]);
    let mut prs = repo.state()["prs"].clone();
    prs[2]["headRefOid"] = json!(repo.git(&["rev-parse", "c"]));
    repo.set("prs", prs);
    fails(repo.sms(&["rebase"]), "contains merge commits");
    assert!(!repo.journal().exists());
}

#[test]
fn rebase_publication_rechecks_pr_topology_before_retry() {
    let repo = Repo::new(0);
    repo.set("reject_push", json!(true));
    fails(repo.sms(&["rebase", "--push"]), "atomic push failed");
    let mut prs = repo.state()["prs"].clone();
    prs[2]["baseRefName"] = json!("a");
    repo.set("prs", prs);
    repo.set("reject_push", json!(false));
    fails(repo.sms(&["rebase", "--continue"]), "branching");
    assert_eq!(repo.remote_tips(), repo.old);
    assert_eq!(
        fs::read_to_string(repo.root.join(".git/mock-push.log"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    ok(repo.sms(&["rebase", "--abort"]));
    assert_eq!(repo.tips(), repo.old);
}

#[test]
fn discovery_fetch_cannot_apply_configured_local_ref_mappings() {
    for rebase in [false, true] {
        let repo = Repo::new(0);
        repo.git(&["branch", "unrelated", "main"]);
        let unrelated = repo.git(&["rev-parse", "unrelated"]);
        repo.git(&[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/heads/a:refs/heads/unrelated",
        ]);
        ok(repo.sms(if rebase {
            &["rebase"]
        } else {
            &["merge", "--dry-run"]
        }));
        assert_eq!(repo.git(&["rev-parse", "unrelated"]), unrelated);
    }
}

#[test]
fn discovery_fetch_cannot_erase_unpublished_stack_commits() {
    for rebase in [false, true] {
        let repo = Repo::new(0);
        repo.git(&["checkout", "a"]);
        repo.commit("unpublished.txt", "keep local work");
        let unpublished = repo.git(&["rev-parse", "a"]);
        repo.git(&["checkout", "c"]);
        repo.git(&[
            "config",
            "--add",
            "remote.origin.fetch",
            "+refs/heads/a:refs/heads/a",
        ]);
        fails(
            repo.sms(if rebase {
                &["rebase"]
            } else {
                &["merge", "--yes"]
            }),
            "differs between local",
        );
        assert_eq!(repo.git(&["rev-parse", "a"]), unpublished);
        assert!(!repo.journal().exists());
        assert_eq!(repo.mutations(), 0);
    }
}

#[test]
fn discovery_fetch_preserves_local_tags_despite_prune_configuration() {
    let repo = Repo::new(0);
    repo.git(&["tag", "-a", "local-only", "main", "-m", "local tag"]);
    let tag = repo.git(&["rev-parse", "refs/tags/local-only"]);
    repo.git(&["config", "fetch.pruneTags", "true"]);
    repo.git(&["config", "remote.origin.pruneTags", "true"]);
    ok(repo.sms(&["merge", "--dry-run"]));
    assert_eq!(repo.git(&["rev-parse", "refs/tags/local-only"]), tag);
}

#[test]
fn symbolic_stack_refs_are_rejected_before_rebase_or_merge() {
    let repo = Repo::new(0);
    repo.git(&["branch", "unrelated", "a"]);
    repo.git(&["symbolic-ref", "refs/heads/a", "refs/heads/unrelated"]);
    fails(repo.sms(&["rebase"]), "symbolic ref");
    fails(repo.sms(&["merge", "--yes"]), "symbolic ref");
    assert_eq!(repo.git(&["rev-parse", "unrelated"]), repo.old[0]);
    assert!(!repo.journal().exists());
    assert_eq!(repo.mutations(), 0);
}

#[test]
fn recovery_rejects_symbolic_original_and_private_refs() {
    for kind in ["original", "stage", "backup", "checkpoint", "base"] {
        let repo = Repo::new(1);
        fails(repo.sms(&["rebase"]), "rebase stopped");
        let journal: Value = serde_json::from_slice(&fs::read(repo.journal()).unwrap()).unwrap();
        let id = journal["id"].as_str().unwrap();
        let name = match kind {
            "original" => "refs/heads/a".to_owned(),
            "stage" => format!("refs/heads/sms-rebase-{id}/0"),
            "backup" => format!("refs/stackymcstackface/{id}/original/0"),
            "checkpoint" => format!("refs/stackymcstackface/{id}/checkpoint/0"),
            _ => format!("refs/stackymcstackface/{id}/base"),
        };
        let before = repo
            .command(&repo.real_git)
            .args(["rev-parse", "--verify", &name])
            .output()
            .unwrap();
        let original = before.status.success().then(|| ok(before));
        let referent = original
            .clone()
            .unwrap_or_else(|| repo.git(&["rev-parse", "HEAD"]));
        repo.git(&["branch", "unrelated", &referent]);
        repo.git(&["symbolic-ref", &name, "refs/heads/unrelated"]);
        fails(repo.sms(&["rebase", "--continue"]), "symbolic ref");
        fails(repo.sms(&["rebase", "--abort"]), "symbolic ref");
        assert_eq!(repo.git(&["rev-parse", "unrelated"]), referent, "{kind}");
        repo.git(&["symbolic-ref", "--delete", &name]);
        if let Some(original) = original {
            repo.git(&["update-ref", &name, &original]);
        }
        ok(repo.sms(&["rebase", "--abort"]));
        assert_eq!(repo.tips(), repo.old);
    }
}

#[test]
fn stack_publication_does_not_follow_annotated_tags() {
    let repo = Repo::new(0);
    // The new base is reachable from every rewritten result, so this tag
    // would otherwise be eligible for push.followTags.
    repo.git(&[
        "tag",
        "-a",
        "unrequested-release",
        "main",
        "-m",
        "do not publish",
    ]);
    repo.git(&["config", "push.followTags", "true"]);
    ok(repo.sms(&["rebase", "--push"]));
    assert_eq!(repo.remote_tips(), repo.tips());
    assert_eq!(
        repo.git(&[
            "ls-remote",
            "--tags",
            "origin",
            "refs/tags/unrequested-release"
        ]),
        ""
    );
}

#[test]
fn merge_rejects_off_chain_ancestors_and_equal_sha_aliases() {
    let repo = Repo::new(0);
    let mut prs = repo.state()["prs"].clone();
    prs[1]["baseRefName"] = json!("main");
    repo.set("prs", prs);
    repo.set("blocked", json!(1));
    repo.git(&["checkout", "b"]);
    fails(repo.sms(&["merge", "--yes"]), "unchecked ancestor");
    assert_eq!(repo.mutations(), 0);
    for cross_repo in [false, true] {
        let repo = Repo::new(0);
        let mut prs = repo.state()["prs"].clone();
        let mut alias = prs[2].clone();
        alias["number"] = json!(4);
        alias["headRefName"] = json!("alias");
        alias["baseRefName"] = json!("main");
        alias["isCrossRepository"] = json!(cross_repo);
        prs.as_array_mut().unwrap().push(alias);
        repo.set("prs", prs);
        fails(repo.sms(&["merge", "--yes"]), "unchecked ancestor");
        assert_eq!(repo.mutations(), 0);
    }
}

#[test]
fn merge_rechecks_off_chain_ancestors_before_each_mutation() {
    for list_call in [2, 3] {
        let repo = Repo::new(0);
        repo.set("late_alias_after", json!(list_call));
        fails(repo.sms(&["merge", "--yes"]), "unchecked ancestor");
        assert_eq!(repo.mutations(), list_call - 2);
    }
}

#[test]
fn symbolic_fetch_destinations_cannot_update_unrelated_branches() {
    let repo = Repo::new(0);
    repo.git(&["branch", "unrelated", "main"]);
    let unrelated = repo.git(&["rev-parse", "unrelated"]);
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ]);
    repo.git(&[
        "symbolic-ref",
        "refs/remotes/origin/a",
        "refs/heads/unrelated",
    ]);
    fails(
        repo.sms(&["merge", "--dry-run"]),
        "symbolic fetch destination",
    );
    fails(repo.sms(&["rebase"]), "symbolic fetch destination");
    assert_eq!(repo.git(&["rev-parse", "unrelated"]), unrelated);
    repo.git(&["symbolic-ref", "--delete", "refs/remotes/origin/a"]);
    ok(repo.sms(&["merge", "--dry-run"]));
    assert_eq!(
        repo.git(&["symbolic-ref", "refs/remotes/origin/HEAD"]),
        "refs/remotes/origin/main"
    );
}

#[test]
fn replacement_graphs_are_rejected_before_discovery_or_mutation() {
    let repo = Repo::new(0);
    repo.git(&["replace", "a", "main"]);
    fails(repo.sms(&["merge", "--yes"]), "replacement refs or grafts");
    fails(repo.sms(&["rebase"]), "replacement refs or grafts");
    assert!(!repo.journal().exists());
    assert_eq!(repo.mutations(), 0);
}

#[test]
fn unrelated_unfetched_fork_pr_does_not_block_a_ready_stack() {
    let repo = Repo::new(0);
    let mut prs = repo.state()["prs"].clone();
    let mut unrelated = prs[0].clone();
    unrelated["number"] = json!(4);
    unrelated["headRefName"] = json!("unrelated-fork");
    unrelated["headRefOid"] = json!("f".repeat(40));
    unrelated["headRepositoryOwner"] = json!({"login": "fork-owner"});
    unrelated["isCrossRepository"] = json!(true);
    prs.as_array_mut().unwrap().push(unrelated);
    repo.set("prs", prs);
    ok(repo.sms(&["merge", "--dry-run"]));
    assert_eq!(repo.mutations(), 0);
    ok(repo.sms(&["merge", "--yes"]));
    assert_eq!(repo.state()["merged"], json!([1, 2, 3]));
}

#[test]
fn shallow_history_is_rejected_before_discovery_or_mutation() {
    let repo = Repo::new(0);
    fs::write(repo.root.join(".git/shallow"), format!("{}\n", repo.old[0])).unwrap();
    fails(repo.sms(&["merge", "--yes"]), "shallow history");
    fails(repo.sms(&["rebase"]), "shallow history");
    assert!(!repo.journal().exists());
    assert_eq!(repo.mutations(), 0);
}
