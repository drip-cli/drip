use crate::commands::read;
use crate::core::{
    compress,
    differ::{self, FileKind},
    ignore::Matcher,
    session::{self, RegistryRecord, Session},
    tokens,
};
use anyhow::{Context, Result};
use std::path::Path;

/// Compressed view shown to the agent (signatures + stubs). The
/// SQLite baseline always stores the untouched original so diffs
/// against future reads work normally.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CompressedView {
    pub text: String,
    pub tokens: i64,
    pub functions_elided: usize,
    pub lines_elided: usize,
    pub original_lines: usize,
    pub source_map: compress::SourceMap,
}

/// Cross-session header decoration on a first read. The actual
/// content is always full on first reads; this only drives the
/// `↔ unchanged` / `↕ changed` summary line.
#[derive(Debug, Clone)]
pub enum RegistryStatus {
    Unknown,
    Unchanged {
        last_seen_secs_ago: i64,
        last_git_branch: Option<String>,
    },
    /// Hash differs — header carries a summary plus a unified diff
    /// trailer.
    Changed {
        last_seen_secs_ago: i64,
        last_git_branch: Option<String>,
        added_lines: usize,
        removed_lines: usize,
        diff_text: String,
    },
}

#[derive(Debug)]
pub enum ReadOutcome {
    /// First read in this session. `compressed`, when present, is
    /// what the agent receives in place of `content`; `tokens` always
    /// counts the original.
    FullFirst {
        content: String,
        tokens: i64,
        compressed: Option<CompressedView>,
        registry: RegistryStatus,
    },
    /// File is unchanged since the last read.
    Unchanged { tokens_full: i64 },
    /// File changed — only a unified diff is sent.
    Delta {
        diff: String,
        tokens_full: i64,
        tokens_sent: i64,
        /// `Some(...)` for 2+-hunk diffs. The renderer turns this
        /// into a `name (ln N), name (ln M)` summary in the header.
        hunk_summary: Option<Vec<(usize, Option<String>)>>,
    },
    /// Fallbacks where a delta would be wrong or unhelpful.
    FullFallback {
        content: String,
        reason: FallbackReason,
        tokens: i64,
    },
    /// Tracked file no longer exists on disk.
    Deleted,
    /// One-shot bypass: the agent just edited this file, so the next
    /// Read should hit native (full content) rather than be replaced
    /// with `[DRIP: unchanged]`. Fixes harness "must read before edit"
    /// checks that don't trust DRIP's deny-as-substitute responses.
    Passthrough,
    /// One-shot post-edit verification — the agent's reading back its
    /// own write. DRIP returns a compact certificate (hash + touched
    /// ranges + symbols) instead of the full file.
    EditCertificate {
        #[allow(dead_code)]
        before_hash: String,
        after_hash: String,
        touched_ranges: Vec<(usize, usize)>,
        touched_symbols: Vec<String>,
        total_lines: usize,
        tokens_full: i64,
        tokens_sent: i64,
    },
    /// Partial read where the requested window is byte-identical
    /// to the baseline. **Window-scoped**: lines outside the window
    /// may still have changed.
    WindowUnchanged {
        start_line: usize,
        end_line: usize,
        tokens_full_window: i64,
    },
    /// Partial read where the window drifted — diff scoped to the
    /// requested lines only.
    WindowDelta {
        diff: String,
        start_line: usize,
        end_line: usize,
        tokens_full_window: i64,
        tokens_sent: i64,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum FallbackReason {
    Binary,
    LargeFile,
    Truncated,
    NonUtf8,
    HugeFile,
    Symlink,
    DiffBiggerThanFile,
    DripOverheadBiggerThanFile,
    /// Diff exceeds `DRIP_MAX_HUNKS` / `DRIP_MAX_CHANGED_PCT` —
    /// mentally applying it would be more error-prone than re-reading.
    DiffTooComplex {
        hunks: usize,
        changed_pct: f32,
    },
    Ignored,
    /// File changed since DRIP's last baseline AND no post-edit
    /// Passthrough fired — i.e., the change came from outside the
    /// hooked tools (Bash `cargo fmt`, `git pull`, an external
    /// editor). Returning a Delta via deny would refresh DRIP's
    /// own ledger but leave Claude Code's read-tracker pinned to
    /// the previous content_hash; the next Edit would then fail
    /// with "file modified since read". Falls back to a full-text
    /// outcome that the Claude hook routes to `allow`, letting
    /// native Read run and refresh the harness.
    ExternalChange,
}

impl FallbackReason {
    pub fn label(&self) -> String {
        match self {
            FallbackReason::Binary => "binary file".into(),
            FallbackReason::LargeFile => "large file, diff skipped".into(),
            FallbackReason::Truncated => "file truncated, full content shown".into(),
            FallbackReason::NonUtf8 => "non-UTF8 content".into(),
            FallbackReason::HugeFile => "file exceeds DRIP hard cap, not loaded".into(),
            FallbackReason::Symlink => "symlink, DRIP_REJECT_SYMLINKS set".into(),
            FallbackReason::DiffBiggerThanFile => {
                "diff would cost more than the file itself".into()
            }
            FallbackReason::DripOverheadBiggerThanFile => {
                "DRIP marker would cost more than native read".into()
            }
            FallbackReason::DiffTooComplex { hunks, changed_pct } => {
                format!(
                    "diff complexity: {hunks} hunks, {pct:.0}% changed",
                    pct = changed_pct * 100.0
                )
            }
            FallbackReason::Ignored => "matched .dripignore".into(),
            FallbackReason::ExternalChange => "file changed externally, refreshing baseline".into(),
        }
    }
}

/// Hard cap enforced before any read so a 10 GB file or `/dev/zero`
/// can't OOM the hook. Above this the file isn't loaded at all.
pub const HARD_SIZE_CAP_BYTES: u64 = 50 * 1024 * 1024;

pub fn process_read(session: &Session, file_path: &str) -> Result<ReadOutcome> {
    process_read_inner(session, file_path, true, FirstReadDelivery::DripRendered)
}

/// Claude Code's native `Read` must execute on first reads so its
/// internal "file was read before edit" tracker is populated. In that
/// path DRIP records the native full payload, not the compressed view
/// used by CLI/MCP/Bash substitutions.
pub fn process_read_native_passthrough(session: &Session, file_path: &str) -> Result<ReadOutcome> {
    process_read_inner(
        session,
        file_path,
        true,
        FirstReadDelivery::NativePassthrough,
    )
}

/// Like `process_read` but skips every DB mutation. Used by
/// `drip read --dry-run`.
pub fn process_read_dry(session: &Session, file_path: &str) -> Result<ReadOutcome> {
    process_read_inner(session, file_path, false, FirstReadDelivery::DripRendered)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstReadDelivery {
    DripRendered,
    NativePassthrough,
}

/// Partial-read interception for `Read(file, offset, limit)`. The
/// baseline is NEVER mutated — only lifetime counters are bumped, so
/// `drip meter` reflects savings while the next full read still
/// diffs against the original baseline.
pub fn process_partial_read(
    session: &Session,
    file_path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<Option<ReadOutcome>> {
    let resolved = session::resolve_path(file_path);
    let canonical = canonical_key(&resolved);

    if !resolved.exists() {
        return Ok(None);
    }
    let meta = match std::fs::metadata(&resolved) {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if !meta.file_type().is_file() {
        return Ok(None);
    }
    if meta.len() > HARD_SIZE_CAP_BYTES {
        return Ok(None);
    }
    let bytes = match std::fs::read(&resolved) {
        Ok(b) => b,
        Err(_) => return Ok(None),
    };
    let disk_text = match std::str::from_utf8(&bytes) {
        Ok(s) => s.to_string(),
        Err(_) => return Ok(None),
    };

    // Match Claude's Read: 1-indexed offsets, default limit 2000.
    const DEFAULT_LIMIT: usize = 2000;
    let start_1 = offset.unwrap_or(1).max(1);
    let count = limit.unwrap_or(DEFAULT_LIMIT);

    // First sighting → install a baseline silently and pass through
    // to native. The next read gets the regular unchanged/delta path.
    let prev = match session.get_read(&canonical).ok().flatten() {
        Some(p) => p,
        None => {
            // Same opt-outs as `process_read`'s baseline-write path.
            let matcher = Matcher::load();
            if matcher.is_ignored(&resolved) || matcher.is_ignored(Path::new(file_path)) {
                return Ok(None);
            }
            if let Ok(lmeta) = std::fs::symlink_metadata(&resolved) {
                if lmeta.file_type().is_symlink()
                    && std::env::var_os("DRIP_REJECT_SYMLINKS").is_some()
                {
                    return Ok(None);
                }
            }
            if differ::is_binary(&bytes) {
                return Ok(None);
            }
            let content_hash = session::hash_content(&bytes);
            if let Err(e) = session.set_baseline(&canonical, &content_hash, &disk_text) {
                eprintln!("drip: silent baseline install failed for {file_path}: {e:#}");
                return Ok(None);
            }
            // Honest meter accounting for the passthrough: the agent
            // really did receive `window_tokens` from native Claude
            // Code, so bump tokens_full AND tokens_sent by that
            // amount (0 savings claimed). Using the *window* size,
            // not the file size — DRIP only saved the agent from
            // re-reading what the agent actually requested. Without
            // this bump, the next sentinel read on the same window
            // would inflate the savings ratio against a
            // tokens_full=0 baseline (looks like 100% reduction
            // when reality is closer to 50%).
            //
            // Track the delivered window in `seen_ranges` so a
            // subsequent partial read on the SAME (or overlapping)
            // window can collapse to a sentinel — the agent has
            // genuinely seen those lines, so claiming Unchanged is
            // honest. Different-window partials still pass through
            // until coverage extends.
            let window = extract_window(&disk_text, start_1, count);
            let window_tokens = tokens::estimate(&window).max(0);
            if let Err(e) = session.record_partial_read(&canonical, window_tokens, window_tokens) {
                eprintln!("drip: passthrough accounting failed for {file_path}: {e:#}");
            }
            let disk_lines = disk_text.lines().count();
            let end_line = (start_1 + count - 1).min(disk_lines.max(start_1));
            if let Err(e) = session.append_seen_range(&canonical, start_1, end_line) {
                eprintln!("drip: append_seen_range failed for {file_path}: {e:#}");
            }
            return Ok(None);
        }
    };

    let baseline_window = extract_window(&prev.content, start_1, count);
    let disk_window = extract_window(&disk_text, start_1, count);

    // Clamp to whichever side is longer so the header doesn't show
    // lines past the file's actual end.
    let disk_lines = disk_text.lines().count();
    let baseline_lines = prev.content.lines().count();
    let last_line_visible = disk_lines.max(baseline_lines).max(start_1);
    let end_line = (start_1 + count - 1).min(last_line_visible);

    let tokens_full_window = tokens::estimate(&disk_window).max(0);

    // External-change refresh: if the file's full hash differs from
    // the baseline, an outside-DRIP write (cargo fmt, git pull,
    // external editor) has happened. A WindowUnchanged or WindowDelta
    // via deny would refresh DRIP's accounting but leave Claude
    // Code's read-tracker pinned to the old hash, breaking the next
    // Edit. Pass through to native + refresh DRIP's baseline so the
    // harness sees fresh content. This applies regardless of
    // seen_ranges coverage — the harness compatibility issue is
    // about whole-file freshness, not window-scoped coverage.
    let disk_hash = session::hash_content(&bytes);
    if prev.content_hash != disk_hash {
        if let Err(e) = session.set_baseline(&canonical, &disk_hash, &disk_text) {
            eprintln!("drip: external-change baseline refresh failed: {e:#}");
        }
        // Drop coverage from the old baseline — those line numbers
        // refer to a file shape that no longer exists. The agent
        // receives the requested window from native, so seed
        // seen_ranges with just that window.
        if let Err(e) = session.reset_seen_ranges(&canonical) {
            eprintln!("drip: reset_seen_ranges after external change failed: {e:#}");
        }
        if let Err(e) = session.append_seen_range(&canonical, start_1, end_line) {
            eprintln!("drip: append_seen_range after external change failed: {e:#}");
        }
        if let Err(e) =
            session.record_partial_read(&canonical, tokens_full_window, tokens_full_window)
        {
            eprintln!("drip: external-change passthrough accounting failed: {e:#}");
        }
        return Ok(None);
    }

    // Window-coverage guard: only intercept if the agent has actually
    // received content for this window before. Either via a full read
    // (seen_ranges = [(1, total)]) or via a previous partial passthrough
    // on the same/overlapping range. Otherwise pass through to native
    // and remember that we delivered it.
    if !session::seen_ranges_cover(&prev.seen_ranges, start_1, end_line) {
        if let Err(e) =
            session.record_partial_read(&canonical, tokens_full_window, tokens_full_window)
        {
            eprintln!("drip: uncovered-window passthrough accounting failed: {e:#}");
        }
        if let Err(e) = session.append_seen_range(&canonical, start_1, end_line) {
            eprintln!("drip: append_seen_range failed for {file_path}: {e:#}");
        }
        return Ok(None);
    }

    if baseline_window == disk_window {
        let rendered_tokens =
            estimate_window_unchanged_tokens(file_path, start_1, end_line, tokens_full_window);
        if rendered_tokens >= tokens_full_window {
            if let Err(e) =
                session.record_partial_read(&canonical, tokens_full_window, tokens_full_window)
            {
                eprintln!("drip: partial-unchanged passthrough accounting failed: {e:#}");
            }
            if let Err(e) = session.append_seen_range(&canonical, start_1, end_line) {
                eprintln!("drip: append_seen_range after unchanged passthrough failed: {e:#}");
            }
            return Ok(None);
        }
        if let Err(e) = session.record_partial_read(&canonical, tokens_full_window, 0) {
            eprintln!("drip: partial-read accounting failed: {e:#}");
        }
        return Ok(Some(ReadOutcome::WindowUnchanged {
            start_line: start_1,
            end_line,
            tokens_full_window,
        }));
    }

    let diff = differ::unified_diff("x", &baseline_window, &disk_window, differ::DEFAULT_CONTEXT)
        .unwrap_or_else(|| "(no diff produced)".to_string());
    let tokens_sent = tokens::estimate(&diff).max(0);
    let rendered_tokens = estimate_window_delta_tokens(
        file_path,
        start_1,
        end_line,
        tokens_full_window,
        tokens_sent,
        &diff,
    );
    if rendered_tokens >= tokens_full_window {
        if let Err(e) =
            session.record_partial_read(&canonical, tokens_full_window, tokens_full_window)
        {
            eprintln!("drip: partial-delta passthrough accounting failed: {e:#}");
        }
        // Native delivers the current window — keep seen_ranges
        // in sync with what the agent has actually received.
        if let Err(e) = session.append_seen_range(&canonical, start_1, end_line) {
            eprintln!("drip: append_seen_range after delta passthrough failed: {e:#}");
        }
        return Ok(None);
    }
    if let Err(e) = session.record_partial_read(&canonical, tokens_full_window, tokens_sent) {
        eprintln!("drip: partial-delta-read accounting failed: {e:#}");
    }
    Ok(Some(ReadOutcome::WindowDelta {
        diff,
        start_line: start_1,
        end_line,
        tokens_full_window,
        tokens_sent,
    }))
}

/// 1-indexed window matching Claude's Read tool. Out-of-range → "".
/// Trailing newline shape is preserved so the diff doesn't fire a
/// spurious "\ No newline at end of file".
fn extract_window(content: &str, start_1: usize, count: usize) -> String {
    if count == 0 || start_1 == 0 {
        return String::new();
    }
    let skip = start_1 - 1;
    let mut window: Vec<&str> = content.lines().skip(skip).take(count).collect();
    if window.is_empty() {
        return String::new();
    }
    let mut s = window.join("\n");
    let total_lines = content.lines().count();
    let last_idx_in_window = skip + window.len();
    let has_trailing_newline_after_window =
        last_idx_in_window < total_lines || content.ends_with('\n');
    if has_trailing_newline_after_window {
        s.push('\n');
    }
    window.clear();
    s
}

/// Estimate the bytes the agent would receive for a given outcome by
/// calling the SAME renderer used in production. Single source of
/// truth: any format change in `read::render_*` is picked up here
/// automatically, so the `DripOverheadBiggerThanFile` gate can't drift
/// against the actual rendered payload.
///
/// `session_seg` carries the live decoration (`⏱` TTL warning, `↺`
/// compaction marker, `ℹ` session-expired notice) so the estimate
/// accounts for everything the agent will see in this read — not just
/// the bare sentinel.
/// Build the live session-decoration segment as the renderer would for
/// a non-FullFirst read. Used at every `DripOverheadBiggerThanFile`
/// gate so the estimate accounts for `⏱` (TTL warning) — and skips the
/// `↺`/`ℹ` notices that the renderer also skips on Unchanged/Delta.
fn current_session_segment(session: &Session) -> String {
    let deco = read::build_session_decoration(session);
    read::build_session_segment(&deco, /* on_first_read = */ false)
}

fn estimate_unchanged_tokens(file_path: &str, tokens_full: i64, session_seg: &str) -> i64 {
    tokens::estimate(&read::render_unchanged(file_path, tokens_full, session_seg)).max(0)
}

fn estimate_window_unchanged_tokens(
    file_path: &str,
    start_line: usize,
    end_line: usize,
    tokens_full_window: i64,
) -> i64 {
    tokens::estimate(&read::render_window_unchanged(
        file_path,
        start_line,
        end_line,
        tokens_full_window,
    ))
    .max(0)
}

fn estimate_delta_tokens(
    file_path: &str,
    tokens_full: i64,
    tokens_sent: i64,
    hunk_summary: Option<&[(usize, Option<String>)]>,
    session_seg: &str,
    diff: &str,
) -> i64 {
    tokens::estimate(&read::render_delta(
        file_path,
        tokens_full,
        tokens_sent,
        hunk_summary,
        session_seg,
        diff,
    ))
    .max(0)
}

fn estimate_window_delta_tokens(
    file_path: &str,
    start_line: usize,
    end_line: usize,
    tokens_full_window: i64,
    tokens_sent: i64,
    diff: &str,
) -> i64 {
    tokens::estimate(&read::render_window_delta(
        file_path,
        start_line,
        end_line,
        tokens_full_window,
        tokens_sent,
        diff,
    ))
    .max(0)
}

fn estimate_edit_certificate_tokens(
    file_path: &str,
    after_hash: &str,
    touched_ranges: &[(usize, usize)],
    touched_symbols: &[String],
    total_lines: usize,
    tokens_full: i64,
    tokens_sent: i64,
) -> i64 {
    tokens::estimate(&read::render_edit_certificate(
        file_path,
        after_hash,
        touched_ranges,
        touched_symbols,
        total_lines,
        tokens_full,
        tokens_sent,
    ))
    .max(0)
}

fn process_read_inner(
    session: &Session,
    file_path: &str,
    commit: bool,
    first_read_delivery: FirstReadDelivery,
) -> Result<ReadOutcome> {
    let resolved = session::resolve_path(file_path);
    let canonical = canonical_key(&resolved);

    // One-shot post-edit verification. If the on-disk file still
    // matches the after-hash recorded by PostToolUse:Edit AND we're
    // inside the cert window, hand the agent a compact certificate
    // instead of falling through to a full re-read.
    if commit && session.take_passthrough(&canonical).unwrap_or(false) {
        let cert_disabled = std::env::var("DRIP_CERT_DISABLE").as_deref() == Ok("1");
        let bytes = std::fs::read(&resolved).ok();
        let hash = bytes.as_deref().map(session::hash_content);

        // Refresh now so the next-next read returns Unchanged. Mark
        // `seen_ranges = [(1, total_lines)]` because Passthrough lets
        // Claude Code's native Read run and deliver the entire
        // post-edit content — partial intercepts on any window are
        // honest from this point on.
        if let (Some(b), Some(h)) = (bytes.as_deref(), hash.as_deref()) {
            if let Ok(text) = std::str::from_utf8(b) {
                let _ = session.set_baseline(&canonical, h, text);
                let total_lines = text.lines().count().max(1);
                let _ = session.mark_full_seen(&canonical, total_lines);
            }
        }

        if !cert_disabled {
            if let (Some(b), Some(h)) = (bytes.as_deref(), hash.as_deref()) {
                if let Ok(text) = std::str::from_utf8(b) {
                    let window = std::env::var("DRIP_CERT_WINDOW_SECS")
                        .ok()
                        .and_then(|s| s.parse::<i64>().ok())
                        .unwrap_or(300);
                    if let Ok(Some(row)) = session.find_edit_cert_candidate(&canonical, h, window) {
                        let _ = session.mark_edit_cert_used(row.id);
                        let ranges: Vec<(usize, usize)> =
                            serde_json::from_str(&row.touched_ranges).unwrap_or_default();
                        let symbols: Vec<String> = row
                            .touched_symbols
                            .as_deref()
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or_default();
                        let total_lines = text.lines().count();
                        let tokens_full = tokens::estimate(text);
                        let body =
                            edit_certificate_body(file_path, h, &ranges, &symbols, total_lines);
                        let tokens_sent = tokens::estimate(&body);
                        let rendered_tokens = estimate_edit_certificate_tokens(
                            file_path,
                            h,
                            &ranges,
                            &symbols,
                            total_lines,
                            tokens_full,
                            tokens_sent,
                        );
                        if rendered_tokens >= tokens_full {
                            let _ = session.record_post_edit_response(
                                &canonical,
                                tokens_full,
                                tokens_full,
                            );
                            return Ok(ReadOutcome::FullFallback {
                                content: text.to_string(),
                                reason: FallbackReason::DripOverheadBiggerThanFile,
                                tokens: tokens_full,
                            });
                        }
                        let _ =
                            session.record_post_edit_response(&canonical, tokens_full, tokens_sent);
                        return Ok(ReadOutcome::EditCertificate {
                            before_hash: row.before_hash,
                            after_hash: row.after_hash,
                            touched_ranges: ranges,
                            touched_symbols: symbols,
                            total_lines,
                            tokens_full,
                            tokens_sent,
                        });
                    }
                }
            }
        }
        // Passthrough fallback: account as a no-savings read so
        // `drip meter` counts the read the agent issued.
        if let (Some(b), Some(_h)) = (bytes.as_deref(), hash.as_deref()) {
            if let Ok(text) = std::str::from_utf8(b) {
                let tokens_full = tokens::estimate(text);
                let _ = session.record_post_edit_response(&canonical, tokens_full, tokens_full);
            }
        }
        return Ok(ReadOutcome::Passthrough);
    }

    // .dripignore short-circuit — placeholder, no file load, no DB write.
    let matcher = Matcher::load();
    if matcher.is_ignored(&resolved) || matcher.is_ignored(Path::new(file_path)) {
        let placeholder = format!("<ignored by .dripignore: {}>", resolved.display());
        let toks = tokens::estimate(&placeholder);
        return Ok(ReadOutcome::FullFallback {
            content: placeholder,
            reason: FallbackReason::Ignored,
            tokens: toks,
        });
    }

    // `symlink_metadata` so a dangling link doesn't error here.
    let lmeta = std::fs::symlink_metadata(&resolved).ok();
    if lmeta.is_none() {
        if session.get_read(&canonical)?.is_some() {
            if commit {
                session.delete_read(&canonical)?;
            }
            return Ok(ReadOutcome::Deleted);
        }
        anyhow::bail!("file not found: {}", resolved.display());
    }
    let lmeta = lmeta.unwrap();

    if lmeta.file_type().is_symlink() && std::env::var_os("DRIP_REJECT_SYMLINKS").is_some() {
        // No baseline write — let native Read handle the symlink.
        let placeholder = String::from("<symlink, not followed>");
        let toks = tokens::estimate(&placeholder);
        return Ok(ReadOutcome::FullFallback {
            content: placeholder,
            reason: FallbackReason::Symlink,
            tokens: toks,
        });
    }

    let meta =
        std::fs::metadata(&resolved).with_context(|| format!("stat {}", resolved.display()))?;

    // Char-device / FIFO / socket DoS guard.
    if !meta.file_type().is_file() {
        anyhow::bail!("refusing to read non-regular file: {}", resolved.display());
    }

    // `drip watch` populates `precomputed_reads`. We validate
    // (mtime, size) AND the current baseline hash, so a post-edit
    // or refresh invalidates the cache automatically.
    if commit && meta.len() <= HARD_SIZE_CAP_BYTES {
        if let Some(hit) = try_precomputed(session, &canonical, &meta)? {
            return Ok(hit);
        }
    }

    if meta.len() > HARD_SIZE_CAP_BYTES {
        let placeholder = format!(
            "<file exceeds {} MB cap, {} bytes>",
            HARD_SIZE_CAP_BYTES / (1024 * 1024),
            meta.len()
        );
        let toks = tokens::estimate(&placeholder);
        if commit {
            // Synthetic hash so a future shrink-and-read re-baselines.
            let synthetic_hash = format!("oversized:{}", meta.len());
            session.upsert_read(&canonical, &synthetic_hash, &placeholder, toks, toks)?;
        }
        return Ok(ReadOutcome::FullFallback {
            content: placeholder,
            reason: FallbackReason::HugeFile,
            tokens: toks,
        });
    }

    let bytes =
        std::fs::read(&resolved).with_context(|| format!("reading {}", resolved.display()))?;
    let kind = differ::classify(&bytes);
    let content_hash = session::hash_content(&bytes);

    // Silent-baseline guard: a `reads` row whose `seen_ranges` does
    // not cover the full baseline means the agent only saw partial
    // windows — never the entire file. Treat that case as "no
    // baseline" so the FullFirst path fires and writes a complete
    // `seen_ranges`, instead of incorrectly claiming Unchanged or
    // Delta against content the agent never received.
    //
    // Exception: when the previous delivery was semantic-compressed,
    // seen_ranges intentionally lists only non-elided regions (so
    // partial reads on elided bodies still pass through). For a full
    // re-read, the diff is computed against the compressed text the
    // agent already has — any elided body that changed will surface
    // as a stub-vs-body diff, which is correct. Bypass the guard so
    // the Unchanged/Delta path stays available.
    let prev = session.get_read(&canonical)?.and_then(|p| {
        if p.was_semantic_compressed {
            return Some(p);
        }
        let baseline_lines = p.content.lines().count().max(1);
        if session::seen_ranges_cover(&p.seen_ranges, 1, baseline_lines) {
            Some(p)
        } else {
            None
        }
    });

    match kind {
        FileKind::Binary => {
            let placeholder = format!("<binary, {} bytes>", bytes.len());
            let toks = tokens::estimate(&placeholder);
            if commit {
                session.upsert_read(&canonical, &content_hash, &placeholder, toks, toks)?;
            }
            Ok(ReadOutcome::FullFallback {
                content: placeholder,
                reason: FallbackReason::Binary,
                tokens: toks,
            })
        }
        FileKind::TooLarge => {
            let text = match std::str::from_utf8(&bytes) {
                Ok(s) => s.to_string(),
                Err(_) => format!("<non-utf8, {} bytes>", bytes.len()),
            };
            let toks = tokens::estimate(&text);

            // Re-read of an unchanged large file: same hash means there
            // is nothing to redeliver. Without this branch the TooLarge
            // gate unconditionally returned `LargeFile` even when the
            // baseline matched disk, which the Claude hook then routed
            // to `allow → native` — and on files past Claude's 25k-
            // token limit native fails with `exceeds maximum allowed
            // tokens`. The result was that a file we'd already
            // delivered as a compressed substitute became unreadable on
            // the very next read. Collapse to the Unchanged sentinel
            // first; the rest of this arm only runs when prev is
            // missing or the hash drifted.
            if let Some(p) = prev.as_ref() {
                if p.content_hash == content_hash {
                    let session_seg = current_session_segment(session);
                    if estimate_unchanged_tokens(file_path, toks, &session_seg) >= toks {
                        if commit {
                            session.upsert_read(&canonical, &content_hash, &text, toks, toks)?;
                        }
                        return Ok(ReadOutcome::FullFallback {
                            content: text,
                            reason: FallbackReason::DripOverheadBiggerThanFile,
                            tokens: toks,
                        });
                    }
                    if commit {
                        session.record_unchanged(&canonical, toks)?;
                    }
                    return Ok(ReadOutcome::Unchanged { tokens_full: toks });
                }
            }

            // Large-file fallback exists because `similar`'s diff cost
            // is super-linear and the agent gets nothing useful from a
            // sprawling diff of a 200 KB file. But on a *first* read
            // there's no diff to compute — just a payload to deliver.
            // For a `DripRendered` first read on text content we
            // attempt semantic compression: a 200 KB Python module
            // with 40 elidable functions routinely shrinks to ~5 KB,
            // which is the agent's only chance to see the file once
            // Claude's `Read` tool refuses anything past ~25 000
            // tokens. If compression doesn't shrink the view under the
            // diff-perf cap we keep the plain LargeFile fallback so
            // the diff path stays bounded on future reads.
            if first_read_delivery == FirstReadDelivery::DripRendered
                && prev.is_none()
                && std::str::from_utf8(&bytes).is_ok()
                && commit
            {
                let lang = compress::detect_language(Path::new(file_path));
                if let Some(c) = compress::compress(&text, lang) {
                    let compressed_tokens = tokens::estimate(&c.text);
                    if c.text.len() <= differ::LARGE_FILE_BYTES && compressed_tokens < toks {
                        let registry_prev = session.get_registry(&canonical).ok().flatten();
                        let registry_status = compute_registry_status(
                            registry_prev.as_ref(),
                            &content_hash,
                            &text,
                            file_path,
                        );
                        let registry_tokens = registry_extra_tokens(&registry_status);
                        let sent_tokens = compressed_tokens + registry_tokens;
                        session.upsert_read_with_compression(
                            &canonical,
                            &content_hash,
                            &text,
                            toks,
                            sent_tokens,
                            Some((true, &c.elided_function_names)),
                            Some(&c.source_map),
                        )?;
                        return Ok(ReadOutcome::FullFirst {
                            content: text,
                            tokens: toks,
                            compressed: Some(CompressedView {
                                text: c.text,
                                tokens: compressed_tokens,
                                functions_elided: c.functions_elided,
                                lines_elided: c.lines_elided,
                                original_lines: c.original_lines,
                                source_map: c.source_map,
                            }),
                            registry: registry_status,
                        });
                    }
                }
            }

            if commit {
                session.upsert_read(&canonical, &content_hash, &text, toks, toks)?;
            }
            Ok(ReadOutcome::FullFallback {
                content: text,
                reason: FallbackReason::LargeFile,
                tokens: toks,
            })
        }
        FileKind::Text => {
            let new_text = match std::str::from_utf8(&bytes) {
                Ok(s) => s.to_string(),
                Err(_) => {
                    let placeholder = format!("<non-utf8, {} bytes>", bytes.len());
                    let toks = tokens::estimate(&placeholder);
                    if commit {
                        session.upsert_read(&canonical, &content_hash, &placeholder, toks, toks)?;
                    }
                    return Ok(ReadOutcome::FullFallback {
                        content: placeholder,
                        reason: FallbackReason::NonUtf8,
                        tokens: toks,
                    });
                }
            };
            let new_tokens = tokens::estimate(&new_text);

            match prev {
                None => {
                    let native_passthrough =
                        first_read_delivery == FirstReadDelivery::NativePassthrough;
                    // Lookup BEFORE `upsert_read` so we don't compare
                    // the file against itself.
                    let registry_prev = session.get_registry(&canonical).ok().flatten();
                    let registry_status = compute_registry_status(
                        registry_prev.as_ref(),
                        &content_hash,
                        &new_text,
                        file_path,
                    );

                    // Compression affects only the agent-facing
                    // payload — `reads.content` stores the original
                    // so diffs work against future reads.
                    let lang = compress::detect_language(Path::new(file_path));
                    let elided_names: std::cell::RefCell<Vec<String>> = Default::default();
                    let compressed = if native_passthrough {
                        None
                    } else {
                        compress::compress(&new_text, lang).and_then(|c| {
                            let toks = tokens::estimate(&c.text);
                            if toks >= new_tokens {
                                return None;
                            }
                            elided_names.replace(c.elided_function_names.clone());
                            Some(CompressedView {
                                text: c.text,
                                tokens: toks,
                                functions_elided: c.functions_elided,
                                lines_elided: c.lines_elided,
                                original_lines: c.original_lines,
                                source_map: c.source_map,
                            })
                        })
                    };
                    let registry_tokens = if native_passthrough {
                        0
                    } else {
                        registry_extra_tokens(&registry_status)
                    };
                    let sent_tokens = compressed.as_ref().map(|c| c.tokens).unwrap_or(new_tokens)
                        + registry_tokens;
                    if commit {
                        let names = elided_names.borrow();
                        let comp_meta: Option<(bool, &[String])> = if compressed.is_some() {
                            Some((true, &names))
                        } else {
                            None
                        };
                        let source_map = compressed.as_ref().map(|c| &c.source_map);
                        session.upsert_read_with_compression(
                            &canonical,
                            &content_hash,
                            &new_text,
                            new_tokens,
                            sent_tokens,
                            comp_meta,
                            source_map,
                        )?;
                    }
                    let delivered_registry = if native_passthrough {
                        RegistryStatus::Unknown
                    } else {
                        registry_status
                    };
                    Ok(ReadOutcome::FullFirst {
                        content: new_text,
                        tokens: new_tokens,
                        compressed,
                        registry: delivered_registry,
                    })
                }
                Some(prev) if prev.content_hash == content_hash => {
                    let session_seg = current_session_segment(session);
                    if estimate_unchanged_tokens(file_path, new_tokens, &session_seg) >= new_tokens
                    {
                        if commit {
                            session.upsert_read(
                                &canonical,
                                &content_hash,
                                &new_text,
                                new_tokens,
                                new_tokens,
                            )?;
                        }
                        return Ok(ReadOutcome::FullFallback {
                            content: new_text,
                            reason: FallbackReason::DripOverheadBiggerThanFile,
                            tokens: new_tokens,
                        });
                    }
                    if commit {
                        session.record_unchanged(&canonical, new_tokens)?;
                    }
                    Ok(ReadOutcome::Unchanged {
                        tokens_full: new_tokens,
                    })
                }
                Some(prev) => {
                    // External-change refresh: when running in
                    // native_passthrough mode (Claude Code Read hook)
                    // a Delta would refresh DRIP's own ledger but
                    // leave Claude's read-tracker pinned to the old
                    // content_hash — the next Edit would then fail
                    // with "file modified since read". Bypass Delta
                    // here and let native Read fire so the harness
                    // sees fresh content. CLI / MCP / Bash callers
                    // (FirstReadDelivery::DripRendered) keep the
                    // Delta optimisation since they don't share
                    // Claude Code's tracker.
                    if first_read_delivery == FirstReadDelivery::NativePassthrough {
                        if commit {
                            session.upsert_read(
                                &canonical,
                                &content_hash,
                                &new_text,
                                new_tokens,
                                new_tokens,
                            )?;
                            // Persistent counter so `drip meter` can
                            // explain low reduction% on agent-edited
                            // repos. Best-effort: a failed bump
                            // shouldn't drop the read on the floor.
                            let _ = session.bump_external_edit_refresh();
                        }
                        return Ok(ReadOutcome::FullFallback {
                            content: new_text,
                            reason: FallbackReason::ExternalChange,
                            tokens: new_tokens,
                        });
                    }
                    if differ::is_truncated(prev.content.len(), new_text.len()) {
                        if commit {
                            session.upsert_read(
                                &canonical,
                                &content_hash,
                                &new_text,
                                new_tokens,
                                new_tokens,
                            )?;
                        }
                        return Ok(ReadOutcome::FullFallback {
                            content: new_text,
                            reason: FallbackReason::Truncated,
                            tokens: new_tokens,
                        });
                    }
                    let label = display_label(file_path);
                    match differ::unified_diff(
                        &label,
                        &prev.content,
                        &new_text,
                        differ::DEFAULT_CONTEXT,
                    ) {
                        Some(diff) => {
                            let delta_tokens = tokens::estimate(&diff);
                            // Tiny files where diff headers cost more
                            // than the file itself: send full content.
                            if delta_tokens >= new_tokens {
                                if commit {
                                    session.upsert_read(
                                        &canonical,
                                        &content_hash,
                                        &new_text,
                                        new_tokens,
                                        new_tokens,
                                    )?;
                                }
                                return Ok(ReadOutcome::FullFallback {
                                    content: new_text,
                                    reason: FallbackReason::DiffBiggerThanFile,
                                    tokens: new_tokens,
                                });
                            }
                            // Complexity gate: a sprawling diff is
                            // more error-prone than a re-read.
                            let total_lines = new_text.lines().count();
                            let complexity = differ::analyze_complexity(&diff, total_lines);
                            if differ::is_too_complex(&complexity) {
                                if commit {
                                    session.upsert_read(
                                        &canonical,
                                        &content_hash,
                                        &new_text,
                                        new_tokens,
                                        new_tokens,
                                    )?;
                                }
                                return Ok(ReadOutcome::FullFallback {
                                    content: new_text,
                                    reason: FallbackReason::DiffTooComplex {
                                        hunks: complexity.hunk_count,
                                        changed_pct: complexity.changed_pct,
                                    },
                                    tokens: new_tokens,
                                });
                            }
                            // 2+-hunk diffs get a function-name TOC.
                            let hunk_summary = if complexity.hunk_count >= 2 {
                                Some(build_hunk_summary(&complexity, &new_text, file_path))
                            } else {
                                None
                            };
                            let session_seg = current_session_segment(session);
                            let rendered_tokens = estimate_delta_tokens(
                                file_path,
                                new_tokens,
                                delta_tokens,
                                hunk_summary.as_deref(),
                                &session_seg,
                                &diff,
                            );
                            if rendered_tokens >= new_tokens {
                                if commit {
                                    session.upsert_read(
                                        &canonical,
                                        &content_hash,
                                        &new_text,
                                        new_tokens,
                                        new_tokens,
                                    )?;
                                }
                                return Ok(ReadOutcome::FullFallback {
                                    content: new_text,
                                    reason: FallbackReason::DripOverheadBiggerThanFile,
                                    tokens: new_tokens,
                                });
                            }
                            if commit {
                                session.upsert_read(
                                    &canonical,
                                    &content_hash,
                                    &new_text,
                                    new_tokens,
                                    delta_tokens,
                                )?;
                            }
                            Ok(ReadOutcome::Delta {
                                diff,
                                tokens_full: new_tokens,
                                tokens_sent: delta_tokens,
                                hunk_summary,
                            })
                        }
                        None => {
                            let session_seg = current_session_segment(session);
                            if estimate_unchanged_tokens(file_path, new_tokens, &session_seg)
                                >= new_tokens
                            {
                                if commit {
                                    session.upsert_read(
                                        &canonical,
                                        &content_hash,
                                        &new_text,
                                        new_tokens,
                                        new_tokens,
                                    )?;
                                }
                                return Ok(ReadOutcome::FullFallback {
                                    content: new_text,
                                    reason: FallbackReason::DripOverheadBiggerThanFile,
                                    tokens: new_tokens,
                                });
                            }
                            if commit {
                                session.record_unchanged(&canonical, new_tokens)?;
                            }
                            Ok(ReadOutcome::Unchanged {
                                tokens_full: new_tokens,
                            })
                        }
                    }
                }
            }
        }
    }
}

/// `mtime` in ns since epoch, or 0 if unsupported.
fn mtime_ns(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Serve a precomputed outcome when fresh. Returns `None` on stale
/// entries (mtime/size drift, baseline hash drift) and on
/// silent-baseline rows where seen_ranges does not cover the full
/// baseline — same correctness check as the inline path: an Unchanged
/// or Delta from a precomputed cache is a lie if the agent never
/// received the content the precompute was diffed against.
fn try_precomputed(
    session: &Session,
    canonical: &str,
    meta: &std::fs::Metadata,
) -> Result<Option<ReadOutcome>> {
    let pre = match session.get_precomputed(canonical, mtime_ns(meta), meta.len() as i64)? {
        Some(p) => p,
        None => return Ok(None),
    };
    // Validate the cached diff was computed against the CURRENT baseline.
    let prev = session.get_read(canonical)?;
    let baseline_ok = match (prev.as_ref(), pre.baseline_hash.as_str()) {
        (Some(p), b) => p.content_hash == b,
        (None, "") => true,
        _ => false,
    };
    if !baseline_ok {
        return Ok(None);
    }
    // Silent-baseline guard: same rule as the inline `prev` filter
    // in `process_read_inner`. Skip the cache hit when the agent has
    // not been delivered the full baseline; the inline path will then
    // run the FullFirst branch and write a complete `seen_ranges`.
    // Compressed prevs bypass the coverage check (see process_read_inner
    // for the rationale) — a Delta against the compressed baseline is
    // still correct.
    if let Some(p) = prev.as_ref() {
        if !p.was_semantic_compressed {
            let baseline_lines = p.content.lines().count().max(1);
            if !session::seen_ranges_cover(&p.seen_ranges, 1, baseline_lines) {
                return Ok(None);
            }
        }
    }
    // Apply the cached outcome and update read counters.
    let session_seg = current_session_segment(session);
    match pre.outcome_kind {
        0 => {
            if estimate_unchanged_tokens(canonical, pre.new_tokens, &session_seg) >= pre.new_tokens
            {
                session.upsert_read(
                    canonical,
                    &pre.content_hash,
                    &pre.new_content,
                    pre.new_tokens,
                    pre.new_tokens,
                )?;
                return Ok(Some(ReadOutcome::FullFallback {
                    content: pre.new_content,
                    reason: FallbackReason::DripOverheadBiggerThanFile,
                    tokens: pre.new_tokens,
                }));
            }
            session.record_unchanged(canonical, pre.new_tokens)?;
            Ok(Some(ReadOutcome::Unchanged {
                tokens_full: pre.new_tokens,
            }))
        }
        1 => {
            let diff = pre.diff_text.unwrap_or_default();
            // The precomputed cache doesn't carry the function-name
            // hunk summary (it's computed inline from `new_text` in
            // the hot path). Pass `None` here AND read.rs's renderer
            // sees `None` on the returned `ReadOutcome::Delta`, so
            // the estimate matches what the agent receives.
            let rendered_tokens = estimate_delta_tokens(
                canonical,
                pre.new_tokens,
                pre.delta_tokens,
                None,
                &session_seg,
                &diff,
            );
            if rendered_tokens >= pre.new_tokens {
                session.upsert_read(
                    canonical,
                    &pre.content_hash,
                    &pre.new_content,
                    pre.new_tokens,
                    pre.new_tokens,
                )?;
                return Ok(Some(ReadOutcome::FullFallback {
                    content: pre.new_content,
                    reason: FallbackReason::DripOverheadBiggerThanFile,
                    tokens: pre.new_tokens,
                }));
            }
            // Promote the new content to the baseline.
            session.upsert_read(
                canonical,
                &pre.content_hash,
                &pre.new_content,
                pre.new_tokens,
                pre.delta_tokens,
            )?;
            Ok(Some(ReadOutcome::Delta {
                diff,
                tokens_full: pre.new_tokens,
                tokens_sent: pre.delta_tokens,
                // Cache path doesn't carry the live text needed for
                // a hunk summary.
                hunk_summary: None,
            }))
        }
        _ => Ok(None),
    }
}

/// Stable key independent of caller-supplied relative/absolute mix.
///
/// Falls back to parent canonicalization when the path itself can't be
/// resolved — typically because the file has been deleted between the
/// baseline write and this lookup. On macOS the parent (`/tmp`,
/// `/var/folders/…`) is usually a symlink, so a deleted-file lookup
/// using only `to_string_lossy` would key on `/tmp/foo` while the
/// baseline lives under `/private/tmp/foo` and the Deleted intercept
/// would miss. Canonicalize the directory part and re-append the file
/// name so deleted files still match their stored row.
pub fn canonical_key(p: &Path) -> String {
    if let Ok(c) = p.canonicalize() {
        return c.to_string_lossy().into_owned();
    }
    if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
        if !parent.as_os_str().is_empty() {
            if let Ok(canon_parent) = parent.canonicalize() {
                return canon_parent.join(name).to_string_lossy().into_owned();
            }
        }
    }
    p.to_string_lossy().into_owned()
}

fn display_label(input: &str) -> String {
    let path = Path::new(input);
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| input.to_string())
}

pub fn edit_certificate_body(
    file_path: &str,
    after_hash: &str,
    touched_ranges: &[(usize, usize)],
    touched_symbols: &[String],
    total_lines: usize,
) -> String {
    let short_hash: String = after_hash.chars().take(12).collect();
    let changed_lines: usize = touched_ranges
        .iter()
        .map(|(s, e)| e.saturating_sub(*s).saturating_add(1))
        .sum();
    let mut body = String::new();
    body.push_str("Edit applied successfully.\n");
    if !touched_symbols.is_empty() || !touched_ranges.is_empty() {
        body.push_str("Changed:\n");
        if !touched_symbols.is_empty() {
            for (i, name) in touched_symbols.iter().enumerate() {
                match touched_ranges.get(i).copied() {
                    Some((s, e)) => body.push_str(&format!("  {name} (L{s}-L{e})\n")),
                    None => body.push_str(&format!("  {name}\n")),
                }
            }
        } else {
            for (s, e) in touched_ranges {
                body.push_str(&format!("  L{s}-L{e}\n"));
            }
        }
    }
    let unchanged = total_lines.saturating_sub(changed_lines);
    body.push_str(&format!("Unchanged regions: {unchanged} lines\n"));
    body.push_str(&format!("File hash: {short_hash}\n"));
    body.push_str(&format!(
        "Use `drip refresh {file_path}` if you need full content.\n"
    ));
    body
}

pub fn registry_extra_tokens(status: &RegistryStatus) -> i64 {
    match status {
        RegistryStatus::Changed { diff_text, .. } => tokens::estimate(diff_text),
        _ => 0,
    }
}

/// `(line, enclosing_name?)` per hunk for the header TOC.
fn build_hunk_summary(
    complexity: &differ::DiffComplexity,
    new_text: &str,
    file_path: &str,
) -> Vec<(usize, Option<String>)> {
    let lines: Vec<&str> = new_text.lines().collect();
    complexity
        .hunk_starts
        .iter()
        .map(|(line_no, _hdr)| {
            let name = nearest_enclosing_name(&lines, *line_no, file_path);
            (*line_no, name)
        })
        .collect()
}

/// Walk back from `line_no` looking for an enclosing function /
/// class declaration. `None` falls back to `ln 42` in the renderer.
fn nearest_enclosing_name(lines: &[&str], line_no: usize, file_path: &str) -> Option<String> {
    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let start = line_no.saturating_sub(1).min(lines.len().saturating_sub(1));
    for i in (0..=start).rev() {
        let raw = lines[i];
        let trimmed = raw.trim_start();
        if let Some(name) = match ext {
            "py" => extract_python_name(trimmed),
            "rs" => extract_rust_name(trimmed),
            "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" => extract_js_name(trimmed),
            "go" => extract_go_name(trimmed),
            _ => extract_cfamily_name(trimmed),
        } {
            return Some(name);
        }
    }
    None
}

fn extract_python_name(s: &str) -> Option<String> {
    let rest = s
        .strip_prefix("async def ")
        .or_else(|| s.strip_prefix("def "))
        .or_else(|| s.strip_prefix("class "))?;
    let end = rest.find(|c: char| !c.is_alphanumeric() && c != '_')?;
    Some(rest[..end].to_string())
}

fn extract_rust_name(s: &str) -> Option<String> {
    let rest = s
        .strip_prefix("pub fn ")
        .or_else(|| s.strip_prefix("pub(crate) fn "))
        .or_else(|| s.strip_prefix("pub async fn "))
        .or_else(|| s.strip_prefix("async fn "))
        .or_else(|| s.strip_prefix("fn "))?;
    let end = rest
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn extract_js_name(s: &str) -> Option<String> {
    if let Some(rest) = s.strip_prefix("function ") {
        let end = rest.find(|c: char| !c.is_alphanumeric() && c != '_')?;
        return Some(rest[..end].to_string());
    }
    // `const foo = (...) =>` / `foo: function(...)`.
    if (s.contains("=>") || s.ends_with('{'))
        && (s.contains("const ") || s.contains("let ") || s.contains("var "))
    {
        for keyword in ["const ", "let ", "var "] {
            if let Some(rest) = s.strip_prefix(keyword) {
                let end = rest.find(|c: char| !c.is_alphanumeric() && c != '_')?;
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

fn extract_go_name(s: &str) -> Option<String> {
    let rest = s.strip_prefix("func ")?;
    // Skip a method receiver `(r *T) Name(...)`.
    let after = if let Some(idx) = rest.find(')') {
        if rest.starts_with('(') {
            rest[idx + 1..].trim_start()
        } else {
            rest
        }
    } else {
        rest
    };
    let end = after.find(|c: char| !c.is_alphanumeric() && c != '_')?;
    Some(after[..end].to_string())
}

fn extract_cfamily_name(s: &str) -> Option<String> {
    // `type name(args) {` — last identifier before `(`.
    let paren = s.find('(')?;
    let head = s[..paren].trim_end();
    let last_word_start = head
        .rfind(|c: char| !c.is_alphanumeric() && c != '_')
        .map(|i| i + 1)
        .unwrap_or(0);
    let name = head[last_word_start..].to_string();
    if name.is_empty() {
        return None;
    }
    if matches!(
        name.as_str(),
        "if" | "for" | "while" | "switch" | "do" | "catch" | "synchronized" | "return"
    ) {
        return None;
    }
    Some(name)
}

/// Cap on the inter-session diff trailer so a wholesale rewrite
/// can't blow the context budget.
const REGISTRY_DIFF_MAX_LINES: usize = 200;

/// Cross-session decoration. `Unchanged` skips the diff entirely.
fn compute_registry_status(
    prev: Option<&RegistryRecord>,
    current_hash: &str,
    current_content: &str,
    file_path: &str,
) -> RegistryStatus {
    let Some(prev) = prev else {
        return RegistryStatus::Unknown;
    };
    let now = session::unix_now();
    let last_seen_secs_ago = (now - prev.last_seen_at).max(0);
    if prev.content_hash == current_hash {
        return RegistryStatus::Unchanged {
            last_seen_secs_ago,
            last_git_branch: prev.last_git_branch.clone(),
        };
    }
    let label = display_label(file_path);
    let diff_text =
        differ::unified_diff(&label, &prev.content, current_content, 3).unwrap_or_default();
    let (added, removed) = count_diff_lines(&diff_text);
    let trimmed = truncate_diff(&diff_text, REGISTRY_DIFF_MAX_LINES);
    RegistryStatus::Changed {
        last_seen_secs_ago,
        last_git_branch: prev.last_git_branch.clone(),
        added_lines: added,
        removed_lines: removed,
        diff_text: trimmed,
    }
}

fn count_diff_lines(diff: &str) -> (usize, usize) {
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

fn truncate_diff(diff: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= max_lines {
        return diff.to_string();
    }
    let kept = &lines[..max_lines];
    let extra = lines.len() - max_lines;
    let mut out = kept.join("\n");
    out.push('\n');
    out.push_str(&format!(
        "... ({extra} more lines truncated; run `drip refresh` for full content)"
    ));
    out
}
