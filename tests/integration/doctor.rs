//! `drip doctor` — installation diagnostics.
//!
//! These tests exercise the human, --json and --quiet output paths
//! through the real binary. Each test runs in an isolated `HOME` /
//! `DRIP_DATA_DIR` so the host machine's actual install state can't
//! leak in.

use crate::common::Drip;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run_doctor(drip: &Drip, project: &Path, home: &Path, args: &[&str]) -> Output {
    let mut c = Command::new(&drip.bin);
    c.arg("doctor")
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        // Make completion / TTY detection deterministic.
        .env("NO_COLOR", "1")
        .env_remove("SHELL");
    c.output().expect("drip doctor failed to spawn")
}

#[test]
fn doctor_handles_missing_db_gracefully() {
    // Fresh install: no DB, no settings.json. Doctor must not crash —
    // it should report each missing piece as a warning/info.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = run_doctor(&drip, project.path(), home.path(), &[]);
    assert!(
        o.status.success() || o.status.code() == Some(1),
        "doctor crashed: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("DRIP Doctor"), "no banner: {out}");
    // The DB section must mention the absence.
    assert!(
        out.contains("Database") && out.to_lowercase().contains("not"),
        "DB section missing or not flagged: {out}"
    );
}

#[test]
fn doctor_detects_missing_hooks() {
    // settings.json exists but is empty (no DRIP hooks). Every one of
    // the five hooks should be flagged as missing.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(claude_dir.join("settings.json"), "{}\n").unwrap();

    let o = run_doctor(&drip, project.path(), home.path(), &[]);
    let out = String::from_utf8_lossy(&o.stdout);
    // Every hook label should be reported as missing.
    for hook in [
        "PreToolUse:Read",
        "PreToolUse:Glob",
        "PreToolUse:Grep",
        "PostToolUse",
    ] {
        assert!(out.contains(hook), "hook label {hook} not in report: {out}");
    }
    // At least one explicit "missing" mention.
    assert!(
        out.to_lowercase().contains("missing"),
        "no missing-hook mention: {out}"
    );
}

#[test]
fn doctor_flags_missing_session_start_hook_as_error_with_recovery_hint() {
    // Upgrade scenario: a user who ran `drip init` on an old build has
    // the four PreToolUse hooks + the PostToolUse one, but no
    // SessionStart entry. Pre-fix this would silently break Edit
    // after every conversation compaction. Doctor must surface this
    // with Error severity (not Warn) so the user notices, plus a
    // hint that points at the right recovery command.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    // Hand-craft a settings.json that mirrors a pre-fix `drip init`
    // run: every hook except SessionStart. We don't bother with real
    // binary paths — `hook_present` matches on the trailing
    // subcommand, not the full path.
    let pre_fix_settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                {"matcher": "Read", "hooks": [{"type": "command", "command": "drip hook claude"}]},
                {"matcher": "Glob", "hooks": [{"type": "command", "command": "drip hook claude-glob"}]},
                {"matcher": "Grep", "hooks": [{"type": "command", "command": "drip hook claude-grep"}]}
            ],
            "PostToolUse": [
                {"matcher": "Edit|Write|MultiEdit|NotebookEdit",
                 "hooks": [{"type": "command", "command": "drip hook claude-post-edit"}]}
            ]
        }
    });
    fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string_pretty(&pre_fix_settings).unwrap(),
    )
    .unwrap();

    let o = run_doctor(&drip, project.path(), home.path(), &[]);
    // Doctor exits 1 when there's an Error-level finding; the missing
    // SessionStart hook MUST be one of those.
    assert_eq!(
        o.status.code(),
        Some(1),
        "expected Error-level exit; stdout={}",
        String::from_utf8_lossy(&o.stdout)
    );
    let out = String::from_utf8_lossy(&o.stdout);

    // The exact user-facing message + hint we agreed on.
    assert!(
        out.contains("SessionStart hook missing"),
        "missing the SessionStart-specific message: {out}"
    );
    assert!(
        out.contains("compaction will break Edit after context reset"),
        "missing the consequence sentence: {out}"
    );
    assert!(
        out.contains("Run `drip init -g` to add the missing hook."),
        "missing the recovery hint: {out}"
    );
    // Severity check: the SessionStart line must use the ❌ glyph
    // (Status::Error). Walk the output line-by-line so we don't
    // accidentally match an unrelated ❌ on a different section.
    let session_start_line = out
        .lines()
        .find(|l| l.contains("SessionStart hook missing"))
        .expect("SessionStart line must exist");
    assert!(
        session_start_line.contains('❌'),
        "SessionStart finding must be Error-level (❌), got: {session_start_line}"
    );

    // Critically: the OTHER hooks must still report ✅ — only the
    // SessionStart entry should be flagged, so the user sees
    // "you upgraded, just re-init", not a wall of broken hooks.
    for hook in ["PreToolUse:Read", "PreToolUse:Glob", "PreToolUse:Grep"] {
        let line = out
            .lines()
            .find(|l| l.contains(hook))
            .unwrap_or_else(|| panic!("hook {hook} missing from report: {out}"));
        assert!(
            line.contains('✅'),
            "hook {hook} should be ✅ in upgrade scenario, got: {line}"
        );
    }
}

#[test]
fn doctor_does_not_error_on_project_claude_md_when_global_covers() {
    // Regression: a user with DRIP installed globally (~/.claude/)
    // who manually creates a project-level CLAUDE.md (without the
    // @drip.md include) used to see ❌ on the project tier even
    // though the global tier already wires DRIP for them. The
    // accompanying hint nudged them to run `drip init` here, which
    // they didn't actually need. Downgrade to ℹ in this scenario.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // Project tier: a settings.json + CLAUDE.md, neither referencing
    // DRIP. Mirrors a user who hand-rolled their own project notes
    // and ran `drip init -g` separately for global coverage.
    let proj_claude_dir = project.path().join(".claude");
    fs::create_dir_all(&proj_claude_dir).unwrap();
    fs::write(
        proj_claude_dir.join("settings.json"),
        "{\"hooks\":{}}\n", // no DRIP hooks
    )
    .unwrap();
    fs::write(
        project.path().join("CLAUDE.md"),
        "# my project\n\nsome notes\n",
    )
    .unwrap();

    let o = run_doctor(&drip, project.path(), home.path(), &[]);
    let out = String::from_utf8_lossy(&o.stdout);

    // The "CLAUDE.md does not reference @drip.md" line MUST be
    // present (we want users to know), but NOT marked Error.
    let line = out
        .lines()
        .find(|l| l.contains("CLAUDE.md does not reference"))
        .expect("must mention the missing @drip.md ref");
    assert!(
        !line.contains('❌'),
        "must NOT be Error-level when global covers — got: {line}"
    );
    assert!(
        line.contains('ℹ') || line.contains("info"),
        "expected Info severity, got: {line}"
    );
    // Detail should explain why we're soft-pedalling.
    assert!(
        out.contains("global install covers")
            || out.contains("only needed for project-scoped DRIP"),
        "must explain that this is optional when global covers: {out}"
    );
}

#[test]
fn doctor_session_start_check_accepts_compact_clear_matcher_only() {
    // The matcher must literally be `compact|clear` — anything else
    // (e.g. a user who hand-edited it to `compact` only, or a
    // typoed matcher) should still trigger the missing-hook
    // warning. This protects against silent half-fixes.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    let typoed_settings = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {"matcher": "compact",
                 "hooks": [{"type": "command", "command": "drip hook claude-session-start"}]}
            ]
        }
    });
    fs::write(
        claude_dir.join("settings.json"),
        serde_json::to_string_pretty(&typoed_settings).unwrap(),
    )
    .unwrap();

    let o = run_doctor(&drip, project.path(), home.path(), &[]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.contains("SessionStart hook missing"),
        "non-canonical matcher must still flag as missing: {out}"
    );
}

#[test]
fn doctor_reports_clean_after_init() {
    // After `drip init -g`, the global hooks + drip.md + CLAUDE.md
    // should all be ✅ in the doctor report.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let init = Command::new(&drip.bin)
        .args(["init", "--global", "--agent", "claude"])
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .expect("init");
    assert!(init.status.success());

    let o = run_doctor(&drip, project.path(), home.path(), &[]);
    let out = String::from_utf8_lossy(&o.stdout);

    // Global hooks all present.
    for hook in [
        "PreToolUse:Read",
        "PreToolUse:Glob",
        "PreToolUse:Grep",
        "PostToolUse",
        "SessionStart",
    ] {
        assert!(out.contains(hook), "hook {hook} missing from report: {out}");
    }
    assert!(out.contains("drip.md"), "drip.md not mentioned: {out}");
    assert!(
        out.contains("@drip.md") || out.to_lowercase().contains("references"),
        "CLAUDE.md reference state not reported: {out}"
    );
}

#[test]
fn doctor_json_output_valid() {
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = run_doctor(&drip, project.path(), home.path(), &["--json"]);
    assert!(
        o.status.code() == Some(0) || o.status.code() == Some(1),
        "unexpected exit code: {:?}",
        o.status.code()
    );
    let v: Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|e| {
        panic!(
            "not valid JSON: {e}\nstdout={}",
            String::from_utf8_lossy(&o.stdout)
        )
    });

    // Schema: top-level keys must include version, errors, warnings, sections.
    assert!(v["version"].is_string(), "missing .version");
    assert!(v["errors"].is_number(), "missing .errors");
    assert!(v["warnings"].is_number(), "missing .warnings");
    assert!(v["sections"].is_object(), "missing .sections");

    let sections = v["sections"].as_object().unwrap();
    for key in [
        "binary",
        "database",
        "cache",
        "hooks_global",
        "hooks_project",
        "session",
    ] {
        assert!(
            sections.contains_key(key),
            "missing section {key}: {sections:?}"
        );
    }
}

#[test]
fn doctor_exit_code_zero_when_clean() {
    // A clean global install with a populated DB: doctor must exit 0.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // Init the integration so hooks + drip.md + CLAUDE.md are present.
    Command::new(&drip.bin)
        .args(["init", "--global", "--agent", "claude"])
        .current_dir(project.path())
        .env("HOME", home.path())
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .expect("init");

    // Touch the DB by running a read.
    let f = project.path().join("hello.txt");
    fs::write(&f, "hello\n").unwrap();
    drip.read_stdout(&f);

    let o = run_doctor(&drip, project.path(), home.path(), &[]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "expected exit 0 on clean install, got {:?}\nstdout={}\nstderr={}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn doctor_exit_code_nonzero_on_error() {
    // Corrupt settings.json: parse will fail → that's an error, not a
    // warning. Doctor must exit 1.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(claude_dir.join("settings.json"), "{ this is not json").unwrap();

    let o = run_doctor(&drip, project.path(), home.path(), &[]);
    assert_eq!(
        o.status.code(),
        Some(1),
        "expected exit 1 on parse error, got {:?}\nstdout={}",
        o.status.code(),
        String::from_utf8_lossy(&o.stdout)
    );
}

#[test]
fn doctor_quiet_output() {
    // --quiet must be terse: just the final summary line, no per-check
    // detail. Exit code remains meaningful.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let o = run_doctor(&drip, project.path(), home.path(), &["--quiet"]);
    let out = String::from_utf8_lossy(&o.stdout);
    // No banner, no per-section header.
    assert!(
        !out.contains("DRIP Doctor — Installation Check"),
        "--quiet leaked banner: {out}"
    );
    assert!(
        !out.contains("Database\n"),
        "--quiet leaked section header: {out}"
    );
    // But still produces a summary.
    let trimmed = out.trim();
    assert!(!trimmed.is_empty(), "--quiet produced empty output");
}

#[test]
fn doctor_detects_drip_md_without_marker() {
    // User's own drip.md (no <!-- drip:memory --> marker) should be
    // flagged as suspicious — DRIP doesn't own it, so we won't auto-
    // overwrite it on init either.
    let drip = Drip::new();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let claude_dir = home.path().join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(claude_dir.join("settings.json"), "{}\n").unwrap();
    fs::write(
        claude_dir.join("drip.md"),
        "# my own drip notes\nnothing to do with the drip CLI\n",
    )
    .unwrap();

    let o = run_doctor(&drip, project.path(), home.path(), &[]);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.to_lowercase().contains("marker"),
        "marker absence not flagged: {out}"
    );
}
