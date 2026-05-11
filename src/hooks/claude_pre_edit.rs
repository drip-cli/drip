//! Claude Code PreToolUse hook for Edit / MultiEdit / Write /
//! NotebookEdit — STOP edits that would land inside a function body
//! the agent never saw.
//!
//! When `compress::compress` elides a long function body, the agent
//! receives a stub like
//!   `...  # [DRIP-elided: original L333-L498, 165 lines | drip refresh to expand]`
//! Without this hook, an agent can still issue an Edit whose
//! `old_string` happens to exist verbatim somewhere in the elided
//! body — Claude Code will run the edit, the change lands, and the
//! agent has just modified bytes it never saw. PostToolUse catches
//! this *after the fact*; the pre-edit hook catches it *before*.
//!
//! Vehicle: `permissionDecision = "deny"` with a reason that lists
//! the elided region(s) hit and tells the agent how to recover (run
//! `drip refresh`, re-Read, retry). Never blocks Edits when there's
//! no source map (uncompressed reads, untracked files) — degrades
//! to the existing PostToolUse warning. Bypass via
//! `DRIP_PRE_EDIT_WARN=0`.

use crate::core::compress::SourceMap;
use crate::core::session::{self, Session};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Debug, Deserialize)]
struct PreToolUse {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<Value>,
}

const EDIT_TOOLS: &[&str] = &["Edit", "MultiEdit", "Write", "NotebookEdit"];

pub fn handle(stdin_payload: &str) -> Result<String> {
    if std::env::var_os("DRIP_DISABLE").is_some()
        || std::env::var("DRIP_PRE_EDIT_WARN").as_deref() == Ok("0")
    {
        return Ok(allow());
    }
    match check(stdin_payload) {
        Ok(Some(reason)) => Ok(deny(reason)),
        // Surface to stderr but never block on a hook failure — the
        // PostToolUse warning is still there as a safety net.
        Ok(None) => Ok(allow()),
        Err(e) => {
            eprintln!("drip: pre-edit warner failed: {e:#}");
            Ok(allow())
        }
    }
}

/// Returns `Some(reason)` when we want to block the edit, `None`
/// otherwise. Returning `None` is the default — we degrade open.
fn check(stdin_payload: &str) -> Result<Option<String>> {
    let p: PreToolUse =
        serde_json::from_str(stdin_payload).context("PreToolUse payload malformed")?;

    let Some(name) = p.tool_name.as_deref() else {
        return Ok(None);
    };
    if !EDIT_TOOLS.contains(&name) {
        return Ok(None);
    }
    let Some(input) = p.tool_input else {
        return Ok(None);
    };
    let Some(file_path) = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .or_else(|| input.get("notebook_path").and_then(|v| v.as_str()))
    else {
        return Ok(None);
    };

    let resolved = session::resolve_path(file_path);
    let canonical = resolved
        .canonicalize()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| resolved.to_string_lossy().into_owned());

    let session = match p.session_id.filter(|s| !s.is_empty()) {
        Some(id) => Session::open_with_id(id)?,
        None => Session::open()?,
    };

    // No source map → no compression fired (or no Read at all). Either
    // way we have nothing to warn about; let the edit proceed and
    // Claude Code's own read-first guard / DRIP's PostToolUse cert
    // handle the rest.
    let Some(source_map) = session.get_source_map(&canonical)? else {
        return Ok(None);
    };
    if source_map.is_empty() {
        return Ok(None);
    }
    // We need the original content to translate `old_string` matches
    // into line numbers. If the row got pruned between the Read and
    // this Edit (rare but possible — concurrent `drip cache gc`),
    // bail open.
    let Some(prev) = session.get_read(&canonical)? else {
        return Ok(None);
    };

    let mut hits = collect_hits(name, &input, &prev.content, &source_map);
    hits.sort_by_key(|h| h.original_start);
    hits.dedup_by(|a, b| a.original_start == b.original_start && a.original_end == b.original_end);
    if hits.is_empty() {
        return Ok(None);
    }
    Ok(Some(render_reason(&canonical, name, &hits)))
}

/// One source-map entry the candidate edit would touch. Carried
/// alongside the original `SourceMapEntry` we matched so we can
/// preserve `symbol_name` for the deny message.
#[derive(Debug, Clone)]
struct Hit {
    original_start: usize,
    original_end: usize,
    symbol_name: Option<String>,
}

fn collect_hits(
    tool_name: &str,
    input: &Value,
    prev_content: &str,
    source_map: &SourceMap,
) -> Vec<Hit> {
    let mut out = Vec::new();
    match tool_name {
        "Edit" => {
            if let Some(old_s) = input.get("old_string").and_then(|v| v.as_str()) {
                push_match_hits(prev_content, old_s, source_map, &mut out);
            }
        }
        "MultiEdit" => {
            if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
                for e in edits {
                    if let Some(old_s) = e.get("old_string").and_then(|v| v.as_str()) {
                        push_match_hits(prev_content, old_s, source_map, &mut out);
                    }
                }
            }
        }
        "Write" => {
            // Whole-file replacement. The agent never saw the elided
            // bodies — overwriting the file means whatever was inside
            // them is gone. Treat every elided region as a hit.
            for entry in source_map.iter().filter(|e| e.elided) {
                out.push(Hit {
                    original_start: entry.original_start,
                    original_end: entry.original_end,
                    symbol_name: entry.symbol_name.clone(),
                });
            }
        }
        "NotebookEdit" => {
            // Notebook edits are cell-scoped and don't carry line
            // numbers we can map back. The compressor doesn't run on
            // .ipynb anyway — leave the post-edit warner to catch
            // these.
        }
        _ => {}
    }
    out
}

/// Find every occurrence of `needle` inside `prev_content`, compute
/// the line range each one spans, and record a Hit when that range
/// overlaps an elided source-map entry. Empty / very short needles
/// would match too aggressively — we require at least 4 bytes so a
/// `def `, `fn `, `}` etc. doesn't trigger spurious blocks.
fn push_match_hits(prev_content: &str, needle: &str, source_map: &SourceMap, hits: &mut Vec<Hit>) {
    if needle.len() < 4 {
        return;
    }
    let mut start = 0usize;
    while let Some(rel) = prev_content[start..].find(needle) {
        let abs = start + rel;
        // Convert byte position → 1-indexed line number. The match's
        // first line is the line containing the match's first byte;
        // its last line is the line containing the match's last byte.
        let first_line = prev_content[..abs].matches('\n').count() + 1;
        let last_line = first_line + needle.matches('\n').count();
        for entry in source_map.iter().filter(|e| e.elided) {
            // Overlap iff [first_line..=last_line] ∩
            // [original_start..=original_end] is non-empty.
            if last_line >= entry.original_start && first_line <= entry.original_end {
                hits.push(Hit {
                    original_start: entry.original_start,
                    original_end: entry.original_end,
                    symbol_name: entry.symbol_name.clone(),
                });
            }
        }
        start = abs + needle.len().max(1);
    }
}

fn render_reason(canonical: &str, tool: &str, hits: &[Hit]) -> String {
    let mut details = String::new();
    for (i, h) in hits.iter().enumerate() {
        if i > 0 {
            details.push_str(", ");
        }
        match &h.symbol_name {
            Some(name) => details.push_str(&format!(
                "`{name}` (L{}-L{})",
                h.original_start, h.original_end
            )),
            None => details.push_str(&format!("L{}-L{}", h.original_start, h.original_end)),
        }
    }
    format!(
        "[DRIP: ⚠ STOP — {tool} targets elided region(s): {details}. \
         Body never sent to agent (semantic compression). \
         Run `drip refresh {canonical}` and Read again before editing, \
         or set DRIP_PRE_EDIT_WARN=0 to bypass this guard.]"
    )
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
