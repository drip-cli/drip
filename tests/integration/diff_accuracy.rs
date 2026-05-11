use crate::common::Drip;
use std::fs;

#[test]
fn first_read_returns_full_content() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("hello.txt");
    fs::write(&f, "alpha\nbeta\ngamma\n").unwrap();

    let out = drip.read_stdout(&f);
    assert!(out.contains("[DRIP: full read"), "header: {out}");
    assert!(out.contains("alpha\nbeta\ngamma"), "body: {out}");
}

#[test]
fn second_identical_read_is_unchanged() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("hello.txt");
    fs::write(
        &f,
        "alpha beta gamma delta epsilon zeta eta theta\n".repeat(20),
    )
    .unwrap();

    drip.read_stdout(&f);
    let out = drip.read_stdout(&f);
    assert!(
        out.contains("[DRIP: unchanged"),
        "expected unchanged, got: {out}"
    );
}

#[test]
fn modified_file_returns_unified_diff_no_information_loss() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("app.py");

    // Large enough that the unified-diff envelope (--- / +++ / @@ / context)
    // is cheaper than resending the file — otherwise DRIP correctly falls
    // back to a full read instead of paying for a bigger diff.
    let mut v1 = String::new();
    for i in 0..50 {
        v1.push_str(&format!("def fn_{i:02}():\n    return {i}\n\n"));
    }
    v1.push_str("def b():\n    return 2\n");
    let v2 = v1.replace("def b():\n    return 2", "def b():\n    return TWO");
    fs::write(&f, v1).unwrap();
    drip.read_stdout(&f);

    fs::write(&f, v2).unwrap();
    let out = drip.read_stdout(&f);

    assert!(out.contains("[DRIP: delta only"), "header: {out}");
    assert!(out.contains("--- app.py (last read)"), "diff header: {out}");
    assert!(out.contains("+++ app.py (current)"), "diff header: {out}");
    assert!(out.contains("-    return 2"), "deleted line missing: {out}");
    assert!(out.contains("+    return TWO"), "added line missing: {out}");
    // Unrelated lines should NOT appear in the patch.
    assert!(
        !out.contains("-def a"),
        "unchanged context wrongly marked deleted: {out}"
    );
}

#[test]
fn binary_file_is_never_diffed() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("blob.bin");
    fs::write(&f, b"PNG\0\x89\x00abc").unwrap();

    let out = drip.read_stdout(&f);
    assert!(out.contains("binary file"), "header: {out}");
}

#[test]
fn nonexistent_file_returns_error_first_time() {
    let drip = Drip::new();
    let f = std::path::PathBuf::from("/no/such/path/__nope__.xyz");
    let o = drip.read(&f);
    assert!(!o.status.success(), "should fail on missing file");
}
