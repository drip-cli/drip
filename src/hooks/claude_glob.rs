//! Claude Code PreToolUse hook for the Glob tool.
//!
//! Re-runs the agent's glob ourselves, applies `.dripignore`, and
//! returns the filtered list as a `permissionDecision: deny` reason —
//! same substitution trick the Read hook uses. The agent never sees the
//! noisy raw match list.
//!
//! We only intercept when:
//!   - the request looks well-formed (a `pattern` field is present), and
//!   - we can canonicalize the search root.
//!
//! Anything ambiguous → `allow` and let Claude's native Glob run. False
//! negatives are fine; false positives (substituting the wrong list) are
//! not, so we err on the side of passthrough.

use crate::core::ignore::Matcher;
use anyhow::{Context, Result};
use globset::Glob;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct PreToolUse {
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
}

const MAX_RESULTS: usize = 1000;

pub fn handle(stdin_payload: &str) -> Result<String> {
    if std::env::var_os("DRIP_DISABLE").is_some() {
        return Ok(allow());
    }
    let p: PreToolUse =
        serde_json::from_str(stdin_payload).context("PreToolUse Glob payload malformed")?;

    if p.tool_name.as_deref() != Some("Glob") {
        return Ok(allow());
    }
    let Some(input) = p.tool_input else {
        return Ok(allow());
    };
    let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) else {
        return Ok(allow());
    };
    let path = input
        .get("path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let glob = match Glob::new(pattern).map(|g| g.compile_matcher()) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("drip: glob hook can't parse pattern {pattern:?}: {e}");
            return Ok(allow());
        }
    };
    let matcher = Matcher::load();

    let results = match collect_matches(&path, &glob, &matcher) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("drip: glob hook walk failed: {e:#}");
            return Ok(allow());
        }
    };

    let body = render(&results, pattern, &path);
    Ok(deny(body))
}

fn collect_matches(root: &Path, glob: &globset::GlobMatcher, ig: &Matcher) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(20)
        .into_iter();
    for entry in walker.filter_entry(|e| {
        // Prune at the directory level — saves descending into
        // node_modules/ etc. Faster *and* respects user intent.
        let rel = e.path().strip_prefix(root).unwrap_or(e.path());
        !ig.is_ignored(rel)
    }) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if !glob.is_match(rel) {
            continue;
        }
        if ig.is_ignored(rel) {
            continue;
        }
        out.push(entry.path().to_path_buf());
        if out.len() >= MAX_RESULTS {
            break;
        }
    }
    // Newest first — same convention Claude Code's native Glob uses.
    out.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| std::cmp::Reverse(d.as_secs()))
            .unwrap_or(std::cmp::Reverse(0))
    });
    Ok(out)
}

fn render(results: &[PathBuf], pattern: &str, root: &Path) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "[DRIP: glob filtered via .dripignore | {} matches | pattern={pattern} | root={}]\n",
        results.len(),
        root.display()
    ));
    if results.is_empty() {
        out.push_str("(no matches)\n");
        return out;
    }
    for p in results {
        out.push_str(&p.display().to_string());
        out.push('\n');
    }
    out
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
