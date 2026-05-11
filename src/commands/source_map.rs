use crate::core::compress::{SourceMap, SourceMapEntry};
use crate::core::inspect;
use crate::core::session;
use anyhow::{anyhow, Result};

pub struct Opts {
    pub file: String,
    /// Resolve a single compressed line N → original range. When set,
    /// the output is one line: `Lo[-Lh] [(symbol)] [elided]`. When
    /// `None`, the full table is printed.
    pub line: Option<usize>,
    /// Emit machine-readable JSON instead of the human report. Mirrors
    /// the shape of the persisted `reads.source_map` column so callers
    /// can pipe through `jq`.
    pub json: bool,
}

pub fn run(opts: Opts) -> Result<String> {
    // Auto-pick the live agent session in cwd when the caller is the
    // user typing `drip source-map foo.py`: their derived shell
    // session never read foo.py, but the agent in this cwd did. Falls
    // back to derived (no swap) when there's no agent session here.
    let (session, _swap) = inspect::pick_session()?;
    let resolved = session::resolve_path(&opts.file);
    let canonical = resolved
        .canonicalize()
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_else(|_| resolved.to_string_lossy().into_owned());

    let map = match session.get_source_map(&canonical)? {
        Some(m) if !m.is_empty() => m,
        _ => {
            // Two reasons we land here: the file was never read in
            // this session (no `reads` row), or it was read but
            // compression didn't fire (uncompressed reads have no
            // map by design — the agent already saw real line
            // numbers). Distinguish them so the user knows which
            // recovery action to take.
            let touched = session.get_read(&canonical)?.is_some();
            let hint = if touched {
                "no compression fired for this read — agent saw raw line numbers, no remapping needed"
            } else {
                "no read tracked in this session — `drip read <file>` first, or run a hook-driven read"
            };
            return Ok(format!("No source map for {canonical}\n  {hint}\n"));
        }
    };

    if let Some(line) = opts.line {
        return Ok(render_single(&canonical, line, &map, opts.json));
    }
    Ok(render_full(&canonical, &map, opts.json))
}

fn render_single(file: &str, line: usize, map: &SourceMap, json: bool) -> String {
    let hit = map.iter().find(|e| e.compressed_line == line);
    if json {
        return match hit {
            Some(e) => serde_json::to_string(e).unwrap_or_else(|_| "{}".into()) + "\n",
            None => format!("{{\"compressed_line\":{line},\"unmapped\":true}}\n"),
        };
    }
    match hit {
        None => format!(
            "compressed L{line} → unmapped\n  this line is past the last entry; \
             the source map only covers lines 1..={} of the compressed view ({file})\n",
            map.last().map(|e| e.compressed_line).unwrap_or(0)
        ),
        Some(e) => format!(
            "compressed L{} → {} ({file})\n",
            e.compressed_line,
            describe_entry(e)
        ),
    }
}

fn render_full(file: &str, map: &SourceMap, json: bool) -> String {
    if json {
        return serde_json::to_string(map).unwrap_or_else(|_| "[]".into()) + "\n";
    }
    let mut out = format!(
        "source map for {file}\n  {} compressed lines, {} elided regions\n\n",
        map.len(),
        map.iter().filter(|e| e.elided).count()
    );
    for e in map {
        out.push_str(&format!(
            "  L{:>4} → {}\n",
            e.compressed_line,
            describe_entry(e)
        ));
    }
    out
}

fn describe_entry(e: &SourceMapEntry) -> String {
    let range = if e.original_start == e.original_end {
        format!("original L{}", e.original_start)
    } else {
        format!("original L{}-L{}", e.original_start, e.original_end)
    };
    let mut parts = vec![range];
    if let Some(name) = &e.symbol_name {
        parts.push(format!("({name})"));
    }
    if e.elided {
        parts.push("[elided]".into());
    }
    parts.join(" ")
}

pub fn parse_line_arg(raw: &str) -> Result<usize> {
    // Accept both `5` and `L5` for ergonomic parity with how the
    // stub messages report ranges (`original L5-L21`).
    let trimmed = raw.trim().trim_start_matches(['L', 'l']);
    trimmed
        .parse::<usize>()
        .map_err(|_| anyhow!("invalid --line value '{raw}': expected a positive integer or `L<n>`"))
        .and_then(|n| {
            if n == 0 {
                Err(anyhow!("--line is 1-indexed; got 0"))
            } else {
                Ok(n)
            }
        })
}
