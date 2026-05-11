use crate::common::Drip;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn run_post_edit(drip: &Drip, payload: Value) {
    run_post_edit_capture(drip, payload);
}

fn run_post_edit_capture(drip: &Drip, payload: Value) -> String {
    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude-post-edit"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn post-edit hook");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success());
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn edit_then_read_returns_unchanged() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("edited.py");
    let before: String = (0..120)
        .map(|i| format!("print('v1 line {i}')  # payload payload payload\n"))
        .collect();
    fs::write(&f, &before).unwrap();

    // Baseline read.
    let out1 = drip.read_stdout(&f);
    assert!(out1.contains("[DRIP: full read"));

    // Model edits the file (Claude Code's Edit tool would do this).
    let after = before.replace("print('v1 line 42')", "print('v2 line 42')");
    fs::write(&f, &after).unwrap();

    // PostToolUse fires.
    run_post_edit(
        &drip,
        json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );

    // Now the model re-reads. With edit-certificates on (default),
    // DRIP returns a compact `[DRIP: edit verified ...]` cert; the
    // read AFTER that returns "unchanged" because the baseline now
    // matches disk content.
    let out2 = drip.read_stdout(&f);
    assert!(
        out2.contains("[DRIP: edit verified"),
        "expected edit verified certificate, got: {out2}"
    );
    let out3 = drip.read_stdout(&f);
    assert!(
        out3.contains("[DRIP: unchanged"),
        "expected unchanged after cert, got: {out3}"
    );
}

#[test]
fn write_tool_also_refreshes_baseline() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("w.txt");
    let before = "old value with enough payload for certificate accounting\n".repeat(80);
    fs::write(&f, &before).unwrap();
    drip.read_stdout(&f);

    let after = before.replacen("old value", "new value", 1);
    fs::write(&f, &after).unwrap();
    run_post_edit(
        &drip,
        json!({
            "tool_name": "Write",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );
    // First read after Write: edit certificate (one-shot).
    let out1 = drip.read_stdout(&f);
    assert!(out1.contains("[DRIP: edit verified"), "got: {out1}");
    // Subsequent identical read: unchanged.
    let out2 = drip.read_stdout(&f);
    assert!(out2.contains("[DRIP: unchanged"), "got: {out2}");
}

#[test]
fn unrelated_tools_dont_touch_baseline() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("g.txt");
    // Make the file big enough that a 1-line diff is cheaper than a full
    // re-read; otherwise DRIP correctly falls back to "diff bigger than
    // file" and this test misreads that as a baseline-refresh bug.
    let mut v1 = String::new();
    for i in 0..40 {
        v1.push_str(&format!("filler {i}\n"));
    }
    v1.push_str("hello\n");
    fs::write(&f, &v1).unwrap();
    drip.read_stdout(&f);

    let v2 = v1.replace("hello\n", "hello world\n");
    fs::write(&f, &v2).unwrap();
    run_post_edit(
        &drip,
        json!({
            "tool_name": "Bash",
            "tool_input": { "command": "echo hi" }
        }),
    );
    let out = drip.read_stdout(&f);
    assert!(
        out.contains("[DRIP: delta only"),
        "Bash tool must not refresh baseline; got: {out}"
    );
}

#[test]
fn warns_when_edit_targets_an_elided_function_body() {
    // First read returns a semantic-compressed view of `big_fn`, so
    // the agent never saw its body. The Edit hook must surface a
    // warning via `additionalContext` so the model knows to re-read
    // before reasoning further about the function.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("elided.py");
    let src = "import os\n\n\
        def big_fn(arg_a, arg_b, arg_c):\n    \
            step_one = arg_a + arg_b\n    \
            step_two = step_one * 2\n    \
            step_three = step_two - arg_c\n    \
            step_four = step_three ** 2\n    \
            step_five = step_four + 1\n    \
            step_six = step_five * 3\n    \
            step_seven = step_six - 7\n    \
            step_eight = step_seven + arg_a\n    \
            return step_eight\n\n\
        def small_fn():\n    return 1\n";
    fs::write(&f, src).unwrap();

    // Force compression even on a sub-1KB fixture.
    let _ = drip
        .cmd()
        .arg("read")
        .arg(&f)
        .env("DRIP_COMPRESS_MIN_BYTES", "0")
        .output()
        .unwrap();

    // Mutate a body line whose text doesn't mention `big_fn` — the
    // hook still has to detect the overlap by locating the edit
    // position inside the elided range.
    let mutated = src.replace(
        "step_one = arg_a + arg_b",
        "step_one = arg_a + arg_b + 9999",
    );
    fs::write(&f, &mutated).unwrap();

    let out = run_post_edit_capture(
        &drip,
        json!({
            "tool_name": "Edit",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "old_string": "step_one = arg_a + arg_b",
                "new_string": "step_one = arg_a + arg_b + 9999",
            }
        }),
    );
    assert!(
        out.contains("edited elided function(s): big_fn"),
        "expected elided-fn warning, got: {out}"
    );
    assert!(
        out.contains("additionalContext"),
        "warning must be delivered via hookSpecificOutput.additionalContext, got: {out}"
    );
}

#[test]
fn no_warning_when_edited_function_body_was_visible() {
    // `small_fn` has a 1-line body — well below DRIP_COMPRESS_MIN_BODY,
    // so the agent saw it intact. Editing it must NOT trigger the
    // elided-function warning (false-positive guard).
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("partial.py");
    let src = "import os\n\n\
        def big_fn(arg_a, arg_b, arg_c):\n    \
            step_one = arg_a + arg_b\n    \
            step_two = step_one * 2\n    \
            step_three = step_two - arg_c\n    \
            step_four = step_three ** 2\n    \
            step_five = step_four + 1\n    \
            step_six = step_five * 3\n    \
            step_seven = step_six - 7\n    \
            step_eight = step_seven + arg_a\n    \
            return step_eight\n\n\
        def small_fn():\n    return 1\n";
    fs::write(&f, src).unwrap();
    let _ = drip
        .cmd()
        .arg("read")
        .arg(&f)
        .env("DRIP_COMPRESS_MIN_BYTES", "0")
        .output()
        .unwrap();

    let mutated = src.replace("return 1\n", "return 42\n");
    fs::write(&f, &mutated).unwrap();

    let out = run_post_edit_capture(
        &drip,
        json!({
            "tool_name": "Edit",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "old_string": "return 1",
                "new_string": "return 42",
            }
        }),
    );
    assert!(
        !out.contains("edited elided function"),
        "small_fn was visible, must not warn; got: {out}"
    );
}

#[test]
fn cert_contains_hash_and_changed_ranges() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("c.py");
    let v1: String = (0..180)
        .map(|i| format!("line {i} payload payload payload\n"))
        .collect();
    fs::write(&f, &v1).unwrap();
    drip.read_stdout(&f);

    let v2 = v1.replacen("line 10", "line 10 EDITED", 1);
    fs::write(&f, &v2).unwrap();
    run_post_edit(
        &drip,
        json!({
            "tool_name": "Edit",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "old_string": "line 10",
                "new_string": "line 10 EDITED",
            }
        }),
    );

    let out = drip.read_stdout(&f);
    assert!(out.contains("[DRIP: edit verified"), "got: {out}");
    // Hash should appear (hex prefix). The exact value differs but
    // we know the cert prints a `hash:` token.
    assert!(out.contains("hash:"), "missing hash in cert: {out}");
    // The cert advertises how much of the larger file stayed untouched.
    assert!(
        out.contains("Unchanged regions:"),
        "missing unchanged-region tally: {out}"
    );
    // Refresh hint must point users back to the full content.
    assert!(out.contains("drip refresh"), "missing refresh hint: {out}");
}

#[test]
fn cert_disabled_falls_back_to_passthrough() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("d.py");
    fs::write(&f, "v1\n").unwrap();
    drip.read_stdout(&f);
    fs::write(&f, "v2\n").unwrap();
    run_post_edit(
        &drip,
        json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );

    // Override env at the read invocation only — the test harness
    // already sets DRIP_DATA_DIR + DRIP_SESSION_ID; we add the disable.
    let mut cmd = drip.cmd();
    cmd.env("DRIP_CERT_DISABLE", "1");
    cmd.arg("read").arg(&f);
    let o = cmd.output().expect("read");
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(
        out.contains("[DRIP: post-edit passthrough"),
        "expected legacy passthrough when cert disabled, got: {out}"
    );
}

#[test]
fn cert_is_one_shot_then_unchanged() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("e.py");
    let before = "v1 payload payload payload payload\n".repeat(100);
    fs::write(&f, &before).unwrap();
    drip.read_stdout(&f);
    let after = before.replacen("v1", "v2", 1);
    fs::write(&f, &after).unwrap();
    run_post_edit(
        &drip,
        json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );
    let cert = drip.read_stdout(&f);
    assert!(cert.contains("[DRIP: edit verified"), "got: {cert}");
    // Second read on the same content returns unchanged, NOT a cert.
    let next = drip.read_stdout(&f);
    assert!(next.contains("[DRIP: unchanged"), "got: {next}");
    // Third read same as second.
    let next2 = drip.read_stdout(&f);
    assert!(next2.contains("[DRIP: unchanged"), "got: {next2}");
}

#[test]
fn cert_requires_existing_baseline() {
    // PostToolUse fires for a file DRIP has never seen — no baseline
    // means no diff to certify, fall back to passthrough.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("u.py");
    fs::write(&f, "fresh\n").unwrap();
    run_post_edit(
        &drip,
        json!({
            "tool_name": "Write",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );
    let out = drip.read_stdout(&f);
    assert!(
        out.contains("[DRIP: post-edit passthrough"),
        "expected passthrough on no-baseline edit, got: {out}"
    );
}

#[test]
fn post_edit_does_not_persist_dripignored_secrets() {
    // Privacy regression: an Edit on a `.env` file used to land its
    // contents in the per-session `reads` table AND the cross-
    // session `file_registry`, where backup tools could carry it
    // off-host. The Read path explicitly substitutes a placeholder
    // for dripignore'd files; the post-edit path now applies the
    // same gate before calling `set_baseline`.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let env_file = dir.path().join(".env");
    let secret = "DATABASE_URL=postgres://user:s3cr3t@db/prod\n";
    fs::write(&env_file, secret).unwrap();

    run_post_edit(
        &drip,
        json!({
            "tool_name": "Edit",
            "tool_input": { "file_path": env_file.to_string_lossy() }
        }),
    );

    // Read the SQLite store directly to confirm the secret never
    // reached either storage table. The DB lives at
    // `<DRIP_DATA_DIR>/sessions.db`.
    let db_path = drip.data_dir.path().join("sessions.db");
    if db_path.exists() {
        let db_bytes = std::fs::read(&db_path).unwrap();
        let needle = "s3cr3t";
        assert!(
            !db_bytes
                .windows(needle.len())
                .any(|w| w == needle.as_bytes()),
            ".env contents leaked into sessions.db — post-edit hook should honour .dripignore"
        );
    }

    // Also check the file-cache directory: large blobs spill there.
    let cache_dir = drip.data_dir.path().join("cache");
    if cache_dir.exists() {
        for entry in std::fs::read_dir(&cache_dir).unwrap().flatten() {
            let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
            assert!(
                !body.contains("s3cr3t"),
                ".env contents leaked into cache/{}",
                entry.file_name().to_string_lossy()
            );
        }
    }
}
