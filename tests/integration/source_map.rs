//! `drip source-map` CLI tests.
//!
//! Step 4 of the source-map arc: end-to-end coverage that the CLI
//! resolves a compressed line to its original range, prints the full
//! map when no `--line` is given, and degrades gracefully when no map
//! exists (uncompressed read, untracked file).

use crate::common::Drip;
use std::fs;

fn long_python_source() -> String {
    // Five top-level functions, each with a 12-line body. The default
    // `min_body_lines` is 8, so every body gets elided — keeps the
    // assertions stable across DRIP_COMPRESS_MIN_BODY tweaks.
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

#[test]
fn source_map_resolves_a_single_compressed_line_to_original_range() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("svc.py");
    fs::write(&f, long_python_source()).unwrap();
    drip.read_stdout(&f);

    // Pull the JSON map first to discover an elided line we can probe
    // — the exact compressed line number depends on stub placement,
    // and hard-coding it would silently break if the compressor's
    // signature/body emission ever shifts by one.
    let json = drip
        .cmd()
        .arg("source-map")
        .arg(&f)
        .arg("--json")
        .output()
        .unwrap();
    assert!(json.status.success());
    let map: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("--json must produce a JSON array");
    let arr = map.as_array().expect("JSON shape is an array");
    let elided = arr
        .iter()
        .find(|e| e.get("elided").and_then(|v| v.as_bool()) == Some(true))
        .expect("at least one entry must be elided for this fixture");
    let compressed_line = elided.get("compressed_line").unwrap().as_u64().unwrap();
    let want_start = elided.get("original_start").unwrap().as_u64().unwrap();
    let want_end = elided.get("original_end").unwrap().as_u64().unwrap();

    let out = drip
        .cmd()
        .arg("source-map")
        .arg(&f)
        .arg("--line")
        .arg(compressed_line.to_string())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains(&format!("compressed L{compressed_line}")),
        "missing compressed line label: {s}"
    );
    assert!(
        s.contains(&format!("original L{want_start}-L{want_end}")),
        "missing original-range label: {s}"
    );
    assert!(s.contains("[elided]"), "should mark elided entries: {s}");
}

#[test]
fn source_map_full_table_lists_every_compressed_line() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("svc.py");
    fs::write(&f, long_python_source()).unwrap();
    drip.read_stdout(&f);

    let out = drip.cmd().arg("source-map").arg(&f).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    // Header advertises the entry count + elided regions count.
    assert!(
        s.contains("compressed lines"),
        "missing header summary: {s}"
    );
    assert!(s.contains("elided regions"), "missing elided count: {s}");
    // Every visible signature line should appear as a 1:1 mapping.
    assert!(
        s.contains("→ original L"),
        "rows must use the canonical arrow format: {s}"
    );
}

#[test]
fn source_map_without_compression_emits_clear_no_map_message() {
    // Tiny file → no compression → no source map. The CLI must say so
    // explicitly (with a hint about which recovery path applies)
    // rather than crash or print an empty table.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("tiny.py");
    fs::write(&f, "def hi():\n    return 1\n").unwrap();
    drip.read_stdout(&f);

    let out = drip.cmd().arg("source-map").arg(&f).output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("No source map"), "expected no-map header: {s}");
    assert!(
        s.contains("no compression fired"),
        "expected reason hint: {s}"
    );
}

#[test]
fn source_map_for_untracked_file_distinguishes_from_uncompressed() {
    // File never read by DRIP — different recovery hint than a read
    // that simply didn't trigger compression.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("never_read.py");
    fs::write(&f, "x = 1\n").unwrap();

    let out = drip.cmd().arg("source-map").arg(&f).output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("no read tracked"),
        "untracked files must surface the read-first hint: {s}"
    );
}

#[test]
fn source_map_accepts_l_prefixed_line_argument() {
    // Stub messages format ranges as `original L5-L21`. Users who
    // copy-paste those into `--line` shouldn't have to strip the
    // leading L by hand.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("svc.py");
    fs::write(&f, long_python_source()).unwrap();
    drip.read_stdout(&f);

    let plain = drip
        .cmd()
        .arg("source-map")
        .arg(&f)
        .arg("--line")
        .arg("3")
        .output()
        .unwrap();
    let prefixed = drip
        .cmd()
        .arg("source-map")
        .arg(&f)
        .arg("--line")
        .arg("L3")
        .output()
        .unwrap();
    assert!(plain.status.success() && prefixed.status.success());
    assert_eq!(
        String::from_utf8_lossy(&plain.stdout),
        String::from_utf8_lossy(&prefixed.stdout),
        "L-prefix and bare digit should produce identical output"
    );
}

#[test]
fn source_map_rejects_zero_and_garbage_line_args() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("svc.py");
    fs::write(&f, long_python_source()).unwrap();
    drip.read_stdout(&f);

    let zero = drip
        .cmd()
        .arg("source-map")
        .arg(&f)
        .arg("--line")
        .arg("0")
        .output()
        .unwrap();
    assert!(!zero.status.success(), "0 should be rejected (1-indexed)");

    let garbage = drip
        .cmd()
        .arg("source-map")
        .arg(&f)
        .arg("--line")
        .arg("nope")
        .output()
        .unwrap();
    assert!(!garbage.status.success(), "non-numeric should be rejected");
}

#[test]
fn drip_refresh_then_reread_regenerates_source_map() {
    // After `drip refresh`, the next read writes a fresh baseline +
    // a fresh source map. Pre-fix would have hit one of two bugs: a
    // stale map (refresh didn't clear the column) or a missing map
    // (refresh nuked the row but the next read didn't rebuild it).
    // Both break the pre-edit guard and `drip source-map --line N`.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("svc.py");
    fs::write(&f, long_python_source()).unwrap();
    drip.read_stdout(&f);

    // Sanity: map exists.
    let before = drip
        .cmd()
        .arg("source-map")
        .arg(&f)
        .arg("--json")
        .output()
        .unwrap();
    assert!(before.status.success());
    let before_map: serde_json::Value = serde_json::from_slice(&before.stdout).unwrap();
    assert!(!before_map.as_array().unwrap().is_empty());

    // Out-of-band edit: append a brand-new function so the new map
    // gets MORE entries. A same-length edit would leave the map
    // shape identical and the before==after check below couldn't
    // distinguish "refresh re-ran" from "refresh was a no-op".
    let mut mutated = long_python_source();
    mutated.push_str("def fn_extra(a, b, c):\n");
    for i in 0..12 {
        mutated.push_str(&format!("    extra_{i:02} = a + b + {i}\n"));
    }
    mutated.push_str("    return extra_11\n\n");
    fs::write(&f, &mutated).unwrap();

    // Refresh + re-read.
    let r = drip.cmd().arg("refresh").arg(&f).output().unwrap();
    assert!(
        r.status.success(),
        "refresh failed: {}",
        String::from_utf8_lossy(&r.stderr)
    );
    drip.read_stdout(&f);

    let after = drip
        .cmd()
        .arg("source-map")
        .arg(&f)
        .arg("--json")
        .output()
        .unwrap();
    assert!(after.status.success());
    let after_map: serde_json::Value = serde_json::from_slice(&after.stdout).unwrap();
    assert!(
        !after_map.as_array().unwrap().is_empty(),
        "refresh + re-read must regenerate the source map, not leave it empty: {after_map}"
    );
    // The new map must have MORE entries (we appended an extra
    // function). If the maps had the same length, refresh either
    // didn't run, or the read after refresh hit a stale cached
    // baseline.
    let before_len = before_map.as_array().unwrap().len();
    let after_len = after_map.as_array().unwrap().len();
    assert!(
        after_len > before_len,
        "expected more entries after appending a function: before={before_len} after={after_len}"
    );
}

#[test]
fn source_map_line_lookup_handles_out_of_range_compressed_line() {
    // `--line N` with N greater than the largest compressed line
    // must fail soft (exit 0, "unmapped" message), not panic. Also
    // covers --json variant for tooling consumers.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("svc.py");
    fs::write(&f, long_python_source()).unwrap();
    drip.read_stdout(&f);

    // Pull the actual map size, then probe one past it.
    let json = drip
        .cmd()
        .arg("source-map")
        .arg(&f)
        .arg("--json")
        .output()
        .unwrap();
    let arr: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let n = arr.as_array().unwrap().len();
    let probe = n + 50;

    let plain = drip
        .cmd()
        .args(["source-map"])
        .arg(&f)
        .arg("--line")
        .arg(probe.to_string())
        .output()
        .unwrap();
    assert!(
        plain.status.success(),
        "must exit 0 on overshoot, not crash"
    );
    let s = String::from_utf8_lossy(&plain.stdout);
    assert!(s.contains("unmapped"), "human form must say unmapped: {s}");

    let j = drip
        .cmd()
        .args(["source-map"])
        .arg(&f)
        .arg("--line")
        .arg(probe.to_string())
        .arg("--json")
        .output()
        .unwrap();
    assert!(j.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&j.stdout).unwrap();
    assert_eq!(
        parsed.get("unmapped").and_then(|v| v.as_bool()),
        Some(true),
        "JSON form must set unmapped:true: {parsed}"
    );
    assert_eq!(
        parsed.get("compressed_line").and_then(|v| v.as_u64()),
        Some(probe as u64),
        "JSON form must echo the requested line so callers can correlate"
    );
}

#[test]
fn source_map_cli_auto_picks_sibling_session_in_cwd() {
    // Round-3 agent-UX: when an agent in session A compresses
    // `svc.py` and the *user* then types `drip source-map svc.py`
    // in their shell (session B, no reads), the CLI must surface
    // session A's map — not the historical "no read tracked" error.
    //
    // This intentionally inverts the pre-round-3 isolation behavior
    // at the CLI surface. The internal `Session::get_source_map`
    // remains strictly per-(session_id, file_path) — see the
    // `source_map_internal_lookup_is_strictly_session_scoped` test
    // below for the correctness guarantee the pre-edit guard
    // depends on.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("svc.py");
    fs::write(&f, long_python_source()).unwrap();

    // Seed session A with a compressed read.
    let mut c = drip.cmd();
    c.arg("read").arg(&f).current_dir(dir.path());
    assert!(c.output().unwrap().status.success());

    // Sanity in the seeded session.
    let same = drip
        .cmd()
        .arg("source-map")
        .arg(&f)
        .arg("--json")
        .current_dir(dir.path())
        .output()
        .unwrap();
    let same_arr: serde_json::Value = serde_json::from_slice(&same.stdout).unwrap();
    assert!(!same_arr.as_array().unwrap().is_empty());

    // Sibling session, same cwd → must find the seeded map via the
    // inspect helper's auto-pick (env-strategy + last_active in cwd).
    let other = drip
        .cmd_in_session("source-map-other-session")
        .arg("source-map")
        .arg(&f)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(other.status.success());
    let s = String::from_utf8_lossy(&other.stdout);
    assert!(
        !s.contains("No source map") && !s.contains("no read tracked"),
        "CLI must auto-pick the sibling session in cwd, got: {s}"
    );
    assert!(
        s.contains("source map for") || s.contains("compressed lines"),
        "CLI must render the map content, got: {s}"
    );
}

#[test]
fn source_map_internal_lookup_is_strictly_session_scoped() {
    // Pre-edit guard correctness: even though the CLI auto-picks
    // sibling sessions, the underlying `Session::get_source_map`
    // (called by internal code that already has the right session id)
    // must still answer ONLY for `(self.id, file_path)`. If it ever
    // started leaking across sessions, the pre-edit guard could
    // confirm an Edit against a map from a different agent run.
    //
    // We assert this by checking that a fresh session id pointing at
    // a *different* cwd (so the CLI auto-pick can't find anything)
    // returns "untracked" — confirming the per-session SQL WHERE
    // clause is doing its job. The CLI surface in this scenario is
    // identical to what the pre-edit guard sees internally.
    let drip = Drip::new();
    let seeded_dir = tempfile::tempdir().unwrap();
    let f = seeded_dir.path().join("svc.py");
    fs::write(&f, long_python_source()).unwrap();
    let mut c = drip.cmd();
    c.arg("read").arg(&f).current_dir(seeded_dir.path());
    assert!(c.output().unwrap().status.success());

    // Different cwd — auto-pick has no candidate, falls back to the
    // (empty) derived session, internal lookup returns None.
    let other_dir = tempfile::tempdir().unwrap();
    let other = drip
        .cmd_in_session("strict-isolation-other-session")
        .arg("source-map")
        .arg(&f)
        .current_dir(other_dir.path())
        .output()
        .unwrap();
    assert!(other.status.success());
    let s = String::from_utf8_lossy(&other.stdout);
    assert!(
        s.contains("No source map") && s.contains("no read tracked"),
        "isolated session in a different cwd must NOT see the seeded map: {s}"
    );
}

#[test]
fn source_map_json_round_trips_through_serde() {
    // The JSON output is the de-facto contract for tooling that wants
    // to consume source maps without parsing the human format. Pin it
    // to the persisted column shape.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("svc.py");
    fs::write(&f, long_python_source()).unwrap();
    drip.read_stdout(&f);

    let out = drip
        .cmd()
        .arg("source-map")
        .arg(&f)
        .arg("--json")
        .output()
        .unwrap();
    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let arr = parsed.as_array().expect("top level must be an array");
    assert!(!arr.is_empty(), "fixture should yield a populated map");
    for entry in arr {
        assert!(entry.get("compressed_line").is_some());
        assert!(entry.get("original_start").is_some());
        assert!(entry.get("original_end").is_some());
    }
}
