//! `drip registry stats` and `drip registry gc` — administer the
//! cross-session file registry introduced by schema v4.
//!
//! `stats` mirrors `drip cache stats` in spirit: counts of known
//! files, breakdown of inline vs. file-cache storage, oldest entry,
//! most-frequently-accessed file. `gc` removes registry entries
//! older than a cutoff, then sweeps any cache blobs they pointed at
//! that no surviving row references.

use crate::core::cache;
use crate::core::session::{unix_now, Session};
use anyhow::Result;
use rusqlite::params;

/// Default age threshold for `drip registry gc` when no
/// `--older-than` is given. 30 days is generous enough that a
/// monthly project re-visit doesn't lose its registry entries.
pub const GC_DEFAULT_AGE_SECS: i64 = 30 * 86_400;

#[derive(Debug, Default, serde::Serialize)]
pub struct RegistryStats {
    pub known_files: i64,
    pub inline_rows: i64,
    pub inline_bytes: i64,
    pub file_rows: i64,
    pub total_reads: i64,
    pub oldest_path: Option<String>,
    pub oldest_age_secs: i64,
    pub most_accessed_path: Option<String>,
    pub most_accessed_reads: i64,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct RegistryGcReport {
    pub age_cutoff_secs: i64,
    pub removed_rows: i64,
    pub removed_blobs: usize,
}

pub fn run_stats() -> Result<String> {
    let s = collect_stats()?;
    Ok(render_stats(&s))
}

pub fn collect_stats() -> Result<RegistryStats> {
    let session = Session::open_readonly()?;
    let mut s = RegistryStats::default();

    // Top-level counts. The whole table is small relative to `reads`
    // (one row per ever-touched file vs. per-session), so a single
    // aggregate query is fine — no per-row materialisation.
    let row: (i64, i64, i64, i64, i64) = session
        .conn
        .query_row(
            "SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN content_storage = 'inline' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN content_storage = 'inline' THEN LENGTH(content) ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN content_storage = 'file'   THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(reads_count), 0)
             FROM file_registry",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap_or((0, 0, 0, 0, 0));
    s.known_files = row.0;
    s.inline_rows = row.1;
    s.inline_bytes = row.2;
    s.file_rows = row.3;
    s.total_reads = row.4;

    // Oldest entry (smallest last_seen_at). Used to surface stale
    // registrations the user might want to clean up.
    let now = unix_now();
    if let Ok((path, age)) = session.conn.query_row(
        "SELECT file_path, ?1 - last_seen_at AS age
         FROM file_registry
         ORDER BY last_seen_at ASC LIMIT 1",
        params![now],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    ) {
        s.oldest_path = Some(path);
        s.oldest_age_secs = age.max(0);
    }

    // Most-accessed file across the lifetime of the registry. Useful
    // signal: the agent's "hot files" — the things it re-reads
    // session after session.
    if let Ok((path, reads)) = session.conn.query_row(
        "SELECT file_path, reads_count
         FROM file_registry
         ORDER BY reads_count DESC LIMIT 1",
        [],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    ) {
        s.most_accessed_path = Some(path);
        s.most_accessed_reads = reads;
    }

    Ok(s)
}

pub fn run_gc(older_than_secs: Option<i64>) -> Result<String> {
    let cutoff_age = older_than_secs.unwrap_or(GC_DEFAULT_AGE_SECS).max(0);
    let session = Session::open_readonly()?;
    let now = unix_now();
    let cutoff = now - cutoff_age;

    // Snapshot blob hashes of the rows we're about to delete so we
    // can sweep their cache files afterwards. `delete_blobs_if_unreferenced`
    // re-checks against the post-DELETE state, so blobs still referenced
    // by surviving registry rows or `reads` rows are left alone.
    let mut stmt = session.conn.prepare(
        "SELECT DISTINCT content_hash FROM file_registry
         WHERE content_storage = 'file'
           AND content_hash != ''
           AND last_seen_at <= ?1",
    )?;
    let doomed: Vec<String> = stmt
        .query_map(params![cutoff], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);

    let removed_rows = session.conn.execute(
        // `<=` rather than `<` so `--older-than 0s` reliably catches
        // rows last seen in the current second. unix_now() is
        // 1-second resolution; the strict-less comparison would
        // otherwise miss everything written within the same tick.
        "DELETE FROM file_registry WHERE last_seen_at <= ?1",
        params![cutoff],
    )? as i64;

    let mut report = RegistryGcReport {
        age_cutoff_secs: cutoff_age,
        removed_rows,
        removed_blobs: 0,
    };
    if !doomed.is_empty() {
        if let Ok(dir) = crate::core::session::data_dir() {
            report.removed_blobs =
                cache::delete_blobs_if_unreferenced(&session.conn, &dir, &doomed).unwrap_or(0);
        }
    }
    Ok(render_gc(&report))
}

fn render_stats(s: &RegistryStats) -> String {
    let mut out = String::new();
    out.push_str("DRIP File Registry\n");
    out.push_str(&format!("  Known files    : {}\n", s.known_files));
    if s.known_files == 0 {
        out.push_str(
            "  (no entries — first read of every file in this session will be 'unknown')\n",
        );
        return out;
    }
    out.push_str(&format!(
        "  Inline rows    : {} ({} stored in DB)\n",
        s.inline_rows,
        format_bytes(s.inline_bytes as u64),
    ));
    out.push_str(&format!("  Cached files   : {}\n", s.file_rows));
    out.push_str(&format!(
        "  Total reads    : {} across all known files\n",
        s.total_reads
    ));
    if let Some(p) = &s.oldest_path {
        out.push_str(&format!(
            "  Oldest entry   : {} ({} ago)\n",
            p,
            format_age(s.oldest_age_secs)
        ));
    }
    if let Some(p) = &s.most_accessed_path {
        out.push_str(&format!(
            "  Most accessed  : {} ({} reads)\n",
            p, s.most_accessed_reads
        ));
    }
    out
}

fn render_gc(r: &RegistryGcReport) -> String {
    let mut out = String::new();
    out.push_str("DRIP registry gc\n");
    out.push_str(&format!(
        "  Age cutoff     : {}\n",
        format_age(r.age_cutoff_secs)
    ));
    out.push_str(&format!("  Removed rows   : {}\n", r.removed_rows));
    out.push_str(&format!(
        "  Removed blobs  : {} (cache .bin files freed)\n",
        r.removed_blobs
    ));
    out
}

fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if n >= MB {
        format!("{:.2} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

fn format_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Parse a human duration string (`30d`, `12h`, `90m`, `45s`) into
/// seconds. Returns `None` for malformed input — the caller falls
/// back on the default cutoff.
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num_part, unit) = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_digit())
        .map(|(i, _)| (&s[..i], &s[i..]))
        .unwrap_or((s, ""));
    let n: i64 = num_part.parse().ok()?;
    let mult: i64 = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    Some(n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_handles_common_formats() {
        assert_eq!(parse_duration("30d"), Some(30 * 86_400));
        assert_eq!(parse_duration("12h"), Some(12 * 3600));
        assert_eq!(parse_duration("45m"), Some(45 * 60));
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("90"), Some(90));
        assert_eq!(parse_duration("nope"), None);
        assert_eq!(parse_duration("30y"), None);
    }
}
