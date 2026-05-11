//! Hybrid content storage: small files inline in SQLite, large files in
//! `<DRIP_DATA_DIR>/cache/<sha256>.bin`. Verifies the threshold logic,
//! deduplication on identical content, missing-cache fallback,
//! `drip cache gc` / `drip cache stats`, and that the diff/unchanged
//! contract is unaffected by the storage choice.

use crate::common::Drip;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn cache_dir(drip: &Drip) -> PathBuf {
    drip.data_dir.path().join("cache")
}

fn cache_file_count(drip: &Drip) -> usize {
    let dir = cache_dir(drip);
    if !dir.exists() {
        return 0;
    }
    fs::read_dir(&dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("bin"))
                .count()
        })
        .unwrap_or(0)
}

fn list_cache_paths(drip: &Drip) -> Vec<PathBuf> {
    let dir = cache_dir(drip);
    if !dir.exists() {
        return Vec::new();
    }
    fs::read_dir(&dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("bin"))
                .collect()
        })
        .unwrap_or_default()
}

fn write_file(path: &Path, content: &str) {
    fs::write(path, content).unwrap();
}

// ─── Threshold-based routing ───────────────────────────────────────

#[test]
fn small_file_stays_inline_no_cache_file_created() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("small.txt");
    write_file(&f, &"x\n".repeat(100)); // 200 bytes, well under 32 KB

    drip.read_stdout(&f);
    assert_eq!(
        cache_file_count(&drip),
        0,
        "small file must NOT spawn a cache/<hash>.bin",
    );
}

#[test]
fn large_file_creates_a_cache_blob() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("big.txt");
    // 50 KB > default 32 KB threshold
    write_file(&f, &"line of content here\n".repeat(2500));

    let mut cmd = drip.cmd();
    cmd.arg("read").arg(&f);
    let o = cmd.output().unwrap();
    assert!(o.status.success());

    assert_eq!(
        cache_file_count(&drip),
        1,
        "large file must produce exactly one cache/<hash>.bin",
    );

    // The cache file should contain the source bytes verbatim.
    let cached = list_cache_paths(&drip);
    let body = fs::read_to_string(&cached[0]).unwrap();
    assert_eq!(body, fs::read_to_string(&f).unwrap());

    // Permissions: 0600 on Unix (defense in depth — the file lives
    // under a 0700 cache dir and the DB itself is 0600).
    let mode = fs::metadata(&cached[0]).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "cache file must be chmod 0600, got {mode:o}");
    let dir_mode = fs::metadata(cache_dir(&drip)).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "cache dir must be chmod 0700, got {dir_mode:o}"
    );
}

#[test]
fn env_override_zero_pushes_everything_to_cache() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("tiny.txt");
    write_file(&f, "a\n"); // 2 bytes — would normally stay inline

    let mut c = drip.cmd();
    c.env("DRIP_INLINE_MAX_BYTES", "0");
    c.arg("read").arg(&f);
    assert!(c.output().unwrap().status.success());

    assert_eq!(
        cache_file_count(&drip),
        1,
        "DRIP_INLINE_MAX_BYTES=0 must force every read into the cache dir",
    );
}

// ─── Dedup ─────────────────────────────────────────────────────────

#[test]
fn identical_content_two_files_share_one_cache_blob() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let body = "shared content\n".repeat(3000); // > 32 KB
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    write_file(&a, &body);
    write_file(&b, &body);

    drip.read_stdout(&a);
    drip.read_stdout(&b);

    // Two `reads` rows, but the cache is keyed by content_hash so
    // there's only one .bin on disk — that's the whole win of
    // hash-addressed storage.
    assert_eq!(
        cache_file_count(&drip),
        1,
        "identical-content files must share one cache blob",
    );
}

// ─── Missing-cache fallback ────────────────────────────────────────

#[test]
fn missing_cache_file_falls_back_to_full_read() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("rehydrate.txt");
    write_file(&f, &"L\n".repeat(20_000)); // > 32 KB

    let first = drip.read_stdout(&f);
    assert!(first.contains("[DRIP: full read"), "first: {first}");

    // Manually delete every cache blob — simulates user `rm` or a
    // partial restore. The next read MUST NOT crash; it should
    // gracefully treat the file as freshly seen.
    for p in list_cache_paths(&drip) {
        fs::remove_file(p).unwrap();
    }

    let second = drip.read_stdout(&f);
    assert!(
        second.contains("[DRIP: full read"),
        "missing cache must trigger fall-back to full read, got: {second}",
    );

    // Cache blob is rewritten on the rehydrate read.
    assert_eq!(cache_file_count(&drip), 1);
}

// ─── Functional parity with inline ─────────────────────────────────

#[test]
fn diff_works_identically_when_content_is_in_cache_file() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("evolves.txt");
    let v1: String = (0..2500).map(|i| format!("line {i}\n")).collect(); // > 32 KB
    write_file(&f, &v1);

    let first = drip.read_stdout(&f);
    assert!(first.contains("[DRIP: full read"), "first: {first}");

    // One-line edit deep into the file. The diff is computed by
    // `differ::diff(prev, new)` — `prev` lives in the cache blob,
    // not inline. If the cache load is broken, we'd get 'first read'
    // again instead of a delta.
    let v2 = v1.replace("line 1234\n", "LINE_1234_MUTATED\n");
    write_file(&f, &v2);

    let second = drip.read_stdout(&f);
    assert!(
        second.contains("[DRIP: delta only"),
        "expected delta — cache-backed baseline failed to load? got: {second}",
    );
    assert!(second.contains("-line 1234"));
    assert!(second.contains("+LINE_1234_MUTATED"));
}

#[test]
fn unchanged_works_identically_when_baseline_is_in_cache() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("stable.txt");
    write_file(&f, &"x\n".repeat(20_000));

    drip.read_stdout(&f);
    let second = drip.read_stdout(&f);
    assert!(
        second.contains("unchanged"),
        "second read with no edits must report unchanged: {second}",
    );
}

// ─── drip cache gc ─────────────────────────────────────────────────

#[test]
fn cache_gc_removes_orphans_and_keeps_active() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();

    // Active blob: a real `reads` row will reference it.
    let active = dir.path().join("active.txt");
    write_file(&active, &"A\n".repeat(20_000));
    drip.read_stdout(&active);

    // Orphan blob: drop a `.bin` that no `reads` row points to.
    let cache = cache_dir(&drip);
    let orphan = cache.join("0000000000000000000000000000000000000000000000000000000000000000.bin");
    fs::write(&orphan, b"orphaned content").unwrap();

    assert!(orphan.exists());
    assert_eq!(cache_file_count(&drip), 2);

    let mut c = drip.cmd();
    c.args(["cache", "gc"]);
    let o = c.output().unwrap();
    assert!(
        o.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&o.stderr)
    );
    let report = String::from_utf8_lossy(&o.stdout);
    assert!(
        report.contains("1 file") || report.contains("removed"),
        "gc should report what it removed, got: {report}",
    );

    assert!(!orphan.exists(), "orphan blob must be deleted");
    assert_eq!(cache_file_count(&drip), 1, "active blob must survive GC",);
}

#[test]
fn purge_drops_cache_blobs_of_expired_sessions() {
    // Regression: purge_stale_sessions used to delete the `reads`
    // rows of expired sessions but left the corresponding `.bin`
    // files on disk, so users had to remember `drip cache gc` to
    // reclaim them. Now the purge cascades into the cache directory
    // and removes any blob whose last referencing row just got
    // deleted (dedup-aware: a hash still alive on a surviving
    // session keeps its blob).
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();

    // Populate the cache via a real `drip read` so the file ends up
    // hash-named in `<DRIP_DATA_DIR>/cache/`.
    let big = dir.path().join("big.txt");
    write_file(&big, &"L\n".repeat(20_000));
    drip.read_stdout(&big);
    assert_eq!(
        cache_file_count(&drip),
        1,
        "expected one cache blob after first read"
    );

    // Backdate the session well past SESSION_TTL_SECS so purge will
    // sweep it. The constant lives in core::session and isn't part
    // of the public API; using last_active = 1 (epoch + 1) is a
    // safe lower bound on any sane cutoff.
    let db = drip.data_dir.path().join("sessions.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute(
        "UPDATE sessions SET last_active = 1 WHERE session_id = ?1",
        rusqlite::params![drip.session_id],
    )
    .unwrap();
    drop(conn);

    // Trigger purge by opening a NEW session — Session::open_inner
    // calls purge_stale_sessions on every open. We use a different
    // DRIP_SESSION_ID so the backdated session is the only one
    // eligible for sweep.
    let trigger = dir.path().join("trigger.txt");
    write_file(&trigger, "x\n");
    let o = drip
        .cmd_in_session("purge-trigger")
        .arg("read")
        .arg(&trigger)
        .output()
        .expect("drip read in fresh session");
    assert!(
        o.status.success(),
        "trigger read failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );

    // After session purge, the blob *survives* — the cross-session
    // file_registry (v4) still references it so a future session
    // can offer the "↔ unchanged since last session" hint without
    // re-staging the file. The blob only dies once the registry
    // entry is also gone.
    assert_eq!(
        cache_file_count(&drip),
        1,
        "registry should keep the blob alive after session purge"
    );
    let o = drip.cmd().args(["cache", "stats"]).output().unwrap();
    assert!(o.status.success());
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("Orphan blobs     : 0"),
        "blob is still referenced by the registry, must NOT be flagged orphan: {s}"
    );

    // Now sweep the registry — `--older-than 0s` matches everything,
    // since the rows were just written and have last_seen_at = now.
    // After this and another session open (which re-runs the purge
    // path that sweeps blobs whose final reference just disappeared),
    // the blob should be gone.
    let o = drip
        .cmd()
        .args(["registry", "gc", "--older-than", "0s"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        cache_file_count(&drip),
        0,
        "blob should be reclaimed once both reads + registry references are gone"
    );
}

#[test]
fn upsert_drops_old_blob_when_file_content_changes() {
    // Regression: editing a file that lives in file storage (i.e.
    // > DRIP_INLINE_MAX_BYTES) used to leave the previous blob as an
    // orphan in the cache directory after every re-read with new
    // content. On a heavy dev-workflow (lots of hash churn on >32KB
    // files) the cache grew unboundedly between `drip cache gc` runs
    // — the dominant source of the "75 orphan blobs" warning we saw
    // in the wild.
    //
    // upsert_read_with_compression now snapshots the current row's
    // hash before the upsert and GCs the blob if the new write made
    // it unreferenced. This test verifies one cache file remains
    // after three different content-versions of the same file.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("big.txt");

    write_file(&f, &"v1\n".repeat(20_000));
    drip.read_stdout(&f);
    assert_eq!(cache_file_count(&drip), 1, "first read should write 1 blob");

    write_file(&f, &"v2\n".repeat(20_000));
    drip.read_stdout(&f);
    assert_eq!(
        cache_file_count(&drip),
        1,
        "second read of mutated file should drop the v1 blob"
    );

    write_file(&f, &"v3\n".repeat(20_000));
    drip.read_stdout(&f);
    assert_eq!(
        cache_file_count(&drip),
        1,
        "third read should drop v2 too — only the current version's blob lives"
    );
}

#[test]
fn refresh_drops_orphan_blob() {
    // Regression: `drip refresh <file>` deletes the reads row but
    // used to leave the blob behind. Catches the small leak path
    // for users who refresh frequently after out-of-band edits.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("big.txt");
    write_file(&f, &"x\n".repeat(20_000));
    drip.read_stdout(&f);
    assert_eq!(cache_file_count(&drip), 1);

    let o = drip.cmd().arg("refresh").arg(&f).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));

    // Registry still references the blob (cross-session orientation),
    // so it survives until `registry gc` runs — same contract as the
    // session-purge path.
    assert_eq!(
        cache_file_count(&drip),
        1,
        "registry keeps blob alive after refresh"
    );
    let o = drip
        .cmd()
        .args(["registry", "gc", "--older-than", "0s"])
        .output()
        .unwrap();
    assert!(o.status.success());
    assert_eq!(
        cache_file_count(&drip),
        0,
        "after registry GC the blob is fully reclaimed"
    );
}

#[test]
fn cache_gc_on_empty_dir_is_a_noop() {
    let drip = Drip::new();
    // Don't even open a session; just run GC against a fresh data dir.
    let mut c = drip.cmd();
    c.args(["cache", "gc"]);
    let o = c.output().unwrap();
    assert!(o.status.success());
}

// ─── drip cache stats ──────────────────────────────────────────────

#[test]
fn cache_stats_reports_inline_and_file_breakdown() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();

    let small = dir.path().join("small.txt");
    write_file(&small, "tiny\n");
    drip.read_stdout(&small);

    let big = dir.path().join("big.txt");
    write_file(&big, &"L\n".repeat(20_000));
    drip.read_stdout(&big);

    let mut c = drip.cmd();
    c.args(["cache", "stats"]);
    let o = c.output().unwrap();
    assert!(o.status.success());
    let s = String::from_utf8_lossy(&o.stdout);

    // Numbers and labels both visible.
    assert!(
        s.contains("Inline rows") || s.contains("inline"),
        "got: {s}"
    );
    assert!(
        s.contains("Cached files") || s.contains("cache"),
        "got: {s}"
    );
}

// ─── drip meter --json: storage block ───────────────────────────────

#[test]
fn drip_meter_json_exposes_storage_block() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("big.txt");
    write_file(&f, &"X\n".repeat(20_000));
    drip.read_stdout(&f);

    let o = drip.cmd().arg("meter").arg("--json").output().unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    let storage = v
        .get("storage")
        .expect("meter --json must surface a `storage` block when there's data");
    assert!(storage["cache_files"].as_i64().unwrap() >= 1);
    assert!(storage["cache_size_bytes"].as_i64().unwrap() > 0);
}

// ─── drip cache compact ────────────────────────────────────────────

#[test]
fn cache_compact_hoists_oversized_inline_rows_to_file_cache() {
    // Simulate the user's real-world scenario: an existing DB carries
    // big inline payloads (from a v1 binary, or from a prior threshold
    // value, or from rows whose content grew over time). After
    // `cache compact` they should live as cache blobs and the DB
    // should shrink correspondingly.
    let drip = Drip::new();
    let db_path = drip.data_dir.path().join("sessions.db");

    // Pre-create a v1-style row by hand: huge inline content,
    // content_storage='inline'. We seed via raw SQL so the test
    // doesn't depend on flipping DRIP_INLINE_MAX_BYTES mid-run.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "
        CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE reads (
            session_id      TEXT NOT NULL,
            file_path       TEXT NOT NULL,
            content_hash    TEXT NOT NULL,
            content         TEXT NOT NULL,
            read_at         INTEGER NOT NULL,
            reads_count     INTEGER NOT NULL DEFAULT 1,
            tokens_full     INTEGER NOT NULL,
            tokens_sent     INTEGER NOT NULL,
            content_storage TEXT NOT NULL DEFAULT 'inline',
            PRIMARY KEY (session_id, file_path)
        );
        CREATE TABLE sessions (
            session_id  TEXT PRIMARY KEY,
            started_at  INTEGER NOT NULL,
            last_active INTEGER NOT NULL,
            cwd         TEXT
        );
        INSERT INTO meta(key, value) VALUES ('schema_version', '2');
        ",
    )
    .unwrap();
    // Use `now` for last_active so purge_stale_sessions (2 h TTL)
    // doesn't wipe the seeded data before compact gets to it.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO sessions VALUES ('s1', ?1, ?1, '/tmp')",
        rusqlite::params![now],
    )
    .unwrap();
    // Big enough that VACUUM's reclaim dwarfs the schema overhead
    // even after future v8/v9/… migrations add new tables. Earlier
    // 120 KB was within ~4 SQLite pages of the schema cost, so any
    // additive migration would flip the post>pre assertion.
    let huge: String = "X".repeat(1_200_000); // 1.2 MB, well above 32 KB
    let small: String = "y\n".repeat(10);
    // 64-char hex hashes — `core::cache::is_valid_hash` rejects
    // anything else, so the seed has to look like a real SHA-256.
    // Different prefixes ('a' vs 'b') so the two rows don't collide.
    let huge_hash = "a".repeat(64);
    let small_hash = "b".repeat(64);
    conn.execute(
        "INSERT INTO reads VALUES ('s1', '/tmp/huge.txt', ?1, ?2, ?3, 1, 1, 1, 'inline')",
        rusqlite::params![huge_hash, huge, now],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO reads VALUES ('s1', '/tmp/small.txt', ?1, ?2, ?3, 1, 1, 1, 'inline')",
        rusqlite::params![small_hash, small, now],
    )
    .unwrap();
    drop(conn);

    let pre_size = fs::metadata(&db_path).unwrap().len();

    let mut c = drip.cmd();
    c.args(["cache", "compact"]);
    let o = c.output().unwrap();
    assert!(
        o.status.success(),
        "compact failed: stderr={}",
        String::from_utf8_lossy(&o.stderr),
    );
    let report = String::from_utf8_lossy(&o.stdout);
    assert!(
        report.contains("1 row") || report.contains("Compacted") || report.contains("compact"),
        "compact must report what it moved: {report}",
    );

    // The huge row is now in the cache; the small row stays inline.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let huge_storage: String = conn
        .query_row(
            "SELECT content_storage FROM reads WHERE file_path = '/tmp/huge.txt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let huge_content: String = conn
        .query_row(
            "SELECT content FROM reads WHERE file_path = '/tmp/huge.txt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let small_storage: String = conn
        .query_row(
            "SELECT content_storage FROM reads WHERE file_path = '/tmp/small.txt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);

    assert_eq!(
        huge_storage, "file",
        "huge row must be hoisted to file cache"
    );
    assert!(
        huge_content.is_empty(),
        "hoisted row's content column must be cleared"
    );
    assert_eq!(small_storage, "inline", "small row must stay inline");

    // Cache blob exists.
    assert_eq!(
        cache_file_count(&drip),
        1,
        "compact must materialise the blob on disk",
    );

    // VACUUM ran → DB file actually shrank. SQLite pages and overhead
    // mean we don't reclaim the full 120 KB byte-for-byte (other rows
    // and indexes share pages), so we just assert "meaningfully
    // smaller", not an exact target. The strong guarantees are
    // covered by the storage/content checks above (blob on disk,
    // content column emptied, content_storage flipped).
    let post_size = fs::metadata(&db_path).unwrap().len();
    assert!(
        post_size < pre_size,
        "compact should run VACUUM to reclaim space — pre={pre_size} post={post_size}",
    );
}

#[test]
fn cache_compact_reads_baseline_correctly_after_hoisting() {
    // Functional integrity: after compact, the agent must still see
    // the same baseline content. A subsequent `drip read` of an
    // unchanged file must report `unchanged` (proves the hoisted
    // blob round-trips through get_read).
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("inline_then_compact.txt");
    let body: String = "L\n".repeat(20_000);
    write_file(&f, &body);

    // Force the first read to land inline by raising the threshold
    // above the file size, then read.
    let mut c = drip.cmd();
    c.env("DRIP_INLINE_MAX_BYTES", "10000000"); // 10 MB
    c.args(["read", f.to_str().unwrap()]);
    let o = c.output().unwrap();
    let body1 = String::from_utf8_lossy(&o.stdout);
    assert!(body1.contains("[DRIP: full read"), "first: {body1}");

    // Now compact at the default threshold (32 KB) — the row is over
    // it and should migrate to the cache.
    let o = drip.cmd().args(["cache", "compact"]).output().unwrap();
    assert!(o.status.success());
    assert_eq!(
        cache_file_count(&drip),
        1,
        "compact should produce one blob",
    );

    // Re-read with no edit. Default threshold; baseline is now in cache.
    let body2 = drip.read_stdout(&f);
    assert!(
        body2.contains("unchanged"),
        "post-compact re-read must hit the cached baseline as Unchanged: {body2}",
    );
}

#[test]
fn cache_compact_is_idempotent_when_no_oversize_inline_rows() {
    // Empty / clean DB: compact is a no-op and exits cleanly.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("tiny.txt");
    write_file(&f, "x\n");
    drip.read_stdout(&f);

    let o = drip.cmd().args(["cache", "compact"]).output().unwrap();
    assert!(o.status.success());
    assert_eq!(
        cache_file_count(&drip),
        0,
        "no oversized rows → no blobs created",
    );
}

#[test]
fn cache_stats_hints_when_inline_bloat_detected() {
    // When `reads` contains inline rows individually larger than the
    // current threshold, `drip cache stats` should suggest running
    // `drip cache compact` so the user notices the wasted DB space.
    let drip = Drip::new();
    let db_path = drip.data_dir.path().join("sessions.db");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "
        CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE reads (
            session_id      TEXT NOT NULL,
            file_path       TEXT NOT NULL,
            content_hash    TEXT NOT NULL,
            content         TEXT NOT NULL,
            read_at         INTEGER NOT NULL,
            reads_count     INTEGER NOT NULL DEFAULT 1,
            tokens_full     INTEGER NOT NULL,
            tokens_sent     INTEGER NOT NULL,
            content_storage TEXT NOT NULL DEFAULT 'inline',
            PRIMARY KEY (session_id, file_path)
        );
        CREATE TABLE sessions (
            session_id  TEXT PRIMARY KEY,
            started_at  INTEGER NOT NULL,
            last_active INTEGER NOT NULL,
            cwd         TEXT
        );
        INSERT INTO meta(key, value) VALUES ('schema_version', '2');
        ",
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO sessions VALUES ('s', ?1, ?1, '/tmp')",
        rusqlite::params![now],
    )
    .unwrap();
    let hash = "c".repeat(64);
    conn.execute(
        "INSERT INTO reads VALUES ('s', '/tmp/big.txt', ?1, ?2, ?3, 1, 1, 1, 'inline')",
        rusqlite::params![hash, &"Z".repeat(100_000), now],
    )
    .unwrap();
    drop(conn);

    let o = drip.cmd().args(["cache", "stats"]).output().unwrap();
    let s = String::from_utf8_lossy(&o.stdout);
    assert!(
        s.contains("compact") || s.contains("Compactable"),
        "stats must hint at compactable inline rows: {s}",
    );
}

// ─── Migration v1 → v2 ─────────────────────────────────────────────

#[test]
fn legacy_v1_db_is_migrated_in_place() {
    // Seed a fake v1 sessions.db (no `content_storage` column) and
    // verify drip migrates it on first open without losing data.
    let drip = Drip::new();
    let db_path = drip.data_dir.path().join("sessions.db");

    // Build a minimal v1 schema by hand.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "
        CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE reads (
            session_id   TEXT NOT NULL,
            file_path    TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            content      TEXT NOT NULL,
            read_at      INTEGER NOT NULL,
            reads_count  INTEGER NOT NULL DEFAULT 1,
            tokens_full  INTEGER NOT NULL,
            tokens_sent  INTEGER NOT NULL,
            PRIMARY KEY (session_id, file_path)
        );
        CREATE TABLE sessions (
            session_id   TEXT PRIMARY KEY,
            started_at   INTEGER NOT NULL,
            last_active  INTEGER NOT NULL,
            cwd          TEXT
        );
        INSERT INTO reads
            (session_id, file_path, content_hash, content, read_at,
             reads_count, tokens_full, tokens_sent)
            VALUES ('legacy', '/tmp/x.txt', 'deadbeef', 'legacy body', 1, 1, 10, 10);
        INSERT INTO meta(key, value) VALUES ('schema_version', '1');
        ",
    )
    .unwrap();
    drop(conn);

    // First post-migration read should not fail. It opens the DB,
    // runs the additive migration, and the existing legacy row keeps
    // working (storage='inline' default).
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("post-migrate.txt");
    write_file(&f, "ok\n");
    drip.read_stdout(&f);

    // Confirm the new column exists and the legacy row has the
    // correct default.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let storage: String = conn
        .query_row(
            "SELECT content_storage FROM reads WHERE session_id = 'legacy'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        storage, "inline",
        "legacy rows must default to inline storage post-migration",
    );
}
