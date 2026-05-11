//! Pre-edit hook tests: block Edit when the target lands inside a
//! function body the agent never saw (semantic-compression elision).
//!
//! Step 5 of the source-map arc. The hook reads `reads.source_map`,
//! finds elided regions, maps `old_string` matches in the original
//! baseline back to original line numbers, and denies the Edit when
//! they overlap. Without source maps (uncompressed reads, untracked
//! files) it degrades open — Claude's read-first guard + DRIP's
//! PostToolUse cert pick up the slack.

use crate::common::Drip;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn long_python_module() -> String {
    // 5 functions × 12-line bodies. Default min_body_lines=8 → every
    // body elided. Pinned across DRIP_COMPRESS_MIN_BODY tweaks.
    let mut s = String::from("import os\nimport sys\n\n");
    for n in 0..5 {
        s.push_str(&format!("def fn_{n}(a, b, c):\n"));
        for i in 0..12 {
            s.push_str(&format!("    step_{i:02} = a + b + {i}\n"));
        }
        s.push_str("    return step_11\n\n");
    }
    s
}

fn run_pre_edit(drip: &Drip, payload: Value, extra_env: &[(&str, &str)]) -> Value {
    let mut cmd = Command::new(&drip.bin);
    cmd.args(["hook", "claude-pre-edit"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        // Strip dev-shell env that would silently bypass the guard
        // (a contributor with DRIP_PRE_EDIT_WARN=0 set globally would
        // otherwise see every "deny" assertion fail locally).
        .env_remove("DRIP_DISABLE")
        .env_remove("DRIP_PRE_EDIT_WARN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn pre-edit hook");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(
        o.status.success(),
        "pre-edit hook errored: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).unwrap_or_else(|_| {
        panic!(
            "pre-edit hook didn't emit JSON: stdout={}",
            String::from_utf8_lossy(&o.stdout)
        )
    })
}

fn permission(resp: &Value) -> &str {
    resp.get("hookSpecificOutput")
        .and_then(|h| h.get("permissionDecision"))
        .and_then(|d| d.as_str())
        .expect("response must include permissionDecision")
}

fn reason(resp: &Value) -> Option<&str> {
    resp.get("hookSpecificOutput")
        .and_then(|h| h.get("permissionDecisionReason"))
        .and_then(|d| d.as_str())
}

fn read_with_compression(drip: &Drip, file: &Path) -> String {
    drip.read_stdout(file)
}

#[test]
fn edit_inside_elided_body_is_denied_with_symbol_and_range() {
    // Agent reads `module.py`, gets stubs for every function, then
    // tries to Edit a body line whose text only exists inside one
    // of the elided regions. The pre-edit hook must block, name the
    // function, and cite its original line range.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("module.py");
    fs::write(&f, long_python_module()).unwrap();
    let out = read_with_compression(&drip, &f);
    assert!(
        out.contains("(semantic-compressed)"),
        "precondition: read must trigger compression: {out}"
    );

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Edit",
        "tool_input": {
            "file_path": f.to_string_lossy(),
            // Body line that only exists inside elided fn_2's body
            // (the `2` index disambiguates: elision substring is
            // unique enough to land in exactly one entry).
            "old_string": "    step_05 = a + b + 5\n    step_06 = a + b + 6\n",
            "new_string": "    step_05 = 'patched'\n    step_06 = 'patched'\n",
        }
    });
    let resp = run_pre_edit(&drip, payload, &[]);
    assert_eq!(permission(&resp), "deny");
    let r = reason(&resp).expect("deny must carry a reason");
    assert!(
        r.contains("STOP"),
        "reason must lead with STOP for visibility: {r}"
    );
    assert!(
        r.contains("`fn_") || r.contains("fn_"),
        "reason must name an elided fn: {r}"
    );
    assert!(
        r.contains('L') && r.contains('-'),
        "reason must cite the original line range: {r}"
    );
    assert!(
        r.contains("drip refresh"),
        "reason must point at the recovery command: {r}"
    );
}

#[test]
fn edit_outside_elided_region_passes_through() {
    // Editing the import block at the top of the file is fine — it
    // was visible to the agent. Hook must allow.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("module.py");
    fs::write(&f, long_python_module()).unwrap();
    let out = read_with_compression(&drip, &f);
    assert!(out.contains("(semantic-compressed)"));

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Edit",
        "tool_input": {
            "file_path": f.to_string_lossy(),
            "old_string": "import os\nimport sys\n",
            "new_string": "import os\nimport sys\nimport json\n",
        }
    });
    let resp = run_pre_edit(&drip, payload, &[]);
    assert_eq!(permission(&resp), "allow", "imports are visible: {resp}");
}

#[test]
fn edit_on_signature_line_passes_through() {
    // The function signature `def fn_3(a, b, c):` is on a visible
    // line in the compressed view. Editing the signature itself
    // should not be blocked (only the elided body is hidden).
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("module.py");
    fs::write(&f, long_python_module()).unwrap();
    read_with_compression(&drip, &f);

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Edit",
        "tool_input": {
            "file_path": f.to_string_lossy(),
            "old_string": "def fn_3(a, b, c):",
            "new_string": "def fn_3(a, b, c, *, debug=False):",
        }
    });
    let resp = run_pre_edit(&drip, payload, &[]);
    assert_eq!(
        permission(&resp),
        "allow",
        "signature edits must pass: {resp}"
    );
}

#[test]
fn write_tool_warns_on_any_compressed_file() {
    // Whole-file Write replaces every byte, including the elided
    // bodies the agent never saw. Always block when there's at
    // least one elided region.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("module.py");
    fs::write(&f, long_python_module()).unwrap();
    read_with_compression(&drip, &f);

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Write",
        "tool_input": {
            "file_path": f.to_string_lossy(),
            "content": "def fn_0():\n    return 1\n",
        }
    });
    let resp = run_pre_edit(&drip, payload, &[]);
    assert_eq!(
        permission(&resp),
        "deny",
        "Write replaces unseen bytes — must block: {resp}"
    );
    let r = reason(&resp).unwrap();
    assert!(r.contains("Write"), "reason should name the tool: {r}");
}

#[test]
fn multiedit_with_one_elided_target_is_denied_for_that_target() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("module.py");
    fs::write(&f, long_python_module()).unwrap();
    read_with_compression(&drip, &f);

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "MultiEdit",
        "tool_input": {
            "file_path": f.to_string_lossy(),
            "edits": [
                // Visible: import block.
                {
                    "old_string": "import os\n",
                    "new_string": "import os  # patched\n",
                },
                // Elided body of fn_4.
                {
                    "old_string": "    step_09 = a + b + 9\n    step_10 = a + b + 10\n",
                    "new_string": "    step_09 = 0\n    step_10 = 0\n",
                },
            ]
        }
    });
    let resp = run_pre_edit(&drip, payload, &[]);
    assert_eq!(
        permission(&resp),
        "deny",
        "any elided target in MultiEdit must trigger deny: {resp}"
    );
}

#[test]
fn no_source_map_means_no_block() {
    // Tiny file → no compression → no source map → the hook must
    // allow. Otherwise we'd block legitimate edits to small files
    // and the user would never know why.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("tiny.py");
    fs::write(&f, "def hi():\n    return 1\n").unwrap();
    drip.read_stdout(&f);

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Edit",
        "tool_input": {
            "file_path": f.to_string_lossy(),
            "old_string": "    return 1\n",
            "new_string": "    return 2\n",
        }
    });
    let resp = run_pre_edit(&drip, payload, &[]);
    assert_eq!(permission(&resp), "allow");
}

#[test]
fn untracked_file_means_no_block() {
    // File never read by DRIP. We have no baseline to map against
    // — Claude Code's own read-first guard will reject anyway, so
    // we shouldn't shadow its error with a misleading deny.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("never_read.py");
    fs::write(&f, "x = 1\n").unwrap();

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Edit",
        "tool_input": {
            "file_path": f.to_string_lossy(),
            "old_string": "x = 1\n",
            "new_string": "x = 2\n",
        }
    });
    let resp = run_pre_edit(&drip, payload, &[]);
    assert_eq!(permission(&resp), "allow");
}

#[test]
fn drip_pre_edit_warn_zero_bypasses_the_guard() {
    // Escape hatch: power users who genuinely want to edit elided
    // bytes (e.g. they expanded the file via `drip refresh` and
    // re-read but haven't yet refreshed the source map row) can
    // set DRIP_PRE_EDIT_WARN=0 to bypass.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("module.py");
    fs::write(&f, long_python_module()).unwrap();
    read_with_compression(&drip, &f);

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Edit",
        "tool_input": {
            "file_path": f.to_string_lossy(),
            "old_string": "    step_05 = a + b + 5\n    step_06 = a + b + 6\n",
            "new_string": "    step_05 = 'x'\n    step_06 = 'x'\n",
        }
    });
    let resp = run_pre_edit(&drip, payload, &[("DRIP_PRE_EDIT_WARN", "0")]);
    assert_eq!(
        permission(&resp),
        "allow",
        "DRIP_PRE_EDIT_WARN=0 must bypass the guard: {resp}"
    );
}

#[test]
fn drip_disable_bypasses_the_guard() {
    // Global DRIP_DISABLE escape hatch must apply here too —
    // consistency with the Read hook.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("module.py");
    fs::write(&f, long_python_module()).unwrap();
    read_with_compression(&drip, &f);

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Edit",
        "tool_input": {
            "file_path": f.to_string_lossy(),
            "old_string": "    step_05 = a + b + 5\n    step_06 = a + b + 6\n",
            "new_string": "    step_05 = 'x'\n    step_06 = 'x'\n",
        }
    });
    let resp = run_pre_edit(&drip, payload, &[("DRIP_DISABLE", "1")]);
    assert_eq!(permission(&resp), "allow");
}

#[test]
fn unknown_tool_passes_through() {
    let drip = Drip::new();
    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Bash",
        "tool_input": { "command": "ls" }
    });
    let resp = run_pre_edit(&drip, payload, &[]);
    assert_eq!(permission(&resp), "allow");
}

#[test]
fn very_short_old_string_does_not_trigger_spurious_block() {
    // `}` or `def ` would match thousands of times if we didn't
    // require a minimum length. Pin the 4-byte floor so future
    // tuning of the heuristic doesn't silently regress.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("module.py");
    fs::write(&f, long_python_module()).unwrap();
    read_with_compression(&drip, &f);

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Edit",
        "tool_input": {
            "file_path": f.to_string_lossy(),
            "old_string": "a",
            "new_string": "A",
        }
    });
    let resp = run_pre_edit(&drip, payload, &[]);
    assert_eq!(
        permission(&resp),
        "allow",
        "1-char old_string must not trigger blocks: {resp}"
    );
}
