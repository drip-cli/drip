//! `drip replay` — chronological dump of every read DRIP intercepted
//! in a session. Backed by `read_events`, capped at 500 events per
//! session (tune with `DRIP_REPLAY_KEEP`, disable with `DRIP_REPLAY_LOG=0`).

use crate::core::inspect;
use crate::core::session::{self, recent_events, ReadEvent, Session};
use crate::core::term;
use crate::core::tokens;
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;

#[derive(Debug, Default)]
pub struct ReplayOpts {
    pub session: Option<String>,
    pub since: Option<String>, // "5m" | "1h" | "30s" | "1d"
    pub file: Option<String>,
    pub limit: Option<i64>,
    pub full: bool,
    pub json: bool,
}

#[derive(Debug, Serialize)]
struct EventDto {
    n: usize,
    occurred_at: i64,
    age_secs: i64,
    file_path: String,
    outcome_kind: String,
    fallback_reason: Option<String>,
    tokens_full: i64,
    tokens_sent: i64,
    tokens_saved: i64,
    rendered: String,
}

#[derive(Debug, Serialize)]
struct ReplayDto {
    session_id: String,
    session_started_at: Option<i64>,
    event_count: usize,
    events: Vec<EventDto>,
    summary: SummaryDto,
}

#[derive(Debug, Serialize)]
struct SummaryDto {
    tokens_full: i64,
    tokens_sent: i64,
    tokens_saved: i64,
    reduction_pct: u32,
}

pub fn run(opts: ReplayOpts) -> Result<String> {
    let (session, session_id) = match opts.session.as_deref() {
        Some(id) if !id.is_empty() => (Session::open_readonly()?, id.to_string()),
        _ => {
            // Prefer the live agent session in cwd (the common case:
            // the user runs `drip replay` from inside their project,
            // expecting "what did the agent just see?"). When that
            // resolves to an empty derived session (e.g. inspecting
            // from an unrelated directory), fall back to the legacy
            // "most-recently-active session with events" global pick
            // so the command still works as a forensic tool.
            let (picked, swap) = inspect::pick_session()?;
            if swap.is_some() {
                let id = picked.id.clone();
                (picked, id)
            } else {
                (
                    picked,
                    pick_default_session_global(&Session::open_readonly()?)?,
                )
            }
        }
    };
    let since_ts = opts.since.as_deref().and_then(parse_since);
    let limit = opts.limit.unwrap_or(50).max(1);

    let events = recent_events(
        &session.conn,
        &session_id,
        limit,
        since_ts,
        opts.file.as_deref(),
    )?;
    // SQL returned newest-first to honor `--limit`; flip for replay.
    let mut events: Vec<ReadEvent> = events.into_iter().rev().collect();
    let started_at: Option<i64> = session
        .conn
        .query_row(
            "SELECT started_at FROM sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
            |r| r.get(0),
        )
        .optional()?;

    if opts.json {
        return Ok(render_json(&session_id, started_at, &events) + "\n");
    }
    Ok(render_human(
        &session_id,
        started_at,
        &mut events,
        opts.full,
    ))
}

fn pick_default_session_global(session: &Session) -> Result<String> {
    // Legacy fallback when `inspect::pick_session()` returned the
    // derived session (no agent session in cwd): pick the
    // most-recently-active session with events globally, so
    // forensics from any directory still surfaces something.
    let row: Option<String> = session
        .conn
        .query_row(
            "SELECT s.session_id
             FROM sessions s
             LEFT JOIN read_events e ON e.session_id = s.session_id
             GROUP BY s.session_id
             ORDER BY (CASE WHEN COUNT(e.id) = 0 THEN 0 ELSE 1 END) DESC,
                      s.last_active DESC
             LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(row.unwrap_or_else(|| session.id.clone()))
}

/// Parses "5m", "30s", "1h", "2d" into an absolute unix timestamp.
fn parse_since(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: i64 = num.parse().ok()?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return None,
    };
    Some(session::unix_now() - secs)
}

fn render_json(session_id: &str, started_at: Option<i64>, events: &[ReadEvent]) -> String {
    let now = session::unix_now();
    let dtos: Vec<EventDto> = events
        .iter()
        .enumerate()
        .map(|(i, e)| EventDto {
            n: i + 1,
            occurred_at: e.occurred_at,
            age_secs: (now - e.occurred_at).max(0),
            file_path: e.file_path.clone(),
            outcome_kind: e.outcome_kind.clone(),
            fallback_reason: e.fallback_reason.clone(),
            tokens_full: e.tokens_full,
            tokens_sent: e.tokens_sent,
            tokens_saved: (e.tokens_full - e.tokens_sent).max(0),
            rendered: e.rendered.clone(),
        })
        .collect();
    let total_full: i64 = events.iter().map(|e| e.tokens_full).sum();
    let total_sent: i64 = events.iter().map(|e| e.tokens_sent).sum();
    let dto = ReplayDto {
        session_id: session_id.to_string(),
        session_started_at: started_at,
        event_count: events.len(),
        events: dtos,
        summary: SummaryDto {
            tokens_full: total_full,
            tokens_sent: total_sent,
            tokens_saved: (total_full - total_sent).max(0),
            reduction_pct: tokens::percent_saved(total_full, total_sent),
        },
    };
    serde_json::to_string_pretty(&dto).unwrap_or_default()
}

fn render_human(
    session_id: &str,
    started_at: Option<i64>,
    events: &mut [ReadEvent],
    full: bool,
) -> String {
    let mut out = String::new();
    let now = session::unix_now();
    let header = match started_at {
        Some(ts) => format!(
            "Session: {}  (started {} ago)",
            short_id(session_id),
            human_duration((now - ts).max(0))
        ),
        None => format!("Session: {} (no `sessions` row)", short_id(session_id)),
    };
    out.push_str(&term::bold(&header));
    out.push('\n');

    if events.is_empty() {
        out.push_str(&term::dim(
            "  no recorded events for this session.\n  \
             Run your agent against DRIP — every read shows up here.\n  \
             (If you set DRIP_REPLAY_LOG=0 the log is disabled.)\n",
        ));
        return out;
    }

    out.push('\n');
    out.push_str(&term::dim(&format!(
        "  {:<3}  {:<10}  {:<35}  {:<11}  {:>6}  {:>6}\n",
        "#", "AGE", "FILE", "OUTCOME", "FULL", "SENT"
    )));
    let mut total_full = 0i64;
    let mut total_sent = 0i64;
    for (i, e) in events.iter().enumerate() {
        let age = human_duration((now - e.occurred_at).max(0));
        let kind_colored = colorize_outcome(&e.outcome_kind);
        let row = format!(
            "  {:<3}  {:<10}  {:<35}  {:<11}  {:>6}  {:>6}",
            i + 1,
            age,
            truncate(&e.file_path, 35),
            kind_colored,
            e.tokens_full,
            e.tokens_sent,
        );
        out.push_str(&row);
        out.push('\n');
        total_full += e.tokens_full;
        total_sent += e.tokens_sent;
    }
    out.push('\n');
    let saved = (total_full - total_sent).max(0);
    let pct = tokens::percent_saved(total_full, total_sent);
    out.push_str(&format!(
        "  Total: {} reads · {} full · {} sent · {} {}\n",
        events.len(),
        total_full,
        total_sent,
        term::green(&format!("{} saved", saved)),
        term::dim(&format!("({}%)", pct)),
    ));

    if full {
        out.push('\n');
        out.push_str(&term::bold("  ── Replay (rendered output per event) ──\n"));
        for (i, e) in events.iter().enumerate() {
            let age = human_duration((now - e.occurred_at).max(0));
            let header = format!(
                "─── #{} · {} · {} · {} ago · sent {} / full {} ───",
                i + 1,
                truncate(&e.file_path, 50),
                e.outcome_kind,
                age,
                e.tokens_sent,
                e.tokens_full,
            );
            out.push('\n');
            out.push_str(&term::dim(&header));
            out.push('\n');
            out.push_str(&e.rendered);
            if !e.rendered.ends_with('\n') {
                out.push('\n');
            }
        }
    }

    out
}

fn colorize_outcome(kind: &str) -> String {
    match kind {
        "first" => term::dim("first"),
        "unchanged" => term::green("unchanged"),
        "delta" => term::green("delta"),
        "fallback" => term::yellow("fallback"),
        "deleted" => term::yellow("deleted"),
        "passthrough" => term::dim("passthrough"),
        other => other.to_string(),
    }
}

fn short_id(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…", &id[..12])
    } else {
        id.to_string()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let take = max.saturating_sub(1);
        let tail: String = s
            .chars()
            .rev()
            .take(take)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    }
}

fn human_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}
