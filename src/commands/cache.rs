//! `drip cache gc` and `drip cache stats` — administer the file-cache
//! directory introduced by hybrid storage.
//!
//! The DB never references blobs that aren't reachable through some
//! `reads.content_storage = 'file'` row, so GC is just "list the
//! directory, drop anything whose hash isn't in the active set". Stats
//! summarise inline-vs-cache byte counts and the dedup hit rate.

use crate::core::cache;
use crate::core::session::Session;
use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;

#[derive(Debug, Default, serde::Serialize)]
pub struct GcReport {
    pub removed: u64,
    pub bytes_freed: u64,
    pub kept: u64,
    pub kept_bytes: u64,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct CacheStats {
    /// Effective threshold from `DRIP_INLINE_MAX_BYTES`, in bytes.
    pub inline_max_bytes: u64,
    /// `reads` rows with `content_storage = 'inline'`.
    pub inline_rows: i64,
    /// SUM(LENGTH(content)) over inline rows.
    pub inline_bytes: i64,
    /// `reads` rows with `content_storage = 'file'`.
    pub file_rows: i64,
    /// `reads` rows that point at a blob whose `.bin` is currently on
    /// disk. Differs from `file_rows` only when a blob has been hand-
    /// deleted or `drip cache gc` ran between writes.
    pub linked_file_rows: i64,
    /// Distinct blobs referenced by `reads`. `file_rows -
    /// unique_hashes` is the dedup count (rows pointing at a shared
    /// blob).
    pub unique_hashes: i64,
    /// Files in the cache directory.
    pub cache_files: u64,
    pub cache_size_bytes: u64,
    /// On-disk size of `sessions.db` (the WAL companion is not
    /// counted; it's checkpoint-bounded and noisy).
    pub db_size_bytes: u64,
    /// Cache files whose hash has no `reads` row pointing at them —
    /// `drip cache gc` will reclaim these.
    pub orphan_files: u64,
    pub orphan_bytes: u64,
    /// Number of `reads` rows that share a blob with at least one
    /// other row (= `file_rows - unique_hashes`, surfaced for
    /// readability).
    pub dedup_savings: i64,
    /// Inline rows whose `content` payload exceeds the *current*
    /// `DRIP_INLINE_MAX_BYTES`. Non-zero typically means the DB was
    /// populated under a higher threshold (or by a v1 binary) and
    /// `drip cache compact` would shrink it.
    pub compactable_rows: i64,
    /// Bytes those rows currently occupy inline in the DB — the rough
    /// upper bound on what `drip cache compact` would reclaim.
    pub compactable_bytes: i64,
}

pub fn run_gc() -> Result<String> {
    let session = Session::open_readonly()?;
    let active: HashSet<String> = active_blob_hashes(&session)?;

    let data_dir = crate::core::session::data_dir()?;
    let cache_dir = cache::cache_dir(&data_dir);
    let mut report = GcReport::default();

    if !cache_dir.exists() {
        return Ok(render_gc(&report, &cache_dir));
    }

    for entry in
        std::fs::read_dir(&cache_dir).with_context(|| format!("listing cache dir {cache_dir:?}"))?
    {
        let entry = entry?;
        let path = entry.path();
        let Some(hash) = blob_hash_from_path(&path) else {
            continue;
        };
        // `entry.metadata()` follows symlinks (returns the *target*'s
        // metadata, including its size). For the cache dir that's a
        // confused-deputy hazard: a symlink → /etc/passwd would be
        // counted as "a 7 KB cache blob" in the kept/freed totals.
        // `entry.file_type()` does NOT follow, so we skip symlinks
        // outright — they shouldn't exist in a well-behaved cache,
        // and `read_blob` rejects them anyway.
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            // Remove the link itself (remove_file does NOT follow);
            // it was either planted by an attacker or left behind by
            // a refactor. Either way it's not a legitimate blob.
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if active.contains(&hash) {
            report.kept += 1;
            report.kept_bytes += size;
        } else {
            std::fs::remove_file(&path).with_context(|| format!("removing orphan {path:?}"))?;
            report.removed += 1;
            report.bytes_freed += size;
        }
    }

    Ok(render_gc(&report, &cache_dir))
}

pub fn run_stats() -> Result<String> {
    let stats = collect_stats()?;
    Ok(render_stats(&stats))
}

pub fn collect_stats() -> Result<CacheStats> {
    let session = Session::open_readonly()?;
    let mut s = CacheStats {
        inline_max_bytes: cache::inline_max_bytes() as u64,
        ..CacheStats::default()
    };

    let (inline_rows, inline_bytes): (i64, i64) = session
        .conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(content)), 0)
             FROM reads WHERE content_storage = 'inline'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    s.inline_rows = inline_rows;
    s.inline_bytes = inline_bytes;

    let (file_rows, unique_hashes): (i64, i64) = session
        .conn
        .query_row(
            "SELECT COUNT(*), COUNT(DISTINCT content_hash)
             FROM reads WHERE content_storage = 'file'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    s.file_rows = file_rows;
    s.unique_hashes = unique_hashes;
    s.dedup_savings = (file_rows - unique_hashes).max(0);

    let data_dir = crate::core::session::data_dir()?;
    let cache_dir = cache::cache_dir(&data_dir);
    let active = active_blob_hashes(&session)?;

    if cache_dir.exists() {
        for entry in std::fs::read_dir(&cache_dir)?.flatten() {
            let path = entry.path();
            let Some(hash) = blob_hash_from_path(&path) else {
                continue;
            };
            // Skip symlinks: see `run_gc` for the rationale. Reporting
            // a symlink target's size as part of "cache size" would
            // mislead the user (and on a malicious link could
            // disclose the existence of arbitrary files via the size).
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if !ft.is_file() {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            s.cache_files += 1;
            s.cache_size_bytes += size;
            if !active.contains(&hash) {
                s.orphan_files += 1;
                s.orphan_bytes += size;
            }
        }
    }
    s.linked_file_rows = active.len() as i64;

    let db_path = data_dir.join("sessions.db");
    s.db_size_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

    // Inline rows that *would* now go to the cache if they were
    // re-written. SQLite's LENGTH() is O(stored size) on TEXT so this
    // is fast; the comparison uses the current threshold (which the
    // user may have tuned since the rows were written).
    let limit_i64 = i64::try_from(cache::inline_max_bytes()).unwrap_or(i64::MAX);
    let (compactable_rows, compactable_bytes): (i64, i64) = session
        .conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(content)), 0)
             FROM reads
             WHERE content_storage = 'inline' AND LENGTH(content) > ?1",
            rusqlite::params![limit_i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    s.compactable_rows = compactable_rows;
    s.compactable_bytes = compactable_bytes;

    Ok(s)
}

#[derive(Debug, Default, serde::Serialize)]
pub struct CompactReport {
    /// `reads` rows whose oversized inline payload was hoisted to
    /// the file cache.
    pub rows_moved: i64,
    /// Bytes copied out of `reads.content` and into the cache. The
    /// SQLite file shrinks by approximately this much after VACUUM.
    pub bytes_moved: i64,
    /// `sessions.db` size before / after the operation. The delta is
    /// what `du`-style readers will see.
    pub db_size_before: u64,
    pub db_size_after: u64,
}

pub fn run_compact() -> Result<String> {
    use rusqlite::params;

    // Need a writable connection — readonly opens are for inspection
    // and would fail the UPDATE / VACUUM below.
    let session = crate::core::session::Session::open()?;
    let data_dir = crate::core::session::data_dir()?;
    let db_path = data_dir.join("sessions.db");
    let limit = cache::inline_max_bytes();
    let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut report = CompactReport {
        db_size_before: std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0),
        ..CompactReport::default()
    };

    // Pull all oversized inline rows in one shot. The list is bounded
    // by the user's actual data; in pathological cases it can be tens
    // of thousands of rows, but each row is at most a couple hundred
    // KB and we only hold (hash, length, content) for one row at a
    // time inside the loop.
    let mut stmt = session.conn.prepare(
        "SELECT session_id, file_path, content_hash
         FROM reads
         WHERE content_storage = 'inline' AND LENGTH(content) > ?1",
    )?;
    let candidates: Vec<(String, String, String)> = stmt
        .query_map(params![limit_i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    if candidates.is_empty() {
        // VACUUM isn't free; skip when there's nothing to compact.
        report.db_size_after = report.db_size_before;
        return Ok(render_compact(&report, /*vacuumed=*/ false));
    }

    for (session_id, file_path, content_hash) in &candidates {
        // Re-fetch the content per row so we don't hold the whole
        // working set in memory at once.
        let content: String = session.conn.query_row(
            "SELECT content FROM reads WHERE session_id = ?1 AND file_path = ?2",
            params![session_id, file_path],
            |r| r.get(0),
        )?;
        let bytes = content.len() as i64;
        cache::write_blob(&data_dir, content_hash, content.as_bytes())?;
        session.conn.execute(
            "UPDATE reads
             SET content = '', content_storage = 'file'
             WHERE session_id = ?1 AND file_path = ?2",
            params![session_id, file_path],
        )?;
        report.rows_moved += 1;
        report.bytes_moved += bytes;
    }

    // VACUUM is the only way SQLite actually returns disk to the OS —
    // without it the freed pages just sit in the file. In WAL mode
    // VACUUM does its work but the file-size stat reflects the new
    // size only after the WAL is checkpointed back into the main DB
    // (and then drop the connection so the OS-level file metadata
    // catches up). Without these two steps the report would show a
    // post-size identical to pre, even though the next `drip cache
    // stats` invocation reports the correct shrunk size.
    session.conn.execute("VACUUM", [])?;
    let _: i64 = session
        .conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
        .unwrap_or(0);
    drop(session);

    report.db_size_after = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    Ok(render_compact(&report, /*vacuumed=*/ true))
}

fn render_compact(r: &CompactReport, vacuumed: bool) -> String {
    let mut out = String::new();
    out.push_str("DRIP cache compact\n");
    if r.rows_moved == 0 {
        out.push_str("  Nothing to do — no inline rows above the current threshold.\n");
        return out;
    }
    out.push_str(&format!(
        "  Compacted: {} row(s), {} hoisted to cache\n",
        r.rows_moved,
        format_bytes(r.bytes_moved.max(0) as u64),
    ));
    if vacuumed {
        let delta = r.db_size_before.saturating_sub(r.db_size_after);
        out.push_str(&format!(
            "  sessions.db: {} → {} (reclaimed {})\n",
            format_bytes(r.db_size_before),
            format_bytes(r.db_size_after),
            format_bytes(delta),
        ));
    }
    out
}

fn active_blob_hashes(session: &Session) -> Result<HashSet<String>> {
    // Union the per-session `reads` table and the cross-session
    // `file_registry`. Without this, `drip cache gc` would happily
    // delete a blob that the registry still points at — agents
    // would then start a new session, look up the file, and hit a
    // missing-blob fallback.
    let mut stmt = session.conn.prepare(
        "SELECT content_hash FROM reads          WHERE content_storage = 'file'
         UNION
         SELECT content_hash FROM file_registry  WHERE content_storage = 'file'",
    )?;
    let rows: HashSet<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

fn blob_hash_from_path(path: &Path) -> Option<String> {
    if path.extension().and_then(|s| s.to_str()) != Some("bin") {
        return None;
    }
    let stem = path.file_stem().and_then(|s| s.to_str())?;
    // SHA-256 hex = 64 chars. Reject anything else as not-our-file.
    if stem.len() == 64 && stem.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(stem.to_string())
    } else {
        None
    }
}

fn render_gc(r: &GcReport, dir: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!("DRIP cache GC ({})\n", dir.display()));
    out.push_str(&format!(
        "  Removed: {} file(s), {} freed\n",
        r.removed,
        format_bytes(r.bytes_freed),
    ));
    out.push_str(&format!(
        "  Kept:    {} file(s), {} in use\n",
        r.kept,
        format_bytes(r.kept_bytes),
    ));
    out
}

fn render_stats(s: &CacheStats) -> String {
    let mode = if s.inline_max_bytes == usize::MAX as u64 {
        "all-inline (DRIP_INLINE_MAX_BYTES=-1)".to_string()
    } else {
        format!("hybrid (inline ≤ {})", format_bytes(s.inline_max_bytes))
    };
    let mut out = String::new();
    out.push_str("DRIP Cache Stats\n");
    out.push_str(&format!("  Mode             : {mode}\n"));
    out.push_str(&format!(
        "  Inline rows      : {} ({} stored in DB)\n",
        s.inline_rows,
        format_bytes(s.inline_bytes.max(0) as u64),
    ));
    out.push_str(&format!(
        "  Cached files     : {} ({} on disk, {} unique blobs)\n",
        s.cache_files,
        format_bytes(s.cache_size_bytes),
        s.unique_hashes,
    ));
    out.push_str(&format!(
        "  Dedup savings    : {} row(s) sharing a blob\n",
        s.dedup_savings,
    ));
    out.push_str(&format!(
        "  Orphan blobs     : {} ({}) — run `drip cache gc` to reclaim\n",
        s.orphan_files,
        format_bytes(s.orphan_bytes),
    ));
    out.push_str(&format!(
        "  sessions.db size : {}\n",
        format_bytes(s.db_size_bytes),
    ));
    if s.compactable_rows > 0 {
        out.push('\n');
        out.push_str(&format!(
            "  ⚠ Compactable     : {} inline row(s) over the threshold ({})\n",
            s.compactable_rows,
            format_bytes(s.compactable_bytes.max(0) as u64),
        ));
        out.push_str(
            "    Run `drip cache compact` to hoist them to the file cache and VACUUM the DB.\n",
        );
    }
    out
}

fn format_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let n_f = n as f64;
    if n_f >= GB {
        format!("{:.2} GB", n_f / GB)
    } else if n_f >= MB {
        format!("{:.2} MB", n_f / MB)
    } else if n_f >= KB {
        format!("{:.1} KB", n_f / KB)
    } else {
        format!("{n} B")
    }
}
