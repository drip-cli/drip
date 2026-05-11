use crate::common::Drip;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn estimate(text: &str) -> i64 {
    if text.is_empty() {
        0
    } else {
        text.len().div_ceil(4) as i64
    }
}

fn db(drip: &Drip) -> Connection {
    Connection::open(drip.data_dir.path().join("sessions.db")).unwrap()
}

fn canonical(path: &Path) -> String {
    path.canonicalize().unwrap().to_string_lossy().into_owned()
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |_| Ok(()),
    )
    .optional()
    .unwrap()
    .is_some()
}

fn assert_no_negative_accounting(drip: &Drip) {
    let conn = db(drip);
    for table in [
        "reads",
        "read_events",
        "lifetime_stats",
        "lifetime_per_file",
        "lifetime_daily",
    ] {
        if !table_exists(&conn, table) {
            continue;
        }
        let mut stmt = conn
            .prepare(&format!(
                "SELECT tokens_full, tokens_sent FROM {table} WHERE tokens_sent > tokens_full"
            ))
            .unwrap();
        let bad: Vec<(i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            bad.is_empty(),
            "{table} contains token-negative accounting rows: {bad:?}"
        );
    }
}

fn latest_event(drip: &Drip) -> (String, i64, i64, String) {
    db(drip)
        .query_row(
            "SELECT outcome_kind, tokens_full, tokens_sent, rendered
             FROM read_events
             WHERE session_id = ?1
             ORDER BY id DESC LIMIT 1",
            params![drip.session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

fn read_row(drip: &Drip, file_path: &Path) -> (i64, i64, i64) {
    db(drip)
        .query_row(
            "SELECT reads_count, tokens_full, tokens_sent
             FROM reads WHERE session_id = ?1 AND file_path = ?2",
            params![drip.session_id, canonical(file_path)],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

fn run_claude_read(
    drip: &Drip,
    file_path: &Path,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Value {
    let mut input = json!({ "file_path": file_path.to_string_lossy() });
    if let Some(offset) = offset {
        input["offset"] = json!(offset);
    }
    if let Some(limit) = limit {
        input["limit"] = json!(limit);
    }
    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Read",
        "tool_input": input
    });
    run_hook(drip, "claude", payload)
}

fn run_post_edit(drip: &Drip, file_path: &Path) {
    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Edit",
        "tool_input": { "file_path": file_path.to_string_lossy() }
    });
    let _ = run_hook(drip, "claude-post-edit", payload);
}

fn run_hook(drip: &Drip, hook: &str, payload: Value) -> Value {
    let mut cmd = Command::new(&drip.bin);
    cmd.args(["hook", hook])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn hook");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "hook {hook} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("hook stdout must be JSON")
}

fn permission_decision(v: &Value) -> &str {
    v["hookSpecificOutput"]["permissionDecision"]
        .as_str()
        .expect("permissionDecision")
}

fn permission_reason(v: &Value) -> &str {
    v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .expect("permissionDecisionReason")
}

fn large_python_fixture() -> String {
    let mut src = String::from("import pathlib\n\n");
    for f in 0..24 {
        src.push_str(&format!("def worker_{f}(value):\n"));
        for i in 0..24 {
            src.push_str(&format!(
                "    step_{i} = value + {i}  # stable payload for accounting audit\n"
            ));
        }
        src.push_str("    return value\n\n");
    }
    src
}

#[test]
fn audit_claude_full_read_then_unchanged_accounts_actual_payload() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("stable.txt");
    let content = "alpha bravo charlie delta echo foxtrot golf hotel\n".repeat(40);
    fs::write(&f, &content).unwrap();
    let full_tokens = estimate(&content);

    let first = run_claude_read(&drip, &f, None, None);
    assert_eq!(permission_decision(&first), "allow");
    let second = run_claude_read(&drip, &f, None, None);
    assert_eq!(permission_decision(&second), "deny");
    let sentinel_tokens = estimate(permission_reason(&second));
    assert!(sentinel_tokens < full_tokens);

    assert_eq!(
        read_row(&drip, &f),
        (2, full_tokens * 2, full_tokens + sentinel_tokens)
    );
    let (kind, tokens_full, tokens_sent, rendered) = latest_event(&drip);
    assert_eq!(kind, "unchanged");
    assert_eq!(tokens_full, full_tokens);
    assert_eq!(tokens_sent, estimate(&rendered));
    assert_no_negative_accounting(&drip);
}

#[test]
fn audit_delta_counts_current_file_and_rendered_diff_payload() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("delta.py");
    let before = large_python_fixture();
    fs::write(&f, &before).unwrap();
    let first = drip.read_stdout(&f);
    assert!(
        estimate(&first) <= estimate(&before),
        "first compressed read must not start with a token loss"
    );

    let after = before.replacen(
        "step_7 = value + 7  # stable payload for accounting audit",
        "step_7 = value + 700  # changed payload for accounting audit",
        1,
    );
    fs::write(&f, &after).unwrap();
    drip.read_stdout(&f);

    let (kind, tokens_full, tokens_sent, rendered) = latest_event(&drip);
    assert_eq!(kind, "delta");
    assert_eq!(tokens_full, estimate(&after));
    assert_eq!(tokens_sent, estimate(&rendered));
    assert!(
        tokens_sent < tokens_full,
        "delta must be cheaper: {rendered}"
    );
    assert_no_negative_accounting(&drip);
}

#[test]
fn audit_partial_read_uses_window_as_counterfactual() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("partial.txt");
    let content: String = (1..=80)
        .map(|i| format!("line {i:02} payload payload payload payload\n"))
        .collect();
    fs::write(&f, &content).unwrap();
    let window: String = (5..=24)
        .map(|i| format!("line {i:02} payload payload payload payload\n"))
        .collect();
    let window_tokens = estimate(&window);

    let first = run_claude_read(&drip, &f, Some(5), Some(20));
    assert_eq!(permission_decision(&first), "allow");
    let second = run_claude_read(&drip, &f, Some(5), Some(20));
    assert_eq!(permission_decision(&second), "deny");
    assert!(permission_reason(&second).contains("unchanged (lines 5-24)"));
    let sentinel_tokens = estimate(permission_reason(&second));
    assert!(sentinel_tokens < window_tokens);

    assert_eq!(
        read_row(&drip, &f),
        (2, window_tokens * 2, window_tokens + sentinel_tokens)
    );
    assert_no_negative_accounting(&drip);
}

#[test]
fn audit_tiny_partial_window_passes_through_when_marker_costs_more() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("tiny-partial.txt");
    let content: String = (1..=10).map(|i| format!("line {i}\n")).collect();
    fs::write(&f, &content).unwrap();
    let window = "line 1\n";
    let window_tokens = estimate(window);

    let first = run_claude_read(&drip, &f, Some(1), Some(1));
    assert_eq!(permission_decision(&first), "allow");
    let second = run_claude_read(&drip, &f, Some(1), Some(1));
    assert_eq!(permission_decision(&second), "allow");

    assert_eq!(
        read_row(&drip, &f),
        (2, window_tokens * 2, window_tokens * 2)
    );
    assert_no_negative_accounting(&drip);
}

#[test]
fn audit_edit_certificate_counts_rendered_certificate_not_full_file() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("edit_cert.txt");
    let before: String = (0..220)
        .map(|i| format!("line {i} payload payload payload\n"))
        .collect();
    fs::write(&f, &before).unwrap();
    let first = run_claude_read(&drip, &f, None, None);
    assert_eq!(permission_decision(&first), "allow");

    let after = before.replace("line 100 payload", "line 100 EDITED payload");
    fs::write(&f, &after).unwrap();
    run_post_edit(&drip, &f);
    let response = run_claude_read(&drip, &f, None, None);
    assert_eq!(permission_decision(&response), "deny");
    assert!(permission_reason(&response).contains("[DRIP: edit verified"));

    let (kind, tokens_full, tokens_sent, rendered) = latest_event(&drip);
    assert_eq!(kind, "edit-cert");
    assert_eq!(tokens_full, estimate(&after));
    assert_eq!(tokens_sent, estimate(&rendered));
    assert!(tokens_sent < tokens_full);
    assert_no_negative_accounting(&drip);
}

#[test]
fn audit_placeholder_fallbacks_are_conservative() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("blob.bin");
    fs::write(&binary, b"\x00\x01\x02\x03\x04").unwrap();

    let response = run_claude_read(&drip, &binary, None, None);
    assert_eq!(permission_decision(&response), "deny");
    assert!(permission_reason(&response).contains("binary file"));
    let (kind, tokens_full, tokens_sent, _) = latest_event(&drip);
    assert_eq!(kind, "fallback");
    assert_eq!(tokens_full, tokens_sent);

    let ignored = dir.path().join(".env");
    fs::write(&ignored, "SECRET=not-recorded\n").unwrap();
    let response = run_claude_read(&drip, &ignored, None, None);
    assert_eq!(permission_decision(&response), "deny");
    assert!(permission_reason(&response).contains("matched .dripignore"));
    let (kind, tokens_full, tokens_sent, _) = latest_event(&drip);
    assert_eq!(kind, "fallback");
    assert_eq!(tokens_full, tokens_sent);
    assert_no_negative_accounting(&drip);
}
