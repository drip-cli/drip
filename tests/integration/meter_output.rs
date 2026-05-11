use crate::common::Drip;
use rusqlite::Connection;
use serde_json::Value;
use std::fs;

#[test]
fn meter_json_has_expected_schema() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("g.txt");
    fs::write(&f, "abc\n".repeat(100)).unwrap();
    drip.read_stdout(&f);
    fs::write(&f, "abc\n".repeat(99) + "xyz\n").unwrap();
    drip.read_stdout(&f);

    let o = drip.cmd().arg("meter").arg("--json").output().unwrap();
    assert!(o.status.success());
    let v: Value = serde_json::from_slice(&o.stdout).expect("valid JSON");

    for key in &[
        "session_id",
        "started_at",
        "elapsed_secs",
        "files_tracked",
        "total_reads",
        "tokens_full",
        "tokens_sent",
        "tokens_saved",
        "reduction_pct",
        "top",
        "history",
    ] {
        assert!(v.get(*key).is_some(), "missing field {key} in JSON: {v}");
    }
    assert!(v["reduction_pct"].is_number());
    assert!(v["top"].is_array());
    assert!(v["top"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t.get("file").is_some() && t.get("reduction_pct").is_some()));
}

#[test]
fn meter_warns_when_lifetime_polluted_by_ghost_files() {
    let drip = Drip::new();

    // Real source file in a tempdir that survives the test.
    let live = tempfile::tempdir().unwrap();
    let live_file = live.path().join("real.txt");
    fs::write(&live_file, "abc\n".repeat(50)).unwrap();
    drip.read_stdout(&live_file);
    fs::write(&live_file, "abc\n".repeat(49) + "xyz\n").unwrap();
    drip.read_stdout(&live_file);

    // Ghost file: read into the lifetime stats, then delete the dir so
    // the recorded path no longer exists. Make it big enough that ghost
    // tokens dominate (>= 50% of `tokens_full`).
    let ghost = tempfile::tempdir().unwrap();
    let ghost_file = ghost.path().join("ghost.txt");
    fs::write(&ghost_file, "abc\n".repeat(5_000)).unwrap();
    drip.read_stdout(&ghost_file);
    drop(ghost); // tempdir RAII removes the file

    // Human output should surface the ⚠ hint.
    let o = drip
        .cmd()
        .arg("meter")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.contains("ghost file"), "expected ghost-file hint: {s}");
    assert!(
        s.contains("drip meter --prune"),
        "hint should mention --prune remediation: {s}"
    );

    // JSON output should expose the structured field for programmatic
    // consumers.
    let o = drip.cmd().arg("meter").arg("--json").output().unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).expect("valid JSON");
    assert!(
        v.get("ghost_pollution").is_some(),
        "ghost_pollution missing from JSON when ghosts present: {v}"
    );
    let g = &v["ghost_pollution"];
    assert!(g["ghost_files"].as_i64().unwrap() >= 1);
    assert!(g["ghost_pct"].as_u64().unwrap() >= 50);

    // After --prune, the hint goes away (and so does the JSON field).
    drip.cmd().arg("meter").arg("--prune").output().unwrap();
    let o = drip.cmd().arg("meter").arg("--json").output().unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).expect("valid JSON");
    assert!(
        v.get("ghost_pollution").is_none(),
        "ghost_pollution should be absent after prune: {v}"
    );
}

#[test]
fn meter_no_ghost_hint_on_clean_install() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("g.txt");
    fs::write(&f, "x\n".repeat(100)).unwrap();
    drip.read_stdout(&f);

    // No ghost files at all → JSON should omit the field.
    let o = drip.cmd().arg("meter").arg("--json").output().unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).expect("valid JSON");
    assert!(
        v.get("ghost_pollution").is_none(),
        "ghost_pollution must not appear when nothing is polluted: {v}"
    );

    let o = drip
        .cmd()
        .arg("meter")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(!s.contains("ghost file"), "false-positive hint: {s}");
}

#[test]
fn lifetime_stats_self_heal_on_drift() {
    // Regression: `lifetime_stats` is the single-row accumulator the
    // headline reads from; `lifetime_per_file` is the per-file detail
    // table the warning percentages are computed from. They must stay
    // in sync, otherwise the warning's "X% of tokens" claim is
    // computed against one base while the displayed "Tokens full"
    // shows another, and the two contradict each other (real-world
    // scenario surfaced from a user DB drifted by 6×).
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("g.txt");
    fs::write(&f, "abc\n".repeat(50)).unwrap();
    drip.read_stdout(&f);
    fs::write(&f, "abc\n".repeat(49) + "xyz\n").unwrap();
    drip.read_stdout(&f);

    // Snapshot the post-read SUM(per_file). This is the truth.
    let db_path = drip.data_dir.path().join("sessions.db");
    let conn = Connection::open(&db_path).unwrap();
    let real_full: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(tokens_full), 0) FROM lifetime_per_file",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(real_full > 0, "fixture did not populate lifetime_per_file");

    // Drift `lifetime_stats` 1000× higher than reality. Mimics what
    // we observed in the wild: an old buggy build / manual sqlite3
    // edit that left the headline accumulator out of sync.
    let drifted = real_full * 1000;
    conn.execute(
        "UPDATE lifetime_stats SET tokens_full = ?1 WHERE id = 1",
        rusqlite::params![drifted],
    )
    .unwrap();
    drop(conn);

    // Sanity: the drift took.
    let conn = Connection::open(&db_path).unwrap();
    let drifted_now: i64 = conn
        .query_row("SELECT tokens_full FROM lifetime_stats", [], |r| r.get(0))
        .unwrap();
    assert_eq!(drifted_now, drifted);
    drop(conn);

    // Any subsequent drip invocation opens the DB through `Session::open`,
    // which now self-heals the drift. We use `meter --json` because it's
    // read-only-ish and the fastest way to round-trip the headline.
    let o = drip.cmd().arg("meter").arg("--json").output().unwrap();
    assert!(o.status.success(), "drip meter failed: {:?}", o);
    let v: Value = serde_json::from_slice(&o.stdout).expect("valid JSON");
    let headline_full = v["tokens_full"].as_i64().unwrap();
    assert_eq!(
        headline_full, real_full,
        "headline tokens_full should be re-synced to SUM(lifetime_per_file)"
    );

    // And the on-disk value is now correct, not just the rendered output.
    let conn = Connection::open(&db_path).unwrap();
    let healed: i64 = conn
        .query_row("SELECT tokens_full FROM lifetime_stats", [], |r| r.get(0))
        .unwrap();
    assert_eq!(healed, real_full);
}

#[test]
fn meter_human_output_is_terse_when_no_color() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("g.txt");
    fs::write(&f, "x\n").unwrap();
    drip.read_stdout(&f);

    // Pipe (non-tty) + NO_COLOR: must contain no ANSI escape sequences.
    let o = drip
        .cmd()
        .arg("meter")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(!s.contains('\x1b'), "ANSI leaked when NO_COLOR set: {s:?}");
    assert!(s.contains("DRIP"), "expected DRIP banner: {s:?}");
    assert!(
        s.to_lowercase().contains("tokens saved"),
        "expected tokens-saved headline: {s:?}"
    );
}
