//! Shared session-picking helpers for read-only inspection commands
//! (`drip meter`, `drip replay`, `drip source-map`).
//!
//! The problem: a shell command typed by the user derives a `git` or
//! `cwd` strategy session — empty or near-empty. Meanwhile the agent
//! (Claude Code, Codex, …) runs with `DRIP_SESSION_ID` set and lands
//! in an `env`-strategy session with all the real read history. An
//! inspection command bound to the derived session shows an empty
//! report and the user pastes UUIDs to escape it.
//!
//! `pick_session` picks the obvious right answer: prefer the live
//! agent session in cwd, only fall back to the derived session when
//! there's nothing better. Callers receive an optional `InspectSwap`
//! so they can surface the substitution in the human report.

use super::session::{Session, SessionStrategy};
use anyhow::Result;
use rusqlite::{params, OptionalExtension};

/// Information about a session-substitute decision. Returned alongside
/// the substituted `Session` so callers can render the swap (e.g.
/// `drip meter`'s `[scoped to Claude Code …]` header).
pub struct InspectSwap {
    pub session_id: String,
    pub agent: Option<String>,
    pub strategy: Option<String>,
}

/// Pick the best session for a read-only inspection command.
///
/// Ladder:
/// 1. Open the derived session read-only via the normal strategy chain.
/// 2. If derived is `env` strategy AND has reads → trust it. The user
///    IS the agent, no swap should happen.
/// 3. Else find the best session whose `cwd` matches the current
///    working directory, preferring `env` strategy, then by
///    `last_active`. Skip empty candidates and same-as-derived.
///
/// Returns `(session, swap)` where `swap` is `Some` only when step 3
/// substituted a different session. When `None`, the derived session
/// is what the caller should use — the report shape is identical to
/// the historical behavior.
pub fn pick_session() -> Result<(Session, Option<InspectSwap>)> {
    let derived = Session::open_readonly()?;

    let derived_reads: i64 = derived
        .conn
        .query_row(
            "SELECT COUNT(*) FROM reads WHERE session_id = ?1",
            params![derived.id],
            |r| r.get(0),
        )
        .unwrap_or(0);

    // Symlink-stable cwd compare (macOS `/tmp` → `/private/tmp`). If
    // we can't canonicalize, skip the lookup — better to return the
    // derived session than to pick the wrong agent's read history.
    let Some(cwd) = std::env::current_dir()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
    else {
        return Ok((derived, None));
    };

    if derived.strategy == SessionStrategy::Env && derived_reads > 0 {
        return Ok((derived, None));
    }

    // Prefer env-strategy sessions (the shipping agent integrations)
    // and rank by recency. Pure read-count would pick a stale agent
    // session over the live one — `last_active` gets "what's happening
    // now" right.
    let best: Option<(String, Option<String>, Option<String>, i64)> = derived
        .conn
        .query_row(
            "SELECT s.session_id, s.agent, s.strategy,
                    (SELECT COUNT(*) FROM reads r WHERE r.session_id = s.session_id) AS reads
             FROM sessions s
             WHERE s.cwd = ?1
             ORDER BY (CASE WHEN s.strategy = 'env' THEN 0 ELSE 1 END),
                      s.last_active DESC
             LIMIT 1",
            params![cwd],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .unwrap_or(None);

    let Some((id, agent, strategy, reads)) = best else {
        return Ok((derived, None));
    };

    // Don't swap into an empty session, and no-op when the best
    // candidate IS the derived session.
    if id == derived.id || reads == 0 {
        return Ok((derived, None));
    }

    let session = Session::open_with_id_readonly(id.clone())?;
    Ok((
        session,
        Some(InspectSwap {
            session_id: id,
            agent,
            strategy,
        }),
    ))
}

/// Convert a persisted `agent` tag to a human-readable label.
/// Centralized so every inspection command renders the same string.
pub fn pretty_agent(tag: Option<String>) -> Option<String> {
    tag.map(|t| match t.as_str() {
        "claude" => "Claude Code".to_string(),
        "codex" => "Codex".to_string(),
        "gemini" => "Gemini".to_string(),
        other => other.to_string(),
    })
}
