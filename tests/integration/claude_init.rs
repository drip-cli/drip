//! `drip init claude` + `drip uninstall claude` — drip.md and CLAUDE.md
//! lifecycle.
//!
//! The Claude Code integration drops a `drip.md` memory file and links
//! it from `CLAUDE.md` via the standard `@drip.md` include directive
//! (so the agent loads DRIP's read-hint guidance every session).
//!
//! Both files live in the project (default) or in `~/.claude/` when
//! `--global` is passed — same toggle that decides where settings.json
//! goes. Tests cover both axes plus the uninstall reverse-pass.

use crate::common::Drip;
use std::fs;
use std::path::Path;
use std::process::Command;

fn run_init_local(drip: &Drip, project: &Path, home: &Path) -> std::process::Output {
    Command::new(&drip.bin)
        .args(["init", "--agent", "claude"])
        .current_dir(project)
        .env("HOME", home)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .expect("drip init claude")
}

fn run_init_global(drip: &Drip, project: &Path, home: &Path) -> std::process::Output {
    Command::new(&drip.bin)
        .args(["init", "--global", "--agent", "claude"])
        .current_dir(project)
        .env("HOME", home)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .expect("drip init --global claude")
}

fn run_uninstall_local(drip: &Drip, project: &Path, home: &Path) -> std::process::Output {
    Command::new(&drip.bin)
        .args(["uninstall", "--agent", "claude"])
        .current_dir(project)
        .env("HOME", home)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .expect("drip uninstall claude")
}

fn run_uninstall_global(drip: &Drip, project: &Path, home: &Path) -> std::process::Output {
    Command::new(&drip.bin)
        .args(["uninstall", "--global", "--agent", "claude"])
        .current_dir(project)
        .env("HOME", home)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .expect("drip uninstall --global claude")
}

#[test]
fn drip_md_created_on_init() {
    // Local install drops drip.md at the project root with the
    // canonical guidance block (so the agent learns the diff format,
    // the refresh command, and what's in/out of scope).
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = run_init_local(&drip, project.path(), home.path());
    assert!(
        o.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let drip_md = project.path().join("drip.md");
    assert!(drip_md.exists(), "drip.md not created at {drip_md:?}");
    let body = fs::read_to_string(&drip_md).unwrap();
    // Must contain the marker so uninstall / future re-init can
    // recognise our content.
    assert!(body.contains("drip:memory"), "marker missing: {body}");
    // Must mention the actual user-facing primitives so the agent
    // knows the contract.
    assert!(
        body.contains("drip refresh"),
        "drip.md missing refresh hint: {body}"
    );
    assert!(
        body.to_lowercase().contains("diff"),
        "drip.md should explain the diff format: {body}"
    );
}

#[test]
fn claude_md_append_preserves_existing_content() {
    // User already has a CLAUDE.md with their own instructions —
    // init must add `@drip.md` without touching anything else.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let pre = "# Project rules\n\
               - prefer integration tests\n\
               - never use `.unwrap()` in src/\n";
    fs::write(project.path().join("CLAUDE.md"), pre).unwrap();

    let o = run_init_local(&drip, project.path(), home.path());
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let after = fs::read_to_string(project.path().join("CLAUDE.md")).unwrap();
    // Pre-existing rules survive verbatim.
    assert!(after.contains("# Project rules"), "title dropped: {after}");
    assert!(after.contains("prefer integration tests"));
    assert!(after.contains("never use `.unwrap()`"));
    // Reference appended.
    assert!(after.contains("@drip.md"), "@drip.md not appended: {after}");
}

#[test]
fn claude_md_append_idempotent() {
    // Running `drip init claude` twice must NOT duplicate the
    // `@drip.md` line — the project memory file would otherwise grow
    // unbounded across re-installs.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let pre = "# Existing\nfoo\n";
    fs::write(project.path().join("CLAUDE.md"), pre).unwrap();

    run_init_local(&drip, project.path(), home.path());
    let first = fs::read_to_string(project.path().join("CLAUDE.md")).unwrap();
    run_init_local(&drip, project.path(), home.path());
    let second = fs::read_to_string(project.path().join("CLAUDE.md")).unwrap();

    assert_eq!(first, second, "CLAUDE.md drifted on second init");
    assert_eq!(
        second.matches("@drip.md").count(),
        1,
        "@drip.md duplicated: {second}"
    );
}

#[test]
fn claude_md_created_if_missing() {
    // Fresh project with no CLAUDE.md: init creates it and writes the
    // `@drip.md` reference inside.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    assert!(!project.path().join("CLAUDE.md").exists());

    let o = run_init_local(&drip, project.path(), home.path());
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let body = fs::read_to_string(project.path().join("CLAUDE.md")).unwrap();
    assert!(
        body.contains("@drip.md"),
        "freshly-created CLAUDE.md missing reference: {body}"
    );
}

#[test]
fn uninstall_removes_reference_only() {
    // Uninstall reverses init: `@drip.md` removed from CLAUDE.md,
    // drip.md deleted, hooks pruned from settings.json — but ANY
    // pre-existing user content in CLAUDE.md must survive untouched.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let pre = "# Project rules\n\
               keep me intact\n";
    fs::write(project.path().join("CLAUDE.md"), pre).unwrap();

    run_init_local(&drip, project.path(), home.path());
    // Sanity: init did add @drip.md.
    let after_init = fs::read_to_string(project.path().join("CLAUDE.md")).unwrap();
    assert!(after_init.contains("@drip.md"));
    assert!(project.path().join("drip.md").exists());

    let o = run_uninstall_local(&drip, project.path(), home.path());
    assert!(
        o.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let final_md = fs::read_to_string(project.path().join("CLAUDE.md")).unwrap();
    // Reference is gone…
    assert!(
        !final_md.contains("@drip.md"),
        "@drip.md still present after uninstall: {final_md}"
    );
    // …but the user's own content survives.
    assert!(
        final_md.contains("# Project rules"),
        "user content lost: {final_md}"
    );
    assert!(final_md.contains("keep me intact"));

    // drip.md itself is removed.
    assert!(
        !project.path().join("drip.md").exists(),
        "drip.md should be deleted on uninstall"
    );

    // settings.json hooks are pruned — re-running init must work
    // (i.e. uninstall didn't leave a half-state that breaks re-install).
    let cfg_path = project.path().join(".claude/settings.json");
    if cfg_path.exists() {
        let cfg = fs::read_to_string(&cfg_path).unwrap();
        assert!(
            !cfg.contains("hook claude"),
            "drip hook entries still in settings.json: {cfg}"
        );
    }
}

#[test]
fn init_registers_session_start_compact_clear_hook() {
    // Regression: pre-fix, DRIP shipped only PreToolUse + PostToolUse
    // entries. After Claude Code compacted the conversation its
    // read-tracker was wiped while session_id stayed the same; DRIP
    // kept its baseline and the next Edit failed with "File must be
    // read first". This pins the SessionStart hook into init's
    // contract so a regression on this gets caught at install time.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = run_init_global(&drip, project.path(), home.path());
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    let settings_path = home.path().join(".claude/settings.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();

    let arr = parsed["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart array missing — install path didn't write the recovery hook");
    assert!(
        arr.iter().any(|m| {
            m["matcher"].as_str() == Some("compact|clear")
                && m["hooks"][0]["command"]
                    .as_str()
                    .map(|c| c.ends_with("hook claude-session-start"))
                    .unwrap_or(false)
        }),
        "SessionStart entry must use the `compact|clear` matcher and our claude-session-start \
         subcommand: {arr:?}"
    );

    // Uninstall must also prune it — otherwise users who rerun init
    // after upgrade would see two entries, or worse, an orphan
    // SessionStart pointing at a stale binary path.
    let report_uninstall = run_uninstall_global(&drip, project.path(), home.path());
    assert!(
        report_uninstall.status.success(),
        "{}",
        String::from_utf8_lossy(&report_uninstall.stderr)
    );
    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    let post_arr = after["hooks"]["SessionStart"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !post_arr.iter().any(|m| {
            m["hooks"][0]["command"]
                .as_str()
                .map(|c| c.contains("hook claude-session-start"))
                .unwrap_or(false)
        }),
        "uninstall must remove our SessionStart entry, kept: {post_arr:?}"
    );
}

#[test]
fn global_install_targets_home_claude_dir() {
    // With --global, drip.md and CLAUDE.md go to ~/.claude/, leaving
    // the project root untouched. Mirrors how settings.json --global
    // already works.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = run_init_global(&drip, project.path(), home.path());
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    assert!(
        home.path().join(".claude/drip.md").exists(),
        "global drip.md missing"
    );
    let claude_md = fs::read_to_string(home.path().join(".claude/CLAUDE.md")).unwrap();
    assert!(
        claude_md.contains("@drip.md"),
        "global CLAUDE.md: {claude_md}"
    );

    // Project root must NOT be polluted.
    assert!(!project.path().join("drip.md").exists());
    assert!(!project.path().join("CLAUDE.md").exists());
}
