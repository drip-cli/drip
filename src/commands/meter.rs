use crate::core::inspect;
use crate::core::session::{
    detect_ghost_pollution, prune_missing_files, unix_now, GhostPollution, PruneReport, Session,
};
use crate::core::term;
use crate::core::tokens;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

pub struct MeterOpts {
    pub history: bool,
    pub graph: bool,
    pub json: bool,
    /// `true` ⇒ single-session report; `false` ⇒ since-install
    /// aggregate (survives the session purge).
    pub session_only: bool,
    /// Explicit session id, only honored when `session_only` is true.
    pub session_id: Option<String>,
    /// Drop `lifetime_per_file` rows for missing files before reporting.
    pub prune: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Lifetime,
    Session,
}

#[derive(Debug, Serialize)]
pub struct PerFile {
    pub file: String,
    pub reads: i64,
    pub tokens_full: i64,
    pub tokens_sent: i64,
    pub tokens_saved: i64,
    pub reduction_pct: u32,
}

#[derive(Debug, Serialize)]
pub struct DayBucket {
    pub day: String,
    pub reads: i64,
    pub tokens_full: i64,
    pub tokens_sent: i64,
    pub tokens_saved: i64,
    pub reduction_pct: u32,
}

#[derive(Debug, Serialize)]
pub struct MeterReport {
    pub scope: Scope,
    pub session_id: String,
    /// In session mode: when this session started.
    /// In lifetime mode: when DRIP was first installed.
    pub started_at: i64,
    /// Lifetime-only: present when the user has run `drip reset --all`
    /// or `drip reset --stats`. When set, `elapsed_secs` is measured
    /// from here (not `started_at`), and the human surface swaps
    /// "Since install" for "Since reset".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reset_at: Option<i64>,
    pub elapsed_secs: i64,
    pub files_tracked: i64,
    pub total_reads: i64,
    /// Distinct edited files. Lifetime-only.
    pub files_edited: i64,
    /// Total post-edit hook fires.
    pub total_edits: i64,
    pub tokens_full: i64,
    pub tokens_sent: i64,
    pub tokens_saved: i64,
    pub reduction_pct: u32,
    /// USD saved at `price_per_mtok` (default Claude Sonnet 4.6 input).
    pub dollars_saved: f64,
    /// Rate used for `dollars_saved` so the JSON is self-describing.
    pub price_per_mtok: f64,
    /// Grams of CO₂e at `co2_g_per_ktok`.
    pub co2_g_saved: f64,
    pub co2_g_per_ktok: f64,
    pub top: Vec<PerFile>,
    pub history: Option<Vec<DayBucket>>,
    /// Lifetime-only nudge: present when a large share of `tokens_full`
    /// comes from files that no longer exist on disk. None on session
    /// reports, when no pollution is detected, or right after `--prune`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ghost_pollution: Option<GhostPollution>,
    /// Session-only: which derivation produced `session_id` (env, git,
    /// pid, cwd). Absent on lifetime reports — strategy is per-session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_strategy: Option<String>,
    /// Branch / `(pid …)` / `(env)` / `(cwd)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_context: Option<String>,
    /// Storage breakdown — lifetime-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<crate::commands::cache::CacheStats>,
    /// Compaction ledger; absent when nothing has been compacted in scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionStats>,
    /// Reads where DRIP detected the file changed out-of-band (cargo
    /// fmt, git pull, non-hooked editor) and shipped full native
    /// content to keep Claude's read-tracker in sync. Absent on
    /// fresh installs / when the counter is 0. Surfaces to users
    /// editing the same repo they're reading — the "why is my %
    /// reduction lower than the README claims" answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_edit_refreshes: Option<ExternalEditStats>,
    /// Set when `--session` (bare) auto-resolved to a different
    /// session than the shell-derived one — typically the agent's
    /// session in the current cwd. Lets the user verify they're
    /// looking at the right scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_picked_session: Option<AutoPickedSession>,
}

#[derive(Debug, Serialize)]
pub struct ExternalEditStats {
    pub count: i64,
    /// `count / total_reads`, rounded. 0 when `total_reads == 0`.
    pub pct_of_reads: u32,
}

/// Surfaced on the human meter + JSON when `drip meter --session`
/// was bare-flagged but the shell-derived session had no reads. The
/// real session that got picked (the agent's) is shown so the user
/// knows what they're looking at — and can pin it explicitly next
/// time with `--session <id>`.
#[derive(Debug, Serialize)]
pub struct AutoPickedSession {
    /// Short, display-friendly version of the picked session id.
    pub session_id: String,
    /// Agent label as `drip sessions` would show it ("Claude Code",
    /// "Codex", custom tag, …). Absent when the row has no agent
    /// recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// `Pid`/`Env`/`Git`/`Cwd` strategy that named the session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
}

/// Compaction ledger surface. Lifetime aggregates across every
/// tracked session; session scope is just this session's row.
#[derive(Debug, Serialize)]
pub struct CompactionStats {
    pub total_compactions: i64,
    /// Most recent unix timestamp; absent when nothing has been compacted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_compaction_at: Option<i64>,
    /// Pre-formatted age string so consumers don't reimplement the humanizer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_compaction_age: Option<String>,
    /// `SUM(reads.tokens_full WHERE context_epoch > 0)` — post-compaction
    /// re-builds that have actually run.
    pub tokens_resent_after_compaction: i64,
}

pub fn run(opts: MeterOpts) -> Result<String> {
    let prune = if opts.prune {
        // Write-capable open — read-only can't DELETE.
        let s = Session::open()?;
        Some(prune_missing_files(&s.conn)?)
    } else {
        None
    };
    let mut report = if opts.session_only {
        collect_session_report(opts.history, opts.session_id.as_deref())?
    } else {
        collect_lifetime_report(opts.history)?
    };

    // Ghost-file detection: lifetime view only, and not when --prune was
    // just run (the prune notice already covers what was wrong).
    if !opts.session_only && prune.is_none() {
        let s = Session::open_readonly()?;
        report.ghost_pollution = detect_ghost_pollution(&s.conn)?;
    }
    if let Some(p) = &prune {
        if opts.json {
            let body = serde_json::to_value(&report)?;
            let envelope = serde_json::json!({
                "prune": prune_to_json(p),
                "report": body,
            });
            return Ok(serde_json::to_string_pretty(&envelope)? + "\n");
        }
    }
    if opts.json {
        return Ok(serde_json::to_string_pretty(&report)? + "\n");
    }
    let mut out = String::new();
    if let Some(p) = &prune {
        out.push_str(&render_prune_notice(p));
        out.push('\n');
    }
    out.push_str(&render_human(&report, opts.graph));
    Ok(out)
}

fn prune_to_json(p: &PruneReport) -> serde_json::Value {
    serde_json::json!({
        "files_pruned": p.files_pruned,
        "reads_reclaimed": p.reads_reclaimed,
        "tokens_full_reclaimed": p.tokens_full_reclaimed,
        "tokens_sent_reclaimed": p.tokens_sent_reclaimed,
        "paths": p.paths,
    })
}

fn render_ghost_hint(g: &GhostPollution) -> String {
    // Disambiguate: percentage is share of `lifetime_per_file`'s
    // tokens_full, not a share of the headline below.
    let body = format!(
        "{} ghost file(s) account for {}% of tracked file tokens — run `drip meter --prune` to drop them.\n",
        g.ghost_files, g.ghost_pct,
    );
    term::yellow(&term::bold(&format!("⚠ {body}")))
}

fn render_prune_notice(p: &PruneReport) -> String {
    if p.files_pruned == 0 {
        return term::dim("Pruned 0 files (lifetime stats already clean).\n").to_string();
    }
    let mut s = String::new();
    s.push_str(&term::green(&term::bold(&format!(
        "Pruned {} file(s) (no longer on disk):\n",
        p.files_pruned
    ))));
    for path in p.paths.iter().take(5) {
        s.push_str(&format!("  · {}\n", term::dim(path)));
    }
    if p.paths.len() > 5 {
        s.push_str(&term::dim(&format!(
            "  · … and {} more\n",
            p.paths.len() - 5
        )));
    }
    s.push_str(&format!(
        "Reclaimed {} tokens_full, {} tokens_sent, {} reads.\n",
        format_compact(p.tokens_full_reclaimed),
        format_compact(p.tokens_sent_reclaimed),
        p.reads_reclaimed,
    ));
    s
}

fn collect_lifetime_report(include_history: bool) -> Result<MeterReport> {
    let session = Session::open_readonly()?;

    let lifetime: Option<(i64, i64, i64, i64, i64)> = session
        .conn
        .query_row(
            "SELECT installed_at, total_reads, tokens_full, tokens_sent,
                    external_edit_refreshes
             FROM lifetime_stats WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;

    let (installed_at, total_reads, tokens_full, tokens_sent, external_edit_refreshes) =
        lifetime.unwrap_or((unix_now(), 0, 0, 0, 0));

    // Reset marker survives `reset --all` (the `meta` table is preserved)
    // and is also written on `reset --stats`. When present, the lifetime
    // counters are post-reset, so anchor `elapsed_secs` and the display
    // label there rather than at the original install time.
    let last_reset_at = crate::core::session::last_reset_at(&session.conn);
    let anchor = last_reset_at.unwrap_or(installed_at);
    let elapsed = (unix_now() - anchor).max(0);
    let files_tracked: i64 = session
        .conn
        .query_row(
            "SELECT COUNT(*) FROM lifetime_per_file WHERE tokens_full > 0",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let (files_edited, total_edits): (i64, i64) = session
        .conn
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(edits), 0) FROM lifetime_edited_files",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));
    // Savings can be negative when DRIP's header overhead exceeds the
    // body bytes it saved (small files re-read once or twice). Keep the
    // raw signed delta — the display layer formats negatives explicitly
    // so users see when DRIP is costing them tokens rather than seeing
    // a misleading clamped 0.
    let saved = tokens_full - tokens_sent;
    let pct = tokens::percent_saved(tokens_full, tokens_sent);

    let mut stmt = session.conn.prepare(
        "SELECT file_path, reads, tokens_full, tokens_sent
         FROM lifetime_per_file
         WHERE tokens_full > tokens_sent
         ORDER BY (tokens_full - tokens_sent) DESC
         LIMIT 10",
    )?;
    let top: Vec<PerFile> = stmt
        .query_map([], |r| {
            let file: String = r.get(0)?;
            let reads: i64 = r.get(1)?;
            let tf: i64 = r.get(2)?;
            let ts: i64 = r.get(3)?;
            Ok(PerFile {
                file,
                reads,
                tokens_full: tf,
                tokens_sent: ts,
                tokens_saved: (tf - ts).max(0),
                reduction_pct: tokens::percent_saved(tf, ts),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let history = if include_history {
        let cutoff = unix_now() - 30 * 86_400;
        let cutoff_day = chrono_day(cutoff);
        let mut stmt = session.conn.prepare(
            "SELECT day, reads, tokens_full, tokens_sent
             FROM lifetime_daily
             WHERE day >= ?1
             ORDER BY day DESC",
        )?;
        let rows: Vec<DayBucket> = stmt
            .query_map(params![cutoff_day], |r| {
                let day: String = r.get(0)?;
                let reads: i64 = r.get(1)?;
                let tf: i64 = r.get(2)?;
                let ts: i64 = r.get(3)?;
                Ok(DayBucket {
                    day,
                    reads,
                    tokens_full: tf,
                    tokens_sent: ts,
                    tokens_saved: (tf - ts).max(0),
                    reduction_pct: tokens::percent_saved(tf, ts),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Some(rows)
    } else {
        None
    };

    Ok(MeterReport {
        scope: Scope::Lifetime,
        session_id: String::new(),
        started_at: installed_at,
        last_reset_at,
        elapsed_secs: elapsed,
        files_tracked,
        total_reads,
        files_edited,
        total_edits,
        tokens_full,
        tokens_sent,
        tokens_saved: saved,
        reduction_pct: pct,
        dollars_saved: tokens::dollars_saved(saved),
        price_per_mtok: tokens::price_per_mtok(),
        co2_g_saved: tokens::co2_g_saved(saved),
        co2_g_per_ktok: tokens::co2_g_per_ktok(),
        top,
        history,
        ghost_pollution: None,
        session_strategy: None,
        session_context: None,
        storage: crate::commands::cache::collect_stats().ok(),
        compaction: collect_compaction_stats_lifetime(&session.conn)
            .ok()
            .flatten(),
        external_edit_refreshes: external_edit_stats(external_edit_refreshes, total_reads),
        auto_picked_session: None,
    })
}

fn external_edit_stats(count: i64, total_reads: i64) -> Option<ExternalEditStats> {
    if count <= 0 {
        return None;
    }
    let pct = if total_reads > 0 {
        ((count as f64 / total_reads as f64) * 100.0).round() as u32
    } else {
        0
    };
    Some(ExternalEditStats {
        count,
        pct_of_reads: pct,
    })
}

/// UTC YYYY-MM-DD without pulling chrono — matches SQLite's
/// `date(?, 'unixepoch')` format byte-for-byte.
fn chrono_day(unix_ts: i64) -> String {
    let secs_per_day = 86_400i64;
    let days = unix_ts.div_euclid(secs_per_day);
    let mut y: i64 = 1970;
    let mut d = days;
    let is_leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    loop {
        let len = if is_leap(y) { 366 } else { 365 };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    let months = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0usize;
    while m < 12 {
        let mlen = months[m] + if m == 1 && is_leap(y) { 1 } else { 0 };
        if d < mlen {
            break;
        }
        d -= mlen;
        m += 1;
    }
    format!("{:04}-{:02}-{:02}", y, m + 1, d + 1)
}

/// Auto-pick the most useful session for a bare `drip meter --session`.
///
/// The shell-derived session (where the user typed the command) is
/// almost always empty — agents own DRIP traffic, the shell does not.
/// Forcing users to look up the agent's UUID via `drip sessions` and
/// paste it back was the friction we got bug-reported on. Now:
///
/// 1. Derive the shell session as before.
/// 2. If it has reads, use it (preserves the original behavior for
///    odd setups where the user IS the caller).
/// 3. Else, look for the most-recently-active session whose `cwd`
///    matches the current working directory and which has at least
///    one read. That's almost certainly the agent the user wants.
/// 4. Else, fall back to the derived session — the report will be
///    empty but at least the JSON shape stays stable.
///
/// Returns the picked `Session` plus a `Some(AutoPickedSession)`
/// when step (3) actually substituted the session, so the renderer
/// can surface the swap.
fn pick_inspect_session() -> Result<(Session, Option<AutoPickedSession>)> {
    let (session, swap) = inspect::pick_session()?;
    let auto = swap.map(|s| AutoPickedSession {
        session_id: s.session_id,
        agent: inspect::pretty_agent(s.agent),
        strategy: s.strategy,
    });
    Ok((session, auto))
}

fn collect_session_report(include_history: bool, explicit_id: Option<&str>) -> Result<MeterReport> {
    // Read-only — inspecting must not create a ghost session row.
    let (session, auto_picked_from) = match explicit_id {
        Some(id) if !id.is_empty() => (Session::open_with_id_readonly(id.to_string())?, None),
        _ => pick_inspect_session()?,
    };
    let started = session.started_at()?;
    let elapsed = (unix_now() - started).max(0);

    let (files_tracked, total_reads, reads_tf, reads_ts) = session.conn.query_row(
        "SELECT COUNT(DISTINCT file_path),
                COALESCE(SUM(reads_count), 0),
                COALESCE(SUM(tokens_full), 0),
                COALESCE(SUM(tokens_sent), 0)
         FROM reads WHERE session_id = ?1",
        params![session.id],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        },
    )?;
    let tokens_full = reads_tf;
    let tokens_sent = reads_ts;
    // Signed: header overhead can push `tokens_sent` past `tokens_full`
    // on small files. The display layer surfaces the loss honestly.
    let saved = tokens_full - tokens_sent;
    let pct = tokens::percent_saved(tokens_full, tokens_sent);

    // "Top savings" only lists files where re-reads produced
    // savings; reads_count=0 baselines and first-read-only rows are noise.
    let mut stmt = session.conn.prepare(
        "SELECT file_path, reads_count, tokens_full, tokens_sent
         FROM reads
         WHERE session_id = ?1 AND tokens_full > tokens_sent
         ORDER BY (tokens_full - tokens_sent) DESC
         LIMIT 10",
    )?;
    let top: Vec<PerFile> = stmt
        .query_map(params![session.id], |r| {
            let file: String = r.get(0)?;
            let reads: i64 = r.get(1)?;
            let tf: i64 = r.get(2)?;
            let ts: i64 = r.get(3)?;
            Ok(PerFile {
                file,
                reads,
                tokens_full: tf,
                tokens_sent: ts,
                tokens_saved: (tf - ts).max(0),
                reduction_pct: tokens::percent_saved(tf, ts),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let history = if include_history {
        let cutoff = unix_now() - 30 * 86_400;
        let mut stmt = session.conn.prepare(
            "SELECT date(read_at, 'unixepoch'),
                    SUM(reads_count),
                    SUM(tokens_full),
                    SUM(tokens_sent)
             FROM reads
             WHERE read_at > ?1
             GROUP BY 1
             ORDER BY 1 DESC",
        )?;
        let rows: Vec<DayBucket> = stmt
            .query_map(params![cutoff], |r| {
                let day: String = r.get(0)?;
                let reads: i64 = r.get(1)?;
                let tf: i64 = r.get(2)?;
                let ts: i64 = r.get(3)?;
                Ok(DayBucket {
                    day,
                    reads,
                    tokens_full: tf,
                    tokens_sent: ts,
                    tokens_saved: (tf - ts).max(0),
                    reduction_pct: tokens::percent_saved(tf, ts),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Some(rows)
    } else {
        None
    };

    let strategy = session.strategy;
    let context = session.context.clone();
    let compaction = collect_compaction_stats_session(&session.conn, &session.id)
        .ok()
        .flatten();
    // Session-scoped OOB-refresh count: pulled from `read_events`
    // (the only per-session source we have — `lifetime_stats` is
    // install-wide, not per session). The events table is capped per
    // session via `DRIP_REPLAY_KEEP` (default 500), so this is
    // accurate as long as the session hasn't rolled over. For
    // typical agent sessions (< a few hundred reads) it's exact.
    let session_oob: i64 = session
        .conn
        .query_row(
            "SELECT COUNT(*) FROM read_events
             WHERE session_id = ?1
               AND outcome_kind = 'fallback'
               AND fallback_reason LIKE 'file changed externally%'",
            params![session.id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(MeterReport {
        scope: Scope::Session,
        session_id: session.id,
        started_at: started,
        last_reset_at: None,
        elapsed_secs: elapsed,
        files_tracked,
        total_reads,
        // Edit stats are install-wide, not per-session — surfaced as 0
        // here on purpose. Run `drip meter` (no flag) to see them.
        files_edited: 0,
        total_edits: 0,
        tokens_full,
        tokens_sent,
        tokens_saved: saved,
        reduction_pct: pct,
        dollars_saved: tokens::dollars_saved(saved),
        price_per_mtok: tokens::price_per_mtok(),
        co2_g_saved: tokens::co2_g_saved(saved),
        co2_g_per_ktok: tokens::co2_g_per_ktok(),
        top,
        history,
        ghost_pollution: None,
        session_strategy: Some(strategy.as_str().to_string()),
        session_context: Some(context),
        storage: None,
        compaction,
        external_edit_refreshes: external_edit_stats(session_oob, total_reads),
        auto_picked_session: auto_picked_from,
    })
}

/// Lifetime aggregate of the v9 compaction ledger. Returns `None`
/// when no session has ever been compacted (cleaner JSON than
/// emitting a zeroed block) — `serde(skip_serializing_if)` then
/// elides the field entirely.
fn collect_compaction_stats_lifetime(
    conn: &rusqlite::Connection,
) -> Result<Option<CompactionStats>> {
    let row: Option<(i64, Option<i64>)> = conn
        .query_row(
            "SELECT COALESCE(SUM(compaction_count), 0),
                    MAX(last_compaction_at)
             FROM sessions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (total, last_at) = row.unwrap_or((0, None));
    if total == 0 {
        return Ok(None);
    }
    let tokens_resent: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(tokens_full), 0)
             FROM reads WHERE context_epoch > 0",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(Some(CompactionStats {
        total_compactions: total,
        last_compaction_age: last_at.map(format_age),
        last_compaction_at: last_at,
        tokens_resent_after_compaction: tokens_resent,
    }))
}

fn collect_compaction_stats_session(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Option<CompactionStats>> {
    let row: Option<(i64, Option<i64>)> = conn
        .query_row(
            "SELECT compaction_count, last_compaction_at
             FROM sessions WHERE session_id = ?1",
            params![session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let (count, last_at) = match row {
        Some(r) => r,
        None => return Ok(None),
    };
    if count == 0 {
        return Ok(None);
    }
    let tokens_resent: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(tokens_full), 0)
             FROM reads
             WHERE session_id = ?1 AND context_epoch > 0",
            params![session_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(Some(CompactionStats {
        total_compactions: count,
        last_compaction_age: last_at.map(format_age),
        last_compaction_at: last_at,
        tokens_resent_after_compaction: tokens_resent,
    }))
}

/// Compact relative-time formatter for the `last_compaction_age`
/// JSON field and the human renderer. `now - ts` collapses into the
/// largest unit that produces a >= 1 result; resolutions stop at
/// days because the ledger is a recent-ops indicator, not an audit
/// log.
fn format_age(ts: i64) -> String {
    let now = unix_now();
    let delta = (now - ts).max(0);
    if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{} min ago", delta / 60)
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

fn render_human(r: &MeterReport, graph: bool) -> String {
    let mut out = String::new();
    let label_w = 18;
    let line_w = 66;

    // ── Ghost-file pollution hint (lifetime view only) ────────────
    if let Some(g) = &r.ghost_pollution {
        out.push_str(&render_ghost_hint(g));
        out.push('\n');
    }

    // ── Header ────────────────────────────────────────────────────
    let title = match r.scope {
        Scope::Lifetime if r.last_reset_at.is_some() => "DRIP Token Savings (Since Reset)",
        Scope::Lifetime => "DRIP Token Savings (Since Install)",
        Scope::Session => "DRIP Token Savings (Current Session)",
    };
    out.push_str(&term::green(&term::bold(title)));
    out.push('\n');
    out.push_str(&term::dim(&"═".repeat(line_w)));
    out.push('\n');

    // ── Auto-picked session notice ────────────────────────────────
    // The bare `--session` flag normally derives the shell's session
    // — almost always empty since agents own DRIP traffic. When we
    // auto-substituted the agent's session in this cwd, tell the
    // user so they know what scope they're looking at.
    if let Some(pick) = &r.auto_picked_session {
        let short_id: String = pick.session_id.chars().take(12).collect();
        let agent_seg = pick
            .agent
            .as_deref()
            .map(|a| format!(" ({a})"))
            .unwrap_or_default();
        out.push_str(&term::dim(&format!(
            "ℹ Auto-picked session {short_id}…{agent_seg} — pass `--session <id>` to pin\n",
        )));
    }
    out.push('\n');

    // ── Stats block ───────────────────────────────────────────────
    write_stat(
        &mut out,
        label_w,
        "Files tracked:",
        &format_compact(r.files_tracked),
    );
    write_stat(
        &mut out,
        label_w,
        "Total reads:",
        &format_compact(r.total_reads),
    );
    if r.scope == Scope::Lifetime && (r.files_edited > 0 || r.total_edits > 0) {
        let edits = format!(
            "{}  {}",
            format_compact(r.files_edited),
            term::dim(&format!("({} edits)", format_compact(r.total_edits))),
        );
        write_stat(&mut out, label_w, "Files edited:", &edits);
    }
    write_stat(
        &mut out,
        label_w,
        "Tokens full:",
        &format_compact(r.tokens_full),
    );
    write_stat(
        &mut out,
        label_w,
        "Tokens sent:",
        &format_compact(r.tokens_sent),
    );

    // The headline number — colored by reduction threshold. Show
    // `(loss)` instead of `(0%)` when tokens_sent exceeded tokens_full
    // (DRIP header overhead outweighed body savings on small files),
    // so the parenthetical reinforces the negative tokens_saved
    // rather than looking like a flat zero.
    let saved_value = format_compact(r.tokens_saved);
    let saved_pct = if r.tokens_saved < 0 {
        String::from("(loss)")
    } else {
        format!("({}%)", r.reduction_pct)
    };
    let saved_colored = if r.tokens_saved <= 0 {
        term::bold(&format!("{saved_value}  {saved_pct}"))
    } else {
        match r.reduction_pct {
            70..=u32::MAX => term::green(&term::bold(&format!("{saved_value}  {saved_pct}"))),
            30..=69 => term::yellow(&term::bold(&format!("{saved_value}  {saved_pct}"))),
            _ => term::bold(&format!("{saved_value}  {saved_pct}")),
        }
    };
    write_stat(&mut out, label_w, "Tokens saved:", &saved_colored);

    // ── File-reads vs bash-commands breakdown ─────────────────────
    // Render whenever there's been at least one read. Even at
    // net-zero or net-negative savings users want to see which lane
    // is which — the file-reads share can dip negative when DRIP's
    // header overhead exceeds body savings on re-reads of small
    // files, but suppress on a fresh install with literally zero
    // reads where a zero line would just be visual noise.

    if r.tokens_saved > 0 {
        let dollars = format!(
            "{}  {}",
            term::green(&format_dollars(r.dollars_saved)),
            term::dim(&format!("(@ ${:.2}/Mtok)", r.price_per_mtok)),
        );
        write_stat(&mut out, label_w, "$ saved:", &dollars);
        let co2 = format!(
            "{}  {}",
            term::green(&format_co2(r.co2_g_saved)),
            term::dim(&format!("(@ {:.2} g/Ktok)", r.co2_g_per_ktok)),
        );
        write_stat(&mut out, label_w, "CO₂ avoided:", &co2);
    }

    let span_label = match r.scope {
        Scope::Lifetime if r.last_reset_at.is_some() => "Since reset:",
        Scope::Lifetime => "Since install:",
        Scope::Session => "Session age:",
    };
    write_stat(
        &mut out,
        label_w,
        span_label,
        &term::dim(&human_duration(r.elapsed_secs)),
    );

    // v9 compaction ledger row: only rendered when at least one
    // compaction has happened in scope. Two-bit body — count + the
    // tokens that have been re-sent since.
    if let Some(c) = &r.compaction {
        let detail = match (&c.last_compaction_age, c.tokens_resent_after_compaction) {
            (Some(age), n) if n > 0 => {
                term::dim(&format!("(last {age}, {} re-sent)", format_compact(n)))
            }
            (Some(age), _) => term::dim(&format!("(last {age})")),
            (None, _) => String::new(),
        };
        let body = if detail.is_empty() {
            format_compact(c.total_compactions)
        } else {
            format!("{}  {detail}", format_compact(c.total_compactions))
        };
        write_stat(&mut out, label_w, "Compactions:", &body);
    }

    // OOB-refresh row: shown when DRIP detected at least one read
    // where the file changed under it (cargo fmt, git pull, an
    // editor outside the Edit/Write hook). These reads ship full
    // native content by design to keep Claude's read-tracker in
    // sync — they're a chunk of the "missing" reduction%.
    if let Some(e) = &r.external_edit_refreshes {
        let body = format!(
            "{}  {}",
            format_compact(e.count),
            term::dim(&format!(
                "({}% of reads — file changed since last read, full content re-shipped)",
                e.pct_of_reads
            )),
        );
        write_stat(&mut out, label_w, "Native refresh:", &body);
    }

    // Efficiency meter — visual percentage bar.
    let meter = meter_bar(r.reduction_pct, 16);
    let meter_pct = match r.reduction_pct {
        70..=u32::MAX => term::green(&format!("{}%", r.reduction_pct)),
        30..=69 => term::yellow(&format!("{}%", r.reduction_pct)),
        _ => term::dim(&format!("{}%", r.reduction_pct)),
    };
    out.push_str(&format!(
        "{:<label_w$} {meter}  {meter_pct}\n",
        "Efficiency meter:",
        label_w = label_w,
    ));

    // ── Top Files ─────────────────────────────────────────────────
    if !r.top.is_empty() {
        out.push('\n');
        out.push_str(&term::green(&term::bold("Top Files")));
        out.push('\n');
        out.push_str(&term::dim(&"─".repeat(line_w)));
        out.push_str("\n\n");

        let max_saved = r.top.iter().map(|f| f.tokens_saved).max().unwrap_or(0);
        // Column widths chosen so the table fits in 80-col terminals.
        out.push_str(&term::dim(&format!(
            "  {:>3}  {:<32}  {:>5}  {:>7}  {:>9}  {}\n",
            "#", "File", "Reads", "Saved", "Reduction", "Impact"
        )));
        for (i, f) in r.top.iter().enumerate() {
            let n = format!("{}.", i + 1);
            // `{:<32}` measures Unicode chars for &str — but the `…`
            // we insert via `truncate_label` plus terminals' real
            // wcwidth quirks make the column drift on rows that
            // didn't get truncated. Pad on visible-char count
            // explicitly so every row lines up to the same column.
            let file = pad_visible(&truncate_label(&f.file, 32), 32);
            let reads = format_compact(f.reads);
            let saved_plain = format_compact(f.tokens_saved);
            let saved_pad = " ".repeat(7usize.saturating_sub(saved_plain.len()));
            let saved_colored = term::green(&saved_plain);
            let pct_plain = format!("{}%", f.reduction_pct);
            let pct_pad = " ".repeat(9usize.saturating_sub(pct_plain.len()));
            let pct_colored = term::color_pct(f.reduction_pct);
            let impact = impact_bar(f.tokens_saved, max_saved, 10);
            out.push_str(&format!(
                "  {n:>3}  {file}  {reads:>5}  {saved_pad}{saved_colored}  {pct_pad}{pct_colored}  {impact}\n",
            ));
        }
    } else {
        out.push('\n');
        out.push_str(&term::dim(
            "  no per-file savings yet — DRIP wins on the second read of a file.\n",
        ));
    }

    // ── History (optional) ────────────────────────────────────────
    if let Some(history) = &r.history {
        out.push('\n');
        out.push_str(&term::green(&term::bold("History (last 30 days)")));
        out.push('\n');
        out.push_str(&term::dim(&"─".repeat(line_w)));
        out.push_str("\n\n");
        for d in history {
            out.push_str(&format!(
                "  {}  reads={:<4}  saved={:<8}  {}\n",
                d.day,
                d.reads,
                format_compact(d.tokens_saved),
                term::color_pct(d.reduction_pct),
            ));
        }
    }

    if graph {
        out.push('\n');
        out.push_str(&render_graph(r.tokens_full, r.tokens_sent));
    }

    out
}

/// Aligned `Label:    value` row. ANSI codes in `value` have zero visible
/// width, so we right-pad the label (which is plain) with `format!`'s
/// width specifier — rather than the value side, which would mis-align.
fn write_stat(out: &mut String, label_w: usize, label: &str, value: &str) {
    out.push_str(&format!(
        "{label:<label_w$} {value}\n",
        label = label,
        label_w = label_w,
        value = value,
    ));
}

/// Compact human-friendly token count: `999`, `12.3K`, `1.4M`, `2.1B`.
/// Keeps the "Top Files" column narrow.
fn format_compact(n: i64) -> String {
    let abs = n.unsigned_abs() as f64;
    let sign = if n < 0 { "-" } else { "" };
    if abs >= 1_000_000_000.0 {
        format!("{sign}{:.1}B", abs / 1_000_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("{sign}{:.1}M", abs / 1_000_000.0)
    } else if abs >= 10_000.0 {
        format!("{sign}{:.1}K", abs / 1_000.0)
    } else if abs >= 1_000.0 {
        format!("{sign}{:.2}K", abs / 1_000.0)
    } else {
        format!("{sign}{}", n.unsigned_abs())
    }
}

/// Solid filled bar with a striped tail. Width chars wide; every char
/// is either fully filled (█, in green) or a striped pattern (▒, dim).
fn meter_bar(pct: u32, width: usize) -> String {
    let pct = pct.min(100) as usize;
    let filled = (pct * width + 50) / 100;
    let empty = width.saturating_sub(filled);
    let filled_str: String = "█".repeat(filled);
    let empty_str: String = "▒".repeat(empty);
    format!("{}{}", term::green(&filled_str), term::dim(&empty_str))
}

/// Per-file impact bar — relative to the file with the most savings.
/// Always at least 1 char for non-zero savings so a small contributor
/// doesn't disappear visually.
fn impact_bar(saved: i64, max_saved: i64, width: usize) -> String {
    if max_saved <= 0 || saved <= 0 {
        return " ".repeat(width);
    }
    let frac = (saved.min(max_saved) as f64) / (max_saved as f64);
    let filled = (((frac * width as f64).round()) as usize).clamp(1, width);
    let filled_str: String = "▓".repeat(filled);
    let empty_str: String = "░".repeat(width - filled);
    format!("{}{}", term::green(&filled_str), term::dim(&empty_str))
}

/// 0.0034 → "$0.00", 0.42 → "$0.42", 12.345 → "$12.35", 1234 → "$1,234".
/// We avoid sub-cent precision (it'd suggest false accuracy on top of an
/// already-rough estimate) but still show $0.00 instead of nothing when
/// the saved amount rounds down — proves the column is working.
fn format_dollars(usd: f64) -> String {
    if usd >= 1.0 {
        format!("${}", format_thousands(usd.round() as i64))
    } else {
        format!("${:.2}", usd)
    }
}

/// 0.4 → "0.4 g", 1234 → "1.23 kg", 1_234_567 → "1.23 t". Output stays
/// short — this column has to fit alongside the dollar one.
fn format_co2(grams: f64) -> String {
    if grams >= 1_000_000.0 {
        format!("{:.2} t", grams / 1_000_000.0)
    } else if grams >= 1_000.0 {
        format!("{:.2} kg", grams / 1_000.0)
    } else if grams >= 10.0 {
        format!("{:.0} g", grams)
    } else {
        format!("{:.1} g", grams)
    }
}

/// 12345678 → "12,345,678". Comma-grouped for readability without
/// pulling in a locale crate. ASCII only, fine in any terminal.
fn format_thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    if n < 0 {
        out.push('-');
    }
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn truncate_label(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        // `…` looks nice but its terminal cell width is unreliable on
        // some setups (rendered 0 cols on a few macOS Terminal fonts),
        // which mis-aligns columns next to non-truncated rows. Stick to
        // ASCII `..` so every char is guaranteed 1 visible cell.
        let take = max.saturating_sub(2);
        let tail: String = s
            .chars()
            .rev()
            .take(take)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("..{tail}")
    }
}

/// Right-pad with ASCII spaces to reach `width` visible chars.
/// Counts Unicode scalars, not bytes — necessary because `format!`'s
/// `{:<width}` on strings can drift when the value contains chars whose
/// terminal cell width (`…`, CJK ranges, …) doesn't match Rust's char
/// count assumption.
fn pad_visible(s: &str, width: usize) -> String {
    let visible = s.chars().count();
    if visible >= width {
        s.to_string()
    } else {
        let mut out = String::with_capacity(s.len() + (width - visible));
        out.push_str(s);
        for _ in 0..(width - visible) {
            out.push(' ');
        }
        out
    }
}

fn human_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d", secs / 86_400)
    }
}

fn render_graph(full: i64, sent: i64) -> String {
    if full <= 0 {
        return "No data yet.\n".into();
    }
    let width = 40usize;
    let sent_fill = ((sent as f64 / full as f64) * width as f64).round() as usize;
    let saved_fill = width - sent_fill.min(width);
    let bar = "█".repeat(sent_fill.min(width)) + &"░".repeat(saved_fill);
    format!("Tokens   [{bar}]\n           sent={sent} / full={full}\n")
}
