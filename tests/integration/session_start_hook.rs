//! SessionStart hook regression tests.
//!
//! Regression: when Claude Code compacts the conversation, its
//! internal "Edit must Read first" tracker is wiped but `session_id`
//! stays the same. Pre-fix, DRIP kept its baseline and returned
//! `[DRIP: unchanged]` / a delta on the next Read via `deny`; Claude
//! Code didn't register the denied call as a successful Read, and the
//! immediately-following Edit failed with "File must be read first".
//!
//! These tests pin the SessionStart hook contract: on `compact` /
//! `clear`, drop the per-session reads so the next Read re-triggers
//! the `FullFirst → allow` passthrough that repopulates Claude's
//! tracker. Other sources (`startup`, `resume`) must NOT trigger the
//! wipe — they either start fresh or preserve the agent's state.

use crate::common::Drip;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn run_session_start(drip: &Drip, payload: Value) -> std::process::Output {
    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude-session-start"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn session-start hook");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn run_read_hook(drip: &Drip, file_path: &str) -> Value {
    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Read",
        "tool_input": { "file_path": file_path }
    });
    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn read hook");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(
        o.status.success(),
        "read hook failed: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).expect("read hook must emit JSON")
}

fn permission_decision(resp: &Value) -> &str {
    resp.get("hookSpecificOutput")
        .and_then(|h| h.get("permissionDecision"))
        .and_then(|d| d.as_str())
        .expect("response must include hookSpecificOutput.permissionDecision")
}

fn permission_reason(resp: &Value) -> &str {
    resp.get("hookSpecificOutput")
        .and_then(|h| h.get("permissionDecisionReason"))
        .and_then(|d| d.as_str())
        .expect("response must include hookSpecificOutput.permissionDecisionReason")
}

#[test]
fn compact_drops_baseline_so_next_read_passes_through_to_native() {
    // Reproduces the bug: pre-fix, the second read after compact
    // would have come back `deny` (with `[DRIP: unchanged]` /
    // delta), and Claude Code's Edit guard would have blocked the
    // next Edit. Post-fix, the second read must come back `allow`
    // — letting Claude's native Read run and repopulate the
    // tracker.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("post_compact.py");
    fs::write(&f, "x = 1\n").unwrap();
    let path = f.to_string_lossy().to_string();

    let first = run_read_hook(&drip, &path);
    assert_eq!(
        permission_decision(&first),
        "allow",
        "first read must allow passthrough so Claude's native Read populates its tracker"
    );

    // Without the SessionStart hook firing, this second read would
    // come back `deny` with a `[DRIP: unchanged]` payload — and
    // that's exactly the regression we're guarding against.
    let o = run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "compact",
            "hook_event_name": "SessionStart"
        }),
    );
    assert!(
        o.status.success(),
        "session-start hook failed: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );

    let second = run_read_hook(&drip, &path);
    assert_eq!(
        permission_decision(&second),
        "allow",
        "after `compact` the next read MUST passthrough so Claude's tracker recovers; \
         got {second}"
    );
}

#[test]
fn claude_first_read_passthrough_records_native_full_tokens_not_compressed() {
    // Claude's Read hook must allow the native first read so Claude's
    // internal "read before edit" tracker is populated. That means
    // token accounting must record the full native payload, not the
    // semantic-compressed view used by CLI/MCP/Bash substitutions.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("large.py");
    let mut body = String::from("import os\n\n");
    for n in 0..8 {
        body.push_str(&format!("def fn_{n}(a, b):\n"));
        for i in 0..30 {
            body.push_str(&format!("    step_{i:02} = a + b + {i}\n"));
        }
        body.push_str("    return step_29\n\n");
    }
    fs::write(&f, body).unwrap();

    let resp = run_read_hook(&drip, f.to_str().unwrap());
    assert_eq!(permission_decision(&resp), "allow", "{resp}");

    let conn = rusqlite::Connection::open(db_path(&drip)).unwrap();
    let row: (i64, i64, i64, Option<String>) = conn
        .query_row(
            "SELECT tokens_full, tokens_sent, was_semantic_compressed, source_map
             FROM reads WHERE session_id = ?1",
            rusqlite::params![drip.session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();

    assert_eq!(row.0, row.1, "native first read has no DRIP savings");
    assert_eq!(row.2, 0, "Claude native read must not be marked compressed");
    assert!(
        row.3.is_none(),
        "source_map would make pre-edit think Claude missed body lines"
    );
}

#[test]
fn claude_first_read_passthrough_replay_does_not_count_registry_trailer() {
    // The file registry is a DRIP-rendered first-read decoration. A
    // Claude native first Read is allowed through, so replay accounting
    // must not add the registry diff trailer that Claude never saw.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("registry_native.py");
    fs::write(&f, "value = 1\n").unwrap();

    let seed = drip
        .cmd_in_session("seed-registry")
        .arg("read")
        .arg(&f)
        .output()
        .expect("seed registry via manual read");
    assert!(
        seed.status.success(),
        "seed read failed: stderr={}",
        String::from_utf8_lossy(&seed.stderr)
    );

    fs::write(&f, "value = 2\n").unwrap();
    let resp = run_read_hook(&drip, f.to_str().unwrap());
    assert_eq!(permission_decision(&resp), "allow", "{resp}");

    let conn = rusqlite::Connection::open(db_path(&drip)).unwrap();
    let row: (i64, i64, String) = conn
        .query_row(
            "SELECT tokens_full, tokens_sent, rendered
             FROM read_events
             WHERE session_id = ?1
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![drip.session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        row.0, row.1,
        "native Claude first read must not count an unsent registry trailer"
    );
    assert_eq!(row.2, "[allow → native Read]");
}

#[test]
fn claude_binary_fallback_is_substituted_not_native_passthrough() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("blob.bin");
    fs::write(&f, b"\x00\x01\x02\x03\xff\xfe").unwrap();

    let resp = run_read_hook(&drip, f.to_str().unwrap());
    assert_eq!(
        permission_decision(&resp),
        "deny",
        "binary placeholder must be delivered by DRIP, not native Read: {resp}"
    );

    let reason = permission_reason(&resp);
    assert!(
        reason.contains("binary file"),
        "expected binary placeholder, got: {reason}"
    );

    let conn = rusqlite::Connection::open(db_path(&drip)).unwrap();
    let row: (i64, i64, String) = conn
        .query_row(
            "SELECT tokens_full, tokens_sent, rendered
             FROM read_events
             WHERE session_id = ?1
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![drip.session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        row.0, row.1,
        "binary fallback must not claim savings against unsent native bytes"
    );
    assert_eq!(
        row.2, reason,
        "recorded replay body must match what Claude received"
    );
}

#[test]
fn clear_source_also_drops_baselines() {
    // `/clear` wipes Claude's tracker the same way compact does.
    // Same recovery contract.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("post_clear.py");
    fs::write(&f, "y = 2\n").unwrap();
    let path = f.to_string_lossy().to_string();

    run_read_hook(&drip, &path);
    run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "clear",
            "hook_event_name": "SessionStart"
        }),
    );

    let next = run_read_hook(&drip, &path);
    assert_eq!(permission_decision(&next), "allow");
}

#[test]
fn startup_source_is_a_noop() {
    // `startup` always carries a fresh session_id — DRIP would have
    // no baseline anyway. We still verify the hook accepts the
    // payload and doesn't accidentally wipe state for the matching
    // session_id (which would happen if a future code change
    // misclassified the source).
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("startup.py");
    fs::write(
        &f,
        "z = 3  # enough payload for unchanged marker to be cheaper\n".repeat(20),
    )
    .unwrap();
    let path = f.to_string_lossy().to_string();

    let first = run_read_hook(&drip, &path);
    assert_eq!(permission_decision(&first), "allow");

    run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "startup",
            "hook_event_name": "SessionStart"
        }),
    );

    // No file change → second read should return `deny` with
    // `[DRIP: unchanged]`. Proves the baseline survived a
    // non-resetting source.
    let second = run_read_hook(&drip, &path);
    assert_eq!(
        permission_decision(&second),
        "deny",
        "startup source must NOT clear baselines: {second}"
    );
}

#[test]
fn resume_source_drops_baselines_too() {
    // Field-report regression: `claude --resume` / `--continue`
    // reload the conversation transcript but the in-memory tracker
    // gets rebuilt from scratch — same failure mode as compact.
    // DRIP must drop its baselines so the next Read passes through
    // and Claude's tracker repopulates. The cost (one full re-read
    // per touched file) is a worthwhile trade against the previous
    // hard failure mode of Edit being rejected mid-task.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("resume.py");
    fs::write(&f, "z = 4\n").unwrap();
    let path = f.to_string_lossy().to_string();

    let first = run_read_hook(&drip, &path);
    assert_eq!(permission_decision(&first), "allow");

    run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "resume",
            "hook_event_name": "SessionStart"
        }),
    );

    let second = run_read_hook(&drip, &path);
    assert_eq!(
        permission_decision(&second),
        "allow",
        "resume must wipe baselines so Claude's tracker recovers — got {second}"
    );
}

#[test]
fn session_start_handler_tolerates_unknown_source_values() {
    // Future-proofing: if Claude Code adds a new source (e.g.
    // `auto-resume`), DRIP must default to keeping the baseline
    // rather than nuking it. Wrong-but-safe is "preserve state".
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("future.py");
    fs::write(
        &f,
        "future = True  # enough payload for unchanged marker to be cheaper\n".repeat(20),
    )
    .unwrap();
    let path = f.to_string_lossy().to_string();

    run_read_hook(&drip, &path);
    let o = run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "auto-resume-2027",
            "hook_event_name": "SessionStart"
        }),
    );
    assert!(o.status.success(), "must not error on unknown source");

    let next = run_read_hook(&drip, &path);
    assert_eq!(permission_decision(&next), "deny");
}

#[test]
fn session_start_emits_empty_json_object() {
    // Claude Code accepts `{}` as a no-op response. We don't try to
    // inject `additionalContext` — the per-file decoration on the
    // next read carries the same info at the right time.
    let drip = Drip::new();
    let o = run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "compact",
            "hook_event_name": "SessionStart"
        }),
    );
    assert!(o.status.success());
    let stdout = String::from_utf8_lossy(&o.stdout);
    let parsed: Value =
        serde_json::from_str(stdout.trim()).expect("hook output must be valid JSON");
    assert!(
        parsed.is_object(),
        "expected an object response, got {parsed}"
    );
}

// ─── v9 compaction visibility ledger ───────────────────────────────

fn db_path(drip: &Drip) -> std::path::PathBuf {
    drip.data_dir.path().join("sessions.db")
}

/// Pull (context_epoch, last_compaction_at, compaction_count) for the
/// shared session id. Returns None when no `sessions` row exists yet.
fn ledger_for(drip: &Drip) -> Option<(i64, Option<i64>, i64)> {
    let conn = rusqlite::Connection::open(db_path(drip)).unwrap();
    conn.query_row(
        "SELECT context_epoch, last_compaction_at, compaction_count
         FROM sessions WHERE session_id = ?1",
        rusqlite::params![drip.session_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .ok()
}

#[test]
fn session_start_compact_bumps_context_epoch_and_count() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("epoch.py");
    fs::write(
        &f, "x = 1
",
    )
    .unwrap();
    let path = f.to_string_lossy().to_string();

    // Establish the sessions row + a baseline.
    let _ = run_read_hook(&drip, &path);
    let (epoch0, last0, count0) = ledger_for(&drip).expect("session row exists");
    assert_eq!(epoch0, 0);
    assert_eq!(count0, 0);
    assert!(last0.is_none(), "no compaction yet → NULL");

    // Fire compact.
    run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "compact",
            "hook_event_name": "SessionStart"
        }),
    );
    let (epoch1, last1, count1) = ledger_for(&drip).expect("row preserved");
    assert_eq!(epoch1, 1, "compact must bump context_epoch by 1");
    assert_eq!(count1, 1, "compact must bump compaction_count by 1");
    assert!(last1.is_some(), "last_compaction_at must be set");

    // Second compact bumps again (stays a counter, not a flag).
    run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "compact",
            "hook_event_name": "SessionStart"
        }),
    );
    let (epoch2, _, count2) = ledger_for(&drip).expect("row still preserved");
    assert_eq!(epoch2, 2);
    assert_eq!(count2, 2);
}

#[test]
fn session_start_compact_emits_additional_context_for_agent() {
    // The FullFirst→allow design preserves Claude's "Edit must Read
    // first" guarantee, but it also means the renderer's per-read
    // ↺ decoration never reaches the agent's tool result. We make
    // the compaction visible to the agent via SessionStart's
    // `additionalContext` channel — injected into the next turn's
    // prompt — so the agent explicitly knows DRIP just reset.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("hello.py");
    fs::write(&f, "x = 1\n").unwrap();
    let path = f.to_string_lossy().to_string();

    let _ = run_read_hook(&drip, &path);

    let o = run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "compact",
            "hook_event_name": "SessionStart"
        }),
    );
    assert!(o.status.success());
    let stdout = String::from_utf8_lossy(&o.stdout);
    let parsed: Value =
        serde_json::from_str(stdout.trim()).expect("hook output must be valid JSON");
    let ctx = parsed
        .pointer("/hookSpecificOutput/additionalContext")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert!(
        ctx.contains("compact"),
        "additionalContext must mention compaction: {ctx}"
    );
    assert!(
        ctx.contains("(#1)"),
        "additionalContext must surface the count: {ctx}"
    );
    assert!(
        ctx.contains("baselines have been reset"),
        "additionalContext must describe the effect: {ctx}"
    );
    // Regression: an earlier build emitted the notice as one long
    // string with embedded multi-space runs ("…(#1)          —
    // per-session…"). Pin the contract that the text is single-spaced
    // throughout — runs of two-or-more spaces are a leak of source
    // formatting into agent-facing output.
    assert!(
        !ctx.contains("  "),
        "additionalContext must not contain double-spaces: {ctx:?}"
    );
}

#[test]
fn session_start_with_inert_source_does_not_emit_additional_context() {
    // Sources that DON'T trigger a wipe (`startup`, unknown future
    // values) must keep the response empty so the agent doesn't get
    // a spurious "DRIP reset" notice on every session start.
    let drip = Drip::new();
    let o = run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "startup",
            "hook_event_name": "SessionStart"
        }),
    );
    assert!(o.status.success());
    let stdout = String::from_utf8_lossy(&o.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(
        parsed
            .pointer("/hookSpecificOutput/additionalContext")
            .is_none(),
        "startup must NOT emit additionalContext: {parsed}"
    );
}

#[test]
fn read_without_compaction_has_no_compaction_header() {
    // Negative case: a fresh session that's never been compacted
    // must NOT emit the ↺ decoration on first reads.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("clean.py");
    fs::write(
        &f, "y = 2
",
    )
    .unwrap();
    let path = f.to_string_lossy().to_string();

    let _ = run_read_hook(&drip, &path);

    let conn = rusqlite::Connection::open(db_path(&drip)).unwrap();
    let rendered: String = conn
        .query_row(
            "SELECT rendered FROM read_events
             WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
            rusqlite::params![drip.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !rendered.contains("context was compacted"),
        "no compactions → no ↺ header, got: {rendered}"
    );
}

#[test]
fn schema_v9_columns_present_after_open() {
    // Cheap migration smoke test: opening the DB must produce a
    // sessions row that exposes the v9 columns and a reads row that
    // exposes its `context_epoch`. Defaults are 0 (epoch) / NULL
    // (last_compaction_at) / 0 (count) — same as a session that
    // has never been compacted.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("schema.py");
    fs::write(
        &f, "z = 3
",
    )
    .unwrap();
    let _ = run_read_hook(&drip, &f.to_string_lossy());

    let conn = rusqlite::Connection::open(db_path(&drip)).unwrap();
    let (epoch, last, count): (i64, Option<i64>, i64) = conn
        .query_row(
            "SELECT context_epoch, last_compaction_at, compaction_count
             FROM sessions WHERE session_id = ?1",
            rusqlite::params![drip.session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((epoch, last, count), (0, None, 0));

    let read_epoch: i64 = conn
        .query_row(
            "SELECT context_epoch FROM reads WHERE session_id = ?1",
            rusqlite::params![drip.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(read_epoch, 0, "pre-compaction baselines record epoch=0");

    // Sanity: the persisted schema_version is at least 9.
    let v: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        v.parse::<i64>().unwrap() >= 9,
        "schema_version must be >= 9, got {v}"
    );
}

#[test]
fn meter_json_includes_compaction_block_after_compact() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("m.py");
    fs::write(
        &f, "n = 42
",
    )
    .unwrap();
    let _ = run_read_hook(&drip, &f.to_string_lossy());
    run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "compact",
            "hook_event_name": "SessionStart"
        }),
    );

    // Lifetime view: total_compactions >= 1 must appear.
    let o = drip.cmd().args(["meter", "--json"]).output().unwrap();
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let comp = v
        .get("compaction")
        .expect("meter --json must expose `compaction` block after a compaction");
    assert!(
        comp.get("total_compactions")
            .and_then(|n| n.as_i64())
            .unwrap_or(0)
            >= 1,
        "compaction.total_compactions >= 1: {comp}"
    );
    assert!(
        comp.get("last_compaction_at").is_some(),
        "compaction.last_compaction_at must be set: {comp}"
    );
}

#[test]
fn doctor_reports_compaction_count_when_session_was_compacted() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("d.py");
    fs::write(
        &f, "k = 9
",
    )
    .unwrap();
    let _ = run_read_hook(&drip, &f.to_string_lossy());
    run_session_start(
        &drip,
        json!({
            "session_id": drip.session_id,
            "source": "compact",
            "hook_event_name": "SessionStart"
        }),
    );

    let o = drip.cmd().args(["doctor", "--json"]).output().unwrap();
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let session = v
        .pointer("/sections/session")
        .expect("doctor JSON must expose /sections/session");
    let count = session
        .get("compaction_count")
        .and_then(|n| n.as_i64())
        .unwrap_or(-1);
    assert!(
        count >= 1,
        "doctor must report compaction_count >= 1: {session}"
    );
    let epoch = session
        .get("context_epoch")
        .and_then(|n| n.as_i64())
        .unwrap_or(-1);
    assert!(
        epoch >= 1,
        "doctor must report context_epoch >= 1: {session}"
    );
}
