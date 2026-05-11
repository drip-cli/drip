//! Integration tests for semantic compression on first reads.

use crate::common::Drip;
use std::fs;

fn long_python_module() -> String {
    let mut s = String::from("import os\nimport sys\n\n");
    for n in 0..6 {
        s.push_str(&format!(
            "def function_{n}(arg_a, arg_b, arg_c):\n    \
                 step_one = arg_a + arg_b\n    \
                 step_two = step_one * 2\n    \
                 step_three = step_two - arg_c\n    \
                 step_four = step_three ** 2\n    \
                 step_five = step_four + 1\n    \
                 step_six = step_five * 3\n    \
                 step_seven = step_six - 7\n    \
                 step_eight = step_seven + arg_a\n    \
                 return step_eight\n\n"
        ));
    }
    s
}

#[test]
fn first_read_python_is_semantically_compressed() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("module.py");
    fs::write(&f, long_python_module()).unwrap();

    let out = drip.read_stdout(&f);
    assert!(
        out.contains("[DRIP: full read (semantic-compressed)"),
        "expected compression header, got: {out}"
    );
    assert!(
        out.contains("DRIP-elided"),
        "expected per-function elision marker: {out}"
    );
    assert!(
        out.contains("def function_0"),
        "signatures must be preserved: {out}"
    );
    // The compressed payload should be much shorter than the original.
    let original_lines = long_python_module().lines().count();
    let compressed_lines = out.lines().count();
    assert!(
        compressed_lines < original_lines / 2,
        "compression should at least halve line count (orig={original_lines}, got={compressed_lines}): {out}"
    );
}

#[test]
fn baseline_kept_uncompressed_so_diffs_still_work() {
    // After a compressed first read, the SQLite baseline must store
    // the ORIGINAL content — otherwise the next read's diff would be
    // computed against the elided text and produce garbage.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("api.py");
    fs::write(&f, long_python_module()).unwrap();
    drip.read_stdout(&f);

    // Mutate ONE function's body in the original. Use a unique substring
    // so we change a single line and the diff stays cheap (otherwise
    // DRIP correctly falls back to a full re-read and we can't observe
    // the diff against the original baseline).
    let original = long_python_module();
    let mutated = original.replacen(
        "    step_one = arg_a + arg_b\n    step_two = step_one * 2",
        "    step_one = arg_a + arg_b\n    step_two = step_one * 9999",
        1,
    );
    fs::write(&f, &mutated).unwrap();
    let out = drip.read_stdout(&f);

    // The diff should mention the actual changed line — proves we
    // diffed against the *original* baseline, not the compressed one.
    assert!(
        out.contains("[DRIP: delta only"),
        "expected delta on second read, got: {out}"
    );
    assert!(
        out.contains("step_two = step_one * 9999"),
        "diff must reflect real change, got: {out}"
    );
    // And it should NOT mention "DRIP-elided" — that string only lives
    // in the compressed payload, never in a clean diff.
    assert!(
        !out.contains("DRIP-elided"),
        "delta should not be polluted with compression markers: {out}"
    );
}

#[test]
fn compression_disabled_via_env() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("off.py");
    fs::write(&f, long_python_module()).unwrap();

    let o = drip
        .cmd()
        .arg("read")
        .arg(&f)
        .env("DRIP_NO_COMPRESS", "1")
        .output()
        .unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        !s.contains("(semantic-compressed)"),
        "DRIP_NO_COMPRESS=1 should suppress compression: {s}"
    );
    assert!(
        s.contains("[DRIP: full read"),
        "still expect full-read header without compression: {s}"
    );
}

#[test]
fn small_files_skip_compression() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("tiny.py");
    fs::write(&f, "def hi():\n    return 1\n").unwrap();
    let out = drip.read_stdout(&f);
    assert!(
        !out.contains("(semantic-compressed)"),
        "tiny files shouldn't be compressed: {out}"
    );
}

#[test]
fn complex_diff_falls_back_to_full_read() {
    // When > DRIP_MAX_HUNKS hunks are scattered through the file, the
    // unified diff costs more than the file itself. DRIP detects this
    // and ships a clean full re-read with a "diff complexity: …"
    // header — better orientation for the agent than a sprawling diff.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("scattered.py");

    let mut src = String::from("import os\n\n");
    for i in 0..20 {
        src.push_str(&format!(
            "def fn_{i}(x):\n    a = x + 1\n    b = a * 2\n    c = b - 3\n    return c\n\n"
        ));
    }
    fs::write(&f, &src).unwrap();
    let out1 = drip.read_stdout(&f);
    assert!(out1.contains("[DRIP: full read"), "baseline: {out1}");

    // Touch 7 functions far apart so the diff has 7 dispersed hunks.
    let mut mutated = src.clone();
    for i in [1usize, 4, 7, 10, 13, 16, 18] {
        mutated = mutated.replace(
            &format!("def fn_{i}(x):\n    a = x + 1"),
            &format!("def fn_{i}(x):\n    a = x + 999  # touched"),
        );
    }
    fs::write(&f, &mutated).unwrap();

    let out2 = drip.read_stdout(&f);
    assert!(
        out2.contains("diff complexity:"),
        "expected complex-diff fallback header, got: {out2}"
    );
    assert!(
        out2.contains("hunks"),
        "fallback header must surface hunk count, got: {out2}"
    );
}

#[test]
fn rust_first_read_compresses_long_functions() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("lib.rs");
    let mut src = String::from("//! generated for test\n\nuse std::io;\n\n");
    for n in 0..5 {
        src.push_str(&format!(
            "pub fn handler_{n}(req: Request) -> Response {{\n    \
                 let body = req.body;\n    \
                 let header = body.header.clone();\n    \
                 let payload = body.payload.clone();\n    \
                 let trace_id = body.trace_id.clone();\n    \
                 let parent = body.parent.clone();\n    \
                 let span = body.span.clone();\n    \
                 let value = process(payload);\n    \
                 let logged = log(trace_id, parent, span);\n    \
                 Response::ok(header, value, logged)\n}}\n\n"
        ));
    }
    fs::write(&f, &src).unwrap();
    let out = drip.read_stdout(&f);
    assert!(
        out.contains("(semantic-compressed)"),
        "expected Rust file to compress, got: {out}"
    );
    assert!(out.contains("pub fn handler_0"), "signature missing: {out}");
    assert!(out.contains("DRIP-elided"), "expected elision: {out}");
}

#[test]
fn compressed_read_persists_source_map_in_db() {
    // Step 2 contract: when compression fires on a first read, the
    // resulting compressed→original line map is JSON-encoded and
    // stored on the `reads.source_map` column. Without this column
    // being populated, `drip source-map`, the post-edit elided-region
    // warning, and `drip replay --full` would all have to re-run the
    // compressor — defeating the point of caching the read.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("svc.py");
    fs::write(&f, long_python_module()).unwrap();

    let out = drip.read_stdout(&f);
    assert!(
        out.contains("(semantic-compressed)"),
        "precondition: compression must fire so we have a map to persist: {out}"
    );

    // Pull the row directly. We deliberately use the raw rusqlite path
    // rather than crate internals — this is end-to-end coverage for
    // the column existing, the migration running, and the writer
    // serialising a non-empty JSON array.
    let db_path = drip.data_dir.path().join("sessions.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let canonical = std::fs::canonicalize(&f).unwrap();
    let json: Option<String> = conn
        .query_row(
            "SELECT source_map FROM reads
              WHERE session_id = ?1 AND file_path = ?2",
            rusqlite::params![&drip.session_id, canonical.to_string_lossy()],
            |row| row.get(0),
        )
        .unwrap();
    let json = json.expect("source_map should be NOT NULL when compression fired");
    assert!(json.starts_with('['), "expected JSON array, got: {json}");

    // Decode and sanity-check shape: at least one entry must record an
    // elided body whose original_end > original_start (a body span,
    // not a single-line signature).
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let entries = parsed
        .as_array()
        .expect("source_map should be a JSON array");
    assert!(
        !entries.is_empty(),
        "source_map should have at least one entry, got: {json}"
    );
    let any_elided = entries.iter().any(|e| {
        e.get("elided").and_then(|v| v.as_bool()) == Some(true)
            && e.get("original_end").and_then(|v| v.as_u64())
                > e.get("original_start").and_then(|v| v.as_u64())
    });
    assert!(
        any_elided,
        "expected at least one elided multi-line entry: {json}"
    );
}

#[test]
fn uncompressed_read_leaves_source_map_null() {
    // Negative case: reads that don't trigger compression must NOT
    // write a stub `[]` into the column — NULL is the agreed-upon
    // "no map for this row" sentinel, and downstream code branches
    // on `Option<SourceMap>` rather than `is_empty()`.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("tiny.py");
    fs::write(&f, "def hi():\n    return 1\n").unwrap();
    drip.read_stdout(&f);

    let db_path = drip.data_dir.path().join("sessions.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let canonical = std::fs::canonicalize(&f).unwrap();
    let json: Option<String> = conn
        .query_row(
            "SELECT source_map FROM reads
              WHERE session_id = ?1 AND file_path = ?2",
            rusqlite::params![&drip.session_id, canonical.to_string_lossy()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        json.is_none(),
        "uncompressed reads must leave source_map NULL, got: {json:?}"
    );
}
