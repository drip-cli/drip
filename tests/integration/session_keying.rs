//! Session-keying strategy: env > git > pid > cwd.
//!
//! Crash-resistance: same cwd + same git branch produces the same
//! session id even across different parent PIDs (Claude Code crashes
//! and gets relaunched), so the agent re-reads of files DRIP already
//! saw return `unchanged` instead of starting from scratch.
//!
//! Branch-isolation: different branch ⇒ different session id ⇒ no
//! cross-branch diff contamination.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn drip_bin() -> String {
    env!("CARGO_BIN_EXE_drip").to_string()
}

fn data_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

/// Build a `drip` command with a clean environment — no inherited
/// `DRIP_SESSION_ID` from the test harness, no inherited strategy
/// override. The caller layers on the env vars they want to test.
fn cmd(data: &Path, cwd: &Path) -> Command {
    let mut c = Command::new(drip_bin());
    c.env("DRIP_DATA_DIR", data);
    c.env_remove("DRIP_SESSION_ID");
    c.env_remove("DRIP_SESSION_STRATEGY");
    c.env_remove("DRIP_TEST_PPID");
    c.current_dir(cwd);
    c
}

/// Run `drip meter --session --json` and parse it. The current-session
/// report is the cleanest way to read back the id + strategy + context
/// the binary just derived for this invocation.
fn current_session(data: &Path, cwd: &Path) -> Value {
    let mut c = cmd(data, cwd);
    c.args(["meter", "--session", "--json"]);
    let o = c.output().expect("spawn drip meter");
    assert!(
        o.status.success(),
        "drip meter failed: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).expect("meter json")
}

fn make_fake_git_repo(root: &Path, branch: &str) {
    let git = root.join(".git");
    fs::create_dir_all(git.join("refs/heads")).unwrap();
    fs::write(git.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    // Touch a sample object so the dir looks vaguely real.
    fs::create_dir_all(git.join("objects")).unwrap();
}

fn switch_fake_branch(root: &Path, branch: &str) {
    fs::write(
        root.join(".git/HEAD"),
        format!("ref: refs/heads/{branch}\n"),
    )
    .unwrap();
}

/// Materialise a fake git worktree: `<wt>/.git` is a *file* pointing at
/// `<primary>/.git/worktrees/<name>/`, and that worktrees subdir owns
/// its own HEAD. This is exactly the layout `git worktree add` produces.
fn make_fake_worktree(primary: &Path, wt_root: &Path, wt_name: &str, branch: &str) {
    let wt_gitdir = primary.join(".git/worktrees").join(wt_name);
    fs::create_dir_all(&wt_gitdir).unwrap();
    fs::write(
        wt_gitdir.join("HEAD"),
        format!("ref: refs/heads/{branch}\n"),
    )
    .unwrap();
    fs::create_dir_all(wt_root).unwrap();
    fs::write(
        wt_root.join(".git"),
        format!("gitdir: {}\n", wt_gitdir.display()),
    )
    .unwrap();
}

// ─── Strategy resolution ───────────────────────────────────────────

#[test]
fn env_id_takes_priority_over_everything() {
    let data = data_dir();
    let repo = tempfile::tempdir().unwrap();
    make_fake_git_repo(repo.path(), "main");

    let mut c = cmd(data.path(), repo.path());
    c.env("DRIP_SESSION_ID", "explicit-test-id");
    c.args(["meter", "--session", "--json"]);
    let o = c.output().unwrap();
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["session_id"].as_str().unwrap(), "explicit-test-id");
    assert_eq!(v["session_strategy"].as_str().unwrap(), "env");
}

#[test]
fn git_strategy_when_in_a_repo() {
    let data = data_dir();
    let repo = tempfile::tempdir().unwrap();
    make_fake_git_repo(repo.path(), "main");

    let v = current_session(data.path(), repo.path());
    assert_eq!(v["session_strategy"].as_str().unwrap(), "git");
    assert_eq!(v["session_context"].as_str().unwrap(), "main");
    let id = v["session_id"].as_str().unwrap();
    assert_eq!(id.len(), 16, "id must be 16 hex chars: {id}");
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn pid_strategy_outside_a_repo() {
    let data = data_dir();
    let dir = tempfile::tempdir().unwrap();
    // No .git anywhere.
    let v = current_session(data.path(), dir.path());
    assert_eq!(v["session_strategy"].as_str().unwrap(), "pid");
    assert!(
        v["session_context"].as_str().unwrap().starts_with("(pid "),
        "got: {v}",
    );
}

#[test]
fn malformed_git_falls_back_to_pid() {
    let data = data_dir();
    let dir = tempfile::tempdir().unwrap();
    // .git directory exists but has no HEAD — broken state.
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    let v = current_session(data.path(), dir.path());
    assert_eq!(
        v["session_strategy"].as_str().unwrap(),
        "pid",
        "broken .git must not crash and must fall back: {v}",
    );
}

#[test]
fn strategy_pid_override_ignores_git() {
    let data = data_dir();
    let repo = tempfile::tempdir().unwrap();
    make_fake_git_repo(repo.path(), "main");

    let mut c = cmd(data.path(), repo.path());
    c.env("DRIP_SESSION_STRATEGY", "pid");
    c.args(["meter", "--session", "--json"]);
    let o = c.output().unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["session_strategy"].as_str().unwrap(), "pid");
}

#[test]
fn strategy_cwd_is_permanent_per_directory() {
    // cwd-only strategy: the id depends on cwd alone, so successive
    // runs (different ppid, different machine reboot, etc.) must
    // produce the SAME id for the SAME directory.
    let data = data_dir();
    let dir = tempfile::tempdir().unwrap();

    let mut c1 = cmd(data.path(), dir.path());
    c1.env("DRIP_SESSION_STRATEGY", "cwd");
    c1.env("DRIP_TEST_PPID", "1111");
    c1.args(["meter", "--session", "--json"]);
    let v1: Value = serde_json::from_slice(&c1.output().unwrap().stdout).unwrap();

    let mut c2 = cmd(data.path(), dir.path());
    c2.env("DRIP_SESSION_STRATEGY", "cwd");
    c2.env("DRIP_TEST_PPID", "2222");
    c2.args(["meter", "--session", "--json"]);
    let v2: Value = serde_json::from_slice(&c2.output().unwrap().stdout).unwrap();

    assert_eq!(v1["session_strategy"].as_str().unwrap(), "cwd");
    assert_eq!(v1["session_id"], v2["session_id"]);
}

#[test]
fn detached_head_uses_short_sha_as_context() {
    let data = data_dir();
    let repo = tempfile::tempdir().unwrap();
    make_fake_git_repo(repo.path(), "main");
    let sha = "deadbeef0123456789abcdef0123456789abcdef";
    fs::write(repo.path().join(".git/HEAD"), format!("{sha}\n")).unwrap();

    let v = current_session(data.path(), repo.path());
    assert_eq!(v["session_strategy"].as_str().unwrap(), "git");
    assert_eq!(v["session_context"].as_str().unwrap(), "deadbeef");
}

// ─── Stability invariants ──────────────────────────────────────────

#[test]
fn git_session_stable_across_simulated_ppid_change() {
    // The crash-recovery scenario: same repo, same branch, different
    // parent PID. Expectation: same session id, so re-reads of the
    // files the previous PID already touched return `unchanged`.
    let data = data_dir();
    let repo = tempfile::tempdir().unwrap();
    make_fake_git_repo(repo.path(), "main");

    let mut c1 = cmd(data.path(), repo.path());
    c1.env("DRIP_TEST_PPID", "1000");
    c1.args(["meter", "--session", "--json"]);
    let v1: Value = serde_json::from_slice(&c1.output().unwrap().stdout).unwrap();

    let mut c2 = cmd(data.path(), repo.path());
    c2.env("DRIP_TEST_PPID", "9999");
    c2.args(["meter", "--session", "--json"]);
    let v2: Value = serde_json::from_slice(&c2.output().unwrap().stdout).unwrap();

    assert_eq!(v1["session_strategy"].as_str().unwrap(), "git");
    assert_eq!(
        v1["session_id"], v2["session_id"],
        "git keying must be PPID-independent",
    );
}

#[test]
fn git_session_changes_when_branch_switches() {
    let data = data_dir();
    let repo = tempfile::tempdir().unwrap();
    make_fake_git_repo(repo.path(), "main");
    let v_main = current_session(data.path(), repo.path());

    switch_fake_branch(repo.path(), "feature/auth");
    let v_feat = current_session(data.path(), repo.path());

    assert_ne!(
        v_main["session_id"], v_feat["session_id"],
        "branches must produce distinct session ids",
    );
    assert_eq!(v_main["session_context"].as_str().unwrap(), "main");
    assert_eq!(v_feat["session_context"].as_str().unwrap(), "feature/auth");
}

#[test]
fn git_worktrees_get_distinct_ids() {
    let data = data_dir();
    let primary = tempfile::tempdir().unwrap();
    make_fake_git_repo(primary.path(), "main");

    let wt_holder = tempfile::tempdir().unwrap();
    let wt_root = wt_holder.path().join("wt-feature");
    make_fake_worktree(primary.path(), &wt_root, "wt-feature", "feature/x");

    let v_primary = current_session(data.path(), primary.path());
    let v_wt = current_session(data.path(), &wt_root);

    assert_eq!(v_primary["session_strategy"].as_str().unwrap(), "git");
    assert_eq!(v_wt["session_strategy"].as_str().unwrap(), "git");
    assert_ne!(
        v_primary["session_id"], v_wt["session_id"],
        "worktrees must isolate even on same repo",
    );
}

#[test]
fn pid_strategy_changes_with_simulated_ppid() {
    // Sanity-check the pid path: different PIDs *do* produce different
    // ids when no git context is available.
    let data = data_dir();
    let dir = tempfile::tempdir().unwrap();

    let mut c1 = cmd(data.path(), dir.path());
    c1.env("DRIP_TEST_PPID", "111");
    c1.args(["meter", "--session", "--json"]);
    let v1: Value = serde_json::from_slice(&c1.output().unwrap().stdout).unwrap();

    let mut c2 = cmd(data.path(), dir.path());
    c2.env("DRIP_TEST_PPID", "222");
    c2.args(["meter", "--session", "--json"]);
    let v2: Value = serde_json::from_slice(&c2.output().unwrap().stdout).unwrap();

    assert_eq!(v1["session_strategy"].as_str().unwrap(), "pid");
    assert_eq!(v2["session_strategy"].as_str().unwrap(), "pid");
    assert_ne!(v1["session_id"], v2["session_id"]);
}

// ─── End-to-end: crash recovery resumes the agent's read history ───

#[test]
fn crash_recovery_resumes_unchanged_reads() {
    // Simulate: agent reads 3 files in a git repo on "main", then
    // crashes. New process starts with a different PPID but same cwd
    // and branch — DRIP must still see those 3 files and return
    // `unchanged` instead of `full read`.
    let data = data_dir();
    let repo = tempfile::tempdir().unwrap();
    make_fake_git_repo(repo.path(), "main");

    let files: Vec<PathBuf> = (0..3)
        .map(|i| {
            let p = repo.path().join(format!("src_{i}.txt"));
            fs::write(
                &p,
                format!("content {i} with enough repeated payload for unchanged recovery\n")
                    .repeat(20),
            )
            .unwrap();
            p
        })
        .collect();

    // ── Pre-crash session ──
    for f in &files {
        let mut c = cmd(data.path(), repo.path());
        c.env("DRIP_TEST_PPID", "1000");
        c.args(["read", f.to_str().unwrap()]);
        let o = c.output().unwrap();
        assert!(o.status.success());
        let body = String::from_utf8_lossy(&o.stdout);
        assert!(
            body.contains("[DRIP: full read"),
            "pre-crash first read must be FullFirst: {body}",
        );
    }

    // ── Post-crash session: different PPID, same git context ──
    for f in &files {
        let mut c = cmd(data.path(), repo.path());
        c.env("DRIP_TEST_PPID", "9999"); // different ppid post-crash
        c.args(["read", f.to_str().unwrap()]);
        let o = c.output().unwrap();
        assert!(o.status.success());
        let body = String::from_utf8_lossy(&o.stdout);
        assert!(
            body.contains("unchanged"),
            "post-crash same-content read must be Unchanged \
             (proves session id stable across PPIDs): {body}",
        );
    }
}

// ─── drip sessions surface ─────────────────────────────────────────

#[test]
fn drip_sessions_shows_strategy_column() {
    let data = data_dir();
    let repo = tempfile::tempdir().unwrap();
    make_fake_git_repo(repo.path(), "main");

    // Materialise one git session by reading any file.
    let f = repo.path().join("a.txt");
    fs::write(&f, "x\n").unwrap();
    let mut c = cmd(data.path(), repo.path());
    c.args(["read", f.to_str().unwrap()]);
    assert!(c.output().unwrap().status.success());

    let mut c = cmd(data.path(), repo.path());
    c.args(["sessions"]);
    let o = c.output().unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("STRATEGY"),
        "sessions output must include STRATEGY column: {s}",
    );
    assert!(
        s.contains("git"),
        "sessions row must show 'git' strategy: {s}",
    );
    assert!(
        s.contains("main"),
        "sessions row must show branch name as context: {s}",
    );
}

#[test]
fn drip_meter_lifetime_does_not_pollute_with_strategy_fields() {
    // The lifetime report (`drip meter` with no flag) should keep the
    // schema it had before — strategy/context belong to a session,
    // not to the install-wide aggregate. JSON schema invariants matter
    // for users who script against `--json`.
    let data = data_dir();
    let repo = tempfile::tempdir().unwrap();
    make_fake_git_repo(repo.path(), "main");

    let mut c = cmd(data.path(), repo.path());
    c.args(["meter", "--json"]);
    let v: Value = serde_json::from_slice(&c.output().unwrap().stdout).unwrap();
    assert!(
        v["session_strategy"].is_null() || !v.as_object().unwrap().contains_key("session_strategy")
    );
    assert!(
        v["session_context"].is_null() || !v.as_object().unwrap().contains_key("session_context")
    );
}
