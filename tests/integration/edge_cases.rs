use crate::common::Drip;
use std::fs;

#[test]
fn deleted_file_after_first_read_reports_deletion() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("doomed.txt");
    fs::write(&f, "hi\n").unwrap();

    drip.read_stdout(&f);
    fs::remove_file(&f).unwrap();

    let o = drip.read(&f);
    let stdout = String::from_utf8_lossy(&o.stdout);
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(
        stdout.contains("[DRIP: file deleted")
            || stderr.contains("file not found")
            || stdout.contains("file deleted"),
        "expected deletion notice, stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn truncation_falls_back_to_full_read() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("shrink.txt");

    let big = "line\n".repeat(400);
    fs::write(&f, &big).unwrap();
    drip.read_stdout(&f);

    fs::write(&f, "line\n".repeat(50)).unwrap();
    let out = drip.read_stdout(&f);
    assert!(
        out.contains("file truncated"),
        "expected truncation notice, got: {out}"
    );
}

#[test]
fn large_file_skips_diff() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("huge.txt");

    let big = "x".repeat(120 * 1024);
    fs::write(&f, &big).unwrap();
    let out = drip.read_stdout(&f);
    assert!(
        out.contains("large file"),
        "expected large-file fallback, got header"
    );
}
