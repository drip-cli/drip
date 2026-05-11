use crate::common::Drip;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn run_claude_hook(drip: &Drip, payload: Value) -> String {
    let mut child = Command::new(&drip.bin)
        .args(["hook", "claude"])
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
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success());
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn read_with_offset_passes_through() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("big.txt");
    fs::write(&f, (0..200).map(|i| format!("l{i}\n")).collect::<String>()).unwrap();

    let out = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 100,
                "limit": 50
            }
        }),
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "partial read on unknown file (no baseline) must pass native: {out}"
    );
}

#[test]
fn read_with_only_limit_also_passes_through() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("g.txt");
    fs::write(&f, "x\n".repeat(50)).unwrap();

    let out = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "limit": 10
            }
        }),
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "limit-only read on unknown file must pass native: {out}"
    );
}

#[test]
fn full_read_after_partial_read_delivers_full_then_collapses() {
    // Correctness over micro-optimization: a partial-read silent
    // baseline only proves that DRIP has seen the file — it does NOT
    // prove the agent has seen it. So the *first* full read after a
    // narrow partial passes through to native (FullFirst → allow) so
    // the agent actually gets the content, and only the *second* full
    // read collapses to unchanged.
    //
    // Edge case: if the partial read happened to cover the WHOLE file
    // (e.g., `offset=1, limit=2000` on a 3-line file), seen_ranges
    // already covers (1, total) and the next full read can safely
    // collapse. We test the narrow-window case here.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("seq.txt");
    // 50 lines so the limit=1 partial read genuinely covers only a
    // tiny slice.
    fs::write(&f, (1..=50).map(|i| format!("l{i}\n")).collect::<String>()).unwrap();

    // Partial read of a single line — passthrough to native, silent
    // baseline pinned, seen_ranges = [(1,1)] only.
    let p = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 0, "limit": 1
            }
        }),
    );
    let vp: Value = serde_json::from_str(p.trim()).unwrap();
    assert_eq!(
        vp["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "partial read still passes through: {p}"
    );

    // First full read — agent has only seen line 1 of a 50-line file,
    // so DRIP must NOT claim unchanged. Native delivers the full file
    // and writes seen_ranges = [(1, 50)].
    let r1 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );
    let v1: Value = serde_json::from_str(r1.trim()).unwrap();
    assert_eq!(
        v1["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "first full read after narrow partial must pass to native (agent hasn't seen full content yet): {r1}"
    );

    // Second full read — agent has now seen the full file, so the
    // unchanged sentinel is honest.
    let r2 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );
    let v2: Value = serde_json::from_str(r2.trim()).unwrap();
    assert_eq!(
        v2["hookSpecificOutput"]["permissionDecision"],
        json!("deny")
    );
    let reason2 = v2["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(reason2.contains("[DRIP: unchanged"));
}

#[test]
fn full_read_after_external_change_passes_through_not_delta() {
    // Regression for the "Error editing file" loop the audit ran
    // into: the agent had Read a file, then ran `cargo fmt` via
    // Bash, then tried to Edit. Claude Code's read-tracker keys on
    // the file's content_hash; after `cargo fmt` the hash drifted
    // and the harness blocks the Edit until a fresh Read fires
    // *natively* (a deny+sentinel doesn't count). DRIP must not
    // serve a Delta in that situation — instead, fall back to
    // FullFallback{ ExternalChange } which the Claude hook routes
    // to allow, letting native Read run and refresh the tracker.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("ext-change.txt");
    fs::write(
        &f,
        (1..=120).map(|i| format!("line {i}\n")).collect::<String>(),
    )
    .unwrap();

    // First Claude-hook full read seeds the baseline natively.
    let r1 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );
    let v1: Value = serde_json::from_str(r1.trim()).unwrap();
    assert_eq!(
        v1["hookSpecificOutput"]["permissionDecision"],
        json!("allow")
    );

    // Simulate `cargo fmt` (or any external editor / `git pull`)
    // rewriting the file outside DRIP's hooked tools.
    let mut text = std::fs::read_to_string(&f).unwrap();
    text = text.replace("line 60\n", "line 60 — TOUCHED EXTERNALLY\n");
    fs::write(&f, &text).unwrap();

    // The agent's next full Read must go to native — not a Delta
    // via deny that would leave the harness stuck on the old hash.
    let r2 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );
    let v2: Value = serde_json::from_str(r2.trim()).unwrap();
    assert_eq!(
        v2["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "external change must trigger native passthrough so the harness's read-tracker refreshes: {r2}"
    );

    // Subsequent reads on the now-stable content collapse to Unchanged.
    let r3 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );
    let v3: Value = serde_json::from_str(r3.trim()).unwrap();
    assert_eq!(
        v3["hookSpecificOutput"]["permissionDecision"],
        json!("deny")
    );
    let reason3 = v3["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason3.contains("[DRIP: unchanged"),
        "after the external-change passthrough, optimisation resumes: {reason3}"
    );
}

// ─── Partial-read interception (window-scoped) ──────────────────────
//
// When DRIP already has a full-file baseline, partial reads now get a
// window-scoped intercept: identical lines in the window → unchanged
// notice; drifted lines → diff scoped to the window. Baseline is
// never mutated either way.

fn make_numbered_file(path: &std::path::Path, n: usize) {
    fs::write(
        path,
        (1..=n).map(|i| format!("line {i}\n")).collect::<String>(),
    )
    .unwrap();
}

#[test]
fn partial_read_on_unchanged_baseline_returns_window_unchanged() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("known.txt");
    make_numbered_file(&f, 200);

    drip.read_stdout(&f);

    let out = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 50,
                "limit": 20
            }
        }),
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("deny"),
        "with baseline, partial read should be intercepted: {out}"
    );
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("[DRIP: unchanged (lines 50-69)"),
        "expected window-scoped unchanged header, got: {reason}"
    );
}

#[test]
fn partial_read_after_external_change_passes_through_to_refresh_harness() {
    // After a non-DRIP write to the file (here `fs::write` simulates
    // `cargo fmt`, `git pull`, or an external editor), the next Read
    // through the Claude hook must pass to native — otherwise a
    // WindowDelta returned via deny would leave Claude Code's
    // read-tracker pinned to the OLD content_hash and the very next
    // Edit would fail with `file modified since read`. This test
    // pins the post-fix behaviour: external change ⇒ allow.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("inrange.txt");
    make_numbered_file(&f, 200);
    drip.read_stdout(&f);

    let mut text = std::fs::read_to_string(&f).unwrap();
    text = text.replace("line 60\n", "line 60 — TOUCHED\n");
    fs::write(&f, &text).unwrap();

    let out = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 50,
                "limit": 20
            }
        }),
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "external change must pass through to refresh harness: {out}"
    );
}

#[test]
fn partial_read_delta_bigger_than_window_passes_through_native() {
    // Same invariant as full-file reads: if the diff costs more than
    // the requested native window, DRIP should not substitute it.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("tiny-window.txt");
    make_numbered_file(&f, 5);
    drip.read_stdout(&f);

    let mut text = std::fs::read_to_string(&f).unwrap();
    text = text.replace("line 3\n", "line 3 changed\n");
    fs::write(&f, &text).unwrap();

    let out = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 3,
                "limit": 1
            }
        }),
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "diff bigger than the requested window should pass native: {out}"
    );
}

#[test]
fn partial_read_after_external_change_outside_window_still_passes_through() {
    // Even when the requested window itself is byte-identical
    // between the old baseline and the new disk content (the
    // external change happened OUTSIDE the window), the file's full
    // content_hash has drifted. Claude Code's read-tracker keys on
    // the whole-file hash, not the window — so claiming
    // WindowUnchanged here would still leave the next Edit blocked
    // with `file modified since read`. The right move is to pass
    // through to native, refresh DRIP's baseline, and let the
    // harness pick up the new hash.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("outrange.txt");
    make_numbered_file(&f, 200);
    drip.read_stdout(&f);

    let mut text = std::fs::read_to_string(&f).unwrap();
    text = text.replace("line 5\n", "line 5 — TOUCHED OUTSIDE\n");
    fs::write(&f, &text).unwrap();

    let out = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 50,
                "limit": 20
            }
        }),
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "external change anywhere in the file must trigger passthrough: {out}"
    );
}

#[test]
fn partial_read_passthrough_accounts_window_tokens_not_file_size() {
    // The first partial read on an unknown file passes through to
    // native, but the meter must still record what the agent
    // actually received: WINDOW-sized tokens, not the full file.
    // tokens_sent == tokens_full → 0 savings on the passthrough,
    // which keeps a subsequent sentinel-collapse honest (otherwise
    // it would compute savings against a zeroed baseline counter
    // and inflate the reduction ratio).
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("accounting.txt");
    make_numbered_file(&f, 200);

    run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 50,
                "limit": 20
            }
        }),
    );

    let json = drip.cmd().args(["meter", "--json"]).output().unwrap();
    assert!(json.status.success());
    let v: Value = serde_json::from_slice(&json.stdout).unwrap();
    let tokens_full = v["tokens_full"].as_i64().unwrap();
    let tokens_sent = v["tokens_sent"].as_i64().unwrap();
    let saved = v["tokens_saved"].as_i64().unwrap();

    // make_numbered_file(200) ≈ 1.7 KB → ~430 tokens. Window of 20
    // lines ≈ ~45 tokens. So `tokens_full` must reflect the window
    // (well under 200), not the whole file (~430).
    let file_tokens = (std::fs::read_to_string(&f).unwrap().len() as i64) / 4;
    assert!(
        tokens_full > 0,
        "passthrough must show in the meter, got 0: {v}"
    );
    assert!(
        tokens_full < file_tokens,
        "passthrough must record WINDOW size, not file size \
         (tokens_full={tokens_full}, file_tokens={file_tokens}): {v}"
    );
    assert_eq!(
        tokens_full, tokens_sent,
        "passthrough claims 0 savings, so tokens_full must equal tokens_sent: {v}"
    );
    assert_eq!(saved, 0, "passthrough must claim 0 savings: {v}");
}

#[test]
fn partial_reads_track_per_window_coverage() {
    // Correctness regression: BEFORE the seen_ranges fix, a 2nd
    // partial read on a *different* window collapsed to unchanged
    // even though the agent had only ever received window 1 from
    // native Claude Code. With per-window coverage tracking, only
    // windows the agent has actually seen can be safely intercepted
    // — same/overlapping windows collapse, fresh windows pass through
    // and themselves get added to the seen set.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("seeded.txt");
    make_numbered_file(&f, 200);

    // 1st partial read: passthrough, silent baseline pinned, window
    // 50-69 added to seen_ranges.
    let r1 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 50,
                "limit": 20
            }
        }),
    );
    let v1: Value = serde_json::from_str(r1.trim()).unwrap();
    assert_eq!(
        v1["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "first partial read on unknown file must pass to native: {r1}"
    );

    // 2nd partial on the *same* window: agent has seen these lines,
    // so the unchanged sentinel is honest.
    let r2 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 50,
                "limit": 20
            }
        }),
    );
    let v2: Value = serde_json::from_str(r2.trim()).unwrap();
    assert_eq!(
        v2["hookSpecificOutput"]["permissionDecision"],
        json!("deny"),
        "second partial on the same window can collapse — agent has seen those lines: {r2}"
    );
    let reason2 = v2["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason2.contains("[DRIP: unchanged (lines 50-69)"),
        "expected window-scoped unchanged header on 2nd partial of same window: {reason2}"
    );

    // 3rd partial on a *different* window must pass through — the
    // agent has not received those lines yet. This is the bug we're
    // guarding against (DRIP used to claim unchanged for unseen content).
    let r3 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 100,
                "limit": 20
            }
        }),
    );
    let v3: Value = serde_json::from_str(r3.trim()).unwrap();
    assert_eq!(
        v3["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "different-window partial must pass through until seen_ranges covers it: {r3}"
    );

    // 4th partial on the same NEW window (100-119) must now collapse
    // — coverage extended after r3 delivered it natively.
    let r4 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 100,
                "limit": 20
            }
        }),
    );
    let v4: Value = serde_json::from_str(r4.trim()).unwrap();
    assert_eq!(
        v4["hookSpecificOutput"]["permissionDecision"],
        json!("deny"),
        "after window 100-119 was delivered natively, the next partial on it can collapse: {r4}"
    );

    // 5th partial on a window that overlaps both 50-69 and 100-119
    // partially (75-105) — straddles a gap in seen_ranges, so must
    // pass through.
    let r5 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 75,
                "limit": 30
            }
        }),
    );
    let v5: Value = serde_json::from_str(r5.trim()).unwrap();
    assert_eq!(
        v5["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "partial straddling a gap in seen_ranges must pass through: {r5}"
    );
}

#[test]
fn partial_read_without_baseline_passes_through_native() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("fresh.txt");
    make_numbered_file(&f, 100);

    let out = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 10,
                "limit": 5
            }
        }),
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "partial read on unknown file must pass through native: {out}"
    );
}

#[test]
fn partial_read_savings_appear_in_session_meter() {
    // Regression: the first cut of partial-read interception bumped
    // only the install-wide `lifetime_*` tables but left the per-row
    // `reads.tokens_full / tokens_sent` columns alone, so
    // `drip meter --session <id>` showed nothing — even though the
    // savings were real and visible in plain `drip meter`. The fix
    // bumps both, leaving content_hash / content untouched.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("session_gain.txt");
    make_numbered_file(&f, 200);

    drip.read_stdout(&f);

    // Snapshot the per-session counters from `drip meter --session`
    // before the partial read.
    let json_before = drip
        .cmd()
        .args(["meter", "--session", &drip.session_id, "--json"])
        .output()
        .unwrap();
    assert!(json_before.status.success());
    let v_before: Value = serde_json::from_slice(&json_before.stdout).unwrap();
    let saved_before = v_before["tokens_saved"].as_i64().unwrap_or(0);

    // Trigger a window-unchanged intercept.
    run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 50,
                "limit": 20
            }
        }),
    );

    let json_after = drip
        .cmd()
        .args(["meter", "--session", &drip.session_id, "--json"])
        .output()
        .unwrap();
    assert!(json_after.status.success());
    let v_after: Value = serde_json::from_slice(&json_after.stdout).unwrap();
    let saved_after = v_after["tokens_saved"].as_i64().unwrap_or(0);

    assert!(
        saved_after > saved_before,
        "session meter should reflect partial-read savings: before={saved_before}, after={saved_after}"
    );
}

#[test]
fn partial_read_on_elided_region_passes_through_native() {
    // Regression: when the FIRST delivery was a semantic-compressed
    // payload (bash `cat foo.py` via the bash hook), the agent only
    // saw stubs for elided function bodies — never the actual lines.
    // A subsequent partial Read whose window falls inside an elided
    // region must NOT claim WindowUnchanged. seen_ranges only covers
    // the visible (non-elided) original-line ranges, so this read
    // misses the coverage check and falls through to native — which
    // is exactly what the agent needs to actually see the body.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("compressed.py");

    // Build a Python file > 1KB with one short visible function and
    // one long function whose body will be elided. Long lines push
    // file size past the compression byte threshold.
    let mut src = String::new();
    for i in 0..30 {
        src.push_str(&format!("VISIBLE_{i} = {i}  # import-style preamble\n"));
    }
    src.push('\n');
    src.push_str("def short_fn(x):\n    return x + 1\n\n");
    src.push_str("def long_fn(arg):\n");
    for i in 0..40 {
        src.push_str(&format!(
            "    step_{i} = arg + {i}  # padding to ensure the file exceeds the min compression byte threshold\n"
        ));
    }
    src.push_str("    return arg\n");
    fs::write(&f, &src).unwrap();
    assert!(
        src.len() > 1024,
        "fixture must exceed compression byte threshold"
    );

    // First read via `drip read` triggers compression (file > 1KB,
    // body of long_fn > min_body_lines).
    let first = drip.read_stdout(&f);
    assert!(
        first.contains("(semantic-compressed)"),
        "fixture must trigger compression: {first}"
    );
    assert!(
        first.contains("DRIP-elided"),
        "long_fn body must be elided: {first}"
    );

    // Now a partial Read into the middle of the elided body. The
    // long function body lives between roughly L36 and L75 in the
    // ORIGINAL file (the agent has only seen a stub for that range).
    let out = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 45,
                "limit": 5
            }
        }),
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "partial read on elided region must pass to native (agent never saw these lines): {out}"
    );
}

#[test]
fn partial_read_on_visible_region_after_compression_returns_window_unchanged() {
    // Companion to the elided-region test: the FIRST few visible
    // lines of a compressed file ARE in seen_ranges, so a partial
    // Read on those lines should still be intercepted as
    // WindowUnchanged.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("compressed_visible.py");
    let mut src = String::new();
    for i in 0..30 {
        src.push_str(&format!("VISIBLE_{i} = {i}  # import-style preamble\n"));
    }
    src.push('\n');
    src.push_str("def short_fn(x):\n    return x + 1\n\n");
    src.push_str("def long_fn(arg):\n");
    for i in 0..40 {
        src.push_str(&format!(
            "    step_{i} = arg + {i}  # padding to ensure the file exceeds the min compression byte threshold\n"
        ));
    }
    src.push_str("    return arg\n");
    fs::write(&f, &src).unwrap();
    assert!(
        src.len() > 1024,
        "fixture must exceed compression byte threshold"
    );

    let first = drip.read_stdout(&f);
    assert!(
        first.contains("(semantic-compressed)"),
        "fixture must compress: {first}"
    );

    // Partial Read on the visible preamble. The window is deliberately
    // large enough that the DRIP marker is cheaper than native content.
    let out = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 1,
                "limit": 20
            }
        }),
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("deny"),
        "partial read on visible region should be intercepted: {out}"
    );
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("[DRIP: unchanged (lines 1-20)"),
        "expected window-scoped unchanged for visible-region read: {reason}"
    );
}

#[test]
fn partial_read_does_not_mutate_baseline() {
    // After a partial read, the next FULL read must still serve the
    // entire file as a delta (or unchanged) against the original
    // baseline — not against whatever slice the partial read returned.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("invariant.txt");
    make_numbered_file(&f, 100);

    drip.read_stdout(&f);

    run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": {
                "file_path": f.to_string_lossy(),
                "offset": 30, "limit": 5
            }
        }),
    );

    let r = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );
    let v: Value = serde_json::from_str(r.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("deny"),
        "full re-read should still be intercepted: {r}"
    );
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("[DRIP: unchanged since last read"),
        "baseline must point at the FULL file, got: {reason}"
    );
}

// ─── First read over Claude's 25k-token limit ────────────────────────
//
// Claude's Read tool refuses files past ~25 000 tokens with
// `File content (X tokens) exceeds maximum allowed tokens (25000)`.
// For these the default `allow → native` route delivers nothing useful,
// so DRIP substitutes the compressed view via `deny` instead — the
// agent gets the file's shape and uses partial Reads to populate the
// editor's tracker.

fn run_hook_with_env(drip: &Drip, payload: Value, extra_env: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(&drip.bin);
    cmd.args(["hook", "claude"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
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

fn long_python_module() -> String {
    let mut s = String::from("import os\nimport sys\nimport json\nimport pathlib\n\n");
    for i in 0..12 {
        s.push_str(&format!("def function_{i}(arg_a, arg_b, arg_c):\n"));
        for j in 0..20 {
            s.push_str(&format!(
                "    step_{j} = arg_a + arg_b * {j} - arg_c  # padding so the fixture exceeds the compression byte threshold\n"
            ));
        }
        s.push_str("    return step_19\n\n");
    }
    s
}

#[test]
fn first_read_over_claude_limit_substitutes_compressed_view() {
    // With the budget cranked down low, even our modest test fixture
    // crosses the threshold — exercising the cross-over path without
    // requiring a multi-megabyte file in the test suite.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("over_limit.py");
    fs::write(&f, long_python_module()).unwrap();

    let out = run_hook_with_env(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
        &[("DRIP_CLAUDE_READ_TOKEN_BUDGET", "100")],
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("deny"),
        "over-limit first read must substitute, not pass to native: {out}"
    );
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("(semantic-compressed)"),
        "substitute must carry the compressed view, got: {reason}"
    );
    assert!(
        reason.contains("DRIP-elided"),
        "compressed body must show elided-function stubs, got: {reason}"
    );
}

#[test]
fn first_read_over_limit_falls_back_to_allow_when_compression_unavailable() {
    // No compression available (DRIP_NO_COMPRESS=1) → we can't offer a
    // smaller-than-native view, so leave the native path alone and let
    // Claude surface its own size error. Substituting a half-baked
    // truncation would be worse than the native error message which
    // already points the agent at `offset`/`limit`.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("over_limit_no_compress.py");
    fs::write(&f, long_python_module()).unwrap();

    let out = run_hook_with_env(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
        &[
            ("DRIP_CLAUDE_READ_TOKEN_BUDGET", "100"),
            ("DRIP_NO_COMPRESS", "1"),
        ],
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "without a compressed view, FullFirst must keep passing native: {out}"
    );
}

#[test]
fn deleted_file_with_baseline_under_symlinked_parent_returns_deleted() {
    // Regression: on macOS `/tmp` is a symlink to `/private/tmp`, so
    // baselines are written under the canonical `/private/tmp/...`
    // path. When the file is deleted, `Path::canonicalize` fails (the
    // file no longer exists) and the previous fallback returned the
    // raw `/tmp/...` string — which never matched the stored row, so
    // `process_read` bailed with "file not found" instead of emitting
    // the `[DRIP: file deleted]` intercept. Now `canonical_key`
    // canonicalises the parent and re-appends the file name so the
    // lookup still hits.
    let drip = Drip::new();
    // tempdir() returns a `/var/folders/...` path which is itself a
    // symlink — equivalent to the `/tmp → /private/tmp` macOS quirk
    // for the purposes of this test (the canonical form differs from
    // the raw form by the same kind of prefix swap).
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("ghost.txt");
    fs::write(&f, "line 1\nline 2\nline 3\n").unwrap();

    let r1 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );
    let v1: Value = serde_json::from_str(r1.trim()).unwrap();
    assert_eq!(
        v1["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "baseline seed: {r1}"
    );

    // Delete the file via a non-DRIP path.
    std::fs::remove_file(&f).unwrap();

    let r2 = run_claude_hook(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
    );
    let v2: Value = serde_json::from_str(r2.trim()).unwrap();
    assert_eq!(
        v2["hookSpecificOutput"]["permissionDecision"],
        json!("deny"),
        "deleted-file read must substitute with the deletion sentinel: {r2}"
    );
    let reason = v2["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("[DRIP: file deleted since last read"),
        "expected deletion-sentinel header, got: {reason}"
    );
}

#[test]
fn unchanged_reread_of_oversized_file_collapses_to_sentinel() {
    // Regression: a file past `LARGE_FILE_BYTES` *and* past Claude's
    // Read token limit was substituted as a compressed first read.
    // The second Read on the same content used to fall through to the
    // unconditional `LargeFile` arm and the Claude hook routed it to
    // `allow → native`, which surfaced Claude's `exceeds maximum
    // allowed tokens` error — so a file we'd just rescued became
    // unreadable on the very next read. TooLarge now honors prev
    // first: matching hash → Unchanged sentinel, same as the regular
    // Text path.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("oversized.py");

    let mut body = String::with_capacity(140 * 1024);
    for i in 0..70 {
        body.push_str(&format!("def function_{i}(arg_a, arg_b, arg_c):\n"));
        for j in 0..30 {
            body.push_str(&format!(
                "    step_{j} = arg_a + arg_b * {j} - arg_c  # padding line to push past LARGE_FILE_BYTES\n"
            ));
        }
        body.push_str("    return step_29\n\n");
    }
    assert!(body.len() > 100 * 1024);
    fs::write(&f, &body).unwrap();

    // First Read — compressed substitute via deny.
    let r1 = run_hook_with_env(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
        &[("DRIP_CLAUDE_READ_TOKEN_BUDGET", "100")],
    );
    let v1: Value = serde_json::from_str(r1.trim()).unwrap();
    assert_eq!(
        v1["hookSpecificOutput"]["permissionDecision"],
        json!("deny"),
        "first read should substitute compressed view: {r1}"
    );

    // Second Read — content unchanged. MUST be Unchanged sentinel,
    // not `allow → native` (which would surface Claude's size error
    // and strand the agent on a file DRIP just rescued).
    let r2 = run_hook_with_env(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
        &[("DRIP_CLAUDE_READ_TOKEN_BUDGET", "100")],
    );
    let v2: Value = serde_json::from_str(r2.trim()).unwrap();
    assert_eq!(
        v2["hookSpecificOutput"]["permissionDecision"],
        json!("deny"),
        "re-read of unchanged oversized file must collapse to sentinel, not bail to native: {r2}"
    );
    let reason = v2["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("[DRIP: unchanged"),
        "expected unchanged-sentinel header, got: {reason}"
    );
}

#[test]
fn first_read_past_large_file_cap_still_compresses() {
    // A 130 KB Python module sits above `LARGE_FILE_BYTES` (100 KB
    // diff-perf cap) — without the compression-on-TooLarge path it'd
    // collapse into `FullFallback{ LargeFile }` and the Claude hook
    // would `allow` native, which on a file this big surfaces
    // Claude's `exceeds maximum allowed tokens` error. With
    // compression we squeeze it to a few KB of signatures + stubs and
    // substitute via `deny`.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("over_largefile_cap.py");

    let mut body = String::with_capacity(140 * 1024);
    for i in 0..70 {
        body.push_str(&format!("def function_{i}(arg_a, arg_b, arg_c):\n"));
        for j in 0..30 {
            body.push_str(&format!(
                "    step_{j} = arg_a + arg_b * {j} - arg_c  # padding line to drive the byte count past the 100 KB cap\n"
            ));
        }
        body.push_str("    return step_29\n\n");
    }
    assert!(
        body.len() > 100 * 1024,
        "fixture must exceed LARGE_FILE_BYTES; got {}B",
        body.len()
    );
    fs::write(&f, &body).unwrap();

    let out = run_hook_with_env(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
        &[("DRIP_CLAUDE_READ_TOKEN_BUDGET", "100")],
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("deny"),
        "huge-but-compressible files must substitute, not fall back to allow: {out}"
    );
    let reason = v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap();
    assert!(
        reason.contains("(semantic-compressed)"),
        "header must mark the payload as the compressed view: {reason}"
    );
}

#[test]
fn first_read_under_limit_still_passes_native() {
    // Below the budget, the contract is unchanged: NativePassthrough on
    // first read so Claude's read-tracker populates. Regression guard
    // against accidentally widening the substitute path.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("under_limit.py");
    fs::write(&f, long_python_module()).unwrap();

    // Default budget (10 000 DRIP tokens). Fixture is ~2 800 tokens —
    // well under, even though it would compress.
    let out = run_hook_with_env(
        &drip,
        json!({
            "tool_name": "Read",
            "tool_input": { "file_path": f.to_string_lossy() }
        }),
        &[],
    );
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        v["hookSpecificOutput"]["permissionDecision"],
        json!("allow"),
        "under-budget first reads must stay on the native path: {out}"
    );
}
