//! Gemini CLI before-compress hook.
//!
//! Gemini's compress event lets a registered hook run a one-off
//! command before the conversation context gets summarised /
//! compressed. The hook is **advisory** — Gemini doesn't read its
//! stdout for substitution semantics, only its exit code, so DRIP
//! uses it for one purpose: bumping the v9 compaction ledger and
//! wiping per-session reads via `Session::reset_for_compaction()`.
//!
//! Why this matters: like Claude Code's `SessionStart:compact`, the
//! agent's in-process read tracker gets rebuilt when the conversation
//! transcript is replayed against compressed context. DRIP's `reads`
//! rows would otherwise survive — the agent would re-issue a Read
//! tool call and DRIP would short-circuit it with an "unchanged"
//! payload that the agent never actually saw, breaking the
//! "Edit must Read first" invariant. Resetting baselines forces the
//! next Read of each file to be a genuine FullFirst the agent's
//! tracker registers.
//!
//! Failure semantics: any error at this stage is logged to stderr
//! and swallowed — never block Gemini's own compression flow. The
//! worst case is a stale baseline surviving the reset, which the
//! TTL-purge sweep cleans up within 2 h.

use crate::core::session::Session;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

/// Payload shape we accept from Gemini. The CLI's hook contract is
/// still loose upstream, so we match anything that smells like a
/// session identifier under either of the two field names we've seen.
#[derive(Debug, Deserialize, Default)]
struct CompressPayload {
    #[serde(default)]
    session_id: Option<String>,
    /// Some Gemini builds use camelCase — accept both rather than
    /// silently no-op when the key shape changes upstream.
    #[serde(default, rename = "sessionId")]
    session_id_camel: Option<String>,
}

pub fn handle(stdin_payload: &str) -> Result<String> {
    if std::env::var_os("DRIP_DISABLE").is_some() {
        return Ok(empty());
    }
    if let Err(e) = drop_baselines(stdin_payload) {
        eprintln!("drip: gemini-compress handler failed: {e:#}");
    }
    Ok(empty())
}

fn drop_baselines(stdin_payload: &str) -> Result<()> {
    // Empty payload is legal — Gemini may invoke the hook with no
    // body. Fall through to the env-derived session id.
    let p: CompressPayload = if stdin_payload.trim().is_empty() {
        CompressPayload::default()
    } else {
        serde_json::from_str(stdin_payload).context("Gemini compress payload malformed")?
    };
    // Resolution order: explicit field → DRIP_SESSION_ID → derived.
    // We do NOT bail when no id is present — `Session::open()`'s
    // derivation ladder (env → git → pid → cwd) finds the active
    // session the same way every other DRIP entry point does. That
    // matters because Gemini doesn't always supply session_id in the
    // hook payload; pinning to env-only would silently no-op.
    let session = match p
        .session_id
        .or(p.session_id_camel)
        .filter(|s| !s.is_empty())
    {
        Some(id) => Session::open_with_id(id)?,
        None => Session::open()?,
    };
    session.reset_for_compaction()?;
    Ok(())
}

/// Gemini doesn't consume the hook's stdout for substitution — any
/// JSON shape works as long as the process exits 0. Returning `{}`
/// matches what Claude Code's SessionStart contract uses, so the
/// shape stays consistent across agents.
fn empty() -> String {
    json!({}).to_string()
}
