//! Gemini before-compress hook + init wiring tests.
//!
//! Exercises the v9 visibility ledger plumbing for Gemini CLI:
//!   - `drip init --agent gemini` writes both the MCP server entry
//!     AND the `hooks.beforeCompress.drip` entry into settings.json.
//!   - The `gemini-compress` hook handler bumps the compaction
//!     ledger and wipes per-session reads, so a re-read after
//!     compaction sees a fresh FullFirst (deny-substitute disabled
//!     for FullFirst, per the same contract Claude uses).
//!   - `drip doctor` warns when MCP is wired but the hook is missing
//!     (typical of upgrades from a pre-v9 Gemini install).

use crate::common::Drip;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn run_init(drip: &Drip, home: &Path, global: bool) -> std::process::Output {
    let mut cmd = Command::new(&drip.bin);
    cmd.arg("init").arg("--agent").arg("gemini");
    if global {
        cmd.arg("-g");
    }
    cmd.env("HOME", home)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .expect("drip init gemini")
}

fn run_compress_hook(drip: &Drip, payload: Value) -> std::process::Output {
    let mut child = Command::new(&drip.bin)
        .args(["hook", "gemini-compress"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gemini-compress hook");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn ledger_for(drip: &Drip) -> Option<(i64, Option<i64>, i64)> {
    let conn = rusqlite::Connection::open(drip.data_dir.path().join("sessions.db")).unwrap();
    conn.query_row(
        "SELECT context_epoch, last_compaction_at, compaction_count
         FROM sessions WHERE session_id = ?1",
        rusqlite::params![drip.session_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .ok()
}

#[test]
fn init_gemini_global_writes_mcp_and_compress_hook() {
    let drip = Drip::new();
    let home = tempfile::tempdir().unwrap();
    let o = run_init(&drip, home.path(), /*global=*/ true);
    assert!(
        o.status.success(),
        "init gemini -g failed: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );

    let path = home.path().join(".gemini/settings.json");
    assert!(path.exists(), "settings.json must be created");
    let raw = fs::read_to_string(&path).unwrap();
    let settings: Value = serde_json::from_str(&raw).unwrap();

    // Both entries land under the well-known dotted paths the doctor
    // check looks for.
    let mcp = settings
        .pointer("/mcpServers/drip")
        .expect("mcpServers.drip must be present");
    assert!(mcp.is_object());
    let hook = settings
        .pointer("/hooks/beforeCompress/drip")
        .expect("hooks.beforeCompress.drip must be present");
    let cmd = hook
        .get("command")
        .and_then(|v| v.as_str())
        .expect("hook entry must carry a `command` string");
    assert!(
        cmd.ends_with("hook gemini-compress"),
        "hook command must invoke `drip hook gemini-compress`, got: {cmd}"
    );
}

#[test]
fn init_gemini_is_idempotent_on_hook_block() {
    // Re-running init must not duplicate or churn the
    // hooks.beforeCompress.drip entry, just like the MCP one.
    let drip = Drip::new();
    let home = tempfile::tempdir().unwrap();
    let _ = run_init(&drip, home.path(), true);
    let cfg1 = fs::read_to_string(home.path().join(".gemini/settings.json")).unwrap();
    let _ = run_init(&drip, home.path(), true);
    let cfg2 = fs::read_to_string(home.path().join(".gemini/settings.json")).unwrap();
    assert_eq!(cfg1, cfg2, "second init churned the settings file");
}

#[test]
fn gemini_compress_hook_bumps_ledger_and_wipes_reads() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("g.py");
    fs::write(&f, "x = 1\n").unwrap();

    // Seed the session: do a read so a `sessions` row exists with a
    // baseline in `reads`.
    let _ = drip.read_stdout(&f);
    let (epoch_before, _, count_before) = ledger_for(&drip).expect("session row exists");
    assert_eq!(epoch_before, 0);
    assert_eq!(count_before, 0);

    // Fire the hook with the explicit session_id payload shape.
    let o = run_compress_hook(
        &drip,
        json!({
            "session_id": drip.session_id,
            "event": "beforeCompress"
        }),
    );
    assert!(
        o.status.success(),
        "compress hook errored: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    // Empty / `{}` JSON shape is fine (Gemini ignores stdout).
    let stdout = String::from_utf8_lossy(&o.stdout);
    let parsed: Value = serde_json::from_str(stdout.trim()).expect("hook must emit valid JSON");
    assert!(parsed.is_object());

    // Ledger bumped.
    let (epoch_after, last_after, count_after) = ledger_for(&drip).expect("row preserved");
    assert_eq!(epoch_after, 1, "context_epoch must bump by 1");
    assert_eq!(count_after, 1, "compaction_count must bump by 1");
    assert!(last_after.is_some(), "last_compaction_at must be set");

    // Per-session `reads` table wiped.
    let conn = rusqlite::Connection::open(drip.data_dir.path().join("sessions.db")).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM reads WHERE session_id = ?1",
            rusqlite::params![drip.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "reads must be wiped after compaction hook");
}

#[test]
fn gemini_compress_hook_tolerates_empty_payload() {
    // Gemini may invoke the hook with an empty stdin (the contract is
    // advisory — only exit code matters to the caller). DRIP must
    // fall back to the env-derived session id rather than crashing.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("e.py");
    fs::write(&f, "y = 2\n").unwrap();
    let _ = drip.read_stdout(&f);

    // Empty stdin (no JSON object).
    let mut child = Command::new(&drip.bin)
        .args(["hook", "gemini-compress"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdin.take()); // close stdin without writing
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "hook must tolerate empty stdin");

    let (_, _, count) = ledger_for(&drip).expect("row exists");
    assert_eq!(
        count, 1,
        "ledger must still bump on empty-payload invocation"
    );
}

#[test]
fn doctor_warns_when_mcp_present_but_hook_missing() {
    // Simulate a pre-v9 install: MCP entry only, no hook. Doctor's
    // `gemini_global` section must surface a Warn so the user knows
    // to re-run init.
    let drip = Drip::new();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(home.path().join(".gemini")).unwrap();
    let pre_v9 = json!({
        "mcpServers": {
            "drip": { "command": "/path/to/drip", "args": ["mcp", "--agent", "gemini"] }
        }
    });
    fs::write(
        home.path().join(".gemini/settings.json"),
        serde_json::to_string_pretty(&pre_v9).unwrap(),
    )
    .unwrap();

    let o = Command::new(&drip.bin)
        .args(["doctor", "--json"])
        .env("HOME", home.path())
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .output()
        .unwrap();
    assert!(o.status.success() || o.status.code() == Some(1));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let section = v
        .pointer("/sections/gemini_global")
        .expect("doctor JSON must expose /sections/gemini_global");
    assert_eq!(
        section.get("status").and_then(|s| s.as_str()),
        Some("warn"),
        "section worst status should be Warn (MCP wired but hook missing): {section}"
    );
    assert_eq!(
        section.get("hook_present").and_then(|b| b.as_bool()),
        Some(false),
        "doctor must report hook_present=false"
    );
}
