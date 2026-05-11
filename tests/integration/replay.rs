//! Integration tests for `drip replay` and the `read_events` log it
//! depends on.

use crate::common::Drip;
use serde_json::Value;
use std::fs;

#[test]
fn replay_records_every_intercepted_read() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.rs");
    // Big enough that a 1-line diff is cheaper than the full file
    // (otherwise DRIP correctly falls back to full and the kind is
    // "fallback" / "DiffBiggerThanFile" instead of "delta").
    let baseline: String = (0..200)
        .map(|i| format!("line {i} contents here\n"))
        .collect();
    fs::write(&f, &baseline).unwrap();
    drip.read_stdout(&f); // first
    drip.read_stdout(&f); // unchanged
    let modified = baseline.replace("line 5 contents here\n", "line 5 CHANGED\n");
    fs::write(&f, &modified).unwrap();
    drip.read_stdout(&f); // delta
    drip.read_stdout(&f); // unchanged

    let o = drip.cmd().arg("replay").arg("--json").output().unwrap();
    assert!(
        o.status.success(),
        "replay --json failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let events = v["events"].as_array().expect("events array");
    assert!(
        events.len() >= 4,
        "expected ≥4 events, got {}",
        events.len()
    );

    let kinds: Vec<String> = events
        .iter()
        .map(|e| e["outcome_kind"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.contains(&"first".to_string()), "kinds: {kinds:?}");
    assert!(kinds.contains(&"delta".to_string()), "kinds: {kinds:?}");
    assert!(
        kinds.iter().filter(|k| *k == "unchanged").count() >= 1,
        "kinds: {kinds:?}"
    );

    // Summary numbers must match the sum across events.
    let sum_full: i64 = events
        .iter()
        .map(|e| e["tokens_full"].as_i64().unwrap())
        .sum();
    let sum_sent: i64 = events
        .iter()
        .map(|e| e["tokens_sent"].as_i64().unwrap())
        .sum();
    assert_eq!(v["summary"]["tokens_full"].as_i64().unwrap(), sum_full);
    assert_eq!(v["summary"]["tokens_sent"].as_i64().unwrap(), sum_sent);
}

#[test]
fn replay_full_includes_rendered_output() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("b.rs");
    fs::write(&f, "v1\n".repeat(50)).unwrap();
    drip.read_stdout(&f);
    drip.read_stdout(&f);

    let o = drip
        .cmd()
        .arg("replay")
        .arg("--full")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(o.status.success());
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(s.contains("Replay (rendered output per event)"), "got: {s}");
    assert!(
        s.contains("[DRIP: full read"),
        "expected first-read header in --full output: {s}"
    );
    assert!(
        s.contains("[DRIP: unchanged"),
        "expected unchanged header in --full output: {s}"
    );
}

#[test]
fn replay_filters_by_file_substring() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("alpha.rs");
    let b = dir.path().join("beta.rs");
    fs::write(&a, "x\n".repeat(20)).unwrap();
    fs::write(&b, "y\n".repeat(20)).unwrap();
    drip.read_stdout(&a);
    drip.read_stdout(&b);

    let o = drip
        .cmd()
        .arg("replay")
        .arg("--json")
        .arg("--file")
        .arg("alpha")
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let events = v["events"].as_array().unwrap();
    assert!(!events.is_empty(), "filter dropped everything: {v}");
    for e in events {
        let p = e["file_path"].as_str().unwrap();
        assert!(p.contains("alpha"), "filter leak: {p}");
    }
}

#[test]
fn replay_log_disabled_via_env() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("d.rs");
    fs::write(&f, "x\n".repeat(20)).unwrap();
    let _ = drip
        .cmd()
        .arg("read")
        .arg(&f)
        .env("DRIP_REPLAY_LOG", "0")
        .output()
        .unwrap();

    let o = drip.cmd().arg("replay").arg("--json").output().unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(
        v["events"].as_array().unwrap().len(),
        0,
        "DRIP_REPLAY_LOG=0 must skip event recording: {v}"
    );
}

#[test]
fn replay_keep_caps_event_count() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("e.rs");
    fs::write(&f, "x\n".repeat(20)).unwrap();
    // Force the per-session cap to 3 and emit 6 reads — only the latest
    // 3 must remain in the log.
    for _ in 0..6 {
        let _ = drip
            .cmd()
            .arg("read")
            .arg(&f)
            .env("DRIP_REPLAY_KEEP", "3")
            .output()
            .unwrap();
    }

    let o = drip
        .cmd()
        .arg("replay")
        .arg("--json")
        .arg("--limit")
        .arg("100")
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let n = v["events"].as_array().unwrap().len();
    assert!(n <= 3, "expected ≤3 events after cap, got {n}: {v}");
}
