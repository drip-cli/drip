use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

mod commands;
mod core;
mod hooks;
mod mcp;

#[derive(Parser, Debug)]
#[command(
    name = "drip",
    version,
    about = "Delta Read Interception Proxy — send only diffs to LLM agents.",
    propagate_version = true
)]
pub struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Read a file: full content on first read, unified diff afterwards.
    Read {
        file: String,
        /// Compute the outcome but do not update the session baseline.
        /// Useful for previewing what DRIP would send to the agent.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install hooks for a supported agent.
    Init {
        /// Install at the user level (~/.claude/) instead of project-level.
        #[arg(short = 'g', long)]
        global: bool,
        /// Target agent.
        #[arg(long, default_value = "claude")]
        agent: String,
    },
    /// Reverse of `init`: prune hooks, remove the @drip.md include
    /// from CLAUDE.md, delete drip.md. Pre-existing user content in
    /// CLAUDE.md is preserved.
    Uninstall {
        /// Uninstall from ~/.claude/ instead of the project tree.
        #[arg(short = 'g', long)]
        global: bool,
        /// Target agent (currently only `claude` is supported).
        #[arg(long, default_value = "claude")]
        agent: String,
    },
    /// Show token-savings statistics. Defaults to *cumulative since
    /// install*; pass --session to scope to the current session only.
    Meter {
        /// Include a per-day breakdown for the last 30 days.
        #[arg(long)]
        history: bool,
        /// Render an ASCII bar of sent vs full tokens.
        #[arg(long)]
        graph: bool,
        /// Emit machine-readable JSON instead of the human report.
        #[arg(long)]
        json: bool,
        /// Scope the report to a specific session. Bare `--session` (no
        /// value) means the current shell session — useful right after
        /// `drip reset` or to verify the agent's session in isolation.
        /// `--session <id>` targets that exact id (run `drip sessions`
        /// to list them). Without the flag, `drip meter` aggregates every
        /// read since DRIP was first installed.
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        session: Option<String>,
        /// Drop entries for files that no longer exist on disk before
        /// printing the report. Useful when benches or `/tmp` files have
        /// inflated the lifetime counters with artifacts. The headline
        /// ratio gets recomputed from the survivors.
        #[arg(long)]
        prune: bool,
    },
    /// Clear DRIP state. Default: just the current session's tracked
    /// reads (cheap, reversible — the next read becomes a fresh first
    /// read). Pass `--all` to nuke EVERY session, lifetime counter,
    /// and on-disk blob — that one is irreversible.
    /// `--stats` zeros the lifetime counters only, leaving baselines
    /// intact.
    Reset {
        /// Delete ALL DRIP data — every session, baseline,
        /// file-registry entry, blob on disk, and the lifetime
        /// counters. Use to start fresh after a bench run pollutes
        /// counters or to wipe the box before uninstall. Asks for
        /// `yes` confirmation unless `--force`.
        #[arg(long)]
        all: bool,
        /// Zero `lifetime_stats` / `lifetime_per_file` / `lifetime_daily`
        /// / `lifetime_edited_files`. Leaves
        /// active sessions and per-file baselines untouched, so an
        /// in-progress agent run keeps its diffs / sentinels.
        #[arg(long)]
        stats: bool,
        /// Skip the interactive confirmation prompt for `--all`.
        /// Required in scripts and CI; ignored otherwise.
        #[arg(long)]
        force: bool,
    },
    /// Drop the cached baseline for one file so the next read returns
    /// full content. Use after an out-of-band edit (manual change in
    /// another editor, `git pull`, …).
    Refresh { file: String },
    /// List recent sessions stored in the SQLite database. A bare
    /// `list` / `ls` positional is accepted as a no-op alias for
    /// symmetry with multi-action CLIs (e.g. `drip cache stats`).
    Sessions {
        /// Optional alias: `list` or `ls`. Both render the same table
        /// as plain `drip sessions`.
        #[arg(value_parser = ["list", "ls"], hide = true)]
        _alias: Option<String>,
    },
    /// Hook entry point — reads a JSON payload from stdin, writes the
    /// rewritten response to stdout. Used by the agent integrations.
    Hook {
        #[arg(value_enum)]
        agent: HookAgentArg,
    },
    /// Run as an MCP (Model Context Protocol) stdio server.
    /// Used by Codex CLI and other MCP-compatible agents.
    Mcp {
        /// Self-identify the calling agent so `drip sessions` can
        /// distinguish Codex / Gemini from a regular shell.
        /// `drip init --agent <X>` writes this flag into the MCP
        /// config it generates; users invoking `drip mcp` directly
        /// can pass it themselves.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Run a background watcher that pre-computes diffs for tracked
    /// files. The Read hook then serves the precomputed result instead
    /// of running the diff inline — making the hook effectively
    /// instantaneous on big files.
    Watch {
        /// Directory to watch (defaults to the current working directory).
        path: Option<std::path::PathBuf>,
    },
    /// Manage the file-cache directory used for large `reads.content`
    /// blobs. Subcommands: `gc` removes orphan blobs whose hash no row
    /// references; `stats` shows inline-vs-file row counts, dedup
    /// hits, on-disk size.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Manage the cross-session file registry. Subcommands: `stats`
    /// (counts of known files, oldest/most-accessed); `gc` (drop
    /// entries unseen for `--older-than DURATION`, default 30d).
    Registry {
        #[command(subcommand)]
        action: RegistryAction,
    },
    /// Generate a shell completion script on stdout. Pipe it into the
    /// conventional file for your shell:
    ///   drip completions zsh  > ~/.zsh/completions/_drip
    ///   drip completions bash > ~/.bash_completion.d/drip.bash
    ///   drip completions fish > ~/.config/fish/completions/drip.fish
    /// `drip init` does this automatically for the detected `$SHELL`.
    Completions {
        /// Shell to generate completions for: bash, zsh, or fish.
        shell: String,
    },
    /// Diagnose the install — binary, DB, file cache, agent hooks,
    /// drip.md/CLAUDE.md, shell completions, current session. Exit
    /// code 0 if no errors, 1 otherwise.
    Doctor {
        /// Emit machine-readable JSON instead of the human report.
        #[arg(long)]
        json: bool,
        /// Print only the final summary line (exit code remains
        /// significant for scripting).
        #[arg(long)]
        quiet: bool,
    },
    /// Upgrade DRIP to the latest released version. Detects how
    /// DRIP was installed (Homebrew, `cargo install`, install
    /// script) and runs the matching upgrade command. No-op when
    /// already on the latest tag.
    Update {
        /// Show what would be done without actually invoking
        /// brew / cargo / the install script.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect the compressed→original line map persisted for a
    /// semantically-compressed read. Without --line, prints the full
    /// table (one row per compressed line). With --line N, resolves
    /// just that line — useful when an Edit landed on `L5` of the
    /// stubbed view and you need the real source range to reason about.
    SourceMap {
        /// Path to the file. The file must have been read in the
        /// current session (`drip read <file>` or via a hook) AND
        /// compression must have fired for a map to exist.
        file: String,
        /// Resolve a single compressed line. Accepts `5` or `L5`.
        #[arg(long)]
        line: Option<String>,
        /// Emit JSON (mirrors the persisted column shape).
        #[arg(long)]
        json: bool,
    },
    /// Replay an agent session — show every read, in order, with the
    /// exact bytes DRIP returned. Killer for debugging "why did the
    /// agent see X here?" without re-running the workflow.
    Replay {
        /// Session id to replay (default: most recent active with events).
        #[arg(long)]
        session: Option<String>,
        /// Filter to events from the last N units (e.g. 5m, 1h, 2d).
        #[arg(long)]
        since: Option<String>,
        /// Filter to events whose path contains this substring.
        #[arg(long)]
        file: Option<String>,
        /// Max number of events to display (default 50).
        #[arg(long)]
        limit: Option<i64>,
        /// Print the full rendered output for each event, not just the
        /// summary table.
        #[arg(long)]
        full: bool,
        /// Emit machine-readable JSON instead of the human report.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum RegistryAction {
    /// Print known-file count, inline/cache breakdown, oldest entry,
    /// most-accessed file.
    Stats,
    /// Drop registry entries whose `last_seen_at` is older than
    /// `--older-than` (default `30d`). Cascade-deletes orphaned
    /// cache blobs.
    Gc {
        /// Age threshold: `30d`, `12h`, `90m`, `45s`. Bare digits
        /// mean seconds. Default: 30d.
        #[arg(long, default_value = "30d")]
        older_than: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum CacheAction {
    /// Remove cache blobs whose hash isn't referenced by any `reads`
    /// row (typically: rows expired by the 2 h session purge).
    Gc,
    /// Print inline / file-cache row counts, dedup hits, sizes.
    Stats,
    /// Hoist any inline `reads.content` rows whose payload is larger
    /// than `DRIP_INLINE_MAX_BYTES` into the file cache, then VACUUM
    /// the SQLite file to actually reclaim disk. Useful right after
    /// upgrading from v1, or when a benchmark has bloated the DB
    /// with huge inline payloads.
    Compact,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum HookAgentArg {
    Claude,
    ClaudeGlob,
    ClaudeGrep,
    ClaudePostEdit,
    ClaudePreEdit,
    ClaudeSessionStart,
    Gemini,
    GeminiCompress,
}

impl From<HookAgentArg> for commands::hook::HookAgent {
    fn from(v: HookAgentArg) -> Self {
        match v {
            HookAgentArg::Claude => commands::hook::HookAgent::Claude,
            HookAgentArg::ClaudeGlob => commands::hook::HookAgent::ClaudeGlob,
            HookAgentArg::ClaudeGrep => commands::hook::HookAgent::ClaudeGrep,
            HookAgentArg::ClaudePostEdit => commands::hook::HookAgent::ClaudePostEdit,
            HookAgentArg::ClaudePreEdit => commands::hook::HookAgent::ClaudePreEdit,
            HookAgentArg::ClaudeSessionStart => commands::hook::HookAgent::ClaudeSessionStart,
            HookAgentArg::Gemini => commands::hook::HookAgent::Gemini,
            HookAgentArg::GeminiCompress => commands::hook::HookAgent::GeminiCompress,
        }
    }
}

fn main() -> Result<()> {
    // Hook-tolerance shim: if argv looks like `drip hook <unknown>`,
    // emit a no-op JSON response and exit 0 instead of letting clap
    // crash. This is the upgrade-path safety net — a stale
    // `settings.json` after a `drip` binary upgrade (e.g. a removed
    // subcommand like the old `claude-bash`) used to take down every
    // matching agent tool call until the user re-ran `drip init`.
    // Now the agent just sees its tool fire natively; DRIP records
    // nothing for the call but doesn't crash the integration.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            return handle_clap_error(e);
        }
    };

    let out = match cli.command {
        Cmd::Read { file, dry_run } => commands::read::run_with(&file, dry_run)?,
        Cmd::Init { global, agent } => {
            let agent = commands::init::Agent::parse(&agent)?;
            commands::init::run(agent, global)?
        }
        Cmd::Uninstall { global, agent } => {
            let agent = commands::init::Agent::parse(&agent)?;
            commands::init::run_uninstall(agent, global)?
        }
        Cmd::Meter {
            history,
            graph,
            json,
            session,
            prune,
        } => {
            // `Some("")` = bare `--session`, `Some(id)` = explicit target,
            // `None` = no flag (default lifetime view).
            let (session_only, session_id) = match session {
                None => (false, None),
                Some(s) if s.is_empty() => (true, None),
                Some(id) => (true, Some(id)),
            };
            commands::meter::run(commands::meter::MeterOpts {
                history,
                graph,
                json,
                session_only,
                session_id,
                prune,
            })?
        }
        Cmd::Reset { all, stats, force } => {
            commands::reset::run(commands::reset::ResetOpts { all, stats, force })?
        }
        Cmd::Refresh { file } => commands::refresh::run(&file)?,
        Cmd::Sessions { _alias: _ } => commands::sessions::run()?,
        Cmd::Hook { agent } => commands::hook::run(agent.into())?,
        Cmd::Mcp { agent } => {
            // Tag BEFORE Session::open so $DRIP_AGENT is set in time.
            // Don't clobber a manually-exported value when --agent
            // isn't passed.
            if let Some(a) = agent {
                std::env::set_var("DRIP_AGENT", a);
            }
            mcp::run()?;
            return Ok(());
        }
        Cmd::Watch { path } => commands::watch::run(path)?,
        Cmd::Cache { action } => match action {
            CacheAction::Gc => commands::cache::run_gc()?,
            CacheAction::Stats => commands::cache::run_stats()?,
            CacheAction::Compact => commands::cache::run_compact()?,
        },
        Cmd::Registry { action } => match action {
            RegistryAction::Stats => commands::registry::run_stats()?,
            RegistryAction::Gc { older_than } => {
                let secs = commands::registry::parse_duration(&older_than);
                commands::registry::run_gc(secs)?
            }
        },
        Cmd::Update { dry_run } => {
            commands::update::run(dry_run)?;
            return Ok(());
        }
        Cmd::SourceMap { file, line, json } => {
            let line = match line {
                None => None,
                Some(raw) => Some(commands::source_map::parse_line_arg(&raw)?),
            };
            commands::source_map::run(commands::source_map::Opts { file, line, json })?
        }
        Cmd::Replay {
            session,
            since,
            file,
            limit,
            full,
            json,
        } => commands::replay::run(commands::replay::ReplayOpts {
            session,
            since,
            file,
            limit,
            full,
            json,
        })?,
        Cmd::Completions { shell } => {
            commands::completions::run(&shell)?;
            return Ok(());
        }
        Cmd::Doctor { json, quiet } => {
            let (out, code) = commands::doctor::run(commands::doctor::DoctorOpts { json, quiet })?;
            print!("{out}");
            if !out.ends_with('\n') {
                println!();
            }
            std::process::exit(code);
        }
    };

    print!("{out}");
    if !out.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// Bridge between clap's strict subcommand parsing and DRIP's
/// "never crash a tool call" invariant. Most parse errors are honest
/// user mistakes (`drip met` instead of `drip meter`) and should hit
/// clap's normal help/usage exit. The exception is `drip hook
/// <invalid>` — argv[1]=="hook", argv[2] doesn't match any current
/// subcommand. That's the upgrade-path footgun: a stale settings.json
/// from a binary that knew `claude-bash` (or any future-removed
/// hook), pointing at a newer `drip` that doesn't. Without this
/// shim, the hook crashes, Claude Code reports "blocking error", and
/// every matching tool call fails until the user re-runs init. With
/// it, we drain the stdin payload (some hooks send big payloads;
/// not draining could deadlock on PIPE) and emit `{}` — Claude Code
/// treats an empty object as "no opinion, proceed normally", which
/// degrades the no-longer-existing hook into a silent passthrough.
fn handle_clap_error(err: clap::Error) -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let is_stale_hook = args.len() >= 3
        && args[1] == "hook"
        && matches!(
            err.kind(),
            clap::error::ErrorKind::InvalidValue
                | clap::error::ErrorKind::UnknownArgument
                | clap::error::ErrorKind::InvalidSubcommand
        );
    if is_stale_hook {
        // Drain stdin so the hook caller's write doesn't block. The
        // 32 KB cap matches the rolling event log; payloads bigger
        // than that get truncated, but Claude Code's hook payloads
        // are well under that.
        use std::io::Read;
        let mut sink = Vec::with_capacity(4096);
        let _ = std::io::stdin().lock().take(32_768).read_to_end(&mut sink);
        // One-shot stderr breadcrumb so a curious user running with
        // hook-debug logs can see what happened. Claude Code ignores
        // stderr from PreToolUse hooks.
        eprintln!(
            "drip: ignoring stale hook subcommand `{}` — re-run `drip init` to clean up settings.json",
            args.get(2).map(String::as_str).unwrap_or("?")
        );
        println!("{{}}");
        return Ok(());
    }
    err.exit();
}
