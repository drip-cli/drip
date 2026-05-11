//! Cross-session file registry — `↔ unchanged` / `↕ changed` headers
//! on first reads, plus the `drip registry stats` / `gc` admin
//! subcommand.

use crate::common::Drip;
use std::fs;
use std::path::Path;

/// Drip the same file from two different "sessions" (different
/// `DRIP_SESSION_ID` values, same data dir). Returns the rendered
/// output of the second-session first read — that's what the
/// registry decoration is checked against.
fn read_in_session(drip: &Drip, session: &str, file: &Path, env: &[(&str, &str)]) -> String {
    let mut c = drip.cmd_in_session(session);
    c.arg("read").arg(file);
    for (k, v) in env {
        c.env(k, v);
    }
    let o = c.output().expect("drip read");
    assert!(
        o.status.success(),
        "drip read failed: stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn registry_unknown_file_uses_existing_first_read_header() {
    // Brand-new file the registry has never seen — the original
    // first-read header must be unchanged. The new "↔ unchanged" /
    // "↕ changed" decorations should NOT appear.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("brand-new.txt");
    fs::write(&f, "first time\n").unwrap();
    let out = read_in_session(&drip, "session-A", &f, &[]);
    assert!(
        !out.contains("↔") && !out.contains("↕"),
        "registry decoration leaked into unknown-file response: {out}"
    );
}

#[test]
fn registry_known_unchanged_emits_unchanged_header() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("api.py");
    fs::write(&f, "def foo():\n    return 1\n").unwrap();

    // Session A "remembers" the file.
    let _ = read_in_session(&drip, "session-A", &f, &[]);

    // Session B's first read of the same file: registry should
    // signal unchanged.
    let out = read_in_session(&drip, "session-B", &f, &[]);
    assert!(
        out.contains("↔ unchanged since last session"),
        "expected unchanged decoration: {out}"
    );
    // No diff trailer for the unchanged case — the agent reads
    // the body uninterrupted.
    assert!(
        !out.contains("Changes since last session"),
        "trailer should NOT appear on unchanged: {out}"
    );
}

#[test]
fn registry_known_changed_emits_diff_trailer() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("api.py");
    fs::write(&f, "def foo():\n    return 1\n").unwrap();
    let _ = read_in_session(&drip, "session-A", &f, &[]);

    // Mutate the file and read in a fresh session.
    fs::write(
        &f,
        "def foo():\n    return 42\n\ndef bar():\n    return 0\n",
    )
    .unwrap();
    let out = read_in_session(&drip, "session-B", &f, &[]);

    // Header line carries the +/- summary.
    assert!(
        out.contains("↕ changed since last session"),
        "expected changed decoration: {out}"
    );
    assert!(
        out.contains("+") && out.contains("lines"),
        "header should mention added lines count: {out}"
    );
    // Trailer with the inter-session diff comes AFTER the body,
    // delimited so the agent can recognise it as out-of-band.
    assert!(
        out.contains("Changes since last session"),
        "diff trailer missing: {out}"
    );
    assert!(
        out.contains("-    return 1") || out.contains("- return 1"),
        "diff content missing -return 1: {out}"
    );
    assert!(
        out.contains("+    return 42") || out.contains("+ return 42"),
        "diff content missing +return 42: {out}"
    );
}

#[test]
fn registry_changed_trailer_counts_as_sent_tokens() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("api.py");
    fs::write(&f, "def foo():\n    return 1\n").unwrap();
    let _ = read_in_session(&drip, "session-A", &f, &[]);

    fs::write(
        &f,
        "def foo():\n    return 42\n\ndef bar():\n    return 0\n",
    )
    .unwrap();
    let out = read_in_session(&drip, "session-B", &f, &[]);
    assert!(out.contains("Changes since last session"), "{out}");

    let conn = rusqlite::Connection::open(drip.data_dir.path().join("sessions.db")).unwrap();
    let (tokens_full, tokens_sent): (i64, i64) = conn
        .query_row(
            "SELECT tokens_full, tokens_sent FROM reads WHERE session_id = 'session-B'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(
        tokens_sent > tokens_full,
        "the diff trailer is extra context and must be counted: full={tokens_full}, sent={tokens_sent}"
    );
}

#[test]
fn registry_disabled_env_var_skips_lookup() {
    // DRIP_REGISTRY_DISABLE=1 must short-circuit both the write and
    // the read side. Even after a "first session" populated the
    // registry, the second session shouldn't decorate its first
    // read when the var is set.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("api.py");
    fs::write(&f, "x\n").unwrap();
    let _ = read_in_session(&drip, "session-A", &f, &[]);

    let out = read_in_session(&drip, "session-B", &f, &[("DRIP_REGISTRY_DISABLE", "1")]);
    assert!(
        !out.contains("↔") && !out.contains("↕"),
        "DRIP_REGISTRY_DISABLE failed to suppress decoration: {out}"
    );
}

#[test]
fn registry_full_content_always_sent_on_first_read() {
    // Even when the registry says "unchanged", the agent still
    // gets the FULL file content — at session start there's
    // nothing in its context to diff against. Only the *header*
    // changes.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("hello.txt");
    let body = "alpha\nbeta\ngamma\n";
    fs::write(&f, body).unwrap();
    let _ = read_in_session(&drip, "session-A", &f, &[]);
    let out = read_in_session(&drip, "session-B", &f, &[]);
    assert!(
        out.contains("alpha") && out.contains("beta") && out.contains("gamma"),
        "full content must be in the response: {out}"
    );
}

#[test]
fn registry_stats_reports_known_files_count() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    for n in 0..3 {
        let f = dir.path().join(format!("f{n}.txt"));
        fs::write(&f, format!("content-{n}\n")).unwrap();
        let _ = read_in_session(&drip, "session-A", &f, &[]);
    }
    let o = drip.cmd().args(["registry", "stats"]).output().unwrap();
    assert!(o.status.success());
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("Known files    : 3"),
        "stats should report 3 known files: {s}"
    );
}

#[test]
fn registry_gc_drops_old_entries() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("temp.txt");
    fs::write(&f, "x\n").unwrap();
    let _ = read_in_session(&drip, "session-A", &f, &[]);

    // gc with a 0s cutoff sweeps everything written so far.
    let o = drip
        .cmd()
        .args(["registry", "gc", "--older-than", "0s"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let report = String::from_utf8_lossy(&o.stdout);
    assert!(
        report.contains("Removed rows   : 1"),
        "gc should report 1 removed row, got: {report}"
    );

    // After gc, stats should show 0 known files.
    let o = drip.cmd().args(["registry", "stats"]).output().unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("Known files    : 0"),
        "registry not emptied: {s}"
    );
}

#[test]
fn registry_gc_default_is_30_days() {
    // Without --older-than, gc uses the 30-day default — recent
    // entries must survive.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("recent.txt");
    fs::write(&f, "x\n").unwrap();
    let _ = read_in_session(&drip, "session-A", &f, &[]);

    let o = drip.cmd().args(["registry", "gc"]).output().unwrap();
    assert!(o.status.success());
    let report = String::from_utf8_lossy(&o.stdout);
    assert!(
        report.contains("Removed rows   : 0"),
        "default gc should not touch recent rows: {report}"
    );
}
