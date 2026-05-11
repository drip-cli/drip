//! Agent-UX improvements (P1/P2/P3 from the live-agent audit).
//!
//! These three were surfaced by an agent actually USING DRIP — not by
//! looking at code — and the regressions would be invisible to unit
//! tests that exercise individual entry points. Pin them as
//! integration tests so the cross-command behavior stays stable.
//!
//! - **P1**: `drip meter` should surface the count of reads where the
//!   file changed under DRIP (cargo fmt, git pull, …) so users on
//!   actively-edited repos understand why their `%` reduction is
//!   lower than the headline marketing numbers.
//! - **P2**: `drip meter --session` (bare) should auto-pick the
//!   agent's session in the cwd instead of pointing at the empty
//!   shell session.
//! - **P3**: `drip hook <unknown-subcommand>` (stale settings.json
//!   after a binary upgrade) must NOT crash the tool call — emit
//!   `{}` and exit 0 so the agent's tool fires natively.

use crate::common::Drip;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn run_claude_read_hook(drip: &Drip, file_path: &str) -> Value {
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
        "read hook failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    serde_json::from_slice(&o.stdout).expect("hook stdout must be JSON")
}

// ── P1 ───────────────────────────────────────────────────────────

#[test]
fn p1_meter_counts_oob_refresh_after_external_change() {
    // Seed the baseline via the Claude hook (native passthrough on
    // first read), change the file out-of-band, then re-read via the
    // Claude hook again — the tracker should detect the change,
    // refresh the baseline, ship native, and bump the persistent
    // OOB-refresh counter. Meter JSON must surface it.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("oob.txt");
    let baseline =
        "first revision with enough payload to keep the diff above sentinel size\n".repeat(20);
    fs::write(&f, &baseline).unwrap();

    let path = f.to_string_lossy().to_string();
    let r1 = run_claude_read_hook(&drip, &path);
    assert_eq!(
        r1["hookSpecificOutput"]["permissionDecision"], "allow",
        "first read: native passthrough seeds the baseline"
    );

    // Simulate an out-of-band edit — Edit/Write hook does NOT fire,
    // so DRIP only notices the change on the next Read.
    let after = "second revision rewritten outside the Edit tool to mimic cargo fmt\n".repeat(20);
    fs::write(&f, &after).unwrap();

    let r2 = run_claude_read_hook(&drip, &path);
    assert_eq!(
        r2["hookSpecificOutput"]["permissionDecision"], "allow",
        "second read: ExternalChange falls through to native to keep Claude's tracker in sync"
    );
    // P1 follow-up (agent-UX audit round 2): allow must NOT be muted —
    // attach `additionalContext` so the agent's next turn explicitly
    // knows DRIP refreshed because the file changed out-of-band. Without
    // this notice, the full re-read looks indistinguishable from a
    // first read and the agent has no signal that DRIP "did something".
    let ctx = r2["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_else(|| panic!("ExternalChange allow must carry additionalContext: {r2}"));
    assert!(
        ctx.contains("native refresh"),
        "additionalContext must label this a `native refresh`: {ctx}"
    );
    assert!(
        ctx.contains("out-of-band") || ctx.contains("changed"),
        "additionalContext must hint at the cause: {ctx}"
    );
    assert!(
        ctx.contains(&path),
        "additionalContext must name the file so the agent can correlate: {ctx}"
    );

    // First read stays muted: a notice on every first read would be
    // pure noise, and Claude already gets the file body via native.
    assert!(
        r1["hookSpecificOutput"].get("additionalContext").is_none(),
        "first-read allow must stay quiet (no notice): {r1}"
    );

    // JSON: external_edit_refreshes must surface a count of 1.
    let o = drip
        .cmd()
        .args(["meter", "--json"])
        .output()
        .expect("meter --json");
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).expect("valid JSON");
    let stats = v
        .get("external_edit_refreshes")
        .unwrap_or_else(|| panic!("meter JSON must include external_edit_refreshes: {v}"));
    assert_eq!(
        stats["count"].as_i64(),
        Some(1),
        "exactly one OOB refresh happened: {stats}"
    );
    assert!(
        stats["pct_of_reads"].as_u64().unwrap_or(0) > 0,
        "pct must be > 0 when at least one OOB refresh occurred: {stats}"
    );

    // Human surface: line should be visible and explain the cause.
    let human = String::from_utf8_lossy(
        &drip
            .cmd()
            .arg("meter")
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    assert!(
        human.contains("Native refresh:"),
        "human meter must surface the OOB-refresh row: {human}"
    );
    assert!(
        human.contains("file changed since last read") || human.contains("re-shipped"),
        "human row must hint at WHY: {human}"
    );
}

#[test]
fn p1_meter_omits_oob_line_when_no_refreshes_yet() {
    // Negative case: a fresh install with no OOB events must NOT
    // emit the "Native refresh:" line — three zeroed metrics would
    // be noise on a freshly-init'd setup.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("normal.txt");
    fs::write(&f, "stable content body\n".repeat(40)).unwrap();
    drip.read_stdout(&f);
    drip.read_stdout(&f); // unchanged sentinel — no OOB event

    let o = drip
        .cmd()
        .args(["meter", "--json"])
        .output()
        .expect("meter --json");
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(
        v.get("external_edit_refreshes").is_none(),
        "external_edit_refreshes must be absent when count is 0: {v}"
    );

    let human = String::from_utf8_lossy(
        &drip
            .cmd()
            .arg("meter")
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    assert!(
        !human.contains("Native refresh:"),
        "no row when zero events: {human}"
    );
}

// ── P2 ───────────────────────────────────────────────────────────

#[test]
fn p2_meter_session_auto_picks_active_session_in_cwd() {
    // Two sessions share the same data dir and the same cwd:
    //   - `agent-sess` seeds 2 reads against a real file
    //   - `shell-sess` does nothing (mimics the user typing in a
    //     terminal where no DRIP traffic has happened)
    //
    // When the user runs `drip meter --session` from `shell-sess`,
    // bare-flagged, DRIP should auto-pick `agent-sess` because it's
    // the most-recently-active session in this cwd with reads.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("file.txt");
    let body = "agent payload payload payload\n".repeat(40);
    fs::write(&f, &body).unwrap();

    // Agent session does 2 reads — second one is the unchanged
    // sentinel, so the session has real savings data.
    let agent_sid = "agent-sess";
    let cwd = dir.path();
    let mut agent_cmd = drip.cmd_in_session(agent_sid);
    agent_cmd.arg("read").arg(&f).current_dir(cwd);
    let r = agent_cmd.output().unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let mut agent_cmd2 = drip.cmd_in_session(agent_sid);
    agent_cmd2.arg("read").arg(&f).current_dir(cwd);
    let r2 = agent_cmd2.output().unwrap();
    assert!(r2.status.success());

    // Shell session does nothing — just queries the meter, bare
    // `--session` flag. Run from the same cwd so the auto-pick can
    // find the agent session.
    let shell_sid = "shell-sess";
    let o = drip
        .cmd_in_session(shell_sid)
        .args(["meter", "--session", "--json"])
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();

    // The picked session id must match the agent's, not the shell.
    assert_eq!(
        v["session_id"].as_str(),
        Some(agent_sid),
        "auto-pick should target the agent's session: {v}"
    );
    let picked = v
        .get("auto_picked_session")
        .unwrap_or_else(|| panic!("auto_picked_session must be set: {v}"));
    assert_eq!(
        picked["session_id"].as_str(),
        Some(agent_sid),
        "auto_picked_session.session_id must match: {picked}"
    );

    // Human surface emits the ℹ notice so the user knows the swap happened.
    let human = String::from_utf8_lossy(
        &drip
            .cmd_in_session(shell_sid)
            .args(["meter", "--session"])
            .current_dir(cwd)
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    assert!(
        human.contains("Auto-picked session"),
        "ℹ notice must mention the auto-pick: {human}"
    );
}

#[test]
fn p2_meter_session_keeps_derived_when_it_has_reads() {
    // Don't substitute when the derived session is genuinely the
    // right one (e.g., the user IS the agent — pinned
    // DRIP_SESSION_ID matches the session that holds the reads).
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("file.txt");
    fs::write(&f, "self-call payload\n".repeat(40)).unwrap();
    drip.read_stdout(&f);
    drip.read_stdout(&f);

    let o = drip
        .cmd()
        .args(["meter", "--session", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(
        v["session_id"].as_str(),
        Some(drip.session_id.as_str()),
        "derived session has reads → no swap: {v}"
    );
    assert!(
        v.get("auto_picked_session").is_none(),
        "auto_picked_session must be absent when derived was kept: {v}"
    );
}

// ── P3 ───────────────────────────────────────────────────────────

#[test]
fn p3_hook_unknown_subcommand_returns_empty_object() {
    // Stale settings.json after a binary upgrade — a removed agent
    // like `claude-bash` no longer parses. The hook must NOT crash;
    // it must drain stdin, emit `{}` to stdout, and exit 0 so the
    // agent's tool call proceeds natively.
    let drip = Drip::new();
    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude-bash"]) // historically valid, removed in current build
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn stale hook");
    let payload = json!({
        "tool_name": "Bash",
        "tool_input": { "command": "echo hello" }
    });
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();

    assert!(
        o.status.success(),
        "stale hook must exit 0, got: {:?} stderr={}",
        o.status,
        String::from_utf8_lossy(&o.stderr)
    );
    let stdout = String::from_utf8_lossy(&o.stdout).trim().to_string();
    assert_eq!(
        stdout, "{}",
        "must emit `{{}}` so the tool call proceeds natively, got: {stdout:?}"
    );

    // Stderr should mention the situation so a curious user can spot it.
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("ignoring stale hook subcommand"),
        "stderr should explain the no-op so debug logs are useful: {stderr}"
    );
}

#[test]
fn p3_hook_known_subcommand_still_works() {
    // Sanity guard: the tolerance shim must NOT swallow legitimate
    // calls. `drip hook claude` is still the canonical Read hook.
    let drip = Drip::new();
    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn claude hook");
    // Empty payload — hook should handle gracefully.
    child.stdin.as_mut().unwrap().write_all(b"{}").unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(
        o.status.success(),
        "known hook must work: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
}

// ── round 2: opt-in first-read compression (DRIP_COMPRESS_FIRST_READ_MIN_BYTES) ─

#[test]
fn compress_first_read_disabled_by_default_keeps_native_passthrough() {
    // Sanity: the *default* behavior is unchanged — a sub-budget code
    // file on its first read returns `allow` so Claude's native Read
    // populates the read-before-edit tracker. Tests that don't set
    // `DRIP_COMPRESS_FIRST_READ_MIN_BYTES` must see the historical
    // shape, otherwise we'd silently break the read-before-edit
    // invariant for everyone.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("svc.ts");
    fs::write(&f, big_compressable_ts(5)).unwrap();
    let path = f.to_string_lossy().to_string();

    let r = run_claude_read_hook(&drip, &path);
    assert_eq!(
        r["hookSpecificOutput"]["permissionDecision"], "allow",
        "default route on first read of a code file MUST be allow → native: {r}"
    );
    assert!(
        r["hookSpecificOutput"]
            .get("permissionDecisionReason")
            .is_none(),
        "no DRIP-substitute payload when disabled: {r}"
    );
}

#[test]
fn compress_first_read_opt_in_substitutes_when_savings_are_meaningful() {
    // With the opt-in env var on, a code file with elidable bodies must
    // be substituted via `deny` on its first read — the payload is the
    // semantically-compressed view, not a stream of full content. We
    // assert the *deny* route (not the *exact* token count) because
    // compression ratios drift across language refactors but the route
    // is the load-bearing invariant.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("big.ts");
    let body = big_compressable_ts(5);
    fs::write(&f, &body).unwrap();
    let path = f.to_string_lossy().to_string();

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Read",
        "tool_input": { "file_path": &path }
    });
    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        // Pin compression body floor so the fixture is deterministic.
        .env("DRIP_COMPRESS_MIN_BODY", "4")
        // The whole point: opt in to early compression for files
        // 1 KB and up. Our fixture is ~2.5 KB.
        .env("DRIP_COMPRESS_FIRST_READ_MIN_BYTES", "1024")
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
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).expect("hook stdout must be JSON");

    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"], "deny",
        "opt-in early compress on a compressable file MUST take the deny-substitute route: {v}"
    );
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap_or_else(|| panic!("deny route must carry the rendered body: {v}"));
    assert!(
        reason.contains("semantic-compressed") || reason.contains("DRIP-elided"),
        "rendered body must show compression markers: {reason}"
    );
    assert!(
        reason.contains(&path),
        "rendered header must name the file: {reason}"
    );
}

#[test]
fn compress_first_read_opt_in_falls_back_to_native_when_no_savings() {
    // Non-compressable content (plain text, no elidable function
    // bodies) under the opt-in path must still fall through to `allow`
    // — the contract is "substitute when worthwhile, else stay out of
    // the way". A user enabling early-compress on a markdown-heavy
    // tree shouldn't have those reads silently re-routed for no win.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("notes.txt");
    // 2+ KB of plain prose — no semantic structure to compress.
    let prose = "Plain prose paragraph without any structure.\n".repeat(80);
    fs::write(&f, &prose).unwrap();
    let path = f.to_string_lossy().to_string();

    let payload = json!({
        "session_id": drip.session_id,
        "tool_name": "Read",
        "tool_input": { "file_path": &path }
    });
    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .env("DRIP_COMPRESS_MIN_BODY", "4")
        .env("DRIP_COMPRESS_FIRST_READ_MIN_BYTES", "1024")
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
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();

    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"], "allow",
        "uncompressable file under opt-in must still fall through to native: {v}"
    );
}

fn big_compressable_ts(method_count: usize) -> String {
    // Body length ≥ 20 lines triggers compression at
    // DRIP_COMPRESS_MIN_BODY=4 (test default). File grows past 1 KB
    // quickly so the size gate fires.
    let mut s = String::new();
    s.push_str("export class BigService {\n");
    for i in 0..method_count {
        s.push_str(&format!("  /** Method {i} */\n"));
        s.push_str(&format!(
            "  async method{i}(x: number): Promise<string> {{\n"
        ));
        for j in 0..20 {
            s.push_str(&format!("    const v{j} = x + {j};\n"));
        }
        s.push_str(&format!("    return `done {i}`;\n"));
        s.push_str("  }\n\n");
    }
    s.push_str("}\n");
    s
}

// ── round 2: `drip sessions list` / `ls` aliases ─────────────────

#[test]
fn sessions_list_and_ls_are_no_op_aliases_of_sessions() {
    // I tried `drip sessions list`, got `error: unexpected argument
    // 'list' found`. The bare `drip sessions` is the right shape, but
    // muscle memory from other CLIs (`docker ps`, `git branch -l`,
    // `kubectl get pods` after a `list`-prefixed alias) makes the typo
    // common. Accept `list` and `ls` as no-op aliases that produce the
    // same table as the bare form.
    let drip = Drip::new();

    let bare = drip.cmd().args(["sessions"]).output().unwrap();
    let list = drip.cmd().args(["sessions", "list"]).output().unwrap();
    let ls = drip.cmd().args(["sessions", "ls"]).output().unwrap();

    assert!(bare.status.success(), "bare `drip sessions` must succeed");
    assert!(list.status.success(), "`drip sessions list` must succeed");
    assert!(ls.status.success(), "`drip sessions ls` must succeed");

    // Output is read directly from the same DB so bare == list == ls
    // assuming nothing wrote between calls. We don't compare byte-for-
    // byte because age-formatting renders `Xs` ticks; assert headers
    // and trailing-line counts instead.
    let so = String::from_utf8_lossy(&bare.stdout);
    let lo = String::from_utf8_lossy(&list.stdout);
    let xo = String::from_utf8_lossy(&ls.stdout);
    for s in [&so, &lo, &xo] {
        assert!(
            s.contains("SESSION") && s.contains("AGENT"),
            "all three forms must render the table header: got {s}"
        );
    }
    // Same number of rows means same data path.
    assert_eq!(
        so.lines().count(),
        lo.lines().count(),
        "row count parity: bare vs list"
    );
    assert_eq!(
        so.lines().count(),
        xo.lines().count(),
        "row count parity: bare vs ls"
    );

    // A bogus subcommand value must still be rejected — the alias is
    // an explicit allowlist, not a swallow-everything trailing arg.
    let bogus = drip.cmd().args(["sessions", "bogus"]).output().unwrap();
    assert!(
        !bogus.status.success(),
        "`drip sessions bogus` must NOT succeed: stdout={}",
        String::from_utf8_lossy(&bogus.stdout)
    );
}

#[test]
fn p3_honest_typos_still_error() {
    // The shim must not swallow user mistakes outside the hook path.
    // `drip met` (typo of `drip meter`) should still hit clap's
    // normal error path so the user gets a `did you mean meter?`
    // suggestion.
    let drip = Drip::new();
    let o = drip
        .cmd()
        .arg("met") // typo, not a real subcommand
        .output()
        .unwrap();
    assert!(!o.status.success(), "honest typo must fail");
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stderr.contains("unrecognized") || stderr.contains("invalid"),
        "user-facing error expected: {stderr}"
    );
}

// ── round 3: replay + source-map share meter's auto-pick ─────────

#[test]
fn replay_auto_picks_agent_session_in_cwd() {
    // Same setup as p2_meter_session_auto_picks_active_session_in_cwd:
    // agent-sess seeds reads, shell-sess runs the inspection command.
    // Pre-round-3 `drip replay` would surface a globally most-recent
    // session — when running tests in parallel that returned events
    // from a different test's session. Now it should target the
    // agent session in cwd.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("file.txt");
    fs::write(&f, "agent payload payload payload\n".repeat(40)).unwrap();

    let agent_sid = format!("agent-{}", std::process::id());
    let mut c1 = drip.cmd_in_session(&agent_sid);
    c1.arg("read").arg(&f).current_dir(dir.path());
    assert!(c1.output().unwrap().status.success());
    let mut c2 = drip.cmd_in_session(&agent_sid);
    c2.arg("read").arg(&f).current_dir(dir.path());
    assert!(c2.output().unwrap().status.success());

    let shell_sid = format!("shell-{}", std::process::id());
    let o = drip
        .cmd_in_session(&shell_sid)
        .args(["replay", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(
        v["session_id"].as_str(),
        Some(agent_sid.as_str()),
        "replay must auto-pick the agent's session in cwd: {v}"
    );
    let event_count = v["event_count"].as_i64().unwrap_or(0);
    assert!(
        event_count >= 1,
        "replay must surface the agent's events (got {event_count}): {v}"
    );
}

#[test]
fn refresh_clears_baselines_across_every_session() {
    // Live agentic friction: the user runs `drip refresh foo.py` in
    // their shell session after a `git pull`, but the baseline lives
    // in the agent's session (different session_id). Pre-round-3,
    // refresh only touched the caller's session — message said
    // "No baseline tracked", agent's next read diffed against a
    // stale snapshot. Now refresh drops baselines for every session
    // holding the file.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("oob.txt");
    let body = "agent-read payload\n".repeat(30);
    fs::write(&f, &body).unwrap();

    // Two distinct sessions each seed a baseline against the file.
    let agent_a = format!("agent-a-{}", std::process::id());
    let agent_b = format!("agent-b-{}", std::process::id());
    for sid in [&agent_a, &agent_b] {
        let mut c = drip.cmd_in_session(sid);
        c.arg("read").arg(&f).current_dir(dir.path());
        assert!(c.output().unwrap().status.success(), "seed session {sid}");
    }

    // Shell session — no read of its own — issues the refresh.
    let shell_sid = format!("shell-{}", std::process::id());
    let o = drip
        .cmd_in_session(&shell_sid)
        .arg("refresh")
        .arg(&f)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let msg = String::from_utf8_lossy(&o.stdout);
    assert!(
        msg.contains("Cleared baseline") && msg.contains("2 sessions"),
        "refresh must clear both agent sessions and surface the count: {msg}"
    );

    // Next read in either agent session must be a full read (not
    // an unchanged sentinel) — baseline is gone.
    for sid in [&agent_a, &agent_b] {
        let mut c = drip.cmd_in_session(sid);
        c.arg("read").arg(&f).current_dir(dir.path());
        let o = c.output().unwrap();
        let stdout = String::from_utf8_lossy(&o.stdout);
        assert!(
            stdout.contains("full read") || stdout.contains("first"),
            "session {sid} must see a full first read after refresh: {stdout}"
        );
    }
}

#[test]
fn source_map_auto_picks_agent_session_in_cwd() {
    // Live agentic friction: I ran `drip source-map file.py` after the
    // agent compressed it and got "no read tracked in this session" —
    // because the bare-shell command derived a different session id
    // than the agent. After round-3 the inspect helper handles this,
    // and source-map sees the agent's compressed read.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("svc.py");
    // Long Python file with one long body so compression fires.
    let mut body = String::from("def helper():\n");
    for i in 0..30 {
        body.push_str(&format!("    x_{i} = {i}\n"));
    }
    body.push_str("    return x_0\n");
    fs::write(&f, &body).unwrap();

    let agent_sid = format!("agent-sm-{}", std::process::id());
    let mut c1 = drip.cmd_in_session(&agent_sid);
    c1.arg("read").arg(&f).current_dir(dir.path());
    assert!(c1.output().unwrap().status.success());

    // Inspect from a different (shell) session.
    let shell_sid = format!("shell-sm-{}", std::process::id());
    let o = drip
        .cmd_in_session(&shell_sid)
        .args(["source-map"])
        .arg(&f)
        .current_dir(dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&o.stdout);
    // Either we got a real source map (compression fired) OR the
    // "no compression fired for this read" hint — both prove that
    // the auto-pick found the agent's session. The pre-round-3 bug
    // returned "no read tracked in this session" (wrong scope).
    assert!(
        o.status.success(),
        "source-map must exit 0: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        !stdout.contains("no read tracked in this session"),
        "auto-pick must find the agent's read; got: {stdout}"
    );
}
