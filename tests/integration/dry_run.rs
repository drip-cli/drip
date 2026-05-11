use crate::common::Drip;
use std::fs;

#[test]
fn dry_run_does_not_persist_baseline() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("dr.txt");
    fs::write(&f, "alpha\n").unwrap();

    let o = drip
        .cmd()
        .arg("read")
        .arg("--dry-run")
        .arg(&f)
        .output()
        .unwrap();
    assert!(o.status.success());
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("[DRIP: dry-run"), "header: {text}");
    assert!(text.contains("[DRIP: full read"), "outcome: {text}");

    // After dry-run, the next real read should still see this as a
    // first read (no baseline persisted).
    let real = drip.read_stdout(&f);
    assert!(
        real.contains("[DRIP: full read"),
        "expected fresh first read: {real}"
    );
}

#[test]
fn dry_run_after_real_read_predicts_unchanged() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("dr2.txt");
    fs::write(
        &f,
        "x y z repeated payload for dry-run unchanged\n".repeat(20),
    )
    .unwrap();
    drip.read_stdout(&f);

    let o = drip
        .cmd()
        .arg("read")
        .arg("--dry-run")
        .arg(&f)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&o.stdout);
    assert!(text.contains("[DRIP: dry-run"));
    assert!(text.contains("[DRIP: unchanged"), "got: {text}");
}
