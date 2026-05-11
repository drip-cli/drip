//! Claude Code PreToolUse hook for the Grep tool.
//!
//! Re-runs the agent's grep using `ripgrep` (`rg`) and applies
//! `.dripignore` as additional exclude rules, then substitutes the
//! filtered output via `permissionDecision: deny` (same trick as the
//! Read and Glob hooks).
//!
//! Why ripgrep? Claude Code's own Grep tool is a ripgrep wrapper, so
//! relying on the same engine keeps results semantically aligned. If
//! `rg` isn't on PATH we just allow the native call through.

use crate::core::ignore::Matcher;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Deserialize)]
struct PreToolUse {
    #[serde(default)]
    tool_name: Option<String>,
    #[serde(default)]
    tool_input: Option<serde_json::Value>,
}

pub fn handle(stdin_payload: &str) -> Result<String> {
    if std::env::var_os("DRIP_DISABLE").is_some() {
        return Ok(allow());
    }
    let p: PreToolUse =
        serde_json::from_str(stdin_payload).context("PreToolUse Grep payload malformed")?;

    if p.tool_name.as_deref() != Some("Grep") {
        return Ok(allow());
    }
    let Some(input) = p.tool_input else {
        return Ok(allow());
    };
    let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) else {
        return Ok(allow());
    };
    if which_rg().is_none() {
        // Without rg we'd be reimplementing regex search ourselves —
        // not worth the divergence from native behavior. Pass through.
        return Ok(allow());
    }

    let path = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let case_insensitive = input.get("-i").and_then(|v| v.as_bool()).unwrap_or(false);
    let multiline = input
        .get("multiline")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let output_mode = input
        .get("output_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("files_with_matches");
    let extra_glob = input.get("glob").and_then(|v| v.as_str());
    let typ = input.get("type").and_then(|v| v.as_str());
    let head_limit = input.get("head_limit").and_then(|v| v.as_i64());
    let context_lines = input.get("-C").and_then(|v| v.as_i64());
    let after_lines = input.get("-A").and_then(|v| v.as_i64());
    let before_lines = input.get("-B").and_then(|v| v.as_i64());
    let line_numbers = input.get("-n").and_then(|v| v.as_bool()).unwrap_or(false);

    let mut cmd = Command::new("rg");
    cmd.arg("--no-config");
    cmd.arg("--no-messages");
    if case_insensitive {
        cmd.arg("-i");
    }
    if multiline {
        cmd.arg("-U").arg("--multiline-dotall");
    }
    match output_mode {
        "content" => {
            if line_numbers {
                cmd.arg("-n");
            }
            if let Some(c) = context_lines {
                cmd.arg("-C").arg(c.to_string());
            }
            if let Some(a) = after_lines {
                cmd.arg("-A").arg(a.to_string());
            }
            if let Some(b) = before_lines {
                cmd.arg("-B").arg(b.to_string());
            }
        }
        "count" => {
            cmd.arg("-c");
        }
        _ => {
            // "files_with_matches" — ripgrep's -l
            cmd.arg("-l");
        }
    }
    if let Some(t) = typ {
        cmd.arg("--type").arg(t);
    }
    if let Some(g) = extra_glob {
        cmd.arg("--glob").arg(g);
    }

    // Add .dripignore patterns as --glob '!pattern' exclusions so rg
    // does the filtering at the source — both faster and consistent
    // with how rg already handles .gitignore.
    let _matcher = Matcher::load();
    for pat in dripignore_patterns_for_rg() {
        cmd.arg("--glob").arg(format!("!{pat}"));
    }

    cmd.arg("-e").arg(pattern);
    cmd.arg("--").arg(path);

    // Stream rg's stdout with a hard byte cap so a pathological pattern
    // against a huge tree can't OOM the hook subprocess. Anything past
    // the cap is dropped — the agent gets a truncated-but-coherent result
    // rather than no result at all (or a crashed hook).
    const MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024; // 4 MiB
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("drip: grep hook failed to spawn rg: {e}");
            return Ok(allow());
        }
    };
    let mut buf = Vec::with_capacity(64 * 1024);
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match out.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let take = n.min(MAX_STDOUT_BYTES.saturating_sub(buf.len()));
                    buf.extend_from_slice(&chunk[..take]);
                    if buf.len() >= MAX_STDOUT_BYTES {
                        // Stop pulling more from rg — let it finish writing
                        // to a closed pipe, which will SIGPIPE/EPIPE-out it.
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
    let _ = child.wait();
    let mut stdout = String::from_utf8_lossy(&buf).into_owned();

    // Belt-and-braces: even with --glob '!...', a pattern like
    // `package-lock.json` (basename, no slashes) only matches when rg
    // sees that file at the top of the search root. So we also drop any
    // result line whose path matches our matcher.
    let matcher = Matcher::load();
    let mut matcher_dropped: usize = 0;
    if matches!(output_mode, "files_with_matches" | "content" | "count") {
        let pre_lines: Vec<&str> = stdout.lines().collect();
        let pre_count = pre_lines.len();
        let kept: Vec<&str> = pre_lines
            .into_iter()
            .filter(|line| {
                // For "content" mode rg prints "<path>:<lineno>:<text>"; we
                // peel off the path portion before checking. If parsing is
                // ambiguous (e.g. paths containing colons), we keep the line
                // — better to over-include than to silently drop a real hit.
                let candidate = match output_mode {
                    "files_with_matches" => Path::new(line),
                    "count" => match line.rfind(':') {
                        Some(i) => Path::new(&line[..i]),
                        None => Path::new(line),
                    },
                    _ => match line.find(':') {
                        Some(i) => Path::new(&line[..i]),
                        None => return true,
                    },
                };
                !matcher.is_ignored(candidate)
            })
            .collect();
        matcher_dropped = pre_count.saturating_sub(kept.len());
        let collected: Vec<&str> = match head_limit {
            Some(n) if n > 0 => kept.into_iter().take(n as usize).collect(),
            _ => kept,
        };
        stdout = collected.join("\n");
        if !stdout.is_empty() {
            stdout.push('\n');
        }
    }

    // Early-out: if the result is empty AND DRIP's matcher didn't drop
    // anything (i.e., the post-rg filter was a no-op), substituting
    // would just charge the agent ~80 bytes of `[DRIP: ...]` header
    // for an "(no matches)" payload that native rg would have rendered
    // as a clean empty exit. The rg upstream `--glob !` excludes
    // *might* have filtered something silently, but that's the less
    // common case for a genuinely-zero-result search; passing through
    // here costs at most a noisy native result on a path that didn't
    // match many files in the first place. Override via
    // `DRIP_GREP_ALWAYS_FILTER=1`.
    let always_filter = std::env::var("DRIP_GREP_ALWAYS_FILTER").as_deref() == Ok("1");
    if !always_filter && stdout.is_empty() && matcher_dropped == 0 {
        return Ok(allow());
    }

    let header =
        format!("[DRIP: grep filtered via .dripignore | mode={output_mode} | pattern={pattern}]\n");
    let body = if stdout.is_empty() {
        format!("{header}(no matches)\n")
    } else {
        format!("{header}{stdout}")
    };
    Ok(deny(body))
}

/// The default ignore list as `rg --glob` patterns. We don't ship the
/// `Matcher`'s raw strings because they include negations and basename
/// matchers, neither of which translate cleanly to a single rg flag.
/// Keeping a curated subset here is pragmatic — it'll catch the loud
/// stuff (node_modules, lock files) which is the whole point.
fn dripignore_patterns_for_rg() -> Vec<&'static str> {
    vec![
        "node_modules",
        ".git",
        "vendor",
        "target",
        "dist",
        "build",
        ".next",
        ".turbo",
        ".svelte-kit",
        "out",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
        "Gemfile.lock",
        "poetry.lock",
        "uv.lock",
        "composer.lock",
        "*.min.js",
        "*.min.css",
    ]
}

fn which_rg() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("rg");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
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
