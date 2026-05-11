//! PostToolUse hook for Edit / Write / MultiEdit / NotebookEdit.
//! Refreshes the baseline so the agent's next Read sees `[DRIP:
//! unchanged]` instead of getting its own edits replayed back.
//! Failures are silent — must never block writes.

use crate::core::session::{self, Session};
use crate::core::tracker::HARD_SIZE_CAP_BYTES;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct PostToolUse {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
}

const WRITE_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Walk a unified diff and return the **actually modified** new-file
/// line ranges (consecutive `+` lines), one per hunk, paired with the
/// hunk's context symbol if extractable.
///
/// Earlier versions reported the raw hunk window `@@ +c,d @@` — but `d`
/// includes ~3 lines of unchanged context on each side, so a one-line
/// edit surfaced as `Changed: (L1-L6)` which misleads readers into
/// thinking five extra lines moved. Walking the body and recording only
/// `+` lines gives a tight `Changed: (L3-L3)` instead.
///
/// Pure-deletion hunks (no `+` lines, e.g. `@@ -1,5 +0,0 @@`) have no
/// new-file line numbers to point at; we emit a zero-width marker at
/// the hunk start (`(start, start)`) so the cert still surfaces "an
/// edit happened here" without claiming any new content.
fn parse_diff_hunks(diff: &str) -> (Vec<(usize, usize)>, Vec<String>) {
    let mut ranges = Vec::new();
    let mut symbols = Vec::new();
    let mut hunk_start: Option<usize> = None;
    let mut next_new_line: usize = 0;
    let mut pending_plus_runs: Vec<(usize, usize)> = Vec::new();
    let mut pending_symbol = String::new();

    let flush = |ranges: &mut Vec<(usize, usize)>,
                 symbols: &mut Vec<String>,
                 plus_runs: &mut Vec<(usize, usize)>,
                 hunk_start: Option<usize>,
                 symbol: &mut String| {
        if let Some(start) = hunk_start {
            if plus_runs.is_empty() {
                // Pure deletion — anchor at the hunk start so the agent
                // still sees where the edit landed.
                ranges.push((start, start));
                symbols.push(std::mem::take(symbol));
            } else {
                for (s, e) in plus_runs.drain(..) {
                    ranges.push((s, e));
                    symbols.push(symbol.clone());
                }
                symbol.clear();
            }
        }
    };

    for line in diff.lines() {
        if line.starts_with("@@") {
            // Close out the previous hunk.
            flush(
                &mut ranges,
                &mut symbols,
                &mut pending_plus_runs,
                hunk_start,
                &mut pending_symbol,
            );
            hunk_start = None;
            next_new_line = 0;

            let after_first = match line.strip_prefix("@@ ") {
                Some(s) => s,
                None => continue,
            };
            let close = match after_first.find("@@") {
                Some(i) => i,
                None => continue,
            };
            let hdr = &after_first[..close].trim();
            pending_symbol = extract_symbol_name(after_first[close + 2..].trim());

            let plus = match hdr.split_whitespace().find(|w| w.starts_with('+')) {
                Some(p) => p,
                None => continue,
            };
            let body = &plus[1..];
            let start = match body.split_once(',') {
                Some((s, _)) => s.parse::<usize>().unwrap_or(0),
                None => body.parse::<usize>().unwrap_or(0),
            };
            if start == 0 {
                // `+0,0` means a pure deletion at file start — anchor
                // at line 1 so the marker is visible.
                hunk_start = Some(1);
                next_new_line = 1;
            } else {
                hunk_start = Some(start);
                next_new_line = start;
            }
            continue;
        }
        if hunk_start.is_none() {
            continue;
        }
        // The diff body uses the first char to mark line origin:
        //   ` ` context (present in both sides)
        //   `+` added (in the new file)
        //   `-` removed (only in the old file)
        // Anything else (`\ No newline at end of file`, blank, etc.)
        // doesn't advance the new-file cursor.
        let mut chars = line.chars();
        match chars.next() {
            Some(' ') => {
                next_new_line = next_new_line.saturating_add(1);
            }
            Some('+') => {
                let cur = next_new_line.max(1);
                match pending_plus_runs.last_mut() {
                    Some(last) if last.1 + 1 == cur => last.1 = cur,
                    _ => pending_plus_runs.push((cur, cur)),
                }
                next_new_line = cur.saturating_add(1);
            }
            Some('-') => {
                // Deletion — no new-file line consumed.
            }
            _ => {}
        }
    }
    flush(
        &mut ranges,
        &mut symbols,
        &mut pending_plus_runs,
        hunk_start,
        &mut pending_symbol,
    );
    (ranges, symbols)
}

/// Extract function name from a hunk-context line (Python, Rust,
/// JS, C-family). Empty string when nothing matches.
fn extract_symbol_name(context: &str) -> String {
    let trimmed = context.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    for kw in ["fn ", "def ", "async def ", "function "] {
        if let Some(rest) = trimmed.find(kw).map(|i| &trimmed[i + kw.len()..]) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return name;
            }
        }
    }
    // Java/C/C++ fallback: last identifier before `(`.
    if let Some(paren) = trimmed.find('(') {
        let head = &trimmed[..paren];
        if let Some(name) = head.split_whitespace().next_back() {
            let cleaned: String = name
                .trim_start_matches(['*', '&'])
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !cleaned.is_empty() {
                return cleaned;
            }
        }
    }
    String::new()
}

pub fn handle(stdin_payload: &str) -> Result<String> {
    if std::env::var_os("DRIP_DISABLE").is_some() {
        return Ok(json!({}).to_string());
    }
    let warning = match update_baseline(stdin_payload) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("drip: post-edit baseline refresh failed: {e:#}");
            None
        }
    };
    // PostToolUse `hookSpecificOutput.additionalContext` injects text
    // into the agent's next-turn context — used here for the
    // "you edited an elided body" warning.
    let body = match warning {
        Some(w) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": w,
            }
        }),
        None => json!({}),
    };
    Ok(body.to_string())
}

fn update_baseline(stdin_payload: &str) -> Result<Option<String>> {
    let p: PostToolUse =
        serde_json::from_str(stdin_payload).context("PostToolUse payload malformed")?;

    let Some(name) = p.tool_name.as_deref() else {
        return Ok(None);
    };
    if !WRITE_TOOLS.contains(&name) {
        return Ok(None);
    }
    let Some(input) = p.tool_input else {
        return Ok(None);
    };
    // Edit/Write/MultiEdit use `file_path`; NotebookEdit uses `notebook_path`.
    let path = input
        .get("file_path")
        .and_then(|v| v.as_str())
        .or_else(|| input.get("notebook_path").and_then(|v| v.as_str()));
    let Some(path) = path else {
        return Ok(None);
    };

    let resolved = session::resolve_path(path);
    if !resolved.exists() {
        return Ok(None);
    }
    // .dripignore guard — same opt-out as the read path. Without
    // this, an Edit on a `.env` (or any dripignore'd file) would
    // persist the *content* into the per-session `reads` table AND
    // the cross-session `file_registry`, where backup tools could
    // carry it off-host. The Read path explicitly substitutes a
    // placeholder for ignored files; the post-edit path must honour
    // the same boundary.
    let matcher = crate::core::ignore::Matcher::load();
    if matcher.is_ignored(&resolved) || matcher.is_ignored(std::path::Path::new(path)) {
        return Ok(None);
    }
    if let Ok(meta) = std::fs::metadata(&resolved) {
        // FIFO/char-device DoS guard, same as the read paths.
        if !meta.file_type().is_file() {
            return Ok(None);
        }
        if meta.len() > HARD_SIZE_CAP_BYTES {
            return Ok(None);
        }
    }
    let bytes =
        std::fs::read(&resolved).with_context(|| format!("reading {}", resolved.display()))?;
    let hash = session::hash_content(&bytes);
    let content = match std::str::from_utf8(&bytes) {
        Ok(s) => s.to_string(),
        Err(_) => return Ok(None),
    };

    let canonical = resolved
        .canonicalize()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| resolved.to_string_lossy().into_owned());

    let session = match p.session_id.filter(|s| !s.is_empty()) {
        Some(id) => Session::open_with_id(id)?,
        None => Session::open()?,
    };

    // Detect "edited an elided body" BEFORE refreshing the baseline.
    let warning = detect_elided_edit_warning(&session, &canonical, &content, &input);

    // Snapshot the before-baseline for the edit certificate. `None` on
    // a first-ever encounter — caller skips the cert path in that case.
    let prev_for_diff = session.get_read(&canonical).ok().flatten();

    session.set_baseline(&canonical, &hash, &content)?;
    session.mark_passthrough(&canonical)?;
    session.bump_edit(&canonical)?;

    if std::env::var("DRIP_CERT_DISABLE").as_deref() != Ok("1") {
        if let Some(prev) = prev_for_diff {
            let before_hash = prev.content_hash.clone();
            // Skip no-op edits (Edit with identical old/new).
            if before_hash != hash {
                let diff = crate::core::differ::unified_diff(
                    &canonical,
                    &prev.content,
                    &content,
                    crate::core::differ::DEFAULT_CONTEXT,
                )
                .unwrap_or_default();
                let (ranges, symbols) = parse_diff_hunks(&diff);
                let ranges_json = serde_json::to_string(&ranges).unwrap_or_else(|_| "[]".into());
                let symbols_json = serde_json::to_string(&symbols).unwrap_or_else(|_| "[]".into());
                let _ = session.record_edit_event(
                    &canonical,
                    &before_hash,
                    &hash,
                    &diff,
                    &ranges_json,
                    &symbols_json,
                );
            }
        }
    }
    Ok(warning)
}

/// Find `(decl_line, end_line)` for the function named `name` —
/// matches Python `def`, `fn`, `function`, and brace-language
/// declarations. Used by the post-edit warner.
fn locate_function_span(lines: &[&str], name: &str) -> Option<(usize, usize)> {
    let needles = [
        format!("def {name}("),
        format!("fn {name}("),
        format!("function {name}("),
    ];
    let mut start: Option<usize> = None;
    for (i, l) in lines.iter().enumerate() {
        if needles.iter().any(|n| l.contains(n.as_str())) {
            start = Some(i);
            break;
        }
    }
    let start = start?;
    // For Python: end at the next non-indented non-blank line.
    // For brace languages: end at the matching close-brace line.
    let decl = lines[start];
    let leading_ws = decl.len() - decl.trim_start().len();
    if decl.trim_start().starts_with("def ") || decl.trim_start().starts_with("async def ") {
        for (i, l) in lines.iter().enumerate().skip(start + 1) {
            if l.trim().is_empty() {
                continue;
            }
            let lws = l.len() - l.trim_start().len();
            if lws <= leading_ws {
                return Some((start, i.saturating_sub(1)));
            }
        }
        return Some((start, lines.len().saturating_sub(1)));
    }
    let mut depth: i32 = 0;
    let mut seen_open = false;
    for (i, l) in lines.iter().enumerate().skip(start) {
        for c in l.chars() {
            if c == '{' {
                depth += 1;
                seen_open = true;
            } else if c == '}' {
                depth -= 1;
                if seen_open && depth == 0 {
                    return Some((start, i));
                }
            }
        }
    }
    Some((start, lines.len().saturating_sub(1)))
}

/// Returns the `additionalContext` warning when a write-tool edit
/// touches a function whose body was elided in the prior compressed
/// read. `None` otherwise.
fn detect_elided_edit_warning(
    session: &Session,
    canonical: &str,
    new_content: &str,
    input: &serde_json::Value,
) -> Option<String> {
    let prev = session.get_read(canonical).ok().flatten()?;
    if !prev.was_semantic_compressed || prev.elided_function_names.is_empty() {
        return None;
    }
    // Heuristic 1: extract names directly from the edit payload —
    // more reliable than a diff scan because we see the exact text.
    let mut touched: Vec<String> = Vec::new();
    let mut sample_lines: Vec<String> = Vec::new();
    if let Some(old_s) = input.get("old_string").and_then(|v| v.as_str()) {
        sample_lines.extend(old_s.lines().map(String::from));
    }
    if let Some(new_s) = input.get("new_string").and_then(|v| v.as_str()) {
        sample_lines.extend(new_s.lines().map(String::from));
    }
    if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
        for e in edits {
            for k in ["old_string", "new_string"] {
                if let Some(s) = e.get(k).and_then(|v| v.as_str()) {
                    sample_lines.extend(s.lines().map(String::from));
                }
            }
        }
    }
    // Heuristic 2: diff prev vs new content. Catches Write tool
    // edits that lack old_string/new_string structure.
    if sample_lines.is_empty() {
        if let Some(diff) = crate::core::differ::unified_diff(
            "x",
            &prev.content,
            new_content,
            crate::core::differ::DEFAULT_CONTEXT,
        ) {
            for line in diff.lines() {
                if line.starts_with('+') || line.starts_with('-') {
                    if line.starts_with("+++") || line.starts_with("---") {
                        continue;
                    }
                    sample_lines.push(line[1..].to_string());
                }
            }
        }
    }
    // For each elided name, look for a declaration or call in the
    // sample lines.
    for name in &prev.elided_function_names {
        let needle_decl_py = format!("def {name}(");
        let needle_decl_rust = format!("fn {name}(");
        let needle_decl_func = format!("function {name}(");
        let needle_call = format!("{name}(");
        for s in &sample_lines {
            if s.contains(&needle_decl_py)
                || s.contains(&needle_decl_rust)
                || s.contains(&needle_decl_func)
                || s.contains(&needle_call)
            {
                touched.push(name.clone());
                break;
            }
        }
    }
    // Heuristic 3: locate `old_string` in prev.content and check
    // whether it falls within an elided function's body — catches
    // edits whose old_string is just a body line.
    if let Some(old_s) = input.get("old_string").and_then(|v| v.as_str()) {
        if !old_s.is_empty() {
            if let Some(pos) = prev.content.find(old_s) {
                let edit_line = prev.content[..pos].matches('\n').count();
                let lines: Vec<&str> = prev.content.lines().collect();
                for name in &prev.elided_function_names {
                    if let Some((start, end)) = locate_function_span(&lines, name) {
                        if edit_line >= start && edit_line <= end {
                            touched.push(name.clone());
                        }
                    }
                }
            }
        }
    }
    if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
        for e in edits {
            if let Some(old_s) = e.get("old_string").and_then(|v| v.as_str()) {
                if old_s.is_empty() {
                    continue;
                }
                if let Some(pos) = prev.content.find(old_s) {
                    let edit_line = prev.content[..pos].matches('\n').count();
                    let lines: Vec<&str> = prev.content.lines().collect();
                    for name in &prev.elided_function_names {
                        if let Some((start, end)) = locate_function_span(&lines, name) {
                            if edit_line >= start && edit_line <= end {
                                touched.push(name.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    touched.sort();
    touched.dedup();
    if touched.is_empty() {
        return None;
    }
    let names = touched.join(", ");
    Some(format!(
        "[DRIP: ⚠ edited elided function(s): {names}. \
         Full bodies now available — next Read returns uncompressed content. \
         Run `drip refresh {canonical}` to re-fetch and verify the edit.]"
    ))
}

#[cfg(test)]
mod parse_diff_hunks_tests {
    use super::parse_diff_hunks;

    fn diff_for(before: &str, after: &str) -> String {
        crate::core::differ::unified_diff("x", before, after, crate::core::differ::DEFAULT_CONTEXT)
            .unwrap_or_default()
    }

    #[test]
    fn one_line_modification_collapses_to_that_line_only() {
        // Regression: pre-fix, this surfaced as `Changed: (L1-L6)`
        // because the parser returned the hunk window including its
        // ±3 lines of unified-diff context. The new parser walks the
        // body and records only the `+` lines.
        let before = "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\n";
        let after = "alpha\nbeta\nGAMMA\ndelta\nepsilon\nzeta\n";
        let diff = diff_for(before, after);
        let (ranges, _) = parse_diff_hunks(&diff);
        assert_eq!(ranges, vec![(3, 3)], "diff:\n{diff}");
    }

    #[test]
    fn contiguous_two_line_change_merges_into_one_range() {
        let before = "a\nb\nc\nd\ne\nf\n";
        let after = "a\nB\nC\nd\ne\nf\n";
        let diff = diff_for(before, after);
        let (ranges, _) = parse_diff_hunks(&diff);
        assert_eq!(ranges, vec![(2, 3)], "diff:\n{diff}");
    }

    #[test]
    fn pure_insertion_reports_only_the_new_lines() {
        let before = "a\nb\nc\n";
        let after = "a\nINSERTED1\nINSERTED2\nb\nc\n";
        let diff = diff_for(before, after);
        let (ranges, _) = parse_diff_hunks(&diff);
        assert_eq!(ranges, vec![(2, 3)], "diff:\n{diff}");
    }

    #[test]
    fn pure_deletion_anchors_at_hunk_start_with_zero_width() {
        // No new-file lines are added, but we still want a marker so
        // the cert can show "an edit happened around here".
        let before = "a\nb\nc\nDELETED\nd\ne\n";
        let after = "a\nb\nc\nd\ne\n";
        let diff = diff_for(before, after);
        let (ranges, _) = parse_diff_hunks(&diff);
        assert_eq!(ranges.len(), 1, "diff:\n{diff}");
        // Marker collapses to the hunk's `+` start (anchored at the
        // surviving line that bordered the deletion).
        let (start, end) = ranges[0];
        assert_eq!(start, end, "deletion marker should be zero-width");
    }

    #[test]
    fn two_distant_hunks_yield_two_separate_ranges() {
        let before: String = (1..=50).map(|i| format!("L{i}\n")).collect();
        let mut after = before.clone();
        after = after.replace("L5\n", "L5_TOUCHED\n");
        after = after.replace("L40\n", "L40_TOUCHED\n");
        let diff = diff_for(&before, &after);
        let (ranges, _) = parse_diff_hunks(&diff);
        assert_eq!(ranges, vec![(5, 5), (40, 40)], "diff:\n{diff}");
    }

    #[test]
    fn replace_block_reports_only_the_new_block() {
        // Replace 3 lines with 5 lines → `+` lines map to the 5
        // new-file positions; the 3 deleted positions don't appear.
        let before = "a\nb\nOLD1\nOLD2\nOLD3\nc\nd\n";
        let after = "a\nb\nNEW1\nNEW2\nNEW3\nNEW4\nNEW5\nc\nd\n";
        let diff = diff_for(before, after);
        let (ranges, _) = parse_diff_hunks(&diff);
        assert_eq!(ranges, vec![(3, 7)], "diff:\n{diff}");
    }
}
