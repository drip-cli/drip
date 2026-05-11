use crate::core::session::Session;
use crate::core::tokens;
use crate::core::tracker::{self, FallbackReason, ReadOutcome, RegistryStatus};
use anyhow::Result;

/// Compact summary of a ReadOutcome — used by `record_event` so the
/// replay log doesn't have to re-derive these from the rendered string.
pub struct OutcomeSummary {
    pub kind: &'static str,
    pub fallback_reason: Option<String>,
    pub tokens_full: i64,
    pub tokens_sent: i64,
}

pub fn summarize(outcome: &ReadOutcome) -> OutcomeSummary {
    match outcome {
        ReadOutcome::FullFirst {
            tokens,
            compressed,
            registry,
            ..
        } => {
            let body_tokens = compressed.as_ref().map(|c| c.tokens).unwrap_or(*tokens);
            OutcomeSummary {
                kind: if compressed.is_some() {
                    "first-compressed"
                } else {
                    "first"
                },
                fallback_reason: None,
                tokens_full: *tokens,
                tokens_sent: body_tokens + tracker::registry_extra_tokens(registry),
            }
        }
        ReadOutcome::Unchanged { tokens_full } => OutcomeSummary {
            kind: "unchanged",
            fallback_reason: None,
            tokens_full: *tokens_full,
            tokens_sent: 0,
        },
        ReadOutcome::Delta {
            tokens_full,
            tokens_sent,
            ..
        } => OutcomeSummary {
            kind: "delta",
            fallback_reason: None,
            tokens_full: *tokens_full,
            tokens_sent: *tokens_sent,
        },
        ReadOutcome::FullFallback { reason, tokens, .. } => OutcomeSummary {
            kind: "fallback",
            fallback_reason: Some(reason.label()),
            tokens_full: *tokens,
            tokens_sent: *tokens,
        },
        ReadOutcome::Deleted => OutcomeSummary {
            kind: "deleted",
            fallback_reason: None,
            tokens_full: 0,
            tokens_sent: 0,
        },
        ReadOutcome::Passthrough => OutcomeSummary {
            kind: "passthrough",
            fallback_reason: None,
            tokens_full: 0,
            tokens_sent: 0,
        },
        ReadOutcome::EditCertificate {
            tokens_full,
            tokens_sent,
            ..
        } => OutcomeSummary {
            kind: "edit-cert",
            fallback_reason: None,
            tokens_full: *tokens_full,
            tokens_sent: *tokens_sent,
        },
        ReadOutcome::WindowUnchanged {
            tokens_full_window, ..
        } => OutcomeSummary {
            kind: "partial-unchanged",
            fallback_reason: None,
            tokens_full: *tokens_full_window,
            tokens_sent: 0,
        },
        ReadOutcome::WindowDelta {
            tokens_full_window,
            tokens_sent,
            ..
        } => OutcomeSummary {
            kind: "partial-delta",
            fallback_reason: None,
            tokens_full: *tokens_full_window,
            tokens_sent: *tokens_sent,
        },
    }
}

/// Live session-level decoration applied to a rendered outcome.
/// Computed once by `render_and_record` and passed through to
/// `render_with_session`. Empty in the dry-run path.
#[derive(Debug, Default)]
pub struct SessionDecoration {
    /// `true` iff this session was found in `expired_sessions` at
    /// open time. The renderer fires a one-shot notice on the
    /// FIRST read; the caller (`render_and_record`) clears the
    /// flag on the live `Session` after rendering so subsequent
    /// reads don't repeat it.
    pub expired_resumed: bool,
    /// Pre-formatted `⏱ session expires in N min` string, present
    /// when the session is in the last 10% of its TTL window.
    pub ttl_warning: Option<String>,
    /// v9 ledger: how many times this session has been compacted
    /// (`SessionStart:compact|clear|resume` fires). When > 0, first
    /// reads are decorated with `↺ context was compacted (#N)`
    /// instead of the generic "session expired" notice — the
    /// `↺` form is more accurate (the wipe was deliberate, not a
    /// TTL purge) and tells the agent the previous baselines were
    /// reset because the conversation got compacted.
    pub compaction_count: i64,
}

/// Render the outcome AND log it to `read_events` so `drip replay` can
/// show the user exactly what the agent received. Replaces the bare
/// `render(...)` call at every interception site (Read hook, Bash hook,
/// MCP `tools/call`, CLI `drip read`).
pub fn render_and_record(session: &Session, file_path: &str, outcome: ReadOutcome) -> String {
    let deco = build_session_decoration(session);
    let summary = summarize(&outcome);
    let rendered = render_with_session(file_path, outcome, &deco);
    if deco.expired_resumed {
        // One-shot: future reads in this revived session shouldn't
        // repeat the notice — it'd confuse the agent on every read.
        session.was_expired.set(false);
    }

    // process_read canonicalizes the path before writing to `reads`
    // and `lifetime_per_file` (so `/tmp/x` and `/private/tmp/x`
    // resolve to one row on macOS) but render_and_record was called
    // with the caller-supplied non-canonical path. Without this
    // step the per-file UPDATEs below would match zero rows.
    let canonical = tracker::canonical_key(std::path::Path::new(file_path));

    // Honest accounting: the agent receives the entire rendered
    // string (DRIP header + substantive body). For substantive
    // intercepts (Unchanged, Delta, FullFirst-deny, EditCertificate,
    // Window*) the inner path's `tokens_sent` only counts the body —
    // top up by the rendered-byte delta so `drip meter` reflects the
    // ~30-50 token header overhead the agent really pays.
    //
    // EXCEPTION — `fallback` kind (Binary / NonUtf8 / Ignored /
    // HugeFile / Symlink placeholders): the placeholder *is* the
    // entire DRIP-rendered substitute and carries 0 savings against
    // the native counterfactual by design. Counting the header here
    // would push tokens_sent past tokens_full and surface a tiny
    // negative-savings line item that the original "binary fallback
    // must not claim savings" invariant explicitly rejects.
    let rendered_tokens = tokens::estimate(&rendered);
    let count_header = !matches!(summary.kind, "fallback");
    let final_tokens_sent = if count_header {
        rendered_tokens
    } else {
        summary.tokens_sent
    };
    let header_overhead = (final_tokens_sent - summary.tokens_sent).max(0);
    if header_overhead > 0 {
        let _ = session.bump_lifetime_overhead(&canonical, header_overhead);
    }

    let _ = session.record_event(
        &canonical,
        summary.kind,
        summary.fallback_reason.as_deref(),
        summary.tokens_full,
        final_tokens_sent,
        &rendered,
    );
    rendered
}

pub fn build_session_decoration(session: &Session) -> SessionDecoration {
    let expired_resumed = session.was_expired.get();
    let ttl_warning = session
        .seconds_until_expiry()
        .filter(|remaining| {
            // The "<10% remaining" threshold the spec calls for.
            *remaining < crate::core::session::session_ttl_secs() / 10
        })
        .map(|remaining| {
            format!(
                "⏱ session expires in {} — run `drip reset` to start fresh",
                format_duration(remaining)
            )
        });
    // count=0 (missing row) → no decoration, same as never-compacted.
    let compaction_count = session
        .compaction_state()
        .ok()
        .flatten()
        .map(|c| c.count)
        .unwrap_or(0);
    SessionDecoration {
        expired_resumed,
        ttl_warning,
        compaction_count,
    }
}

/// Format the session-level decoration into a header segment. Each
/// piece is prefixed with ` | ` so it can be appended directly into
/// the existing header line. `on_first_read` gates the
/// "session expired" notice — by definition you can only land on a
/// FullFirst after a tombstone-flagged reopen; on Unchanged/Delta
/// the session was alive when we recorded the baseline.
pub fn build_session_segment(deco: &SessionDecoration, on_first_read: bool) -> String {
    let mut out = String::new();
    if on_first_read {
        // The v9 compaction notice takes precedence over the generic
        // "session expired" decoration: when both apply (compaction
        // wipes per-session reads, which the renderer's first-read
        // path will hit) the ↺ notice is more accurate — the wipe
        // was a deliberate compaction-driven reset, not an inactivity
        // TTL purge.
        if deco.compaction_count > 0 {
            out.push_str(&format!(
                " | ↺ context was compacted (#{}) — baseline reset",
                deco.compaction_count
            ));
        } else if deco.expired_resumed {
            out.push_str(" | ℹ session expired — fresh baseline started");
        }
    }
    if let Some(w) = &deco.ttl_warning {
        out.push_str(" | ");
        out.push_str(w);
    }
    out
}

fn format_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else {
        format!("{}h{:02}", secs / 3600, (secs % 3600) / 60)
    }
}

/// Render a `drip read <file>` invocation to stdout-ready text.
pub fn run(file_path: &str) -> Result<String> {
    run_with(file_path, false)
}

pub fn run_with(file_path: &str, dry_run: bool) -> Result<String> {
    let session = Session::open()?;
    let outcome = if dry_run {
        tracker::process_read_dry(&session, file_path)?
    } else {
        tracker::process_read(&session, file_path)?
    };
    let body = if dry_run {
        // Dry-run skips the replay log too — no state mutated, period.
        render(file_path, outcome)
    } else {
        render_and_record(&session, file_path, outcome)
    };
    if dry_run {
        Ok(format!("[DRIP: dry-run, no state mutated]\n{body}"))
    } else {
        Ok(body)
    }
}

pub fn render(file_path: &str, outcome: ReadOutcome) -> String {
    render_with_session(file_path, outcome, &SessionDecoration::default())
}

/// Single source of truth for the `unchanged` header. Used by
/// `render_with_session` to ship the sentinel to the agent AND by
/// `tracker.rs` to decide whether the sentinel actually saves tokens
/// versus the native counterfactual (see `DripOverheadBiggerThanFile`).
/// `session_seg` carries `⏱` / `↺` / `ℹ` decorations the runtime
/// computed for this read — must be the literal segment the renderer
/// would emit so the estimate matches the real rendered bytes.
pub fn render_unchanged(file_path: &str, tokens_full: i64, session_seg: &str) -> String {
    if tokens_full == 0 {
        // 0-byte file → bytes/4 = 0, so `0 tokens` reads like a bug.
        // Make the empty case explicit so callers can distinguish a
        // "we have no content" sentinel from a real zero-savings claim.
        format!("[DRIP: unchanged (empty file) | {file_path}]\n")
    } else {
        format!(
            "[DRIP: unchanged since last read | 0 tokens sent ({tokens_full} saved){session_seg} | {file_path}]\n"
        )
    }
}

/// Same as `render_unchanged` but for a partial read where only a
/// window of the file is in scope. `WindowUnchanged` outcomes never
/// carry session decorations (the window logic predates the v9 ledger
/// and the per-window sentinel is short enough already), so no
/// `session_seg` parameter.
pub fn render_window_unchanged(
    file_path: &str,
    start_line: usize,
    end_line: usize,
    tokens_full_window: i64,
) -> String {
    format!(
        "[DRIP: unchanged (lines {start_line}-{end_line}) | 0 tokens sent ({tokens_full_window} saved) | {file_path}]\n"
    )
}

/// Render the `delta only` header + diff body. `hunk_summary` is the
/// per-hunk function-name TOC (`None` when the diff has < 2 hunks or
/// the precomputed cache didn't capture it); passing the same value
/// the renderer would emit is required for the estimate to match.
pub fn render_delta(
    file_path: &str,
    tokens_full: i64,
    tokens_sent: i64,
    hunk_summary: Option<&[(usize, Option<String>)]>,
    session_seg: &str,
    diff: &str,
) -> String {
    let pct = tokens::percent_saved(tokens_full, tokens_sent);
    let summary_seg = build_hunk_summary_segment(hunk_summary);
    let header = format!(
        "[DRIP: delta only | {pct}% token reduction ({tokens_sent}/{tokens_full}){summary_seg}{session_seg} | {file_path}]\n"
    );
    format!("{header}{diff}")
}

pub fn render_window_delta(
    file_path: &str,
    start_line: usize,
    end_line: usize,
    tokens_full_window: i64,
    tokens_sent: i64,
    diff: &str,
) -> String {
    let pct = tokens::percent_saved(tokens_full_window, tokens_sent);
    let header = format!(
        "[DRIP: delta only (lines {start_line}-{end_line}) | {pct}% token reduction ({tokens_sent}/{tokens_full_window}) | {file_path}]\n"
    );
    format!("{header}{diff}")
}

pub fn render_edit_certificate(
    file_path: &str,
    after_hash: &str,
    touched_ranges: &[(usize, usize)],
    touched_symbols: &[String],
    total_lines: usize,
    tokens_full: i64,
    tokens_sent: i64,
) -> String {
    let pct = tokens::percent_saved(tokens_full, tokens_sent);
    let short_hash: String = after_hash.chars().take(12).collect();
    let body = tracker::edit_certificate_body(
        file_path,
        after_hash,
        touched_ranges,
        touched_symbols,
        total_lines,
    );
    format!(
        "[DRIP: edit verified | {pct}% reduction ({tokens_sent}/{tokens_full} tokens) | hash: {short_hash} | {file_path}]\n{body}"
    )
}

fn build_hunk_summary_segment(hunk_summary: Option<&[(usize, Option<String>)]>) -> String {
    hunk_summary
        .map(|h| {
            let parts: Vec<String> = h
                .iter()
                .take(6) // keep header readable on >6-hunk diffs
                .map(|(ln, name)| match name {
                    Some(n) => format!("{n} (ln {ln})"),
                    None => format!("ln {ln}"),
                })
                .collect();
            let extra = h.len().saturating_sub(6);
            let body = parts.join(", ");
            if extra > 0 {
                format!(" | {} hunks: {body}, +{extra} more", h.len())
            } else {
                format!(" | {} hunks: {body}", h.len())
            }
        })
        .unwrap_or_default()
}

pub fn render_with_session(
    file_path: &str,
    outcome: ReadOutcome,
    deco: &SessionDecoration,
) -> String {
    let session_seg = build_session_segment(
        deco,
        /* on_first_read = */ matches!(&outcome, ReadOutcome::FullFirst { .. }),
    );
    match outcome {
        ReadOutcome::FullFirst {
            content,
            tokens,
            compressed,
            registry,
        } => {
            let (registry_segment, registry_trailer) = render_registry(&registry);
            match compressed {
                Some(c) => {
                    let sent = c.tokens + tracker::registry_extra_tokens(&registry);
                    let pct = tokens::percent_saved(tokens, sent);
                    let header = format!(
                        "[DRIP: full read (semantic-compressed) | {pct}% reduction \
                         ({sent}/{full} tokens) | {funcs} functions elided, \
                         {lines} lines hidden{reg}{ses} | run `drip refresh` for full content | {file_path}]\n",
                        sent = sent,
                        full = tokens,
                        funcs = c.functions_elided,
                        lines = c.lines_elided,
                        reg = registry_segment,
                        ses = session_seg,
                    );
                    format!("{header}{}{registry_trailer}", c.text)
                }
                None if content.is_empty() => {
                    // 0-byte file → bytes/4 = 0, so `0 tokens` is technically
                    // correct but reads like a bug. Make the empty case
                    // explicit instead.
                    format!("[DRIP: empty file | {file_path}]\n")
                }
                None => {
                    let header = format!(
                        "[DRIP: full read | {tokens} tokens{registry_segment}{session_seg} | {file_path}]\n"
                    );
                    format!("{header}{content}{registry_trailer}")
                }
            }
        }
        ReadOutcome::Unchanged { tokens_full } => {
            render_unchanged(file_path, tokens_full, &session_seg)
        }
        ReadOutcome::Delta {
            diff,
            tokens_full,
            tokens_sent,
            hunk_summary,
        } => render_delta(
            file_path,
            tokens_full,
            tokens_sent,
            hunk_summary.as_deref(),
            &session_seg,
            &diff,
        ),
        ReadOutcome::FullFallback {
            content,
            reason,
            tokens,
        } => {
            let label = reason.label();
            match reason {
                FallbackReason::DripOverheadBiggerThanFile => content,
                FallbackReason::Binary | FallbackReason::NonUtf8 => {
                    format!("[DRIP: {label} | {tokens} tokens | {file_path}]\n{content}\n")
                }
                _ => {
                    let header = format!("[DRIP: {label} | {tokens} tokens | {file_path}]\n");
                    format!("{header}{content}")
                }
            }
        }
        ReadOutcome::Deleted => {
            format!("[DRIP: file deleted since last read | {file_path}]\n")
        }
        ReadOutcome::Passthrough => {
            format!(
                "[DRIP: post-edit passthrough | next read after this is normal | {file_path}]\n"
            )
        }
        ReadOutcome::EditCertificate {
            after_hash,
            touched_ranges,
            touched_symbols,
            total_lines,
            tokens_full,
            tokens_sent,
            ..
        } => render_edit_certificate(
            file_path,
            &after_hash,
            &touched_ranges,
            &touched_symbols,
            total_lines,
            tokens_full,
            tokens_sent,
        ),
        ReadOutcome::WindowUnchanged {
            start_line,
            end_line,
            tokens_full_window,
        } => render_window_unchanged(file_path, start_line, end_line, tokens_full_window),
        ReadOutcome::WindowDelta {
            diff,
            start_line,
            end_line,
            tokens_full_window,
            tokens_sent,
        } => render_window_delta(
            file_path,
            start_line,
            end_line,
            tokens_full_window,
            tokens_sent,
            &diff,
        ),
    }
}

/// Render the cross-session registry decoration for a first-read
/// header. Returns `(header_segment, trailer)`:
///
/// - `header_segment` slots into the `[DRIP: ...]` line as
///   ` | ↔ unchanged since last session (3h ago)` etc. Empty when
///   the file is unknown to the registry.
/// - `trailer` is appended AFTER the file content for the changed
///   case, showing a unified diff against the prior session's
///   content. Empty for `Unknown` and `Unchanged` — the agent reads
///   the body uninterrupted.
fn render_registry(status: &RegistryStatus) -> (String, String) {
    match status {
        RegistryStatus::Unknown => (String::new(), String::new()),
        RegistryStatus::Unchanged {
            last_seen_secs_ago,
            last_git_branch,
        } => {
            let age = format_age(*last_seen_secs_ago);
            let branch = match last_git_branch {
                Some(b) => format!(", branch: {b}"),
                None => String::new(),
            };
            (
                format!(" | ↔ unchanged since last session ({age}{branch})"),
                String::new(),
            )
        }
        RegistryStatus::Changed {
            last_seen_secs_ago,
            last_git_branch,
            added_lines,
            removed_lines,
            diff_text,
        } => {
            let age = format_age(*last_seen_secs_ago);
            let branch = match last_git_branch {
                Some(b) => format!(", branch was: {b}"),
                None => String::new(),
            };
            let segment = format!(
                " | ↕ changed since last session ({age}): +{added_lines} lines, -{removed_lines} lines{branch}"
            );
            // The trailer is fenced with horizontal rules so the
            // agent can recognise it as out-of-band orientation —
            // not part of the file content.
            let trailer = format!(
                "\n\n── Changes since last session ──────────────────────────────\n\
                 {diff_text}\n\
                 ────────────────────────────────────────────────────────────\n"
            );
            (segment, trailer)
        }
    }
}

fn format_age(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    //! Pin the contract that the pure header-format helpers
    //! (`render_unchanged`, `render_delta`, …) produce byte-identical
    //! output to `render_with_session` for the same outcome shape.
    //!
    //! Why we need this: `tracker.rs` calls these helpers to estimate
    //! the rendered cost BEFORE deciding whether to serve a sentinel
    //! or fall back to `DripOverheadBiggerThanFile`. If a future edit
    //! changes `render_with_session` without updating the helper (or
    //! vice versa), the gate decision and the actual delivery would
    //! drift silently — DRIP would underestimate the cost on every
    //! decorated read, occasionally claiming "savings" on a sentinel
    //! that's larger than native. This test fails fast if anyone
    //! breaks the single-source-of-truth contract.
    use super::*;
    use crate::core::tracker::ReadOutcome;
    fn no_deco() -> SessionDecoration {
        SessionDecoration::default()
    }
    #[test]
    fn render_with_session_unchanged_matches_helper() {
        let out = render_with_session(
            "/p.txt",
            ReadOutcome::Unchanged { tokens_full: 800 },
            &no_deco(),
        );
        let expected = render_unchanged("/p.txt", 800, "");
        assert_eq!(out, expected);
    }
    #[test]
    fn render_with_session_unchanged_empty_file_matches_helper() {
        let out = render_with_session(
            "/empty.txt",
            ReadOutcome::Unchanged { tokens_full: 0 },
            &no_deco(),
        );
        // Empty-file branch goes through render_unchanged too.
        let expected = render_unchanged("/empty.txt", 0, "");
        assert_eq!(out, expected);
    }
    #[test]
    fn render_with_session_unchanged_carries_ttl_warning() {
        // Regression for the session-decoration plumbing gap: an
        // Unchanged read in the last 10% of the TTL window has a `⏱`
        // segment appended. The estimator must produce the same string
        // — that's the whole point of plumbing session_seg through.
        let deco = SessionDecoration {
            ttl_warning: Some("⏱ session expires in 5 min — run `drip reset`".into()),
            ..SessionDecoration::default()
        };
        let out = render_with_session("/p.txt", ReadOutcome::Unchanged { tokens_full: 800 }, &deco);
        let seg = build_session_segment(&deco, false);
        let expected = render_unchanged("/p.txt", 800, &seg);
        assert_eq!(out, expected);
        assert!(
            out.contains("⏱ session expires"),
            "TTL warning must surface: {out}"
        );
    }
    #[test]
    fn render_with_session_delta_matches_helper() {
        let diff = "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n".to_string();
        let summary = vec![(10usize, Some("fn foo".to_string()))];
        let outcome = ReadOutcome::Delta {
            diff: diff.clone(),
            tokens_full: 1000,
            tokens_sent: 50,
            hunk_summary: Some(summary.clone()),
        };
        let out = render_with_session("/p.rs", outcome, &no_deco());
        let expected = render_delta("/p.rs", 1000, 50, Some(&summary), "", &diff);
        assert_eq!(out, expected);
    }
    #[test]
    fn render_with_session_delta_no_summary_matches_helper() {
        let diff = "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n".to_string();
        let outcome = ReadOutcome::Delta {
            diff: diff.clone(),
            tokens_full: 1000,
            tokens_sent: 50,
            hunk_summary: None,
        };
        let out = render_with_session("/p.rs", outcome, &no_deco());
        let expected = render_delta("/p.rs", 1000, 50, None, "", &diff);
        assert_eq!(out, expected);
    }
    #[test]
    fn render_with_session_window_unchanged_matches_helper() {
        let outcome = ReadOutcome::WindowUnchanged {
            start_line: 10,
            end_line: 30,
            tokens_full_window: 200,
        };
        let out = render_with_session("/p.txt", outcome, &no_deco());
        let expected = render_window_unchanged("/p.txt", 10, 30, 200);
        assert_eq!(out, expected);
    }
    #[test]
    fn render_with_session_window_delta_matches_helper() {
        let diff = "--- a\n+++ b\n@@ -10,3 +10,3 @@\n line 10\n-line 11\n+line 11 new\n line 12\n"
            .to_string();
        let outcome = ReadOutcome::WindowDelta {
            diff: diff.clone(),
            start_line: 10,
            end_line: 30,
            tokens_full_window: 200,
            tokens_sent: 25,
        };
        let out = render_with_session("/p.txt", outcome, &no_deco());
        let expected = render_window_delta("/p.txt", 10, 30, 200, 25, &diff);
        assert_eq!(out, expected);
    }
    #[test]
    fn render_with_session_edit_certificate_matches_helper() {
        let outcome = ReadOutcome::EditCertificate {
            before_hash: "deadbeef".into(),
            after_hash: "abcdef1234567890".into(),
            touched_ranges: vec![(10, 12)],
            touched_symbols: vec!["fn bar".into()],
            total_lines: 200,
            tokens_full: 500,
            tokens_sent: 40,
        };
        let out = render_with_session("/p.rs", outcome, &no_deco());
        let expected = render_edit_certificate(
            "/p.rs",
            "abcdef1234567890",
            &[(10, 12)],
            &["fn bar".into()],
            200,
            500,
            40,
        );
        assert_eq!(out, expected);
    }
}
