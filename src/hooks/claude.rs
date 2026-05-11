use crate::commands::read;
use crate::core::session::Session;
use crate::core::tracker::{self, FallbackReason, ReadOutcome};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

/// Claude Code PreToolUse payload (subset we care about).
/// See: https://docs.claude.com/en/docs/claude-code/hooks
#[derive(Debug, Deserialize)]
struct PreToolUse {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
}

/// Claude Code reads JSON from the hook on stdout. We respond by either:
///   - allowing the call through (no special output, exit 0), or
///   - denying it and feeding the diff back as the deny reason — which
///     is how Claude actually receives the delta.
///
/// We use `permissionDecision = "deny"` only as a transport — the model
/// reads the reason as if it were the tool result.
pub fn handle(stdin_payload: &str) -> Result<String> {
    // Emergency escape hatch: `DRIP_DISABLE=1 claude …` bypasses interception
    // entirely without uninstalling the hooks.
    if std::env::var_os("DRIP_DISABLE").is_some() {
        return Ok(allow());
    }
    let payload: PreToolUse =
        serde_json::from_str(stdin_payload).context("PreToolUse JSON payload is malformed")?;

    if payload.tool_name.as_deref() != Some("Read") {
        return Ok(allow());
    }
    let Some(input) = payload.tool_input else {
        return Ok(allow());
    };
    let Some(file_path) = input.get("file_path").and_then(|v| v.as_str()) else {
        return Ok(allow());
    };
    let session = match payload.session_id.filter(|s| !s.is_empty()) {
        Some(id) => Session::open_with_id(id)?,
        None => Session::open()?,
    };

    // Partial reads (offset/limit) get a window-scoped intercept when
    // we have a baseline; otherwise they pass through to native (we
    // can't honestly claim unchanged without prior content to diff
    // against). Crucially, partial reads NEVER mutate the baseline —
    // the agent may have seen only a slice, so the next full read
    // must still serve the whole file.
    let offset = input
        .get("offset")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    let limit = input
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    if offset.is_some() || limit.is_some() {
        match tracker::process_partial_read(&session, file_path, offset, limit) {
            Ok(Some(outcome)) => {
                let rendered = read::render_and_record(&session, file_path, outcome);
                return Ok(deny(rendered));
            }
            Ok(None) => return Ok(allow()),
            Err(e) => {
                eprintln!("drip: process_partial_read failed for {file_path}: {e:#}");
                return Ok(allow());
            }
        }
    }

    // Claude's Read tool refuses files whose tokenized size exceeds
    // ~25 000 tokens with `File content (X tokens) exceeds maximum
    // allowed tokens (25000). Use offset and limit parameters…`. On
    // those files the default FullFirst → `allow` route delivers
    // nothing useful — the agent gets only the error. When the file's
    // estimated size sits past the budget, route through the
    // `DripRendered` entry point instead: it computes the compressed
    // view (skipped in NativePassthrough mode for cost reasons) and
    // writes sparse `seen_ranges` matching the body the agent will
    // actually receive. If compression yields a workable substitute,
    // we deliver it via `deny`; if it doesn't (non-code file, raw
    // data, `DRIP_NO_COMPRESS=1`), we fall back to `allow` and let
    // Claude's native error tell the agent to use `offset`/`limit`.
    // Threshold tuned to Claude's tokenizer (~2.5× tighter than
    // DRIP's `bytes/4` heuristic): 10 000 DRIP tokens ≈ 24-26k Claude
    // tokens, matching the empirical cutoff. Override via
    // `DRIP_CLAUDE_READ_TOKEN_BUDGET=<int>`.
    //
    // **Opt-in early-compress** (`DRIP_COMPRESS_FIRST_READ_MIN_BYTES`):
    // power users can widen the DripRendered route to fire on smaller
    // files. Trade-off documented in drip.md: substituting via `deny`
    // skips native Read, so Claude's read-before-edit tracker for that
    // file isn't populated and a subsequent Edit may fail. When the
    // file's estimated savings don't justify compression, the existing
    // `FullFirst { compressed: None }` fallthrough still routes to
    // `allow`, so this is a "compress when worthwhile, else native"
    // policy — never a worse outcome than disabled.
    let budget = claude_read_token_budget();
    let bytes_on_disk = std::fs::metadata(file_path)
        .ok()
        .map(|m| m.len())
        .unwrap_or(0);
    let estimated_drip_tokens = (bytes_on_disk / 4) as i64;
    let early_compress_min_bytes = compress_first_read_min_bytes();
    let want_compress_early =
        early_compress_min_bytes > 0 && bytes_on_disk >= early_compress_min_bytes;
    let route_through_drip_rendered = estimated_drip_tokens > budget || want_compress_early;

    let outcome = if route_through_drip_rendered {
        tracker::process_read(&session, file_path)
    } else {
        tracker::process_read_native_passthrough(&session, file_path)
    };
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            // Surface to stderr so users running Claude Code with debug
            // logging can see why DRIP backed off; never block the read.
            eprintln!("drip: process_read failed for {file_path}: {e:#}");
            return Ok(allow());
        }
    };

    // Over-budget FullFirst with a usable compressed view → substitute
    // instead of letting native fail.
    if route_through_drip_rendered {
        if let ReadOutcome::FullFirst {
            compressed: Some(_),
            ..
        } = &outcome
        {
            let rendered = read::render_and_record(&session, file_path, outcome);
            return Ok(deny(rendered));
        }
    }

    match &outcome {
        // Placeholder fallbacks are *substitutes*, not passthroughs.
        // If we let Claude's native Read run here, accounting would
        // charge the placeholder while the agent saw native output.
        ReadOutcome::FullFallback {
            reason:
                tracker::FallbackReason::Ignored
                | tracker::FallbackReason::Binary
                | tracker::FallbackReason::NonUtf8
                | tracker::FallbackReason::HugeFile
                | tracker::FallbackReason::Symlink,
            ..
        } => {
            let rendered = read::render_and_record(&session, file_path, outcome);
            Ok(deny(rendered))
        }
        ReadOutcome::Passthrough
        | ReadOutcome::FullFirst { .. }
        | ReadOutcome::FullFallback {
            reason:
                FallbackReason::LargeFile
                | FallbackReason::Truncated
                | FallbackReason::DiffBiggerThanFile
                | FallbackReason::DripOverheadBiggerThanFile
                | FallbackReason::DiffTooComplex { .. }
                | FallbackReason::ExternalChange,
            ..
        } => {
            // First read, native-equivalent full-text fallback, or
            // one-shot post-edit passthrough — let Claude's native Read
            // return the same file bytes DRIP accounted.
            // Still log it for `drip replay` so the user can see "first
            // read passed through here" in the trace.
            let summary = read::summarize(&outcome);
            let _ = session.record_event(
                file_path,
                summary.kind,
                summary.fallback_reason.as_deref(),
                summary.tokens_full,
                summary.tokens_sent,
                "[allow → native Read]",
            );
            // On the ExternalChange path we previously emitted a bare
            // `allow` — the agent saw a full re-read with no signal
            // explaining why DRIP "did nothing". Surface a one-line
            // notice via `additionalContext` so the agent's next turn
            // knows the baseline was refreshed because the file
            // changed under DRIP. Other passthrough reasons stay
            // silent: they're either expected (first read) or already
            // marked in the rendered placeholder content
            // (LargeFile/Truncated produce a header inside the body).
            if let ReadOutcome::FullFallback {
                reason: FallbackReason::ExternalChange,
                tokens,
                ..
            } = &outcome
            {
                return Ok(allow_with_context(&format!(
                    "[DRIP: native refresh | {tokens} tokens | \
                     {file_path} changed out-of-band since DRIP's last \
                     baseline — full content re-shipped natively to keep \
                     Claude's read-tracker in sync. Future re-reads of \
                     this file will return the normal unchanged/delta \
                     view.]"
                )));
            }
            Ok(allow())
        }
        ReadOutcome::Unchanged { .. }
        | ReadOutcome::Delta { .. }
        | ReadOutcome::Deleted
        | ReadOutcome::EditCertificate { .. } => {
            let rendered = read::render_and_record(&session, file_path, outcome);
            Ok(deny(rendered))
        }
        // Window outcomes only come back from `process_partial_read`,
        // which we already handled above for `offset`/`limit` payloads.
        // Reaching them here would mean `process_read` returned a
        // partial-read variant, which it never does — but `match`
        // exhaustiveness needs the arm.
        ReadOutcome::WindowUnchanged { .. } | ReadOutcome::WindowDelta { .. } => Ok(allow()),
    }
}

fn allow() -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow"
        }
    })
    .to_string()
}

/// `allow` + an out-of-band notice rendered into the agent's next-turn
/// context. Used for cases like ExternalChange where DRIP wants to
/// stay out of the way (let native Read fire so Claude's read-tracker
/// stays in sync) but still owes the agent an explanation for why no
/// `[DRIP: ...]` header arrived alongside the full re-read.
fn allow_with_context(context: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "additionalContext": context,
        }
    })
    .to_string()
}

fn deny(reason: String) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason
        }
    })
    .to_string()
}

/// DRIP-token budget past which Claude's Read tool would refuse the
/// file with `exceeds maximum allowed tokens (25000)`. Empirical
/// default: Claude's tokenizer is ~2.5× tighter than DRIP's `bytes/4`,
/// so 10 000 DRIP tokens ≈ 24-26k Claude tokens. Configurable via
/// `DRIP_CLAUDE_READ_TOKEN_BUDGET=<int>` for users on a different
/// model class (limit varies by Anthropic API revision).
const DEFAULT_CLAUDE_READ_TOKEN_BUDGET: i64 = 10_000;

fn claude_read_token_budget() -> i64 {
    std::env::var("DRIP_CLAUDE_READ_TOKEN_BUDGET")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_CLAUDE_READ_TOKEN_BUDGET)
}

/// Byte threshold above which DRIP attempts semantic compression on
/// the FIRST read (in addition to the existing over-budget path).
/// `0` (default) disables the early-compress route — the historical
/// behavior, kept on by default so Claude's read-before-edit tracker
/// is populated by the native Read on every reasonably-sized file.
///
/// Set `DRIP_COMPRESS_FIRST_READ_MIN_BYTES=<bytes>` to opt in. When the
/// file is at least that big AND a code language DRIP knows how to
/// compress, the first read returns the elided view via the `deny`
/// substitution channel. When compression yields no usable savings
/// (plain text, non-code, no long bodies), DRIP silently falls back to
/// the native passthrough — so the opt-in is non-destructive: at worst
/// it costs a no-op pass through the compressor.
fn compress_first_read_min_bytes() -> u64 {
    std::env::var("DRIP_COMPRESS_FIRST_READ_MIN_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(0)
}
