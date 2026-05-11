//! `.dripignore` integration tests.

use crate::common::Drip;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

/// Run a hook with custom env (so we can point DRIP_IGNORE_FILE at a
/// per-test fixture).
fn run_hook(
    drip: &Drip,
    agent: &str,
    payload: Value,
    ignore_file: Option<&std::path::Path>,
) -> String {
    let mut cmd = Command::new(&drip.bin);
    cmd.args(["hook", agent])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id);
    if let Some(p) = ignore_file {
        cmd.env("DRIP_IGNORE_FILE", p);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(
        o.status.success(),
        "hook failed: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn read_of_lock_file_is_substituted_with_placeholder() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("package-lock.json");
    fs::write(&f, "{\"name\": \"x\", \"lockfileVersion\": 3}").unwrap();

    let out = run_hook(
        &drip,
        "claude",
        json!({
            "session_id": &drip.session_id,
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
        None,
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"].as_str(),
        Some("deny"),
        "ignored file must produce a substitute, not pass through"
    );
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("ignored by .dripignore") || reason.contains("matched .dripignore"),
        "unexpected reason: {reason}"
    );
    // Crucially: the lock file content must NOT leak.
    assert!(
        !reason.contains("lockfileVersion"),
        "lock-file content leaked despite .dripignore: {reason}"
    );
}

#[test]
fn user_dripignore_extends_defaults() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret.env");
    fs::write(&secret, "API_KEY=abc").unwrap();

    let ignore_file = dir.path().join("dripignore");
    fs::write(&ignore_file, "*.env\n").unwrap();

    let out = run_hook(
        &drip,
        "claude",
        json!({
            "session_id": &drip.session_id,
            "tool_name": "Read",
            "tool_input": { "file_path": secret.to_string_lossy() }
        }),
        Some(&ignore_file),
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"].as_str(),
        Some("deny")
    );
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(!reason.contains("API_KEY"), "secret leaked: {reason}");
}

#[test]
fn glob_hook_filters_out_node_modules() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::create_dir_all(dir.path().join("node_modules/lib")).unwrap();
    fs::write(dir.path().join("src/app.js"), "// real code").unwrap();
    fs::write(dir.path().join("node_modules/lib/index.js"), "// junk").unwrap();

    let out = run_hook(
        &drip,
        "claude-glob",
        json!({
            "tool_name": "Glob",
            "tool_input": {
                "pattern": "**/*.js",
                "path": dir.path().to_string_lossy()
            }
        }),
        None,
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"].as_str(),
        Some("deny")
    );
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("src/app.js"),
        "real match missing: {reason}"
    );
    assert!(
        !reason.contains("node_modules/lib/index.js"),
        "node_modules leaked into glob results: {reason}"
    );
}

#[test]
fn drip_disable_bypasses_dripignore_for_glob() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();

    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude-glob"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_DISABLE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            json!({
                "tool_name": "Glob",
                "tool_input": {
                    "pattern": "**/*.js",
                    "path": dir.path().to_string_lossy()
                }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let o = child.wait_with_output().unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    let v: Value = serde_json::from_str(s.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"].as_str(),
        Some("allow"),
        "DRIP_DISABLE must short-circuit glob hook to allow: {s}"
    );
}

#[test]
fn trailing_slash_pattern_matches_descendants_like_gitignore() {
    // The QA report flagged that `dir/` did NOT match `dir/foo.txt`
    // — divergent from gitignore where `dir/` is shorthand for
    // "directory dir and everything inside". This test pins the fixed
    // behavior end-to-end through `drip read` so a future refactor
    // can't silently regress the alignment.
    //
    // We spawn the binary with cwd set to the workspace root and pass
    // relative paths, which is how an agent typically invokes reads
    // when the file_path comes out of a glob/grep relative to the
    // project. The matcher then evaluates `playground/foo.txt`
    // directly against the pattern `playground/` (expanded to
    // `playground/**` post-fix). With cwd = workspace, the
    // ".dripignore" lookup also picks up the file we drop here without
    // needing DRIP_IGNORE_FILE — exercising the realistic load path.
    let drip = Drip::new();
    let workspace = tempfile::tempdir().unwrap();
    fs::create_dir_all(workspace.path().join("playground/a")).unwrap();
    fs::write(workspace.path().join("playground/foo.txt"), "immediate\n").unwrap();
    fs::write(workspace.path().join("playground/a/b.txt"), "nested\n").unwrap();
    fs::write(workspace.path().join("not-playground.txt"), "sibling\n").unwrap();
    // Trailing-slash form — exactly what the user wrote in the QA.
    fs::write(workspace.path().join(".dripignore"), "playground/\n").unwrap();

    let read = |relpath: &str| -> String {
        let o = drip
            .cmd()
            .arg("read")
            .arg(relpath)
            .current_dir(workspace.path())
            .output()
            .expect("drip read");
        assert!(
            o.status.success(),
            "drip read {relpath} failed: stderr={}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    };

    let immediate = read("playground/foo.txt");
    assert!(
        immediate.contains("ignored by .dripignore"),
        "playground/foo.txt should be ignored under `playground/`, got: {immediate}"
    );
    let nested = read("playground/a/b.txt");
    assert!(
        nested.contains("ignored by .dripignore"),
        "playground/a/b.txt should be ignored under `playground/`, got: {nested}"
    );
    let outside = read("not-playground.txt");
    assert!(
        !outside.contains("ignored by .dripignore"),
        "not-playground.txt is a sibling, must NOT be ignored: {outside}"
    );
}
