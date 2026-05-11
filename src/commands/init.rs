use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Atomic write via `rename(2)` so a Ctrl-C / power loss can't leave a
/// partial settings.json. Existing perms are preserved so a chmod'd
/// 0600 (Codex MCP config has secrets) isn't silently widened.
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("path has no filename component")?;
    let tmp = parent.join(format!(".{file_name}.drip-tmp"));
    std::fs::write(&tmp, content).with_context(|| format!("writing temp file {tmp:?}"))?;
    preserve_mode(path, &tmp);
    rename_with_retry(&tmp, path).with_context(|| format!("renaming {tmp:?} -> {path:?}"))?;
    Ok(())
}

/// On Windows, `rename` can fail with ERROR_SHARING_VIOLATION (32) or
/// ERROR_ACCESS_DENIED (5) when another process is reading the
/// destination (Claude Code, an editor watching `settings.json`). A
/// short exponential-backoff retry resolves it.
#[cfg(unix)]
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(windows)]
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut delay_ms = 30u64;
    for attempt in 0..4 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 3 && is_sharing_violation(&e) => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms *= 2;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

#[cfg(windows)]
fn is_sharing_violation(e: &std::io::Error) -> bool {
    // ERROR_SHARING_VIOLATION = 32, ERROR_ACCESS_DENIED = 5
    matches!(e.raw_os_error(), Some(32) | Some(5))
}

/// Copy `target`'s Unix mode onto `tmp` so the rename doesn't reset
/// perms. No-op on non-Unix or when `target` is fresh.
#[cfg(unix)]
fn preserve_mode(target: &Path, tmp: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(target) {
        let mode = meta.permissions().mode();
        let _ = std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode));
    }
}

#[cfg(not(unix))]
fn preserve_mode(_target: &Path, _tmp: &Path) {}

#[derive(Debug, Clone, Copy)]
pub enum Agent {
    Claude,
    Codex,
    Gemini,
}

impl Agent {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Agent::Claude,
            "codex" | "codex-cli" | "openai-codex" => Agent::Codex,
            "gemini" | "gemini-cli" => Agent::Gemini,
            other => {
                anyhow::bail!("unknown agent '{other}' (expected: claude|codex|gemini)")
            }
        })
    }
}

pub fn run(agent: Agent, global: bool) -> Result<String> {
    match agent {
        Agent::Claude => init_claude(global),
        Agent::Codex => init_codex(),
        Agent::Gemini => init_gemini(global),
    }
}

// -------- Claude Code ------------------------------------------------------

fn settings_path_claude(global: bool) -> Result<PathBuf> {
    if global {
        let home = dirs::home_dir().context("no home directory")?;
        Ok(home.join(".claude").join("settings.json"))
    } else {
        Ok(PathBuf::from(".claude").join("settings.json"))
    }
}

/// Marker so `drip init` recognises its own output and `drip
/// uninstall` knows the file is safe to delete. Files without it are
/// user-owned and left alone.
const DRIP_MD_MARKER: &str = "<!-- drip:memory -->";

const DRIP_MD_BODY: &str = "<!-- drip:memory -->\n\
# DRIP — file-read hints\n\
\n\
File reads in this session are intercepted by DRIP.\n\
\n\
**Within a session:**\n\
- 1st Claude `Read` of a file → native full content, so Claude's \
read-before-edit tracker is populated. DRIP records the baseline but \
does not claim compression savings for that native passthrough.\n\
- 1st DRIP-substituted read (MCP/manual/Bash file read) → full content \
(compressed if applicable).\n\
- subsequent reads of an unchanged file → `[DRIP: unchanged since last read]` (zero bytes).\n\
- subsequent reads of a changed file → unified diff only (`--- old / +++ new / @@ hunks`). \
Apply hunks mentally to the prior content — do NOT request the file again.\n\
\n\
**Partial reads** (`Read(file, offset=N, limit=M)`): when DRIP already \
has a full-file baseline, the same diff/unchanged logic is scoped to \
the requested window. Headers you may see:\n\
- `[DRIP: unchanged (lines X-Y) | …]` → those specific lines are \
  byte-identical between baseline and disk. The claim is **window-scoped** \
  — parts of the file outside the window may still have changed.\n\
- `[DRIP: delta only (lines X-Y) | NN% reduction (...) | …]` → those \
  lines drifted; you receive a unified diff of just that range.\n\
Partial reads never mutate the baseline, so a later full read still \
diffs against the original contents — useful when you want to confirm \
nothing else moved.\n\
\n\
**Across sessions** (cross-session registry, v4+): on the very first read of a \
file in a new session, the header may include one of:\n\
- `↔ unchanged since last session (Xh ago)` → file is byte-identical to what \
  DRIP last saw. The full content is still sent so you have it in context, but \
  you can trust that nothing changed since the previous session.\n\
- `↕ changed since last session: +N lines, -M lines` → file changed. The full \
  current content is sent, followed by an `── Changes since last session ──` \
  trailer with a unified diff so you can immediately see what changed.\n\
\n\
**Read after your own edit** (PostToolUse:Edit fired in this session): \
the immediately-following Read returns a compact certificate instead of \
the full file:\n\
- `[DRIP: edit verified | NN% reduction (.../...) | hash: <prefix> | …]` → \
  DRIP confirms the edit landed and lists the touched line ranges \
  (and symbol names when extractable from the diff). Trust the cert; \
  if you genuinely need the full post-edit content, run \
  `drip refresh <path>` and Read again.\n\
\n\
**Out-of-band edits** (another tool wrote the file, `git pull`, manual edit): \
run `drip refresh <path>` to drop DRIP's baseline so the next read returns \
full content. When you Read a file DRIP knows but whose disk content has \
drifted since DRIP's baseline, DRIP refreshes the baseline by letting \
Claude's native Read fire (so Claude's read-before-edit tracker stays in \
sync) AND emits a one-shot `additionalContext` notice:\n\
- `[DRIP: native refresh | <tokens> tokens | <path> changed out-of-band \
since DRIP's last baseline …]` → trust it: the full content you just \
received is current, the baseline has been refreshed, and the NEXT read \
of this file will use the normal unchanged/delta path. Run \
`drip meter` to inspect token savings (the `Native refresh:` row \
counts these events).\n\
\n\
**Context compaction** (`/compact`, `/clear`, `--resume`): when Claude \
Code wipes its in-process read tracker, DRIP resets its baselines too \
and decorates the first read of each file with:\n\
- `[DRIP: full read | ↺ context was compacted (#N) — baseline reset | …]` \
→ expected after a long session. Subsequent reads use unchanged/delta \
as usual; the `↺` marker only appears on the first read of each file \
after the compaction.\n\
\n\
**Why DRIP responses look like `<error>` in Claude Code:** the only \
stable Claude Code PreToolUse contract that can *substitute* what the \
model receives is `permissionDecision: deny` + `permissionDecisionReason`. \
DRIP uses that channel as a transport — the model reads the reason as if \
it were the tool result. So an `[DRIP: unchanged ...]` / `[DRIP: delta only \
...]` / `[DRIP: edit verified ...]` payload wrapped in `<error>` is a \
**success**, not a failure. Treat the bracketed header as the tool result \
and proceed normally. (Known protocol limitation — `additionalContext` \
exists as an alternate channel but has rendering caveats we accept rather \
than work around.)\n\
\n\
**Optional opt-in: compress big files on first read.** By default the \
first Claude Read of a file passes through natively (so Claude's tracker \
gets populated with real bytes) and DRIP only kicks in from the second \
read onward. Set `DRIP_COMPRESS_FIRST_READ_MIN_BYTES=<bytes>` in the \
shell that launches Claude Code to also compress files at least that big \
on their FIRST read, returning a semantically-compressed view (long \
function bodies elided, signatures preserved) via the substitute channel. \
Trade-off: a substituted first read skips native Read, so the read-before- \
edit tracker is NOT populated and a follow-up Edit on that file may need \
a fresh native Read first. DRIP also falls back to native silently when \
the file doesn't compress (plain text, no long bodies), so the opt-in is \
never *worse* than disabled — just more aggressive. Recommended only on \
trees where you mostly read and rarely edit. `DRIP_CLAUDE_READ_TOKEN_BUDGET` \
adjusts the *unavoidable* over-budget threshold (default ~10 000 DRIP \
tokens ≈ 25 000 Claude tokens — Claude refuses bigger files natively, \
so DRIP substitutes them no matter what).\n\
\n\
Edits, writes, glob, and grep are unaffected. This guidance only applies to reads.\n";

/// The line we add to CLAUDE.md so Claude Code loads `drip.md` as
/// project memory. `@<path>` is the documented include directive.
const CLAUDE_MD_REF: &str = "@drip.md";

fn claude_memory_dir(global: bool) -> Result<PathBuf> {
    if global {
        let home = dirs::home_dir().context("no home directory")?;
        Ok(home.join(".claude"))
    } else {
        Ok(PathBuf::from("."))
    }
}

fn init_claude(global: bool) -> Result<String> {
    let path = settings_path_claude(global)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {parent:?}"))?;
    }

    let mut settings: Value = if path.exists() {
        let raw = std::fs::read_to_string(&path).with_context(|| format!("reading {path:?}"))?;
        if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing existing {path:?} as JSON"))?
        }
    } else {
        json!({})
    };

    let hooks = settings
        .as_object_mut()
        .context("settings.json root is not a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks_obj = hooks.as_object_mut().context("hooks must be an object")?;

    // Absolute path: hook subprocesses don't inherit the user's
    // interactive PATH, so a bare `drip` would silently fail.
    let drip_raw = current_drip_path();
    // Shell-quote so paths-with-spaces survive the agent's shell parser.
    let drip = shell_quote(&drip_raw);
    let cmd_read = format!("{drip} hook claude");
    let cmd_glob = format!("{drip} hook claude-glob");
    let cmd_grep = format!("{drip} hook claude-grep");
    let cmd_post = format!("{drip} hook claude-post-edit");
    let cmd_pre_edit = format!("{drip} hook claude-pre-edit");
    let cmd_session_start = format!("{drip} hook claude-session-start");

    {
        let pre = hooks_obj.entry("PreToolUse").or_insert_with(|| json!([]));
        let pre_arr = pre.as_array_mut().context("PreToolUse must be an array")?;
        add_or_keep(pre_arr, "Read", &cmd_read);
        add_or_keep(pre_arr, "Glob", &cmd_glob);
        add_or_keep(pre_arr, "Grep", &cmd_grep);
        // Pre-edit guard: blocks Edits targeting elided function bodies.
        // Matcher matches the PostToolUse cert hook for symmetry.
        add_or_keep(pre_arr, "Edit|MultiEdit|Write|NotebookEdit", &cmd_pre_edit);
        // Legacy: earlier DRIP versions registered a `Bash` PreToolUse
        // hook. The bash interception was dropped in this release —
        // remove any stale entry so the user's settings.json no longer
        // points at a non-existent subcommand.
        remove_hook(pre_arr, "Bash", "hook claude-bash");
    }
    {
        let post = hooks_obj.entry("PostToolUse").or_insert_with(|| json!([]));
        let post_arr = post
            .as_array_mut()
            .context("PostToolUse must be an array")?;
        add_or_keep(post_arr, "Edit|Write|MultiEdit|NotebookEdit", &cmd_post);
    }
    {
        // On `compact`/`clear`, Claude Code wipes its read tracker but
        // keeps `session_id`. Without this hook, DRIP would keep
        // returning unchanged sentinels for files Claude has forgotten,
        // and the next Edit would fail with "File must be read first".
        let session_start = hooks_obj.entry("SessionStart").or_insert_with(|| json!([]));
        let session_start_arr = session_start
            .as_array_mut()
            .context("SessionStart must be an array")?;
        add_or_keep(session_start_arr, "compact|clear", &cmd_session_start);
    }

    let pretty = serde_json::to_string_pretty(&settings)? + "\n";
    atomic_write(&path, &pretty).with_context(|| format!("writing {path:?}"))?;

    // drip.md + CLAUDE.md memory wiring (project root or ~/.claude/).
    let mem_dir = claude_memory_dir(global)?;
    std::fs::create_dir_all(&mem_dir).with_context(|| format!("creating {mem_dir:?}"))?;
    let drip_md = mem_dir.join("drip.md");
    let claude_md = mem_dir.join("CLAUDE.md");
    let drip_md_added = ensure_drip_md(&drip_md)?;
    let claude_md_added = ensure_claude_md_ref(&claude_md)?;

    // Best-effort shell-completion install. Failures are non-fatal:
    // user can always run `drip completions <shell>` manually.
    let comp_line = match crate::commands::completions::install_for_detected_shell() {
        Ok(Some(p)) => {
            let shell = crate::commands::completions::detect_shell();
            let mut block = format!(
                "  - installed {} completions → {}\n",
                shell.map(|s| s.name()).unwrap_or("?"),
                p.display(),
            );
            // Zsh won't load ~/.zsh/completions without `$fpath`
            // updates; surface the per-shell hint inline so users
            // don't hit "drip <TAB> just lists files".
            if let Some(s) = shell {
                for line in s.activation_hint().lines() {
                    block.push_str("      ");
                    block.push_str(line);
                    block.push('\n');
                }
            }
            block
        }
        Ok(None) => String::new(),
        Err(e) => format!("  - shell completions: skipped ({e})\n"),
    };

    Ok(format!(
        "Installed Claude Code hooks at {}\n  \
         - PreToolUse  Read  → {drip} hook claude\n  \
         - PreToolUse  Glob  → {drip} hook claude-glob (filters via .dripignore)\n  \
         - PreToolUse  Grep  → {drip} hook claude-grep (filters via .dripignore, requires `rg`)\n  \
         - PreToolUse  Edit/MultiEdit/Write/NotebookEdit → {drip} hook claude-pre-edit (blocks edits to elided fn bodies)\n  \
         - PostToolUse Edit/Write/MultiEdit/NotebookEdit → {drip} hook claude-post-edit\n  \
         - SessionStart compact/clear → {drip} hook claude-session-start (drops baselines so post-compact Edits don't fail with `must read first`)\n  \
         - {} {}\n  \
         - {} {}\n\
         {comp_line}\
         Restart Claude Code to activate.",
        path.display(),
        if drip_md_added { "wrote" } else { "kept" },
        drip_md.display(),
        if claude_md_added { "linked" } else { "kept" },
        claude_md.display(),
    ))
}

/// Write `drip.md` if absent, rewrite if our marker is present.
/// Returns `false` when an unknown user-authored file is preserved.
fn ensure_drip_md(path: &Path) -> Result<bool> {
    if path.exists() {
        let cur = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
        if !cur.contains(DRIP_MD_MARKER) {
            return Ok(false);
        }
        if cur == DRIP_MD_BODY {
            return Ok(false);
        }
    }
    atomic_write(path, DRIP_MD_BODY).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

/// Append `@drip.md` to `CLAUDE.md` if missing. Returns `true` on
/// append/create.
fn ensure_claude_md_ref(path: &Path) -> Result<bool> {
    let existing = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?
    } else {
        String::new()
    };
    if claude_md_has_ref(&existing) {
        return Ok(false);
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.is_empty() {
        next.push('\n');
    }
    next.push_str(CLAUDE_MD_REF);
    next.push('\n');
    atomic_write(path, &next).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

/// True iff CLAUDE.md imports `drip.md` on its own line — the agent
/// only treats standalone lines as includes.
fn claude_md_has_ref(content: &str) -> bool {
    content.lines().any(|l| l.trim() == CLAUDE_MD_REF)
}

fn add_or_keep(arr: &mut Vec<Value>, matcher: &str, command: &str) {
    // Recognise our own existing entries by exact command OR by
    // matcher + `hook claude[-suffix]` shape, so an upgrade that
    // changes quoting (path moved into a folder with a space)
    // rewrites the entry instead of appending a duplicate.
    let existing_idx = arr.iter().position(|m| {
        if m.get("matcher").and_then(|v| v.as_str()) != Some(matcher) {
            return false;
        }
        m.get("hooks")
            .and_then(|h| h.as_array())
            .map(|hs| {
                hs.iter().any(|h| {
                    let cmd = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    cmd == command || is_owned_drip_hook_command(cmd)
                })
            })
            .unwrap_or(false)
    });
    // Shape-match without exact-command match → rewrite to upgrade.
    if let Some(idx) = existing_idx {
        if let Some(hooks_arr) = arr[idx].get_mut("hooks").and_then(|h| h.as_array_mut()) {
            for h in hooks_arr.iter_mut() {
                let cur = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
                if cur != command && is_owned_drip_hook_command(cur) {
                    if let Some(obj) = h.as_object_mut() {
                        obj.insert("command".to_string(), Value::String(command.to_string()));
                    }
                }
            }
        }
        return;
    }
    let entry = json!({
        "matcher": matcher,
        "hooks": [ { "type": "command", "command": command } ]
    });
    arr.push(entry);
}

/// Strip a previously-registered DRIP hook from `arr` (no-op if absent).
/// Used to clean up entries for hooks that have been removed from DRIP
/// — leaving a stale `hook claude-bash` registration would make every
/// matching tool call fail with `unknown subcommand` until the user
/// re-runs `drip init`.
fn remove_hook(arr: &mut Vec<Value>, matcher: &str, command_substring: &str) {
    arr.retain(|m| {
        if m.get("matcher").and_then(|v| v.as_str()) != Some(matcher) {
            return true;
        }
        let still_useful = m
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hs| {
                hs.iter().any(|h| {
                    let cmd = h.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    !cmd.contains(command_substring)
                })
            })
            .unwrap_or(true);
        still_useful
    });
}

// -------- Uninstall (Claude Code) -----------------------------------------

pub fn run_uninstall(agent: Agent, global: bool) -> Result<String> {
    match agent {
        Agent::Claude => uninstall_claude(global),
        Agent::Codex => uninstall_codex(),
        Agent::Gemini => uninstall_gemini(global),
    }
}

fn uninstall_claude(global: bool) -> Result<String> {
    let settings_path = settings_path_claude(global)?;
    let mem_dir = claude_memory_dir(global)?;
    let drip_md = mem_dir.join("drip.md");
    let claude_md = mem_dir.join("CLAUDE.md");

    let hooks_pruned = prune_claude_hooks(&settings_path)?;
    let ref_removed = remove_claude_md_ref(&claude_md)?;
    let drip_md_removed = remove_drip_md(&drip_md)?;
    let comp_removed = crate::commands::completions::uninstall_for_detected_shell().unwrap_or(None);

    let mut report = String::new();
    report.push_str("Uninstalled Claude Code integration.\n");
    report.push_str(&format!(
        "  - {} {}\n",
        if hooks_pruned {
            "pruned hooks in"
        } else {
            "no hooks in"
        },
        settings_path.display()
    ));
    report.push_str(&format!(
        "  - {} {}\n",
        if ref_removed {
            "removed @drip.md from"
        } else {
            "no @drip.md in"
        },
        claude_md.display()
    ));
    report.push_str(&format!(
        "  - {} {}\n",
        if drip_md_removed { "deleted" } else { "no" },
        drip_md.display()
    ));
    if let Some(p) = comp_removed {
        report.push_str(&format!("  - removed shell completions {}\n", p.display()));
    }
    Ok(report)
}

/// Drop hook entries we own from settings.json. Conservative: matches
/// only entries whose hook command contains ` hook claude` (with
/// leading space — i.e. our subcommand, not somebody's path that
/// happens to contain "claude"). Other user-defined hooks survive.
fn prune_claude_hooks(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
    if raw.trim().is_empty() {
        return Ok(false);
    }
    let mut settings: Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {path:?}"))?;
    let Some(hooks) = settings
        .as_object_mut()
        .and_then(|o| o.get_mut("hooks"))
        .and_then(|v| v.as_object_mut())
    else {
        return Ok(false);
    };

    let mut changed = false;
    for ev in ["PreToolUse", "PostToolUse", "SessionStart"] {
        if let Some(arr) = hooks.get_mut(ev).and_then(|v| v.as_array_mut()) {
            let before = arr.len();
            arr.retain(|entry| !entry_is_drip(entry));
            if arr.len() != before {
                changed = true;
            }
        }
    }
    if !changed {
        return Ok(false);
    }
    let pretty = serde_json::to_string_pretty(&settings)? + "\n";
    atomic_write(path, &pretty).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

fn entry_is_drip(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(is_drip_hook_command)
            })
        })
        .unwrap_or(false)
}

fn is_drip_hook_command(cmd: &str) -> bool {
    // Match the last two tokens: `hook claude[-suffix]`.
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    if toks.len() < 2 {
        return false;
    }
    let last = toks[toks.len() - 1];
    let prev = toks[toks.len() - 2];
    if prev != "hook" {
        return false;
    }
    matches!(
        last,
        "claude"
            | "claude-glob"
            | "claude-grep"
            | "claude-post-edit"
            | "claude-pre-edit"
            | "claude-session-start"
    )
}

/// True iff DRIP itself wrote the hook: first shell-token's basename
/// is `drip`, last two tokens are `hook claude[-suffix]`.
fn is_owned_drip_hook_command(cmd: &str) -> bool {
    if !is_drip_hook_command(cmd) {
        return false;
    }
    let first = match first_shell_token(cmd) {
        Some(t) => t,
        None => return false,
    };
    let basename = first.rsplit('/').next().unwrap_or(&first);
    let stem = basename.strip_suffix(".exe").unwrap_or(basename);
    stem == "drip"
}

/// First shell token of `cmd`. Handles POSIX `'…'` literals with
/// the `'\''` close-reopen escape. `None` on empty input or
/// unterminated quote.
fn first_shell_token(cmd: &str) -> Option<String> {
    let cmd = cmd.trim_start();
    if let Some(rest) = cmd.strip_prefix('\'') {
        let mut out = String::new();
        let bytes = rest.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\'' {
                if bytes.get(i + 1..i + 4) == Some(b"\''") {
                    out.push('\'');
                    i += 4;
                    continue;
                }
                return Some(out);
            }
            out.push(bytes[i] as char);
            i += 1;
        }
        None
    } else {
        Some(cmd.split_whitespace().next()?.to_string())
    }
}

fn remove_claude_md_ref(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let cur = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
    if !claude_md_has_ref(&cur) {
        return Ok(false);
    }
    let kept: Vec<&str> = cur.lines().filter(|l| l.trim() != CLAUDE_MD_REF).collect();
    let mut next = kept.join("\n");
    if cur.ends_with('\n') && !next.ends_with('\n') {
        next.push('\n');
    }
    atomic_write(path, &next).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

/// Delete `drip.md` only when our marker is present. Don't clobber
/// a user-authored file that happens to share the name.
fn remove_drip_md(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let cur = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
    if !cur.contains(DRIP_MD_MARKER) {
        return Ok(false);
    }
    std::fs::remove_file(path).with_context(|| format!("removing {path:?}"))?;
    Ok(true)
}

// -------- Codex CLI --------------------------------------------------------

fn init_codex() -> Result<String> {
    let home = dirs::home_dir().context("no home directory")?;
    let codex_dir = home.join(".codex");
    std::fs::create_dir_all(&codex_dir).with_context(|| format!("creating {codex_dir:?}"))?;

    let config_path = codex_dir.join("config.toml");
    let agents_path = codex_dir.join("AGENTS.md");

    let drip_bin = current_drip_path();

    let cfg_added = ensure_codex_mcp(&config_path, &drip_bin)?;
    let agents_added = ensure_agents_md(&agents_path)?;

    let mut report = String::new();
    report.push_str(&format!(
        "Codex CLI configured at {}\n",
        codex_dir.display()
    ));
    report.push_str(&format!(
        "  - {} {}\n",
        if cfg_added { "added" } else { "kept" },
        config_path.display()
    ));
    report.push_str(&format!(
        "  - {} {}\n",
        if agents_added { "appended" } else { "kept" },
        agents_path.display()
    ));
    report.push_str("Restart Codex CLI to load the MCP server.\n");
    Ok(report)
}

const CODEX_MCP_BLOCK_HEADER: &str = "[mcp_servers.drip]";

/// Marker delimiting DRIP's guidance block in agent instruction
/// files. Anchors append idempotency and uninstall block-removal.
const AGENTS_MD_MARKER: &str = "<!-- drip:agents-instructions -->";

/// Body of the guidance block we append. Same text for every MCP
/// agent — they all consume the `read_file` tool the same way, so
/// there's no point per-agent prose. Lives below the marker.
const AGENTS_MD_BODY: &str = "## File reads via DRIP\n\
\n\
Whenever you need to read a file, prefer the `read_file` tool from the\n\
`drip` MCP server over `cat`, `head`, `tail`, or any other shell read.\n\
DRIP returns the full file on the first read, then only a unified diff\n\
on subsequent reads — saving 60–95% of tokens. The diff format is\n\
standard unified diff (`--- old / +++ new / @@ hunks`).\n\
\n\
Edits and writes are unaffected; this guidance only applies to reads.\n";

/// JSON key under which `mcpServers.drip` lives across all MCP-aware
/// agents (Gemini, Claude Desktop). Codex is the odd one out — it
/// uses TOML `[mcp_servers.drip]` instead.
const MCP_SERVER_NAME: &str = "drip";

fn ensure_codex_mcp(path: &std::path::Path, drip_bin: &str) -> Result<bool> {
    let existing = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?
    } else {
        String::new()
    };

    // TOML-safe quoting: prefer `'literal'`, fall back to escaped
    // `"basic"` when the path contains `'`. Naive `\"{path}\"` would
    // be a TOML-injection footgun on Windows paths with `\`.
    let drip_bin_toml = encode_toml_string(drip_bin);
    let desired_block = format!(
        "\n{CODEX_MCP_BLOCK_HEADER}\n\
         command = {drip_bin_toml}\n\
         args = [\"mcp\", \"--agent\", \"codex\"]\n"
    );

    // Existing block may be stale (binary moved, missing `--agent`
    // arg, etc.). Rewrite when it diverges from what we'd write today
    // so users don't have to do the uninstall/init dance manually.
    if existing.contains(CODEX_MCP_BLOCK_HEADER) {
        if existing.contains(desired_block.trim_start()) {
            return Ok(false);
        }
        let stripped = strip_codex_drip_block(&existing);
        let mut next = stripped;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&desired_block);
        atomic_write(path, &next).with_context(|| format!("writing {path:?}"))?;
        return Ok(true);
    }

    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&desired_block);
    atomic_write(path, &next).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

fn ensure_agents_md(path: &std::path::Path) -> Result<bool> {
    ensure_marker_block(path, AGENTS_MD_MARKER, AGENTS_MD_BODY)
}

/// Append a marker-delimited block to a Markdown file. Idempotent —
/// returns `Ok(false)` when the marker is already present.
fn ensure_marker_block(path: &Path, marker: &str, body: &str) -> Result<bool> {
    let existing = if path.exists() {
        std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?
    } else {
        String::new()
    };
    if existing.contains(marker) {
        return Ok(false);
    }
    let block = format!("\n{marker}\n{body}");
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&block);
    atomic_write(path, &next).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

/// Inverse of `ensure_marker_block`. We always append at EOF, so
/// everything past the marker line is part of our block; the user's
/// content above is preserved verbatim.
fn remove_marker_block(path: &Path, marker: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
    let Some(idx) = raw.find(marker) else {
        return Ok(false);
    };
    let line_start = raw[..idx].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let mut head = raw[..line_start].to_string();
    while head.ends_with("\n\n") {
        head.pop();
    }
    if !head.is_empty() && !head.ends_with('\n') {
        head.push('\n');
    }
    atomic_write(path, &head).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

/// TOML-safe string. Prefers `'literal'`; falls back to escaped
/// `"basic"` when `s` contains `'` (literal strings can't represent it).
fn encode_toml_string(s: &str) -> String {
    let has_control = s.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}');
    if !s.contains('\'') && !has_control {
        return format!("'{s}'");
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// POSIX shell-safe quoting. Hook `command` fields are shell-strings
/// — agents tokenise on whitespace, so paths-with-spaces mis-tokenise
/// and the hook silently fails to launch. Single-quoted, with the
/// `'\''` close-reopen trick for paths containing `'`.
pub(crate) fn shell_quote(s: &str) -> String {
    let needs_quote = s
        .chars()
        .any(|c| c.is_whitespace() || "'\"\\$`!*?[](){}<>|;&#~".contains(c));
    if !needs_quote && !s.is_empty() {
        return s.to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn current_drip_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "drip".to_string())
}

fn uninstall_codex() -> Result<String> {
    let home = dirs::home_dir().context("no home directory")?;
    let codex_dir = home.join(".codex");
    let config_path = codex_dir.join("config.toml");
    let agents_path = codex_dir.join("AGENTS.md");

    let mcp_removed = remove_codex_mcp_toml(&config_path)?;
    let agents_removed = remove_marker_block(&agents_path, AGENTS_MD_MARKER)?;

    let mut report = String::new();
    report.push_str("Uninstalled Codex CLI integration.\n");
    report.push_str(&format!(
        "  - {} {}\n",
        if mcp_removed {
            "removed [mcp_servers.drip] from"
        } else {
            "no [mcp_servers.drip] in"
        },
        config_path.display()
    ));
    report.push_str(&format!(
        "  - {} {}\n",
        if agents_removed {
            "removed DRIP block from"
        } else {
            "no DRIP block in"
        },
        agents_path.display()
    ));
    Ok(report)
}

/// Remove the `[mcp_servers.drip]` block. The block shape is fixed
/// (we always write the same header + two keys), so we splice
/// header-line through the next `[section]` header or EOF — no TOML
/// parser needed. Hand-edited extras inside our section get dropped
/// (everything in `[mcp_servers.drip]` is by definition our config).
fn remove_codex_mcp_toml(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
    if !raw.contains(CODEX_MCP_BLOCK_HEADER) {
        return Ok(false);
    }
    let text = strip_codex_drip_block(&raw);
    atomic_write(path, &text).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

/// Pure-string splice-out. Used by uninstall and by re-init when the
/// stored args have drifted from what we'd write today.
fn strip_codex_drip_block(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let Some(header_idx) = lines
        .iter()
        .position(|l| l.trim() == CODEX_MCP_BLOCK_HEADER)
    else {
        return raw.to_string();
    };
    let mut end = lines.len();
    for (i, l) in lines.iter().enumerate().skip(header_idx + 1) {
        let t = l.trim_start();
        if t.starts_with('[') && t != CODEX_MCP_BLOCK_HEADER {
            end = i;
            break;
        }
    }
    let mut start = header_idx;
    while start > 0 && lines[start - 1].trim().is_empty() {
        start -= 1;
    }
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    out.extend_from_slice(&lines[..start]);
    out.extend_from_slice(&lines[end..]);
    let mut text = out.join("\n");
    if raw.ends_with('\n') && !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

// -------- Gemini -----------------------------------------------------------
//
// Gemini's settings.json uses the Claude Desktop MCP shape:
// `{"mcpServers": {"<name>": {"command": "...", "args": [...]}}}`.

fn gemini_settings_path(global: bool) -> Result<PathBuf> {
    if global {
        Ok(dirs::home_dir()
            .context("no home directory")?
            .join(".gemini")
            .join("settings.json"))
    } else {
        Ok(PathBuf::from(".gemini").join("settings.json"))
    }
}

fn gemini_md_path(global: bool) -> Result<PathBuf> {
    if global {
        Ok(dirs::home_dir()
            .context("no home directory")?
            .join(".gemini")
            .join("GEMINI.md"))
    } else {
        Ok(PathBuf::from("GEMINI.md"))
    }
}

fn init_gemini(global: bool) -> Result<String> {
    let drip_bin = current_drip_path();
    let settings_path = gemini_settings_path(global)?;
    let md_path = gemini_md_path(global)?;
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {parent:?}"))?;
    }
    let mcp_added = ensure_mcp_json(&settings_path, &drip_bin, "gemini")?;
    // Advisory before-compress hook: bumps DRIP's v9 compaction
    // ledger + wipes baselines so the next Read after Gemini's
    // context compression is a genuine FullFirst the agent's
    // tracker registers.
    let hook_added = ensure_gemini_compress_hook(&settings_path, &drip_bin)?;
    let md_added = ensure_marker_block(&md_path, AGENTS_MD_MARKER, AGENTS_MD_BODY)?;
    Ok(format!(
        "Gemini CLI integration installed.\n  \
         - {} {}\n  \
         - {} (compaction hook) {}\n  \
         - {} {}\n\
         Restart `gemini` to pick up the MCP server.\n",
        if mcp_added {
            "wired mcpServers.drip in"
        } else {
            "kept"
        },
        settings_path.display(),
        if hook_added {
            "wired hooks.beforeCompress.drip in"
        } else {
            "kept"
        },
        settings_path.display(),
        if md_added {
            "appended DRIP block to"
        } else {
            "kept"
        },
        md_path.display(),
    ))
}

fn uninstall_gemini(global: bool) -> Result<String> {
    let settings_path = gemini_settings_path(global)?;
    let md_path = gemini_md_path(global)?;
    let mcp_removed = remove_mcp_json(&settings_path)?;
    let hook_removed = remove_gemini_compress_hook(&settings_path)?;
    let md_removed = remove_marker_block(&md_path, AGENTS_MD_MARKER)?;
    Ok(format!(
        "Uninstalled Gemini CLI integration.\n  \
         - {} {}\n  \
         - {} (compaction hook) {}\n  \
         - {} {}\n",
        if mcp_removed {
            "removed mcpServers.drip from"
        } else {
            "no mcpServers.drip in"
        },
        settings_path.display(),
        if hook_removed {
            "removed hooks.beforeCompress.drip from"
        } else {
            "no hooks.beforeCompress.drip in"
        },
        settings_path.display(),
        if md_removed {
            "removed DRIP block from"
        } else {
            "no DRIP block in"
        },
        md_path.display(),
    ))
}

/// Set `mcpServers.drip` to our standard shape, preserving every
/// other key. `agent_tag` lands as `--agent <tag>` so `Session::open`
/// can record the caller. Idempotent. Malformed JSON errors out.
fn ensure_mcp_json(path: &Path, drip_bin: &str, agent_tag: &str) -> Result<bool> {
    let mut root: Value = if path.exists() {
        let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
        if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&raw).with_context(|| format!("parsing {path:?} as JSON"))?
        }
    } else {
        json!({})
    };
    let obj = root
        .as_object_mut()
        .context("MCP config must be a JSON object")?;
    let servers = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| json!({}));
    let servers_obj = servers
        .as_object_mut()
        .context("`mcpServers` must be a JSON object")?;
    let desired = json!({
        "command": drip_bin,
        "args": ["mcp", "--agent", agent_tag],
    });
    if servers_obj.get(MCP_SERVER_NAME) == Some(&desired) {
        return Ok(false);
    }
    servers_obj.insert(MCP_SERVER_NAME.to_string(), desired);
    let pretty = serde_json::to_string_pretty(&root)? + "\n";
    atomic_write(path, &pretty).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

/// Drop only `mcpServers.drip`. Removes `mcpServers` itself if empty
/// after the removal so we don't leave a dangling `{}`.
fn remove_mcp_json(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
    if raw.trim().is_empty() {
        return Ok(false);
    }
    let mut root: Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {path:?} as JSON"))?;
    let Some(obj) = root.as_object_mut() else {
        return Ok(false);
    };
    let Some(servers) = obj.get_mut("mcpServers").and_then(|v| v.as_object_mut()) else {
        return Ok(false);
    };
    if servers.remove(MCP_SERVER_NAME).is_none() {
        return Ok(false);
    }
    if servers.is_empty() {
        obj.remove("mcpServers");
    }
    let pretty = serde_json::to_string_pretty(&root)? + "\n";
    atomic_write(path, &pretty).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

/// Wire the advisory before-compress hook into Gemini's
/// settings.json: `hooks.beforeCompress.drip = { command }`.
/// Idempotent — re-running init is a no-op.
fn ensure_gemini_compress_hook(path: &Path, drip_bin: &str) -> Result<bool> {
    let mut root: Value = if path.exists() {
        let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
        if raw.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&raw).with_context(|| format!("parsing {path:?} as JSON"))?
        }
    } else {
        json!({})
    };
    let obj = root
        .as_object_mut()
        .context("Gemini settings must be a JSON object")?;
    let hooks = obj.entry("hooks".to_string()).or_insert_with(|| json!({}));
    let hooks_obj = hooks
        .as_object_mut()
        .context("`hooks` must be a JSON object")?;
    let event = hooks_obj
        .entry("beforeCompress".to_string())
        .or_insert_with(|| json!({}));
    let event_obj = event
        .as_object_mut()
        .context("`hooks.beforeCompress` must be a JSON object")?;
    let desired = json!({
        "command": format!("{} hook gemini-compress", shell_quote(drip_bin)),
    });
    if event_obj.get(MCP_SERVER_NAME) == Some(&desired) {
        return Ok(false);
    }
    event_obj.insert(MCP_SERVER_NAME.to_string(), desired);
    let pretty = serde_json::to_string_pretty(&root)? + "\n";
    atomic_write(path, &pretty).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

/// Drop only our `hooks.beforeCompress.drip` entry; clean up parent
/// objects when they become empty.
fn remove_gemini_compress_hook(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
    if raw.trim().is_empty() {
        return Ok(false);
    }
    let mut root: Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {path:?} as JSON"))?;
    let Some(obj) = root.as_object_mut() else {
        return Ok(false);
    };
    let Some(hooks) = obj.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return Ok(false);
    };
    let removed = match hooks
        .get_mut("beforeCompress")
        .and_then(|v| v.as_object_mut())
    {
        Some(event) => {
            let was_present = event.remove(MCP_SERVER_NAME).is_some();
            if event.is_empty() {
                hooks.remove("beforeCompress");
            }
            was_present
        }
        None => false,
    };
    if !removed {
        return Ok(false);
    }
    if hooks.is_empty() {
        obj.remove("hooks");
    }
    let pretty = serde_json::to_string_pretty(&root)? + "\n";
    atomic_write(path, &pretty).with_context(|| format!("writing {path:?}"))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_passthrough_for_safe_paths() {
        assert_eq!(shell_quote("/usr/local/bin/drip"), "/usr/local/bin/drip");
        assert_eq!(
            shell_quote("/Users/foo/.cargo/bin/drip"),
            "/Users/foo/.cargo/bin/drip"
        );
    }

    #[test]
    fn shell_quote_handles_spaces_in_path() {
        // The bug Hugo hits in his own install: the `Projets perso`
        // path tokenises into separate argv entries without quoting.
        let q = shell_quote("/Users/hugo/Projets perso/Drip/target/release/drip");
        assert_eq!(q, "'/Users/hugo/Projets perso/Drip/target/release/drip'");
    }

    #[test]
    fn shell_quote_handles_path_with_single_quote() {
        // Edge case: path itself contains a `'`. Use the POSIX-portable
        // close-quote/escape/reopen trick: ' \' '.
        let q = shell_quote("/tmp/o'brien/drip");
        assert_eq!(q, r"'/tmp/o'\''brien/drip'");
    }

    #[test]
    fn shell_quote_quotes_other_shell_metacharacters() {
        for c in ["$", "`", ";", "&", "|", "(", ")", "<", ">", "*", "?"] {
            let path = format!("/tmp/foo{c}/drip");
            let q = shell_quote(&path);
            assert!(
                q.starts_with('\'') && q.ends_with('\''),
                "expected quoting for shell metachar {c:?}, got: {q}"
            );
        }
    }

    #[test]
    fn is_owned_drip_hook_command_identifies_drip_hooks() {
        // Owned: bare drip, absolute drip, quoted drip-with-space.
        assert!(is_owned_drip_hook_command("drip hook claude"));
        assert!(is_owned_drip_hook_command(
            "/usr/local/bin/drip hook claude"
        ));
        assert!(is_owned_drip_hook_command(
            "'/Users/hugo/Projets perso/Drip/target/release/drip' hook claude"
        ));
        assert!(is_owned_drip_hook_command(
            "/path/drip.exe hook claude-glob"
        ));
        // Other tools with `hook claude` last 2 tokens stay un-owned.
        assert!(!is_owned_drip_hook_command("/tmp/copycat hook claude"));
    }

    #[test]
    fn encode_toml_string_picks_literal_for_safe_paths() {
        assert_eq!(
            encode_toml_string("/usr/local/bin/drip"),
            "'/usr/local/bin/drip'"
        );
    }

    #[test]
    fn encode_toml_string_falls_back_to_basic_for_apostrophe_paths() {
        // Literal strings can't contain `'`; basic strings can with no escape needed.
        assert_eq!(
            encode_toml_string("/tmp/o'brien/drip"),
            "\"/tmp/o'brien/drip\""
        );
    }

    #[test]
    fn encode_toml_string_escapes_basic_string_special_chars() {
        // Backslash + apostrophe in a "literal-illegal" path force basic escaping.
        let out = encode_toml_string(r"c:\users\o'brien\drip.exe");
        assert!(out.starts_with('"') && out.ends_with('"'));
        assert!(out.contains(r"\\"), "backslashes must be escaped: {out}");
    }
}
