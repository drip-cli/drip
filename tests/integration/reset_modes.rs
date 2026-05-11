//! `drip reset --all` / `--stats` / `--force` coverage.
//!
//! Three flag combinations, three contracts:
//!
//! - `--all --force`: wipes EVERY table + every cache blob; report
//!   surfaces the bucket counts so a misconfigured DRIP_DATA_DIR
//!   doesn't silently succeed.
//! - `--stats`: zeros only the lifetime counters; per-session reads
//!   and baselines survive so an in-progress agent run doesn't
//!   regress its diffs/sentinels mid-task.
//! - `--all` without `--force`: prompts for "yes" on stdin; "no"
//!   (or anything else) aborts and leaves data intact.

use crate::common::Drip;
use std::fs;
use std::io::Write;
use std::process::Stdio;

fn seed(drip: &Drip) -> std::path::PathBuf {
    // 1 read pair → populates reads, lifetime_stats,
    // lifetime_per_file, lifetime_daily, file_registry.
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("seed.py");
    let body: String = (0..120).map(|i| format!("line {i}\n")).collect();
    fs::write(&f, &body).unwrap();
    drip.read_stdout(&f);
    drip.read_stdout(&f);

    // Tempdir would drop on return — leak it via forget so the file
    // survives the test body. The test's own tempdir cleans up via
    // Drip::data_dir on Drop; this one we just need alive long enough
    // for the seeded reads to mean anything.
    let path = f.clone();
    std::mem::forget(dir);
    path
}

fn meter_total_reads(drip: &Drip) -> i64 {
    let o = drip.cmd().arg("meter").arg("--json").output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    v["total_reads"].as_i64().unwrap()
}

fn db_row_count(drip: &Drip, table: &str) -> i64 {
    let db = drip.data_dir.path().join("sessions.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or(0)
}

#[test]
fn reset_all_force_wipes_every_table_and_reports_counts() {
    let drip = Drip::new();
    seed(&drip);
    assert!(meter_total_reads(&drip) > 0, "fixture seeded");
    assert!(db_row_count(&drip, "sessions") >= 1);
    assert!(db_row_count(&drip, "lifetime_stats") >= 1);
    assert!(db_row_count(&drip, "file_registry") >= 1);

    let o = drip
        .cmd()
        .args(["reset", "--all", "--force"])
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "reset --all --force exited non-zero: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.contains("All DRIP data cleared"),
        "expected wipe-confirmation header: {out}"
    );
    // The report must surface non-zero bucket counts so a wipe of
    // an empty/wrong data dir is distinguishable.
    assert!(
        out.contains("sessions") && out.contains("reads") && out.contains("registry"),
        "report must enumerate buckets that got cleared: {out}"
    );

    // Every table is empty after the wipe.
    for tbl in [
        "sessions",
        "reads",
        "file_registry",
        "lifetime_stats",
        "lifetime_per_file",
        "lifetime_daily",
    ] {
        assert_eq!(
            db_row_count(&drip, tbl),
            0,
            "{tbl} should be empty after reset --all"
        );
    }

    // Re-running meter doesn't crash and reports zero.
    assert_eq!(
        meter_total_reads(&drip),
        0,
        "meter after wipe must report 0 reads"
    );
}

#[test]
fn reset_stats_zeros_lifetime_but_preserves_session_baselines() {
    let drip = Drip::new();
    let f = seed(&drip);
    assert!(meter_total_reads(&drip) > 0);
    let reads_before = db_row_count(&drip, "reads");
    assert!(reads_before > 0, "must have a session row to preserve");

    let o = drip.cmd().args(["reset", "--stats"]).output().unwrap();
    assert!(o.status.success());
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("Lifetime counters cleared"), "{out}");
    assert!(
        out.contains("baselines untouched"),
        "must reassure user that sessions survive: {out}"
    );

    // Lifetime tables drained.
    assert_eq!(db_row_count(&drip, "lifetime_stats"), 0);
    assert_eq!(db_row_count(&drip, "lifetime_per_file"), 0);
    assert_eq!(db_row_count(&drip, "lifetime_daily"), 0);
    // Session-scoped state intact.
    assert_eq!(
        db_row_count(&drip, "reads"),
        reads_before,
        "per-session reads MUST survive --stats"
    );
    // Baseline still produces an unchanged-sentinel on next read —
    // proves the per-session diff machinery is intact.
    let next = drip.read_stdout(&f);
    assert!(
        next.contains("[DRIP: unchanged since last read"),
        "next read must still hit the unchanged path: {next}"
    );
}

#[test]
fn reset_all_without_force_prompts_and_no_answer_aborts() {
    let drip = Drip::new();
    seed(&drip);
    let reads_before = db_row_count(&drip, "reads");
    let lifetime_before = db_row_count(&drip, "lifetime_stats");
    assert!(reads_before > 0 && lifetime_before > 0);

    // Pipe "no" on stdin → should NOT delete anything.
    let mut child = drip
        .cmd()
        .args(["reset", "--all"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"no\n").unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(o.status.success(), "non-confirmed abort still exits 0");
    let out = String::from_utf8_lossy(&o.stdout);
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        out.contains("Aborted"),
        "stdout must announce the abort: stdout={out} stderr={err}"
    );
    // The prompt itself goes to stderr so scripts capturing stdout
    // get only the result line.
    assert!(
        err.contains("Type 'yes' to confirm"),
        "prompt must hit stderr: {err}"
    );

    // Data intact.
    assert_eq!(db_row_count(&drip, "reads"), reads_before);
    assert_eq!(db_row_count(&drip, "lifetime_stats"), lifetime_before);
}

#[test]
fn reset_all_and_stats_together_is_a_clear_error() {
    let drip = Drip::new();
    let o = drip
        .cmd()
        .args(["reset", "--all", "--stats"])
        .output()
        .unwrap();
    assert!(!o.status.success(), "must reject the conflicting combo");
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("mutually exclusive"),
        "error message must explain why: {err}"
    );
}

#[test]
fn meter_flips_to_since_reset_label_after_reset_stats() {
    // Before any reset: lifetime meter advertises "Since install".
    // After `reset --stats`: same lifetime view should swap to "Since
    // reset", and the JSON surface should grow a `last_reset_at` field.
    // Without this, a user who blew away their counters keeps reading
    // a misleadingly long "since install" duration that no longer
    // matches the (now-tiny) accumulators below it.
    let drip = Drip::new();
    seed(&drip);
    assert!(meter_total_reads(&drip) > 0);

    // Pre-reset: human surface says "install", JSON has no reset marker.
    let pre = drip
        .cmd()
        .arg("meter")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let pre_txt = String::from_utf8_lossy(&pre.stdout);
    assert!(
        pre_txt.contains("Since install:") || pre_txt.contains("Since Install"),
        "fresh install should say 'Since install': {pre_txt}"
    );
    assert!(
        !pre_txt.contains("Since reset:") && !pre_txt.contains("Since Reset"),
        "pre-reset must not advertise reset: {pre_txt}"
    );
    let pre_json: serde_json::Value = serde_json::from_slice(
        &drip
            .cmd()
            .args(["meter", "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(
        pre_json.get("last_reset_at").is_none(),
        "last_reset_at must be absent before any reset: {pre_json}"
    );

    // Trigger the reset.
    let r = drip.cmd().args(["reset", "--stats"]).output().unwrap();
    assert!(r.status.success());

    // Post-reset: label flipped, JSON carries the timestamp.
    let post = drip
        .cmd()
        .arg("meter")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let post_txt = String::from_utf8_lossy(&post.stdout);
    assert!(
        post_txt.contains("Since reset:") || post_txt.contains("Since Reset"),
        "after --stats the label must read 'Since reset': {post_txt}"
    );
    assert!(
        !post_txt.contains("Since install:") && !post_txt.contains("Since Install"),
        "after --stats the 'Since install' framing must be gone: {post_txt}"
    );

    let post_json: serde_json::Value = serde_json::from_slice(
        &drip
            .cmd()
            .args(["meter", "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let ts = post_json["last_reset_at"]
        .as_i64()
        .expect("last_reset_at must be set after reset --stats");
    let installed_at = post_json["started_at"].as_i64().unwrap();
    assert!(
        ts >= installed_at,
        "last_reset_at ({ts}) must be ≥ installed_at ({installed_at})"
    );
}

#[test]
fn meter_flips_to_since_reset_label_after_reset_all() {
    // `reset --all` wipes the DB but preserves the `meta` table so the
    // reset marker survives. The lifetime headline below the wipe must
    // therefore also advertise "Since reset", not "Since install" — the
    // pre-wipe install date is no longer meaningful for the post-wipe
    // counters.
    let drip = Drip::new();
    seed(&drip);

    let r = drip
        .cmd()
        .args(["reset", "--all", "--force"])
        .output()
        .unwrap();
    assert!(r.status.success());

    let json: serde_json::Value = serde_json::from_slice(
        &drip
            .cmd()
            .args(["meter", "--json"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(
        json.get("last_reset_at").is_some(),
        "reset --all must stamp last_reset_at so the meter label flips: {json}"
    );

    let txt = String::from_utf8_lossy(
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
        txt.contains("Since reset:") || txt.contains("Since Reset"),
        "after --all the human surface must read 'Since reset': {txt}"
    );
}
