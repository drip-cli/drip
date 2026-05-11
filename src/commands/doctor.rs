//! `drip doctor` — installation diagnostics. Side-effect-free:
//! never writes to the DB, never derives a new session id. SQLite is
//! opened read-only.

use crate::commands::cache as cache_cmd;
use crate::core::cache;
use crate::core::session::{self, SessionStrategy};
use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct DoctorOpts {
    pub json: bool,
    pub quiet: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Error,
    Info,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Status::Ok => "✅",
            Status::Warn => "⚠️ ",
            Status::Error => "❌",
            Status::Info => "ℹ️ ",
        }
    }
    fn json_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Error => "error",
            Status::Info => "info",
        }
    }
}

#[derive(Debug, Clone)]
struct Check {
    label: String,
    status: Status,
    detail: Option<String>,
    hint: Option<String>,
}

impl Check {
    fn new(status: Status, label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            status,
            detail: None,
            hint: None,
        }
    }
    fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
    fn hint(mut self, h: impl Into<String>) -> Self {
        self.hint = Some(h.into());
        self
    }
}

#[derive(Debug, Clone)]
struct Section {
    name: String,
    /// Stable key used for the JSON payload.
    key: &'static str,
    checks: Vec<Check>,
    /// Section-level metadata for JSON consumers (raw counts, sizes,
    /// flags). Not rendered in the human view.
    meta: Value,
}

impl Section {
    fn new(key: &'static str, name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            key,
            checks: Vec::new(),
            meta: json!({}),
        }
    }
    fn push(&mut self, c: Check) {
        self.checks.push(c);
    }
    fn worst(&self) -> Status {
        self.checks
            .iter()
            .map(|c| c.status)
            .max_by_key(|s| match s {
                Status::Error => 3,
                Status::Warn => 2,
                Status::Info => 1,
                Status::Ok => 0,
            })
            .unwrap_or(Status::Ok)
    }
}

pub fn run(opts: DoctorOpts) -> Result<(String, i32)> {
    let sections = collect_all();
    let errors = sections
        .iter()
        .flat_map(|s| s.checks.iter())
        .filter(|c| c.status == Status::Error)
        .count();
    let warnings = sections
        .iter()
        .flat_map(|s| s.checks.iter())
        .filter(|c| c.status == Status::Warn)
        .count();
    let exit_code = if errors > 0 { 1 } else { 0 };

    let out = if opts.json {
        render_json(&sections, errors, warnings)
    } else if opts.quiet {
        render_quiet(errors, warnings)
    } else {
        render_human(&sections, errors, warnings)
    };
    Ok((out, exit_code))
}

// ---------- collection -----------------------------------------------------

fn collect_all() -> Vec<Section> {
    vec![
        check_binary(),
        check_database(),
        check_cache(),
        check_claude_integration(true),
        check_claude_integration(false),
        check_gemini_integration(true),
        check_gemini_integration(false),
        check_completions(),
        check_session(),
    ]
}

/// Check Gemini's `settings.json` for the DRIP MCP server entry and
/// the before-compress hook. Warn when MCP is wired but the hook is
/// missing (re-run `drip init --agent gemini`). Neither present →
/// uninstalled (Info, not a warning).
fn check_gemini_integration(global: bool) -> Section {
    let (key, name, settings_path): (&'static str, String, PathBuf) = if global {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        (
            "gemini_global",
            "Gemini CLI — Global (~/.gemini/)".into(),
            home.join(".gemini").join("settings.json"),
        )
    } else {
        (
            "gemini_project",
            "Gemini CLI — Project (./.gemini/)".into(),
            PathBuf::from(".gemini").join("settings.json"),
        )
    };
    let mut s = Section::new(key, name);

    if !settings_path.exists() {
        s.push(
            Check::new(Status::Info, "settings.json not found")
                .detail(settings_path.display().to_string())
                .hint(if global {
                    "Run `drip init -g --agent gemini` to install."
                } else {
                    "Run `drip init --agent gemini` from the project root to install project-scoped DRIP."
                }),
        );
        s.meta = json!({
            "installed": false,
            "settings_path": settings_path.display().to_string(),
        });
        return s;
    }

    let raw = match std::fs::read_to_string(&settings_path) {
        Ok(r) => r,
        Err(e) => {
            s.push(Check::new(Status::Error, "Cannot read settings.json").detail(e.to_string()));
            return s;
        }
    };
    let to_parse = if raw.trim().is_empty() { "{}" } else { &raw };
    let parsed: Value = match serde_json::from_str(to_parse) {
        Ok(v) => v,
        Err(e) => {
            s.push(
                Check::new(Status::Error, "settings.json is not valid JSON")
                    .detail(e.to_string())
                    .hint("Edit the file and fix the parse error before re-running."),
            );
            s.meta = json!({
                "installed": true,
                "settings_path": settings_path.display().to_string(),
                "parseable": false,
            });
            return s;
        }
    };

    let mcp_present = parsed
        .pointer("/mcpServers/drip")
        .map(|v| v.is_object())
        .unwrap_or(false);
    let hook_present = parsed
        .pointer("/hooks/beforeCompress/drip")
        .map(|v| v.is_object())
        .unwrap_or(false);

    if !mcp_present && !hook_present {
        // settings.json exists but no DRIP entry — same shape as
        // the Claude project tier when the user has their own
        // Gemini config and hasn't run `drip init`.
        s.push(
            Check::new(Status::Info, "settings.json present, no DRIP entries")
                .detail(settings_path.display().to_string())
                .hint(if global {
                    "Run `drip init -g --agent gemini` to wire DRIP in."
                } else {
                    "Run `drip init --agent gemini` to install project-scoped DRIP."
                }),
        );
        s.meta = json!({
            "installed": false,
            "settings_path": settings_path.display().to_string(),
            "mcp_present": false,
            "hook_present": false,
        });
        return s;
    }

    s.push(Check::new(Status::Ok, "settings.json found"));
    if mcp_present {
        s.push(Check::new(Status::Ok, "MCP server `drip` registered"));
    } else {
        s.push(
            Check::new(Status::Warn, "MCP server `drip` missing")
                .hint("Re-run `drip init --agent gemini` to restore."),
        );
    }
    if hook_present {
        s.push(Check::new(Status::Ok, "Hook hooks.beforeCompress.drip"));
    } else {
        // The hook is the v9-and-later visibility ledger plumbing.
        // Pre-v9 installs only had the MCP entry; this check nudges
        // them to re-run init now that the hook exists.
        s.push(
            Check::new(
                Status::Warn,
                "Compaction hook missing — context compaction won't reset DRIP baselines",
            )
            .hint(if global {
                "Re-run `drip init -g --agent gemini` to add the hook."
            } else {
                "Re-run `drip init --agent gemini` to add the hook."
            }),
        );
    }

    s.meta = json!({
        "installed": true,
        "settings_path": settings_path.display().to_string(),
        "mcp_present": mcp_present,
        "hook_present": hook_present,
    });
    s
}

fn check_binary() -> Section {
    let mut s = Section::new("binary", "Binary");
    let version = env!("CARGO_PKG_VERSION");
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unknown path)".to_string());
    s.push(Check::new(Status::Ok, format!("drip {version}")).detail(path.clone()));

    // Update check is gated behind `DRIP_CHECK_UPDATES=1` so the
    // default `drip doctor` stays sub-10ms and offline-friendly.
    // When enabled it shells out to curl with a tight timeout —
    // see commands::update::detect_install_method for the path-based
    // install detector.
    let update_meta = if std::env::var_os("DRIP_CHECK_UPDATES").is_some() {
        match crate::commands::update::check_for_update() {
            Ok(Some(latest)) => {
                s.push(
                    Check::new(
                        Status::Warn,
                        format!("Update available: {latest} — run `drip update`"),
                    )
                    .hint("Run `drip update` to upgrade."),
                );
                json!({ "available": true, "latest": latest })
            }
            Ok(None) => json!({ "available": false }),
            Err(_) => json!({ "available": null, "error": "check failed" }),
        }
    } else {
        json!({ "checked": false })
    };
    s.meta = json!({ "version": version, "path": path, "update": update_meta });
    s
}

fn check_database() -> Section {
    let mut s = Section::new("database", "Database");
    let data_dir = match session::data_dir() {
        Ok(d) => d,
        Err(_) => {
            s.push(Check::new(Status::Error, "Cannot resolve DRIP_DATA_DIR"));
            s.meta = json!({ "found": false });
            return s;
        }
    };
    let db_path = data_dir.join("sessions.db");

    if !db_path.exists() {
        s.push(
            Check::new(Status::Info, "sessions.db not initialised")
                .detail(db_path.display().to_string())
                .hint("Run any drip command (e.g. `drip read <file>`) to create it."),
        );
        s.meta = json!({ "found": false, "path": db_path.display().to_string() });
        return s;
    }

    let size = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);
    s.push(Check::new(Status::Ok, "sessions.db found").detail(format!(
        "{} at {}",
        fmt_bytes(size),
        db_path.display()
    )));

    let conn = match Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(e) => {
            s.push(
                Check::new(Status::Error, "Cannot open sessions.db")
                    .detail(e.to_string())
                    .hint("File may be locked or corrupt — inspect with `sqlite3 sessions.db .schema`."),
            );
            s.meta = json!({
                "found": true,
                "path": db_path.display().to_string(),
                "size_bytes": size,
                "openable": false,
            });
            return s;
        }
    };

    let schema_version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .ok();
    match schema_version.as_deref() {
        Some(v) => {
            s.push(Check::new(Status::Ok, format!("Schema version: {v}")));
        }
        None => s.push(
            Check::new(Status::Warn, "Schema version not set")
                .hint("Run any drip command — the migration runs on first open."),
        ),
    }

    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap_or_else(|_| "(unknown)".to_string());
    let wal = journal_mode.eq_ignore_ascii_case("wal");
    if wal {
        s.push(Check::new(Status::Ok, "WAL mode: active"));
    } else {
        s.push(
            Check::new(Status::Warn, format!("Journal mode: {journal_mode}"))
                .hint("Expected `wal`. Concurrent agents may serialise on writes."),
        );
    }

    s.meta = json!({
        "found": true,
        "path": db_path.display().to_string(),
        "size_bytes": size,
        "schema_version": schema_version,
        "wal": wal,
    });
    s
}

fn check_cache() -> Section {
    let mut s = Section::new("cache", "Cache");
    let stats = match cache_cmd::collect_stats() {
        Ok(s) => s,
        Err(_) => {
            s.push(
                Check::new(Status::Info, "Cache stats unavailable").hint("DB not initialised yet."),
            );
            s.meta = json!({ "available": false });
            return s;
        }
    };

    let data_dir = session::data_dir().ok();
    let cache_dir = data_dir
        .as_deref()
        .map(cache::cache_dir)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(unknown)".to_string());

    s.push(Check::new(Status::Ok, "Cache directory").detail(format!(
        "{} ({} blobs, {})",
        cache_dir,
        stats.cache_files,
        fmt_bytes(stats.cache_size_bytes)
    )));
    if stats.orphan_files == 0 {
        s.push(Check::new(Status::Ok, "No orphan blobs"));
    } else {
        s.push(
            Check::new(
                Status::Warn,
                format!(
                    "{} orphan blob(s) ({})",
                    stats.orphan_files,
                    fmt_bytes(stats.orphan_bytes)
                ),
            )
            .hint("Run `drip cache gc` to reclaim disk."),
        );
    }
    if stats.compactable_rows > 0 {
        s.push(
            Check::new(
                Status::Info,
                format!(
                    "{} inline row(s) above the file-cache threshold",
                    stats.compactable_rows
                ),
            )
            .detail(fmt_bytes(stats.compactable_bytes as u64))
            .hint("Run `drip cache compact` to hoist these to the file cache."),
        );
    }

    s.meta = json!({
        "blobs": stats.cache_files,
        "size_bytes": stats.cache_size_bytes,
        "orphans": stats.orphan_files,
        "compactable_rows": stats.compactable_rows,
    });
    s
}

/// Map an `EXPECTED_HOOKS` label to its settings.json event key.
fn event_for_label(label: &str) -> &'static str {
    if label.starts_with("PreToolUse") {
        "PreToolUse"
    } else if label.starts_with("PostToolUse") {
        "PostToolUse"
    } else if label.starts_with("SessionStart") {
        "SessionStart"
    } else {
        "Unknown"
    }
}

/// Hooks DRIP installs into Claude Code.
const EXPECTED_HOOKS: &[(&str, &str, &str)] = &[
    ("PreToolUse:Read", "Read", "claude"),
    ("PreToolUse:Glob", "Glob", "claude-glob"),
    ("PreToolUse:Grep", "Grep", "claude-grep"),
    (
        "PreToolUse:Edit/MultiEdit/Write/NotebookEdit",
        "Edit|MultiEdit|Write|NotebookEdit",
        "claude-pre-edit",
    ),
    (
        "PostToolUse:Edit/Write/MultiEdit/NotebookEdit",
        "Edit|Write|MultiEdit|NotebookEdit",
        "claude-post-edit",
    ),
    (
        "SessionStart:compact|clear",
        "compact|clear",
        "claude-session-start",
    ),
];

/// True iff every DRIP hook is registered in `~/.claude/settings.json`
/// — drives the "covered by global" verdict for project tiers.
fn global_hooks_complete() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let path = home.join(".claude/settings.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    EXPECTED_HOOKS.iter().all(|(label, matcher, suffix)| {
        let event = if label.starts_with("Pre") {
            "PreToolUse"
        } else {
            "PostToolUse"
        };
        hook_present(&settings, event, matcher, suffix)
    })
}

fn check_claude_integration(global: bool) -> Section {
    let (key, name, base_dir): (&'static str, String, PathBuf) = if global {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        (
            "hooks_global",
            "Claude Code — Global (~/.claude/)".into(),
            home.join(".claude"),
        )
    } else {
        (
            "hooks_project",
            "Claude Code — Project (./.claude/)".into(),
            PathBuf::from(".claude"),
        )
    };
    let mut s = Section::new(key, name);

    let settings_path = base_dir.join("settings.json");
    let drip_md_path = if global {
        base_dir.join("drip.md")
    } else {
        PathBuf::from("drip.md")
    };
    let claude_md_path = if global {
        base_dir.join("CLAUDE.md")
    } else {
        PathBuf::from("CLAUDE.md")
    };

    if !settings_path.exists() {
        if global {
            s.push(
                Check::new(Status::Warn, "settings.json not found")
                    .detail(settings_path.display().to_string())
                    .hint("Run `drip init -g` to install hooks globally."),
            );
        } else if global_hooks_complete() {
            // Global already covers everything — project tier is optional.
            s.push(
                Check::new(Status::Ok, "Covered by global hooks (~/.claude/)")
                    .detail("project-level init is optional"),
            );
            s.meta = json!({
                "installed": false,
                "covered_by_global": true,
                "settings_path": settings_path.display().to_string(),
            });
            return s;
        } else {
            s.push(
                Check::new(Status::Info, "No project-level init")
                    .hint("Run `drip init` from a project root to add project hooks."),
            );
        }
        // Whole tier uninstalled → Info, not Warn.
        if drip_md_path.exists() {
            push_drip_md_check(&mut s, &drip_md_path);
        } else {
            s.push(Check::new(Status::Info, "drip.md not found"));
        }
        if claude_md_path.exists() {
            push_claude_md_check_with_severity(&mut s, &claude_md_path, Status::Info);
        } else {
            s.push(Check::new(Status::Info, "CLAUDE.md not found"));
        }
        s.meta = json!({
            "installed": false,
            "settings_path": settings_path.display().to_string(),
        });
        return s;
    }

    let raw = match std::fs::read_to_string(&settings_path) {
        Ok(r) => r,
        Err(e) => {
            s.push(Check::new(Status::Ok, "settings.json found"));
            s.push(Check::new(Status::Error, "Cannot read settings.json").detail(e.to_string()));
            return s;
        }
    };
    let settings: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            s.push(Check::new(Status::Ok, "settings.json found"));
            s.push(
                Check::new(Status::Error, "settings.json is not valid JSON")
                    .detail(e.to_string())
                    .hint("Edit the file and fix the parse error before re-running."),
            );
            s.meta = json!({
                "installed": true,
                "settings_path": settings_path.display().to_string(),
                "parseable": false,
            });
            return s;
        }
    };

    let presences: Vec<bool> = EXPECTED_HOOKS
        .iter()
        .map(|(label, matcher, suffix)| {
            let event = event_for_label(label);
            hook_present(&settings, event, matcher, suffix)
        })
        .collect();
    let any_present = presences.iter().any(|&b| b);

    // Project tier has its own settings.json but no DRIP hooks: don't
    // warn-per-hook. If global covers everything, ✅; else suggest
    // running `drip init` here.
    if !global && !any_present {
        if global_hooks_complete() {
            s.push(
                Check::new(Status::Ok, "Covered by global hooks (~/.claude/)")
                    .detail("project-level init is optional"),
            );
            s.meta = json!({
                "installed": false,
                "covered_by_global": true,
                "settings_path": settings_path.display().to_string(),
                "any_drip_hooks": false,
            });
            return s;
        }
        s.push(
            Check::new(Status::Info, "settings.json present, no DRIP hooks")
                .detail(settings_path.display().to_string())
                .hint("Run `drip init` from this directory if you want project-scoped DRIP."),
        );
        if drip_md_path.exists() {
            push_drip_md_check(&mut s, &drip_md_path);
        } else {
            s.push(Check::new(Status::Info, "drip.md not found"));
        }
        if claude_md_path.exists() {
            push_claude_md_check_with_severity(&mut s, &claude_md_path, Status::Info);
        } else {
            s.push(Check::new(Status::Info, "CLAUDE.md not found"));
        }
        s.meta = json!({
            "installed": false,
            "settings_path": settings_path.display().to_string(),
            "any_drip_hooks": false,
        });
        return s;
    }

    s.push(Check::new(Status::Ok, "settings.json found"));

    let mut hooks_status = serde_json::Map::new();
    for (i, (label, _matcher, suffix)) in EXPECTED_HOOKS.iter().enumerate() {
        let present = presences[i];
        hooks_status.insert((*label).to_string(), Value::Bool(present));
        if present {
            s.push(
                Check::new(Status::Ok, format!("Hook {label}"))
                    .detail(format!("drip hook {suffix}")),
            );
        } else if *suffix == "claude-session-start" {
            // SessionStart is upgraded to Error (not Warn) because it
            // gates a correctness invariant, not just savings: without
            // it, Claude Code's read-tracker is wiped on `/compact` /
            // `/clear` while DRIP keeps its baseline, and the next
            // Edit fails with "File must be read first" mid-task. Pre-
            // upgrade installs land here on the first `drip doctor`
            // run after upgrade, which is the right time to nudge
            // them to re-init.
            s.push(
                Check::new(
                    Status::Error,
                    "SessionStart hook missing — compaction will break Edit after context reset",
                )
                .hint(if global {
                    "Run `drip init -g` to add the missing hook."
                } else {
                    "Run `drip init` to add the missing hook."
                }),
            );
        } else {
            // A *partial* install (some DRIP hooks present, others
            // missing) is genuinely worth a warning — the integration
            // is broken in the user's eyes.
            s.push(
                Check::new(Status::Warn, format!("Hook {label} missing")).hint(if global {
                    "Re-run `drip init -g` to restore."
                } else {
                    "Re-run `drip init` to restore."
                }),
            );
        }
    }

    if drip_md_path.exists() {
        push_drip_md_check(&mut s, &drip_md_path);
    } else {
        s.push(Check::new(Status::Warn, "drip.md missing").hint(if global {
            "Re-run `drip init -g` to restore."
        } else {
            "Re-run `drip init` to restore."
        }));
    }

    if claude_md_path.exists() {
        push_claude_md_check(&mut s, &claude_md_path);
    } else {
        s.push(
            Check::new(Status::Error, "CLAUDE.md not found").hint(if global {
                "Re-run `drip init -g` to create it with @drip.md."
            } else {
                "Re-run `drip init` to create it with @drip.md."
            }),
        );
    }

    s.meta = json!({
        "installed": true,
        "settings_path": settings_path.display().to_string(),
        "hooks": Value::Object(hooks_status),
        "drip_md": drip_md_path.exists(),
        "claude_md": claude_md_path.exists(),
    });
    s
}

fn push_drip_md_check(s: &mut Section, path: &Path) {
    let body = std::fs::read_to_string(path).unwrap_or_default();
    if body.contains("<!-- drip:memory -->") {
        s.push(
            Check::new(Status::Ok, "drip.md found").detail("<!-- drip:memory --> marker present"),
        );
    } else {
        s.push(
            Check::new(Status::Warn, "drip.md present but missing marker")
                .detail("file is user-authored — DRIP will not overwrite it")
                .hint("Move/rename your file then re-run init to install DRIP's drip.md."),
        );
    }
}

fn push_claude_md_check(s: &mut Section, path: &Path) {
    push_claude_md_check_with_severity(s, path, Status::Error);
}

/// Same logic as `push_claude_md_check` but lets the caller pick the
/// severity for "ref missing". Use `Status::Error` when this tier is
/// the canonical install (the global tier with DRIP hooks present);
/// use `Status::Info` when the tier is optional — typically a
/// project-level CLAUDE.md sitting next to a settings.json that has
/// no DRIP hooks and is implicitly covered by the global install.
/// Surfacing `❌` in that case nudges users to run `drip init` here
/// when they don't actually need to.
fn push_claude_md_check_with_severity(s: &mut Section, path: &Path, missing_severity: Status) {
    let body = std::fs::read_to_string(path).unwrap_or_default();
    let has_ref = body.lines().any(|l| l.trim() == "@drip.md");
    if has_ref {
        s.push(Check::new(Status::Ok, "CLAUDE.md references @drip.md"));
    } else {
        let mut check = Check::new(missing_severity, "CLAUDE.md does not reference @drip.md");
        check = match missing_severity {
            Status::Info => check
                .detail("global install covers this project — only needed for project-scoped DRIP"),
            _ => check.hint("Re-run `drip init` to add the @drip.md include."),
        };
        s.push(check);
    }
}

fn hook_present(settings: &Value, event: &str, matcher: &str, suffix: &str) -> bool {
    let Some(arr) = settings
        .get("hooks")
        .and_then(|h| h.get(event))
        .and_then(|v| v.as_array())
    else {
        return false;
    };
    arr.iter().any(|entry| {
        entry.get("matcher").and_then(|m| m.as_str()) == Some(matcher)
            && entry
                .get("hooks")
                .and_then(|h| h.as_array())
                .is_some_and(|hs| {
                    hs.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|cmd| {
                                let toks: Vec<&str> = cmd.split_whitespace().collect();
                                toks.len() >= 2
                                    && toks[toks.len() - 2] == "hook"
                                    && toks[toks.len() - 1] == suffix
                            })
                    })
                })
    })
}

fn check_completions() -> Section {
    let mut s = Section::new("completions", "Shell Completions");
    let shell = std::env::var("SHELL").unwrap_or_default();
    let detected = if shell.contains("zsh") {
        Some(("zsh", "~/.zsh/completions/_drip", ".zsh/completions/_drip"))
    } else if shell.contains("bash") {
        Some((
            "bash",
            "~/.bash_completion.d/drip.bash",
            ".bash_completion.d/drip.bash",
        ))
    } else if shell.contains("fish") {
        Some((
            "fish",
            "~/.config/fish/completions/drip.fish",
            ".config/fish/completions/drip.fish",
        ))
    } else {
        None
    };

    let Some((name, display, rel)) = detected else {
        s.push(Check::new(Status::Info, "Shell not detected (set $SHELL)"));
        s.meta = json!({ "shell": null, "installed": false });
        return s;
    };

    let abs = dirs::home_dir().unwrap_or_default().join(rel);
    if abs.exists() {
        s.push(
            Check::new(Status::Ok, format!("{name} completions installed"))
                .detail(display.to_string()),
        );
        s.meta = json!({ "shell": name, "installed": true, "path": display });
    } else {
        s.push(
            Check::new(Status::Info, format!("No completions installed for {name}"))
                .detail(display.to_string()),
        );
        s.meta = json!({ "shell": name, "installed": false, "path": display });
    }
    s
}

fn check_session() -> Section {
    let mut s = Section::new("session", "Session");
    let data_dir = match session::data_dir() {
        Ok(d) => d,
        Err(_) => {
            s.push(Check::new(Status::Info, "Data dir unavailable"));
            return s;
        }
    };
    let db_path = data_dir.join("sessions.db");
    if !db_path.exists() {
        s.push(Check::new(Status::Info, "No session yet (DB absent)"));
        return s;
    }
    let conn = match Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(_) => {
            s.push(Check::new(
                Status::Warn,
                "Cannot open sessions.db read-only",
            ));
            return s;
        }
    };

    // Most-recently-active session. v9 adds the compaction ledger
    // (epoch + count) — pulled here so the section can surface
    // `↺ N compactions` underneath the strategy line.
    type SessionRow = (String, Option<String>, Option<String>, i64, i64, i64);
    let row: Option<SessionRow> = conn
        .query_row(
            "SELECT session_id, strategy, context,
                    COALESCE((SELECT COUNT(*) FROM reads r WHERE r.session_id = s.session_id), 0),
                    COALESCE(context_epoch, 0),
                    COALESCE(compaction_count, 0)
             FROM sessions s
             ORDER BY s.last_active DESC
             LIMIT 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .ok();

    match row {
        None => {
            s.push(Check::new(Status::Info, "No sessions recorded yet"));
        }
        Some((id, strategy, context, files, epoch, compactions)) => {
            let strategy_str = strategy
                .as_deref()
                .and_then(SessionStrategy::parse)
                .map(|s| s.as_str())
                .unwrap_or("unknown");
            let ctx_str = context.unwrap_or_else(|| "(none)".to_string());
            s.push(
                Check::new(Status::Ok, format!("Strategy: {strategy_str}"))
                    .detail(format!("context: {ctx_str}")),
            );
            let id_short: String = id.chars().take(20).collect();
            s.push(Check::new(
                Status::Ok,
                format!("Most recent session: {id_short}"),
            ));
            s.push(Check::new(Status::Ok, format!("Files tracked: {files}")));

            // v9 visibility ledger — only print when this session
            // has actually been compacted. Keeps the doctor output
            // unchanged for the common case (no compactions) and
            // surfaces `↺ context epoch: N` only when relevant.
            if compactions > 0 {
                let body = if epoch == compactions {
                    format!("↺ {compactions} context compaction(s) this session")
                } else {
                    format!("↺ {compactions} compaction(s), context epoch: {epoch}")
                };
                s.push(
                    Check::new(Status::Info, body)
                        .detail("baselines were reset automatically — first reads after each compaction are decorated with the same `↺` marker"),
                );
            }

            let saved: i64 = conn
                .query_row(
                    "SELECT COALESCE(SUM(tokens_full - tokens_sent), 0) FROM reads",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            s.push(Check::new(
                Status::Ok,
                format!("Tokens saved (lifetime): {}", fmt_thousands(saved.max(0))),
            ));
            s.meta = json!({
                "strategy": strategy_str,
                "context": ctx_str,
                "session_id": id,
                "files_tracked": files,
                "tokens_saved": saved,
                "context_epoch": epoch,
                "compaction_count": compactions,
            });
        }
    }
    s
}

// ---------- rendering ------------------------------------------------------

fn render_human(sections: &[Section], errors: usize, warnings: usize) -> String {
    let color = use_color();
    let mut out = String::new();
    out.push_str("DRIP Doctor — Installation Check\n");
    out.push_str(&"═".repeat(62));
    out.push('\n');
    out.push('\n');

    for sec in sections {
        out.push_str(&sec.name);
        out.push('\n');
        for c in &sec.checks {
            out.push_str("  ");
            out.push_str(&paint(c.status.glyph(), c.status, color));
            out.push_str("  ");
            out.push_str(&c.label);
            if let Some(d) = &c.detail {
                out.push_str("  (");
                out.push_str(d);
                out.push(')');
            }
            out.push('\n');
            if let Some(h) = &c.hint {
                out.push_str("      → ");
                out.push_str(h);
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out.push_str(&"─".repeat(62));
    out.push('\n');
    if errors == 0 && warnings == 0 {
        out.push_str("  ");
        out.push_str(&paint("✅", Status::Ok, color));
        out.push_str("  Everything looks good. DRIP is fully operational.\n");
    } else if errors == 0 {
        out.push_str("  ");
        out.push_str(&paint("⚠️ ", Status::Warn, color));
        out.push_str(&format!("  {} warning(s) — see hints above.\n", warnings));
    } else {
        out.push_str("  ");
        out.push_str(&paint("❌", Status::Error, color));
        out.push_str(&format!(
            "  {} warning(s), {} error(s) found. Run the suggested commands above.\n",
            warnings, errors
        ));
    }
    out
}

fn render_quiet(errors: usize, warnings: usize) -> String {
    if errors == 0 && warnings == 0 {
        "ok\n".to_string()
    } else {
        format!("{errors} error(s), {warnings} warning(s)\n")
    }
}

fn render_json(sections: &[Section], errors: usize, warnings: usize) -> String {
    let mut sec_obj = serde_json::Map::new();
    for s in sections {
        let checks: Vec<Value> = s
            .checks
            .iter()
            .map(|c| {
                json!({
                    "status": c.status.json_str(),
                    "label": c.label,
                    "detail": c.detail,
                    "hint": c.hint,
                })
            })
            .collect();
        let mut entry = serde_json::Map::new();
        entry.insert("status".into(), Value::String(s.worst().json_str().into()));
        entry.insert("checks".into(), Value::Array(checks));
        if let Value::Object(meta) = &s.meta {
            for (k, v) in meta {
                entry.insert(k.clone(), v.clone());
            }
        }
        sec_obj.insert(s.key.to_string(), Value::Object(entry));
    }
    let payload = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "errors": errors,
        "warnings": warnings,
        "sections": Value::Object(sec_obj),
    });
    serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into()) + "\n"
}

// ---------- helpers --------------------------------------------------------

fn use_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

fn paint(glyph: &str, status: Status, color: bool) -> String {
    if !color {
        return glyph.to_string();
    }
    let code = match status {
        Status::Ok => "32",
        Status::Warn => "33",
        Status::Error => "31",
        Status::Info => "36",
    };
    format!("\x1b[{code}m{glyph}\x1b[0m")
}

fn fmt_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if n >= GB {
        format!("{:.2} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.2} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

fn fmt_thousands(n: i64) -> String {
    let s = n.abs().to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    if n < 0 {
        out.push('-');
    }
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_bytes_thresholds() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2 * 1024), "2.0 KB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024), "3.00 MB");
    }

    #[test]
    fn fmt_thousands_basic() {
        assert_eq!(fmt_thousands(0), "0");
        assert_eq!(fmt_thousands(1234), "1,234");
        assert_eq!(fmt_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn section_worst_picks_most_severe() {
        let mut s = Section::new("x", "X");
        s.push(Check::new(Status::Ok, "a"));
        s.push(Check::new(Status::Info, "b"));
        s.push(Check::new(Status::Warn, "c"));
        assert_eq!(s.worst(), Status::Warn);
        s.push(Check::new(Status::Error, "d"));
        assert_eq!(s.worst(), Status::Error);
    }

    #[test]
    fn hook_present_matches_drip_subcommand() {
        let s: Value = serde_json::from_str(
            r#"{
              "hooks": {
                "PreToolUse": [
                  {"matcher": "Read",
                   "hooks": [{"type":"command","command":"/path/drip hook claude"}]}
                ]
              }
            }"#,
        )
        .unwrap();
        assert!(hook_present(&s, "PreToolUse", "Read", "claude"));
        assert!(!hook_present(&s, "PreToolUse", "Read", "claude-bash"));
        assert!(!hook_present(&s, "PreToolUse", "Bash", "claude"));
    }
}
