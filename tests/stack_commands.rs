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
fn paused_rebase_in_another_worktree_reserves_its_branch() {
    for backend in ["--merge", "--apply"] {
        let repo = Repo::new(1);
        let other = repo.temp.join("other worktree");
        let other_path = other.to_str().unwrap();
        repo.git(&["worktree", "add", other_path, "a"]);
        let native = repo
            .command(&repo.real_git)
            .current_dir(&other)
            .args(["rebase", backend, "main"])
            .output()
            .unwrap();
        assert!(!native.status.success(), "{}", text(&native));
        assert_eq!(
            repo.git(&["-C", other_path, "branch", "--show-current"]),
            ""
        );

        fails(repo.sms(&["rebase"]), "another worktree");
        fails(repo.sms(&["merge", "--yes"]), "in-progress operation");
        assert!(!repo.journal().exists());
        assert_eq!(repo.mutations(), 0);
        repo.git(&["-C", other_path, "rebase", "--abort"]);
        assert_eq!(repo.tips(), repo.old);
    }
}

#[test]
fn rebase_preserves_ignored_files_when_the_new_base_tracks_their_path() {
    let repo = Repo::new(0);
    repo.git(&["switch", "main"]);
    repo.commit("local.env", "upstream");
    repo.git(&["push", "origin", "main"]);
    repo.set("base", json!(repo.git(&["rev-parse", "main"])));
    repo.git(&["switch", "c"]);
    fs::write(repo.root.join(".git/info/exclude"), "local.env\nkeep.env\n").unwrap();
    let local = repo.root.join("local.env");
    let keep = repo.root.join("keep.env");
    fs::write(&local, "local settings").unwrap();
    fs::write(&keep, "unrelated ignored file").unwrap();
    assert_eq!(repo.git(&["status", "--porcelain"]), "");

    fails(
        repo.sms(&["rebase"]),
        "could overwrite an untracked or ignored local path",
    );
    assert_eq!(fs::read_to_string(&local).unwrap(), "local settings");
    assert_eq!(repo.tips(), repo.old);
    fs::rename(&local, repo.temp.join("local.env")).unwrap();
    ok(repo.sms(&["rebase", "--continue"]));
    assert_eq!(fs::read_to_string(&local).unwrap(), "upstream");
    assert_eq!(fs::read_to_string(&keep).unwrap(), "unrelated ignored file");
}

#[test]
fn rebase_continue_protects_an_ignored_file_added_during_a_conflict() {
    let repo = Repo::new(0);
    repo.commit("shared", "topic change");
    repo.commit("local.env", "topic settings");
    repo.git(&["push", "origin", "c"]);
    let mut prs = repo.state()["prs"].clone();
    prs[2]["headRefOid"] = json!(repo.git(&["rev-parse", "c"]));
    repo.set("prs", prs);
    repo.git(&["switch", "main"]);
    repo.commit("shared", "upstream change");
    repo.git(&["push", "origin", "main"]);
    repo.set("base", json!(repo.git(&["rev-parse", "main"])));
    repo.git(&["switch", "c"]);
    fails(repo.sms(&["rebase"]), "rebase stopped");

    fs::write(repo.root.join(".git/info/exclude"), "local.env\n").unwrap();
    let local = repo.root.join("local.env");
    fs::write(&local, "local settings").unwrap();
    fs::write(repo.root.join("shared"), "resolved").unwrap();
    repo.git(&["add", "shared"]);
    fails(
        repo.sms(&["rebase", "--continue"]),
        "could overwrite an untracked or ignored local path",
    );
    assert_eq!(fs::read_to_string(&local).unwrap(), "local settings");
    fs::rename(&local, repo.temp.join("local.env")).unwrap();
    ok(repo.sms(&["rebase", "--continue"]));
    assert_eq!(fs::read_to_string(&local).unwrap(), "topic settings");
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
fn a_layer_becoming_draft_after_initial_preflight_prevents_all_merges() {
    let repo = Repo::new(0);
    repo.set("draft_after_first_read", json!(3));
    fails(
        repo.sms(&["merge", "--yes"]),
        "PR #3 is not open and non-draft",
    );
    assert_eq!(repo.mutations(), 0);
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

#[test]
fn view_and_navigation_from_middle_are_stateless_and_do_not_fetch() {
    let repo = Repo::new(0);
    repo.git(&["checkout", "b"]);
    repo.set("draft", json!(3));
    let view = ok(repo.sms(&["view"]));
    assert!(view.contains("#1 [OPEN] a"), "{view}");
    assert!(view.contains("#2 [OPEN] b — PR 2  <- current"), "{view}");
    assert!(view.contains("#3 [DRAFT] c"), "{view}");
    assert!(view.find("#1").unwrap() < view.find("#2").unwrap());
    assert!(view.find("#2").unwrap() < view.find("#3").unwrap());
    assert!(!repo.root.join(".git/stackymcstackface.lock").exists());
    assert_eq!(repo.git(&["branch", "--show-current"]), "b");
    ok(repo.sms(&["up"]));
    assert_eq!(repo.git(&["branch", "--show-current"]), "c");
    fails(repo.sms(&["up"]), "no next PR");
    ok(repo.sms(&["down"]));
    assert_eq!(repo.git(&["branch", "--show-current"]), "b");
    ok(repo.sms(&["jump", "1"]));
    assert_eq!(repo.git(&["branch", "--show-current"]), "a");
    fails(repo.sms(&["down"]), "no prior PR");
    fails(repo.sms(&["jump", "42"]), "not in this view");
    fails(repo.sms(&["jump", "0"]), "invalid value");
    assert_eq!(repo.tips(), repo.old);
    assert_eq!(repo.remote_tips(), repo.old);
    let log = fs::read_to_string(repo.root.join(".git/mock-git.log")).unwrap();
    for line in log.lines() {
        let args: Vec<String> = serde_json::from_str(line).unwrap();
        assert!(!["fetch", "push", "reset", "stash"].contains(&args[0].as_str()));
    }
    assert_eq!(repo.mutations(), 0);
}

#[test]
fn view_shows_branches_but_up_requires_an_explicit_choice() {
    let repo = Repo::new(0);
    let mut prs = repo.state()["prs"].clone();
    prs[2]["baseRefName"] = json!("a");
    repo.set("prs", prs);
    repo.git(&["checkout", "a"]);
    let view = ok(repo.sms(&["view"]));
    assert!(view.lines().any(|l| l.starts_with("#2 [OPEN]")), "{view}");
    assert!(view.lines().any(|l| l.starts_with("#3 [OPEN]")), "{view}");
    assert_eq!(view.matches("base: #1 (a)").count(), 2);
    assert!(!view.contains("└─"));
    assert!(!view.contains('\x1b'));
    fails(repo.sms(&["up"]), "multiple next PRs: #2, #3");
    assert_eq!(repo.git(&["branch", "--show-current"]), "a");
    ok(repo.sms(&["jump", "3"]));
    ok(repo.sms(&["down"]));
    assert_eq!(repo.git(&["branch", "--show-current"]), "a");
}

#[test]
fn view_gets_status_in_the_existing_list_but_navigation_does_not() {
    let repo = Repo::new(0);
    let mut prs = repo.state()["prs"].clone();
    prs[0]["mergeable"] = json!("MERGEABLE");
    prs[0]["mergeStateStatus"] = json!("BEHIND");
    prs[0]["reviewDecision"] = json!("CHANGES_REQUESTED");
    prs[1]["mergeable"] = json!("CONFLICTING");
    prs[1]["mergeStateStatus"] = json!("DIRTY");
    prs[1]["reviewDecision"] = json!("APPROVED");
    prs[2]["mergeable"] = json!("UNKNOWN");
    prs[2]["mergeStateStatus"] = Value::Null;
    prs[2]["reviewDecision"] = Value::Null;
    repo.set("prs", prs);
    let view = ok(repo.sms(&["view"]));
    assert!(
        view.contains("merge: no conflicts · GitHub: behind base · reviews: changes requested"),
        "{view}"
    );
    assert!(
        view.contains("merge: conflicts · GitHub: conflicts · reviews: approved"),
        "{view}"
    );
    assert!(
        view.contains("merge: unknown · GitHub: unknown · reviews: no decision"),
        "{view}"
    );
    let log_path = repo.root.join(".git/mock-gh.log");
    let log = fs::read_to_string(&log_path).unwrap();
    assert_eq!(log.lines().count(), 2, "no per-PR status requests");
    assert!(log.contains("mergeable,mergeStateStatus,reviewDecision"));
    fs::write(&log_path, "").unwrap();
    ok(repo.sms(&["down"]));
    ok(repo.sms(&["up"]));
    ok(repo.sms(&["jump", "1"]));
    let log = fs::read_to_string(log_path).unwrap();
    assert!(!log.contains("mergeable"));
    assert!(!log.contains("mergeStateStatus"));
    assert!(!log.contains("reviewDecision"));
}

#[test]
fn default_branch_view_lists_roots_and_jump_uses_real_pr_numbers() {
    let repo = Repo::new(0);
    let mut prs = repo.state()["prs"].clone();
    prs[2]["baseRefName"] = json!("main");
    prs[2]["number"] = json!(123);
    repo.set("prs", prs);
    // From a member, an unrelated root is not a jump destination.
    repo.git(&["checkout", "b"]);
    fails(repo.sms(&["jump", "123"]), "not in this view");
    repo.git(&["checkout", "main"]);
    let view = ok(repo.sms(&["view"]));
    assert!(view.contains("#123 [OPEN] c"));
    assert!(view.contains("#1 [OPEN] a"));
    fails(repo.sms(&["up"]), "default branch is not a PR");
    ok(repo.sms(&["jump", "123"]));
    assert_eq!(repo.git(&["branch", "--show-current"]), "c");
}

#[test]
fn explicit_history_survives_retargeting_and_supports_safe_jump() {
    let repo = Repo::new(0);
    let mut prs = repo.state()["prs"].clone();
    prs[1]["body"] = json!(
        "Stacked on #1 (`a`) by [stackymcstackface](https://github.com/cjrh/stackymcstackface) ❤️"
    );
    prs[2]["body"] = json!("Stacked on #2");
    repo.set("prs", prs);
    repo.set("merged", json!([1]));
    let view = ok(repo.sms(&["view"]));
    assert!(view.contains("base: main"), "{view}");
    assert!(view.contains("#1 [MERGED] a"), "{view}");
    assert!(view.contains("#2 previously stacked on #1"), "{view}");
    assert!(!view.contains("#3 previously stacked on #2"));
    let log = fs::read_to_string(repo.root.join(".git/mock-gh.log")).unwrap();
    let calls: Vec<Vec<String>> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(
        calls.len(),
        3,
        "repo metadata + open list + linked parent only"
    );
    assert_eq!(calls[2][..3], ["pr", "view", "1"]);
    ok(repo.sms(&["jump", "1"]));
    let view = ok(repo.sms(&["view"]));
    assert!(view.contains("#1 [MERGED] a — PR 1  <- current"), "{view}");
    assert!(view.contains("#3 [OPEN] c"), "{view}");
    ok(repo.sms(&["jump", "2"]));
    fails(repo.sms(&["down"]), "no prior PR");
    // Do not mistake a changed/reused local branch for the historical PR.
    repo.git(&["branch", "-f", "a", "main"]);
    fails(repo.sms(&["jump", "1"]), "missing, changed, or reused");
    assert_eq!(repo.git(&["branch", "--show-current"]), "b");
}

#[test]
fn shared_history_reconnects_retargeted_siblings_and_fetches_parent_once() {
    let repo = Repo::new(0);
    let mut prs = repo.state()["prs"].clone();
    for i in [1, 2] {
        prs[i]["baseRefName"] = json!("a");
        prs[i]["body"] = json!("Stacked on #1");
    }
    repo.set("prs", prs);
    repo.set("merged", json!([1]));
    let view = ok(repo.sms(&["view"]));
    assert!(view.contains("#2 [OPEN] b"), "{view}");
    assert!(view.contains("#3 [OPEN] c"), "{view}");
    let log = fs::read_to_string(repo.root.join(".git/mock-gh.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|l| l.contains("\"pr\", \"view\", \"1\""))
            .count(),
        1
    );
    ok(repo.sms(&["jump", "2"]));
}

#[test]
fn history_is_optional_bounded_and_does_not_override_current_bases() {
    let repo = Repo::new(0);
    let mut prs = repo.state()["prs"].clone();
    prs[1]["body"] = json!("Stacked on #1");
    prs[2]["body"] = json!("Stacked on #999");
    repo.set("prs", prs);
    repo.set("merged", json!([1]));
    repo.set("unavailable", json!(1));
    let view = ok(repo.sms(&["view"]));
    assert!(view.contains("linked #1 is unavailable"), "{view}");
    assert!(view.contains("linked #999 is unavailable"), "{view}");
    assert!(view.contains("#2 [OPEN] b"), "{view}");
    // Up/down need no history requests or bodies.
    fs::write(repo.root.join(".git/mock-gh.log"), "").unwrap();
    ok(repo.sms(&["down"]));
    let log = fs::read_to_string(repo.root.join(".git/mock-gh.log")).unwrap();
    assert_eq!(log.lines().count(), 2);
    assert!(!log.contains("body"));
}

#[test]
fn navigation_preserves_changes_and_respects_git_worktree_guards() {
    let repo = Repo::new(0);
    fs::write(repo.root.join("shared"), "uncommitted\n").unwrap();
    ok(repo.sms(&["down"]));
    assert_eq!(
        fs::read_to_string(repo.root.join("shared")).unwrap(),
        "uncommitted\n"
    );
    // Switching down would remove b.txt and must not discard this edit.
    fs::write(repo.root.join("b.txt"), "uncommitted b\n").unwrap();
    fails(repo.sms(&["down"]), "would be overwritten");
    assert_eq!(repo.git(&["branch", "--show-current"]), "b");
    assert_eq!(
        fs::read_to_string(repo.root.join("b.txt")).unwrap(),
        "uncommitted b\n"
    );
    repo.git(&["restore", "shared", "b.txt"]);
    let other = repo.temp.join("other worktree");
    repo.git(&["worktree", "add", other.to_str().unwrap(), "a"]);
    fails(repo.sms(&["down"]), "already");
    assert_eq!(repo.git(&["branch", "--show-current"]), "b");
}

#[test]
fn navigation_preserves_ignored_files_tracked_by_the_destination() {
    let repo = Repo::new(0);
    repo.git(&["switch", "b"]);
    fs::write(repo.root.join(".git/info/exclude"), "c.txt\n").unwrap();
    let local = repo.root.join("c.txt");
    fs::write(&local, "local-only content").unwrap();
    assert_eq!(repo.git(&["status", "--porcelain"]), "");

    fails(repo.sms(&["up"]), "would be overwritten");
    assert_eq!(repo.git(&["branch", "--show-current"]), "b");
    assert_eq!(fs::read_to_string(&local).unwrap(), "local-only content");
}

#[test]
fn navigation_refuses_missing_local_branches_and_in_progress_operations() {
    let repo = Repo::new(0);
    repo.git(&["branch", "-D", "b"]);
    ok(repo.sms(&["view"]));
    fails(repo.sms(&["down"]), "not available locally");
    fs::write(repo.journal(), "{}").unwrap();
    fails(repo.sms(&["jump", "1"]), "stack rebase is in progress");
    // Viewing is still read-only and useful during a recovery.
    ok(repo.sms(&["view"]));
    fs::remove_file(repo.journal()).unwrap();
    fs::write(repo.root.join(".git/MERGE_HEAD"), &repo.old[0]).unwrap();
    fails(repo.sms(&["jump", "1"]), "merge is already in progress");
    assert_eq!(repo.git(&["branch", "--show-current"]), "c");
}

#[test]
fn view_refuses_truncation_cycles_duplicates_and_detached_head() {
    let repo = Repo::new(0);
    repo.set("truncated", json!(true));
    fails(repo.sms(&["view"]), "1000-PR limit");
    repo.set("truncated", json!(false));
    let mut prs = repo.state()["prs"].clone();
    prs[0]["baseRefName"] = json!("c");
    repo.set("prs", prs.clone());
    fails(repo.sms(&["view"]), "cycle in active PR stack");
    prs[0]["baseRefName"] = json!("main");
    prs[1]["headRefName"] = json!("c");
    repo.set("prs", prs);
    fails(repo.sms(&["view"]), "multiple open PRs");
    repo.git(&["checkout", "--detach"]);
    fails(repo.sms(&["view"]), "HEAD is detached");
    fails(repo.sms(&["up"]), "HEAD is detached");
}

#[test]
fn closed_history_cycles_warn_without_hiding_active_prs() {
    let repo = Repo::new(0);
    let mut prs = repo.state()["prs"].as_array().unwrap().clone();
    prs[2]["body"] = json!("Stacked on #4");
    for (number, parent) in [(4, 5), (5, 4)] {
        let mut historical = prs[0].clone();
        historical["number"] = json!(number);
        historical["state"] = json!("CLOSED");
        historical["headRefName"] = json!(format!("old-{number}"));
        historical["body"] = json!(format!("Stacked on #{parent}"));
        prs.push(historical);
    }
    repo.set("prs", json!(prs));
    let view = ok(repo.sms(&["view"]));
    assert!(view.contains("#4 [CLOSED] old-4"), "{view}");
    assert!(view.contains("#5 [CLOSED] old-5"), "{view}");
    assert!(view.contains("cyclic history link"), "{view}");
    assert!(view.contains("#3 [OPEN] c"), "{view}");
    fails(repo.sms(&["jump", "4"]), "missing, changed, or reused");
    assert_eq!(repo.git(&["branch", "--show-current"]), "c");
}

#[test]
fn history_lookup_limit_is_visible_and_stops_requests() {
    let repo = Repo::new(0);
    let mut prs = repo.state()["prs"].as_array().unwrap().clone();
    prs[2]["body"] = json!("Stacked on #4");
    for number in 4..=105 {
        let mut historical = prs[0].clone();
        historical["number"] = json!(number);
        historical["state"] = json!("CLOSED");
        historical["headRefName"] = json!(format!("old-{number}"));
        historical["body"] = json!(format!("Stacked on #{}", number + 1));
        prs.push(historical);
    }
    repo.set("prs", json!(prs));
    let view = ok(repo.sms(&["view"]));
    assert!(
        view.contains("history lookup limit (100) reached"),
        "{view}"
    );
    assert!(view.contains("#103 [CLOSED]"));
    assert!(!view.contains("#104 [CLOSED]"));
    let log = fs::read_to_string(repo.root.join(".git/mock-gh.log")).unwrap();
    assert_eq!(
        log.lines()
            .filter(|l| l.contains("\"pr\", \"view\""))
            .count(),
        100
    );
}

#[test]
fn historical_jump_refuses_open_branch_reuse_and_cross_repo_parents() {
    let repo = Repo::new(0);
    let mut prs = repo.state()["prs"].as_array().unwrap().clone();
    let mut historical = prs[0].clone();
    historical["number"] = json!(4);
    historical["state"] = json!("CLOSED");
    prs[2]["body"] = json!("Stacked on #4");
    prs.push(historical);
    repo.set("prs", json!(prs));
    let view = ok(repo.sms(&["view"]));
    assert!(view.contains("#4 [CLOSED] a"));
    fails(repo.sms(&["jump", "4"]), "missing, changed, or reused");
    prs[3]["isCrossRepository"] = json!(true);
    repo.set("prs", json!(prs));
    let view = ok(repo.sms(&["view"]));
    assert!(!view.contains("#4 [CLOSED]"));
    assert!(view.contains("linked #4 is cross-repository or changed"));
    fails(repo.sms(&["jump", "4"]), "not in this view");
}
