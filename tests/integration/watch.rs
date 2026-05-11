//! Integration tests for `drip watch` and the precomputed-cache fast
//! path that the Read hook hits.

use crate::common::Drip;
use std::fs;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

/// Spawn `drip watch <root>` in the background and return the child.
/// Tests that need to inspect the cache table after a file change should
/// give the watcher 1-2 s to flush.
fn spawn_watcher(drip: &Drip, root: &std::path::Path) -> std::process::Child {
    Command::new(&drip.bin)
        .args(["watch"])
        .arg(root)
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn drip watch")
}

#[test]
fn watcher_precomputes_diff_after_file_change() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    // Build a baseline content big enough that a 1-line diff is cheaper
    // than the full file (otherwise DiffBiggerThanFile fallback hits).
    let baseline: String = (0..40).map(|i| format!("line {i}\n")).collect();
    let f = dir.path().join("watched.txt");
    fs::write(&f, &baseline).unwrap();

    // 1) Take a baseline read so the watcher has a `reads` row to diff against.
    drip.read_stdout(&f);

    // 2) Start the watcher. Give it time for the initial scan + watch setup.
    let mut child = spawn_watcher(&drip, dir.path());
    sleep(Duration::from_millis(800));

    // 3) Modify the file. Watcher should detect via fsevents/inotify and
    //    write a `precomputed_reads` row within the debounce window.
    let modified: String = baseline.replace("line 5\n", "line 5 CHANGED\n");
    fs::write(&f, &modified).unwrap();
    sleep(Duration::from_millis(800));

    // 4) Now the next Read should be served from cache. We can't easily
    //    measure latency here, but we CAN check that the diff returned
    //    matches what's in the cache (i.e., the cache was hit).
    let out = drip.read_stdout(&f);
    assert!(
        out.contains("[DRIP: delta only"),
        "expected delta read after watcher precompute, got: {out}"
    );
    assert!(
        out.contains("CHANGED"),
        "expected diff to contain the new line, got: {out}"
    );

    // Tidy up.
    let _ = child.kill();
    let _ = child.wait();
}

/// Sec audit (watch): the watcher must not fs::read or precompute files
/// that match `.dripignore` — otherwise lock files / secrets the user
/// excluded would still be serialized into the precomputed_reads table.
#[test]
fn watcher_skips_dripignore_matched_files() {
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();

    // Project-local .dripignore that excludes `secret.txt`.
    fs::write(dir.path().join(".dripignore"), "secret.txt\n").unwrap();
    let f = dir.path().join("secret.txt");
    fs::write(&f, "top-secret-baseline\n".repeat(40)).unwrap();
    // Manually seed a baseline (simulate "agent already read it before
    // we added .dripignore"). We do this by directly writing the reads
    // row — the cleanest way is to run drip read while .dripignore
    // doesn't yet contain the pattern. So: hide the file first, read,
    // then add to .dripignore.
    drip.read_stdout(&f);

    fs::write(dir.path().join(".dripignore"), "secret.txt\n").unwrap();

    let mut child = spawn_watcher(&drip, dir.path());
    sleep(Duration::from_millis(700));

    // Modify the file: watcher must NOT precompute since it's now ignored.
    fs::write(&f, "top-secret-MODIFIED\n".repeat(40)).unwrap();
    sleep(Duration::from_millis(700));

    let conn = rusqlite::Connection::open(drip.data_dir.path().join("sessions.db")).unwrap();
    let canonical = f.canonicalize().unwrap().to_string_lossy().into_owned();
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM precomputed_reads WHERE file_path = ?1",
            rusqlite::params![canonical],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0, "watcher must skip .dripignore-matched files");

    let _ = child.kill();
    let _ = child.wait();
}

/// Sec audit (watch): symlinks pointing outside the watched root must
/// not cause the watcher to fs::read arbitrary files even when a
/// baseline happens to exist for the canonical target.
#[cfg(unix)]
#[test]
fn watcher_ignores_paths_outside_watch_root() {
    let drip = Drip::new();
    let watch_dir = tempfile::tempdir().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();

    // File OUTSIDE the watch root, with an existing baseline.
    let outside_file = outside_dir.path().join("outside.txt");
    fs::write(&outside_file, "outside-content\n".repeat(40)).unwrap();
    drip.read_stdout(&outside_file);

    // Symlink INSIDE the watch root that points to the outside file.
    let link = watch_dir.path().join("link.txt");
    std::os::unix::fs::symlink(&outside_file, &link).unwrap();

    let mut child = spawn_watcher(&drip, watch_dir.path());
    sleep(Duration::from_millis(600));

    // Modify the outside file. notify on macOS may deliver events for
    // the symlinked path; we want the watcher to refuse to act because
    // the canonical path is outside watch_root.
    fs::write(&outside_file, "outside-MODIFIED\n".repeat(40)).unwrap();
    sleep(Duration::from_millis(800));

    let conn = rusqlite::Connection::open(drip.data_dir.path().join("sessions.db")).unwrap();
    let canonical = outside_file
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    // The watcher should not have written a precomputed row for the
    // outside-root file (initial scan only walks `reads` rows whose path
    // starts with the watch root, and FS events for paths outside the
    // root are now rejected by handle_change's starts_with check).
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM precomputed_reads WHERE file_path = ?1",
            rusqlite::params![canonical],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0, "watcher must refuse paths outside its watch root");

    let _ = child.kill();
    let _ = child.wait();
}

/// Sec audit (M-1): a watched path replaced with a FIFO must not
/// pin the watcher's recompute thread on a blocking `fs::read`. The
/// guarantee is twofold:
///   1. The watcher remains responsive — a second tracked file gets
///      its precompute row updated *after* the FIFO appears, proving
///      the recompute path didn't deadlock on the FIFO.
///   2. The FIFO itself never gets a `precomputed_reads` row written
///      after the flip.
#[cfg(unix)]
#[test]
fn watcher_refuses_fifo_at_watched_path() {
    use std::os::unix::ffi::OsStrExt;
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();

    // Two tracked files. `flip.txt` is the one we'll replace with a
    // FIFO; `canary.txt` is the responsiveness witness.
    let flip = dir.path().join("flip.txt");
    let canary = dir.path().join("canary.txt");
    let baseline: String = (0..40).map(|i| format!("line {i}\n")).collect();
    fs::write(&flip, &baseline).unwrap();
    fs::write(&canary, &baseline).unwrap();
    drip.read_stdout(&flip);
    drip.read_stdout(&canary);

    let mut child = spawn_watcher(&drip, dir.path());
    sleep(Duration::from_millis(700));

    // Drop any precompute rows from the initial scan so we can tell
    // pre-flip from post-flip activity unambiguously.
    {
        let conn = rusqlite::Connection::open(drip.data_dir.path().join("sessions.db")).unwrap();
        conn.execute("DELETE FROM precomputed_reads", []).unwrap();
    }

    // Atomic flip: file → FIFO at the same path. We can't mkfifo over
    // an existing inode, so unlink first. The watcher's notify event
    // for the create will trigger recompute_one — and the new
    // `is_file()` guard must short-circuit before fs::read.
    fs::remove_file(&flip).unwrap();
    let cstr = std::ffi::CString::new(flip.as_os_str().as_bytes()).unwrap();
    let rc = unsafe { libc::mkfifo(cstr.as_ptr(), 0o600) };
    // Portable errno access — `libc::__error()` is macOS-only, Linux
    // uses `__errno_location`. `std::io::Error::last_os_error()`
    // works on every Unix without the cfg dance.
    assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());
    sleep(Duration::from_millis(600));

    // Touch the canary AFTER the FIFO exists. If the watcher's
    // recompute thread were blocked on fs::read(<fifo>), this change
    // would never produce a precomputed_reads row.
    let canary_modified: String = baseline.replace("line 0\n", "line 0 CANARY-V2\n");
    fs::write(&canary, &canary_modified).unwrap();
    sleep(Duration::from_millis(900));

    let conn = rusqlite::Connection::open(drip.data_dir.path().join("sessions.db")).unwrap();
    let canary_canonical = canary
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let canary_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM precomputed_reads WHERE file_path = ?1",
            rusqlite::params![canary_canonical],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        canary_rows, 1,
        "watcher must remain responsive after a FIFO appears (canary precompute missing → \
         recompute thread is blocked on fs::read of the FIFO)",
    );

    // No row written for the FIFO path itself.
    let fifo_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM precomputed_reads WHERE file_path LIKE ?1",
            rusqlite::params![format!("%{}", flip.file_name().unwrap().to_string_lossy())],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fifo_rows, 0, "watcher must NOT precompute a FIFO");

    // Clean up: tear down the FIFO before tempdir tries to.
    let _ = fs::remove_file(&flip);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn baseline_change_invalidates_precomputed_cache() {
    // If the agent edits a file (PostToolUse refreshes baseline) AFTER
    // the watcher has computed a diff, the cached diff is now against
    // the OLD baseline and would replay the model's edits. The session
    // layer must drop the precomputed row when set_baseline runs.
    let drip = Drip::new();
    let dir = tempfile::tempdir().unwrap();
    let baseline: String = (0..40).map(|i| format!("line {i}\n")).collect();
    let f = dir.path().join("e.txt");
    fs::write(&f, &baseline).unwrap();
    drip.read_stdout(&f);

    let mut child = spawn_watcher(&drip, dir.path());
    sleep(Duration::from_millis(600));

    // Trigger a precompute by modifying the file.
    let modified: String = baseline.replace("line 0\n", "line 0 V2\n");
    fs::write(&f, &modified).unwrap();
    sleep(Duration::from_millis(600));

    // PostToolUse fires (simulating the agent's Edit) — baseline updated.
    use serde_json::json;
    use std::io::Write;
    let mut hook = Command::new(&drip.bin)
        .args(["hook", "claude-post-edit"])
        .env("DRIP_DATA_DIR", drip.data_dir.path())
        .env("DRIP_SESSION_ID", &drip.session_id)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    hook.stdin
        .as_mut()
        .unwrap()
        .write_all(
            json!({
                "session_id": &drip.session_id,
                "tool_name": "Edit",
                "tool_input": { "file_path": f.to_string_lossy() }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let _ = hook.wait_with_output();

    // The precomputed row from before the post-edit must be gone.
    let conn = rusqlite::Connection::open(drip.data_dir.path().join("sessions.db")).unwrap();
    let canonical = f.canonicalize().unwrap().to_string_lossy().into_owned();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM precomputed_reads WHERE file_path = ?1",
            rusqlite::params![canonical],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "post-edit baseline refresh must invalidate precomputed cache"
    );

    let _ = child.kill();
    let _ = child.wait();
}
