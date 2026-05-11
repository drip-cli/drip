//! Claude Code SessionStart hook.
//!
//! Why we need this: Claude Code's internal `Edit must Read first`
//! guard tracks the set of files the agent has Read in *its* current
//! conversation state. On `compact` (auto/manual `/compact`),
//! `clear` (`/clear`), AND `resume` (`claude --resume` /
//! `--continue` / `/resume`), the in-process tracker is rebuilt
//! from scratch — the old process is gone, conversation transcript
//! is replayed, but the per-tool tool-call state is NOT persisted.
//! DRIP, keying by `session_id`, would otherwise see existing
//! `reads` rows and return `[DRIP: unchanged]` / delta via deny —
//! which Claude Code does NOT register as a successful Read, so
//! the next Edit fails with "File must be read first".
//!
//! Initial fix shipped only `compact` / `clear`. Field reports of
//! the same bug post-`--resume` confirmed `resume` belongs in the
//! list too: the docs claim resume "preserves agent state", but
//! that refers to the conversation transcript, not the in-memory
//! read-tracker. Adding `resume` costs at most one full re-read
//! per file the agent touches after resuming — cheap insurance
//! against the much more painful Edit-rejection failure mode.
//!
//! `source = "startup"` always carries a fresh session_id (no DRIP
//! baselines yet by definition) — left as a no-op; calling
//! `reset()` on a never-seen id would be harmless but wasteful.

use crate::core::session::Session;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct SessionStart {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    source: Option<String>,
}

/// Sources that wipe Claude Code's read-tracker. `startup` carries
/// a fresh session_id so DRIP has nothing to drop and the call
/// would be a no-op anyway. Future / unknown values we leave alone
/// — wrong-but-safe is "keep baselines": the worst case is paying
/// full content for files we'd otherwise have diffed.
const TRACKER_RESETTING_SOURCES: &[&str] = &["compact", "clear", "resume"];

pub fn handle(stdin_payload: &str) -> Result<String> {
    if std::env::var_os("DRIP_DISABLE").is_some() {
        return Ok(empty());
    }
    let mut compaction_notice: Option<String> = None;
    if let Err(e) = drop_baselines_if_needed(stdin_payload, &mut compaction_notice) {
        // Never block the session start. Surface to stderr so users
        // running with debug logging can spot why DRIP didn't recover
        // after a compaction.
        eprintln!("drip: SessionStart handler failed: {e:#}");
    }
    // v9: when a compaction wiped the per-session baselines, surface
    // a one-shot notice in `additionalContext` so the agent's first
    // turn after compaction explicitly knows "DRIP just reset its
    // baselines" — without that, the next file Read returns full
    // content via FullFirst→allow with no in-band signal explaining
    // why the conversation just lost its file memory.
    Ok(match compaction_notice {
        Some(msg) => json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": msg,
            }
        })
        .to_string(),
        None => empty(),
    })
}

fn drop_baselines_if_needed(
    stdin_payload: &str,
    compaction_notice: &mut Option<String>,
) -> Result<()> {
    let p: SessionStart =
        serde_json::from_str(stdin_payload).context("SessionStart payload malformed")?;
    let source = p.source.as_deref().unwrap_or("").to_string();
    if !TRACKER_RESETTING_SOURCES.contains(&source.as_str()) {
        return Ok(());
    }
    let session_id = match p.session_id.filter(|s| !s.is_empty()) {
        Some(id) => id,
        // No session_id → can't scope the wipe safely. Bail rather
        // than nuking a session we can't identify.
        None => return Ok(()),
    };
    // Re-open the session under the same id so the wipe finds the
    // right rows. `reset_for_compaction` (v9) deletes per-session
    // reads + GCs orphan blobs and ALSO bumps the compaction ledger
    // (`context_epoch` / `last_compaction_at` / `compaction_count`)
    // so the next first-read of each file picks up the
    // `↺ context was compacted (#N)` decoration. Differs from plain
    // `reset()` — which is bound to the user-facing `drip reset`
    // command — by preserving the `sessions` row so the counters
    // survive the wipe.
    let session = Session::open_with_id(session_id)?;
    session.reset_for_compaction()?;
    // Surface the count back to the caller so the SessionStart
    // response can carry an `additionalContext` notice. Failure to
    // read the freshly-bumped ledger isn't fatal — we'd just emit a
    // generic notice without the count.
    let count = session
        .compaction_state()
        .ok()
        .flatten()
        .map(|c| c.count)
        .unwrap_or(0);
    let count_str = if count > 0 {
        format!(" (#{count})")
    } else {
        String::new()
    };
    *compaction_notice = Some(format!(
        "DRIP: ↺ context was {source}ed{count_str} — per-session baselines have been reset. \
         The next read of each file will return full content (or a semantically-compressed view); \
         subsequent re-reads use the normal unchanged/delta path."
    ));
    Ok(())
}

fn empty() -> String {
    // SessionStart accepts `hookSpecificOutput.additionalContext` to
    // inject text into the agent's first prompt of the new turn. We
    // could surface a "DRIP: dropped N baselines after compaction"
    // notice here, but it's noise — the per-file `↔ unchanged since
    // last session` decoration on the next read carries the same
    // information at the right time.
    json!({}).to_string()
}
