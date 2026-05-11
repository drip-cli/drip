//! `drip watch [path]` — pre-compute diffs in a long-lived process so
//! the hook hot path skips fs::read + sha256 + diff entirely (50ms on
//! big files becomes a single cache lookup).

use crate::core::differ::{self, FileKind};
use crate::core::ignore::Matcher;
use crate::core::session::{
    self, baselines_for_file, baselines_under, set_precomputed_on, Session,
};
use crate::core::tokens;
use anyhow::{Context, Result};
use notify::event::{EventKind, ModifyKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Coalesce editor bursts (rename + truncate + write within ms).
const DEBOUNCE: Duration = Duration::from_millis(150);

/// Re-poll the `reads` table as a safety net for watcher backends that
/// coalesce or drop events around special files / editor rename bursts.
const DEFAULT_RESCAN_INTERVAL: Duration = Duration::from_secs(1);

pub fn run(path: Option<PathBuf>) -> Result<String> {
    let root =
        path.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let root = root
        .canonicalize()
        .with_context(|| format!("canonicalizing watch root {root:?}"))?;
    let root_str = root.to_string_lossy().into_owned();
    let root_owned = root.clone();

    let session = Session::open()?;

    eprintln!("drip watch: scanning baselines under {} …", root.display());
    let initial = baselines_under(&session.conn, &root_str)?;
    eprintln!(
        "drip watch: precomputing {} file(s) up front",
        initial.len()
    );
    for row in &initial {
        let _ = recompute_one(
            &session,
            &row.session_id,
            &row.file_path,
            &row.content_hash,
            &row.content,
            &root_owned,
        );
    }

    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .context("creating fs watcher")?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", root.display()))?;

    eprintln!("drip watch: watching {}  (Ctrl-C to stop)", root.display());

    let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
    let rescan_interval = rescan_interval();
    let mut last_rescan = Instant::now();

    loop {
        // Block briefly for new events; tick out for debounce flushing.
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(event)) => {
                if !relevant_event(&event.kind) {
                    continue;
                }
                for p in event.paths {
                    pending.insert(p, Instant::now());
                }
            }
            Ok(Err(e)) => eprintln!("drip watch: watcher error: {e:?}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Flush debounced events.
        let now = Instant::now();
        let due: Vec<PathBuf> = pending
            .iter()
            .filter(|(_, t)| now.duration_since(**t) >= DEBOUNCE)
            .map(|(p, _)| p.clone())
            .collect();
        for p in due {
            pending.remove(&p);
            handle_change(&session, &p, &root_owned);
        }

        // Periodically rescan for newly tracked files.
        if now.duration_since(last_rescan) >= rescan_interval {
            last_rescan = now;
            let _ = rescan_under(&session, &root_str, &root_owned);
        }
    }
    Ok(String::from("watcher exited"))
}

fn rescan_interval() -> Duration {
    std::env::var("DRIP_WATCH_RESCAN_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_RESCAN_INTERVAL)
}

fn relevant_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Any)
            | EventKind::Modify(ModifyKind::Name(_))
            | EventKind::Remove(_)
    )
}

fn handle_change(session: &Session, p: &Path, watch_root: &Path) {
    let canonical_path = match p.canonicalize() {
        Ok(c) => c,
        Err(_) => {
            // Deleted/renamed away — drop stale precomputed rows.
            let p_str = p.to_string_lossy().into_owned();
            let _ = session.conn.execute(
                "DELETE FROM precomputed_reads WHERE file_path = ?1",
                rusqlite::params![p_str],
            );
            return;
        }
    };
    // Defense in depth: macOS notify can deliver events for symlinked
    // subdirs whose canonical path falls outside the watched root —
    // refuse to fs::read anything outside.
    if !canonical_path.starts_with(watch_root) {
        return;
    }
    let canonical = canonical_path.to_string_lossy().into_owned();
    let rows = match baselines_for_file(&session.conn, &canonical) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("drip watch: lookup failed for {canonical}: {e}");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }
    for row in &rows {
        let _ = recompute_one(
            session,
            &row.session_id,
            &row.file_path,
            &row.content_hash,
            &row.content,
            watch_root,
        );
    }
}

fn rescan_under(session: &Session, root: &str, watch_root: &Path) -> Result<()> {
    let rows = baselines_under(&session.conn, root)?;
    for row in &rows {
        // Skip entries with a fresh cache — saves fs::read on quiet trees.
        let resolved = std::path::PathBuf::from(&row.file_path);
        let meta = match std::fs::metadata(&resolved) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mt = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        let already_fresh: bool = session
            .conn
            .query_row(
                "SELECT 1 FROM precomputed_reads
                 WHERE session_id = ?1 AND file_path = ?2
                    AND file_mtime_ns = ?3 AND file_size = ?4",
                rusqlite::params![row.session_id, row.file_path, mt, meta.len() as i64],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if already_fresh {
            continue;
        }
        let _ = recompute_one(
            session,
            &row.session_id,
            &row.file_path,
            &row.content_hash,
            &row.content,
            watch_root,
        );
    }
    Ok(())
}

fn recompute_one(
    session: &Session,
    session_id: &str,
    file_path: &str,
    baseline_hash: &str,
    baseline_content: &str,
    watch_root: &Path,
) -> Result<()> {
    let resolved = PathBuf::from(file_path);
    // Defense in depth — the initial scan filters by SQL prefix but
    // non-canonical baselines could slip through.
    if !resolved.starts_with(watch_root) {
        return Ok(());
    }
    // `symlink_metadata` so a planted `→ /etc/passwd` doesn't pull
    // the target into the precompute table.
    let lmeta = match std::fs::symlink_metadata(&resolved) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    if lmeta.file_type().is_symlink() && std::env::var_os("DRIP_REJECT_SYMLINKS").is_some() {
        return Ok(());
    }
    let meta = match std::fs::metadata(&resolved) {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    // FIFO would block fs::read forever and pin the recompute thread;
    // char devices report len()==0 and slip past the size cap.
    if !meta.file_type().is_file() {
        return Ok(());
    }
    if meta.len() > crate::core::tracker::HARD_SIZE_CAP_BYTES {
        return Ok(());
    }
    let matcher = Matcher::load_with_root(Some(watch_root));
    if matcher.is_ignored(&resolved) {
        return Ok(());
    }

    let bytes = match std::fs::read(&resolved) {
        Ok(b) => b,
        Err(_) => return Ok(()),
    };
    let kind = differ::classify(&bytes);
    if !matches!(kind, FileKind::Text) {
        return Ok(());
    }
    let new_content = match std::str::from_utf8(&bytes) {
        Ok(s) => s.to_string(),
        Err(_) => return Ok(()),
    };
    let content_hash = session::hash_content(&bytes);
    let new_tokens = tokens::estimate(&new_content);

    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);

    if content_hash == baseline_hash {
        set_precomputed_on(
            &session.conn,
            session_id,
            file_path,
            mtime_ns,
            meta.len() as i64,
            &content_hash,
            &new_content,
            new_tokens,
            0,
            None,
            0,
            baseline_hash,
        )?;
        return Ok(());
    }

    if differ::is_truncated(baseline_content.len(), new_content.len()) {
        return Ok(());
    }
    let label = std::path::Path::new(file_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_path.to_string());
    let diff = match differ::unified_diff(
        &label,
        baseline_content,
        &new_content,
        differ::DEFAULT_CONTEXT,
    ) {
        Some(d) => d,
        None => return Ok(()),
    };
    let delta_tokens = tokens::estimate(&diff);
    if delta_tokens >= new_tokens {
        // Diff-bigger-than-file → FullFallback (uncacheable).
        return Ok(());
    }
    let complexity = differ::analyze_complexity(&diff, new_content.lines().count());
    if differ::is_too_complex(&complexity) {
        // Keep behaviour identical to the foreground read path: a
        // sprawling diff must fall back to a clean full read.
        return Ok(());
    }
    set_precomputed_on(
        &session.conn,
        session_id,
        file_path,
        mtime_ns,
        meta.len() as i64,
        &content_hash,
        &new_content,
        new_tokens,
        delta_tokens,
        Some(&diff),
        1,
        baseline_hash,
    )?;
    Ok(())
}
