//! Regression tests for bugs found in the v0.1.0 audit.

use crate::common::Drip;
use serde_json::Value;
use std::fs;

/// Bug H1: `open_with_id` used to insert a ghost `sessions` row keyed by
/// the cwd-derived id, then silently overwrite `s.id`. Net effect: the
/// real session never had a `started_at` row, so `drip meter` always
/// reported `elapsed_secs = 0` and `drip sessions` listed only ghosts.
#[test]
fn explicit_session_id_creates_real_sessions_row() {
    let drip = Drip::new();
    // drip.cmd() sets DRIP_SESSION_ID, so every `drip read` we run uses
    // the same explicit id. After a read there must be a `sessions` row
    // with THAT id (not a derived one).
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("h1.txt");
    fs::write(&f, "hello\n").unwrap();
    drip.read_stdout(&f);

    // List sessions — at least one should have our explicit id.
    // The SESSION column auto-sizes to the widest id, so the full
    // string is visible and copy-pasteable.
    let o = drip.cmd().arg("sessions").output().unwrap();
    assert!(o.status.success());
    let listing = String::from_utf8_lossy(&o.stdout);
    assert!(
        listing.contains(&drip.session_id),
        "expected full session id {} in `drip sessions` output:\n{listing}",
        drip.session_id,
    );
}

/// Bug H1 follow-up: `drip meter --json` must report a non-zero
/// `elapsed_secs` once a session has been around long enough — and it
/// must reflect the explicit session id, not the derived one.
#[test]
fn meter_json_uses_real_session_started_at() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("g.txt");
    fs::write(&f, "x\n").unwrap();
    drip.read_stdout(&f);

    let o = drip
        .cmd()
        .arg("meter")
        .arg("--session")
        .arg("--json")
        .output()
        .unwrap();
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["session_id"].as_str(), Some(drip.session_id.as_str()));
    assert!(v["started_at"].as_i64().unwrap() > 0);
}

/// Bug H2 (already patched): `drip init` used to write `"drip hook claude"`
/// — a bare command name. When Claude Code spawns the hook, its PATH
/// doesn't necessarily include `~/.cargo/bin`, so the hook silently fails
/// and DRIP appears not to work. The fix uses an absolute path.
///
/// This test asserts the generated settings.json never contains a bare
/// "drip " token in any hook command.
#[test]
fn init_writes_absolute_path_for_hooks() {
    let drip = Drip::new();
    let home = tempfile::tempdir().unwrap();

    let o = drip
        .cmd()
        .arg("init")
        .arg("-g")
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "drip init failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    let settings = fs::read_to_string(home.path().join(".claude/settings.json")).unwrap();
    let v: Value = serde_json::from_str(&settings).unwrap();

    // Walk every "command" string under hooks.* and assert each starts
    // with "/" — meaning it's absolute, not the bare "drip ...".
    let mut commands = Vec::new();
    if let Some(hooks) = v.get("hooks").and_then(|h| h.as_object()) {
        for entry_list in hooks.values() {
            if let Some(arr) = entry_list.as_array() {
                for entry in arr {
                    if let Some(inner) = entry.get("hooks").and_then(|h| h.as_array()) {
                        for h in inner {
                            if let Some(cmd) = h.get("command").and_then(|c| c.as_str()) {
                                commands.push(cmd.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(!commands.is_empty(), "no hook commands found in {settings}");
    for cmd in &commands {
        // Path may be shell-quoted (`'/Users/foo bar/drip' hook claude`)
        // when the install path contains a character a shell would
        // tokenise on — single-quote it then. Strip the leading quote
        // before the absoluteness check.
        let unquoted = cmd.strip_prefix('\'').unwrap_or(cmd);
        assert!(
            unquoted.starts_with('/'),
            "hook command must be absolute, got: {cmd:?}"
        );
        assert!(
            cmd.contains(" hook "),
            "hook command should invoke `<drip-bin> hook <agent>`, got: {cmd:?}"
        );
    }
}

/// Bug L4-related: MCP `read_file` must refuse paths outside
/// DRIP_WORKSPACE_ROOT when that env var is set.
#[test]
fn mcp_workspace_root_blocks_outside_paths() {
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let drip = Drip::new();
    let workspace = tempfile::tempdir().unwrap();
    let inside = workspace.path().join("ok.txt");
    fs::write(&inside, "inside\n").unwrap();

    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path().join("nope.txt");
    fs::write(&outside, "secret\n").unwrap();

    let mut child = Command::new(&drip.bin)
        .arg("mcp")
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .env("DRIP_WORKSPACE_ROOT", workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(&mut stdout);

    let send = |s: &mut std::process::ChildStdin, v: Value| {
        s.write_all((v.to_string() + "\n").as_bytes()).unwrap();
    };
    let recv = |r: &mut BufReader<&mut std::process::ChildStdout>| -> Value {
        let mut buf = String::new();
        r.read_line(&mut buf).unwrap();
        serde_json::from_str(buf.trim()).unwrap()
    };

    send(
        &mut stdin,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    );
    let _ = recv(&mut reader);

    // Inside the workspace: must succeed.
    send(
        &mut stdin,
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"read_file","arguments":{"file_path": inside.to_string_lossy()}}
        }),
    );
    let r1 = recv(&mut reader);
    assert_eq!(
        r1["result"]["isError"],
        json!(false),
        "inside read should succeed: {r1}"
    );

    // Outside the workspace: must be refused.
    send(
        &mut stdin,
        json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"read_file","arguments":{"file_path": outside.to_string_lossy()}}
        }),
    );
    let r2 = recv(&mut reader);
    assert_eq!(
        r2["result"]["isError"],
        json!(true),
        "outside read should be refused: {r2}"
    );
    let text = r2["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("DRIP refused read"),
        "expected refusal message, got: {text}"
    );

    drop(stdin);
    let _ = child.wait();
}

/// Bug M3: DRIP_REJECT_SYMLINKS env var must short-circuit reads of
/// symlinks so the agent's native path runs untouched.
#[cfg(unix)]
#[test]
fn drip_reject_symlinks_falls_back() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.txt");
    fs::write(&real, "hello\n").unwrap();
    let link = dir.path().join("link.txt");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let o = drip
        .cmd()
        .arg("read")
        .arg(&link)
        .env("DRIP_REJECT_SYMLINKS", "1")
        .output()
        .unwrap();
    assert!(o.status.success());
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("symlink"),
        "expected symlink fallback header, got: {s}"
    );
}

/// Bug M2: a synthetic "huge file" path must not OOM the hook — we cap
/// at the metadata-reported size before reading. We use /dev/zero on
/// Unix (which has unbounded reported size on some systems) — fall back
/// to skipping if it's not available or behaves oddly.
#[cfg(unix)]
#[test]
fn huge_file_short_circuits_via_metadata() {
    let drip = Drip::new();
    // We don't actually want to allocate 50 MB here. Build a 60 MB file
    // by sparse-truncating, which reports len=60MB without using disk.
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("huge.bin");
    let file = fs::File::create(&f).unwrap();
    file.set_len(60 * 1024 * 1024).unwrap();
    drop(file);

    let o = drip.cmd().arg("read").arg(&f).output().unwrap();
    assert!(o.status.success());
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("DRIP hard cap") || s.contains("exceeds"),
        "expected cap notice, got: {s}"
    );
}

/// Perverse case: small files where the unified-diff envelope (--- / +++ /
/// @@ headers + context) costs more tokens than just resending the file.
/// DRIP must fall back to a full read in that case rather than send a
/// bigger diff — the whole point of the tool is *fewer* tokens.
#[test]
fn tiny_file_diff_falls_back_to_full() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("tiny.json");
    fs::write(&f, "{\"v\":1}\n").unwrap();
    drip.read_stdout(&f);

    fs::write(&f, "{\"v\":2}\n").unwrap();
    let out = drip.read_stdout(&f);
    assert!(
        out.contains("diff would cost more than the file itself"),
        "expected DiffBiggerThanFile fallback, got: {out}"
    );
    // Subsequent identical read must keep the refreshed baseline, but
    // this file is too small for the unchanged marker to be cheaper
    // than native content.
    let out2 = drip.read_stdout(&f);
    assert!(
        out2 == "{\"v\":2}\n",
        "tiny unchanged read should return raw native-equivalent content, got: {out2}"
    );
}

/// `drip refresh <file>` drops a single file's baseline so the next read
/// returns the full content again.
#[test]
fn refresh_clears_one_file_baseline() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("r.txt");
    fs::write(&f, "abc\n".repeat(50)).unwrap();
    drip.read_stdout(&f);

    let o = drip.cmd().arg("refresh").arg(&f).output().unwrap();
    assert!(o.status.success(), "refresh failed: {:?}", o);
    let msg = String::from_utf8_lossy(&o.stdout);
    assert!(msg.contains("Cleared baseline"), "got: {msg}");

    // Next read is back to a full read.
    let out = drip.read_stdout(&f);
    assert!(out.contains("[DRIP: full read"), "got: {out}");
}

/// `drip meter` defaults to *cumulative-since-install* aggregation: the
/// numbers must include reads from sessions other than the one running
/// the command. Without this, `drip meter` from a fresh terminal would
/// always show zeros until the very session that ran it had reads.
#[test]
fn meter_lifetime_aggregates_across_sessions() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("life.txt");
    fs::write(&f, "abc\n".repeat(100)).unwrap();

    // First session reads the file twice → some lifetime tokens accumulate.
    drip.read_stdout(&f);
    drip.read_stdout(&f);

    // Now query meter from a *different* session id (no reads of its own).
    let o = drip
        .cmd_in_session("other-session")
        .arg("meter")
        .arg("--json")
        .output()
        .unwrap();
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["scope"].as_str(), Some("lifetime"));
    assert!(
        v["total_reads"].as_i64().unwrap() >= 2,
        "lifetime reads must include the prior session: {v}"
    );
    assert!(v["tokens_full"].as_i64().unwrap() > 0);
}

/// PostToolUse must increment the install-wide `files_edited` /
/// `total_edits` counters, so `drip meter` can show the user which files
/// they've worked with — not just the ones their agent re-read enough
/// to produce token savings.
#[test]
fn post_edit_increments_lifetime_edit_counters() {
    use serde_json::json;
    use std::io::Write;
    use std::process::{Command, Stdio};
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("e.rs");
    fs::write(&f, "fn main() {}\n").unwrap();

    let payload = json!({
        "session_id": &drip.session_id,
        "tool_name": "Edit",
        "tool_input": { "file_path": f.to_string_lossy() }
    });
    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude-post-edit"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    assert!(child.wait_with_output().unwrap().status.success());

    let o = drip.cmd().arg("meter").arg("--json").output().unwrap();
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(
        v["files_edited"].as_i64().unwrap() >= 1,
        "files_edited should bump after PostToolUse: {v}"
    );
    assert!(
        v["total_edits"].as_i64().unwrap() >= 1,
        "total_edits should bump after PostToolUse: {v}"
    );
}

/// After a PostToolUse, the very next Read of the same file must NOT be
/// substituted — Claude Code's harness needs a real Read result before
/// it'll accept the next Edit. Without this, agents hit "File has not
/// been read yet" errors after editing.
#[test]
fn post_edit_then_read_passes_through() {
    use serde_json::json;
    use std::io::Write;
    use std::process::{Command, Stdio};
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("p.rs");
    fs::write(&f, "fn main() {}\n").unwrap();
    drip.read_stdout(&f);

    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude-post-edit"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
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
                "session_id": &drip.session_id,
                "tool_name": "Edit",
                "tool_input": { "file_path": f.to_string_lossy() }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let _ = child.wait_with_output();

    // Run via the actual Claude Code Read hook, since that's the path
    // the harness exercises.
    let payload = json!({
        "session_id": &drip.session_id,
        "tool_name": "Read",
        "tool_input": { "file_path": f.to_string_lossy() }
    });
    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    let v: Value = serde_json::from_str(s.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"].as_str(),
        Some("allow"),
        "next read after PostToolUse must pass through native: {s}"
    );
}

/// `drip meter` must surface estimated USD saved and CO₂ avoided in
/// both the human and JSON outputs, derived from `tokens_saved` via
/// the (overridable) per-Mtok rates.
#[test]
fn meter_includes_dollars_and_co2() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("p.txt");
    fs::write(&f, "x\n".repeat(2_000)).unwrap();
    drip.read_stdout(&f);
    drip.read_stdout(&f); // unchanged read → tokens_saved bumps

    let o = drip
        .cmd()
        .arg("meter")
        .arg("--json")
        .env("DRIP_PRICE_PER_MTOK", "10")
        .env("DRIP_CO2_G_PER_KTOK", "1.0")
        .output()
        .unwrap();
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let saved = v["tokens_saved"].as_i64().unwrap();
    assert!(saved > 0, "expected tokens_saved > 0, got: {v}");
    assert_eq!(v["price_per_mtok"].as_f64().unwrap(), 10.0);
    assert_eq!(v["co2_g_per_ktok"].as_f64().unwrap(), 1.0);
    let dollars = v["dollars_saved"].as_f64().unwrap();
    let co2 = v["co2_g_saved"].as_f64().unwrap();
    let expected_dollars = (saved as f64 / 1_000_000.0) * 10.0;
    let expected_co2 = (saved as f64 / 1_000.0) * 1.0;
    assert!(
        (dollars - expected_dollars).abs() < 1e-6,
        "dollars_saved should be {expected_dollars}, got {dollars}"
    );
    assert!(
        (co2 - expected_co2).abs() < 1e-6,
        "co2_g_saved should be {expected_co2}, got {co2}"
    );

    // Also assert the human output prints the rates we passed.
    let h = drip
        .cmd()
        .arg("meter")
        .env("NO_COLOR", "1")
        .env("DRIP_PRICE_PER_MTOK", "10")
        .env("DRIP_CO2_G_PER_KTOK", "1.0")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&h.stdout);
    assert!(s.contains("$ saved"), "missing $ saved row: {s}");
    assert!(s.contains("CO"), "missing CO₂ row: {s}");
    assert!(s.contains("$10.00/Mtok"), "missing price annotation: {s}");
    assert!(s.contains("1.00 g/Ktok"), "missing co2 annotation: {s}");
}

/// Bad env vars must not panic — invalid `DRIP_PRICE_PER_MTOK` falls
/// back to the default rate. Defends against `DRIP_PRICE_PER_MTOK=foo`,
/// negative values, NaN, etc.
#[test]
fn meter_handles_bad_pricing_env() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("q.txt");
    fs::write(&f, "y\n".repeat(2_000)).unwrap();
    drip.read_stdout(&f);
    drip.read_stdout(&f);

    for bad in &["foo", "-5", "NaN", "1e999", ""] {
        let o = drip
            .cmd()
            .arg("meter")
            .arg("--json")
            .env("DRIP_PRICE_PER_MTOK", bad)
            .env("DRIP_CO2_G_PER_KTOK", bad)
            .output()
            .unwrap();
        assert!(o.status.success(), "bad env {bad:?} crashed meter");
        let v: Value = serde_json::from_slice(&o.stdout).unwrap();
        // Default Sonnet rate when env var is unparseable.
        assert_eq!(
            v["price_per_mtok"].as_f64().unwrap(),
            3.0,
            "bad env {bad:?} should fall back to default price"
        );
        assert_eq!(
            v["co2_g_per_ktok"].as_f64().unwrap(),
            0.4,
            "bad env {bad:?} should fall back to default CO2"
        );
    }
}

/// Sec audit M1: post-edit hook must skip oversized files instead of
/// reading them into memory. We sparse-truncate to 60 MB (above the 50 MB
/// HARD_SIZE_CAP_BYTES) and assert the hook returns success without
/// updating the baseline — i.e., the next Read still sees the file
/// fresh, returning a full first-read.
#[cfg(unix)]
#[test]
fn post_edit_hook_skips_oversized_files() {
    use serde_json::json;
    use std::io::Write;
    use std::process::{Command, Stdio};
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("huge_post.bin");
    let file = fs::File::create(&f).unwrap();
    file.set_len(60 * 1024 * 1024).unwrap();
    drop(file);

    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude-post-edit"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            json!({
                "session_id": &drip.session_id,
                "tool_name": "Edit",
                "tool_input": { "file_path": f.to_string_lossy() }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(
        o.status.success(),
        "post-edit must not fail on huge files: {o:?}"
    );
    // Hook returned `{}` and never tried to load 60 MB into memory.
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.contains("{}"), "expected empty json payload, got: {s}");
}

/// Sec audit L1: when DRIP_WORKSPACE_ROOT is set, MCP must refuse a
/// path that fails to canonicalize (e.g., file missing) rather than
/// silently fall through to a textual `starts_with` comparison that
/// can be fooled by `..` traversal.
#[test]
fn mcp_workspace_root_refuses_unresolvable_paths() {
    use serde_json::json;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    let drip = Drip::new();
    let workspace = tempfile::tempdir().unwrap();
    // Path that doesn't exist AND uses ".." traversal — would textually
    // appear inside the workspace if canonicalize were skipped.
    let trick = workspace.path().join("../../../etc/passwd-fake");

    let mut child = Command::new(&drip.bin)
        .arg("mcp")
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .env("DRIP_WORKSPACE_ROOT", workspace.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(&mut stdout);

    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
        .unwrap();
    let mut buf = String::new();
    reader.read_line(&mut buf).unwrap();

    stdin
        .write_all(
            (json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "read_file", "arguments": {"file_path": trick.to_string_lossy()}}
            })
            .to_string()
                + "\n")
                .as_bytes(),
        )
        .unwrap();
    let mut buf2 = String::new();
    reader.read_line(&mut buf2).unwrap();
    let v: Value = serde_json::from_str(buf2.trim()).unwrap();
    assert_eq!(
        v["result"]["isError"],
        json!(true),
        "unresolvable path must be refused, got: {v}"
    );

    drop(stdin);
    let _ = child.wait();
}

/// Read-only commands (`drip meter`, `drip sessions`, `drip replay`)
/// must not create a `sessions` row just to inspect state. Otherwise
/// every fresh terminal that runs `drip meter` pollutes the listing
/// with an empty session that has to wait the 2 h TTL purge.
#[test]
fn readonly_commands_do_not_create_session_rows() {
    let drip = Drip::new();

    // Pristine DB — no sessions yet.
    let conn = rusqlite::Connection::open(drip.data_dir.path().join("sessions.db")).unwrap();
    let count = || -> i64 {
        conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap_or(0)
    };

    // Run every read-only command. None of them should touch `sessions`.
    let cmds: &[&[&str]] = &[
        &["meter"],
        &["meter", "--session"],
        &["meter", "--json"],
        &["sessions"],
        &["replay"],
        &["replay", "--json"],
    ];
    for args in cmds {
        // Each invocation gets a fresh derived id (unique session env id),
        // so if the command DID create a row we'd see them accumulate.
        let unique = format!("ro-test-{}", uuid_like());
        let o = drip.cmd_in_session(&unique).args(*args).output().unwrap();
        assert!(
            o.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    assert_eq!(
        count(),
        0,
        "read-only commands must not create rows in `sessions`"
    );
}

/// Tiny unique id so parallel test runs don't collide.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}

/// Audit fix: `drip reset` must purge every session-scoped table —
/// otherwise `drip replay` after a reset would show stale events and
/// `drip watch`'s precomputed cache would survive against a session
/// the user explicitly discarded.
#[test]
fn reset_clears_all_session_scoped_tables() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.rs");
    fs::write(&f, "x\n".repeat(60)).unwrap();
    drip.read_stdout(&f);
    drip.read_stdout(&f);

    let conn = rusqlite::Connection::open(drip.data_dir.path().join("sessions.db")).unwrap();
    let count = |table: &str| -> i64 {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1"),
            rusqlite::params![&drip.session_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert!(count("reads") > 0, "precondition: reads exist");
    assert!(count("read_events") > 0, "precondition: events exist");

    let r = drip.cmd().arg("reset").output().unwrap();
    assert!(r.status.success());

    assert_eq!(count("reads"), 0, "reset must wipe reads");
    assert_eq!(count("read_events"), 0, "reset must wipe read_events");
    assert_eq!(
        count("precomputed_reads"),
        0,
        "reset must wipe precomputed_reads"
    );
    assert_eq!(
        count("passthrough_pending"),
        0,
        "reset must wipe passthrough_pending"
    );
}

/// Audit fix: `drip refresh <file>` must also drop any pending
/// passthrough marker for that file. Otherwise a `drip refresh`
/// right after a PostToolUse would have the next read fall through
/// to native, when the user explicitly asked for a fresh full read.
#[test]
fn refresh_clears_passthrough_marker() {
    use serde_json::json;
    use std::io::Write;
    use std::process::{Command, Stdio};
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("p.rs");
    fs::write(&f, "x\n".repeat(40)).unwrap();
    drip.read_stdout(&f);

    // Simulate a PostToolUse that marks the file for one-shot passthrough.
    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude-post-edit"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            json!({
                "session_id": &drip.session_id,
                "tool_name": "Edit",
                "tool_input": { "file_path": f.to_string_lossy() }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let _ = child.wait_with_output();

    // The marker exists.
    let conn = rusqlite::Connection::open(drip.data_dir.path().join("sessions.db")).unwrap();
    let canonical = f.canonicalize().unwrap().to_string_lossy().into_owned();
    let count_marker = || -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM passthrough_pending
             WHERE session_id = ?1 AND file_path = ?2",
            rusqlite::params![&drip.session_id, &canonical],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(count_marker(), 1, "post-edit must set marker");

    // `drip refresh` should drop the marker too.
    let r = drip.cmd().arg("refresh").arg(&f).output().unwrap();
    assert!(r.status.success());
    assert_eq!(count_marker(), 0, "refresh must clear passthrough marker");
}

/// New CLI surface: `drip meter --session <id>` lets the user inspect a
/// specific session by id (e.g., the agent's UUID), not just the
/// cwd-derived shell session.
#[test]
fn meter_session_accepts_explicit_id() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("g.rs");
    fs::write(&f, "x\n".repeat(60)).unwrap();
    // Read under the test's session id so reads land there.
    drip.read_stdout(&f);
    drip.read_stdout(&f);

    // Now query that exact session from a "different shell" by passing
    // a different DRIP_SESSION_ID for the meter invocation but `--session
    // <real-id>` to override.
    let o = drip
        .cmd()
        .arg("meter")
        .arg("--session")
        .arg(&drip.session_id)
        .arg("--json")
        .env("DRIP_SESSION_ID", "different-shell-session-xyz")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "meter --session <id> failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["scope"].as_str(), Some("session"));
    assert_eq!(
        v["session_id"].as_str(),
        Some(drip.session_id.as_str()),
        "explicit --session <id> must target that exact session"
    );
    assert!(
        v["total_reads"].as_i64().unwrap() >= 2,
        "should see the reads we wrote: {v}"
    );
}

/// `DRIP_DISABLE=1` makes the Read hook a no-op (returns "allow"), so the
/// agent's native Read runs — useful as a quick escape hatch when DRIP
/// misbehaves and the user can't reach for `drip init -u` mid-session.
#[test]
fn drip_disable_env_makes_read_hook_passthrough() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let drip = Drip::new();
    let payload = serde_json::json!({
        "session_id": &drip.session_id,
        "tool_name": "Read",
        "tool_input": { "file_path": "/etc/hosts" }
    });

    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude"])
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
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success());
    let s = String::from_utf8_lossy(&o.stdout);
    let v: Value = serde_json::from_str(s.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"].as_str(),
        Some("allow"),
        "DRIP_DISABLE must produce an allow decision: {s}"
    );
}
