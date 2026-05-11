use crate::core::cache;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default idle window before a session is purged. `touch()` fires on
/// every read/edit/baseline-refresh, so a continuously-active 6-hour
/// session never expires. Override with `DRIP_SESSION_TTL_SECS`
/// (clamped to a 30 min floor).
const DEFAULT_SESSION_TTL_SECS: i64 = 2 * 60 * 60;
const MIN_SESSION_TTL_SECS: i64 = 30 * 60;

pub fn session_ttl_secs() -> i64 {
    std::env::var("DRIP_SESSION_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(DEFAULT_SESSION_TTL_SECS)
        .max(MIN_SESSION_TTL_SECS)
}

/// Bumped on every incompatible on-disk schema change. Higher versions
/// in the DB abort the binary with a clear error; equal-or-lower is
/// accepted (additive ALTER TABLE migrations live in `migrate_schema`).
pub const SCHEMA_VERSION: i64 = 11;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS reads (
    session_id              TEXT NOT NULL,
    file_path               TEXT NOT NULL,
    content_hash            TEXT NOT NULL,
    content                 TEXT NOT NULL,
    read_at                 INTEGER NOT NULL,
    reads_count             INTEGER NOT NULL DEFAULT 1,
    tokens_full             INTEGER NOT NULL,
    tokens_sent             INTEGER NOT NULL,
    content_storage         TEXT NOT NULL DEFAULT 'inline',
    was_semantic_compressed INTEGER NOT NULL DEFAULT 0,
    elided_functions        TEXT,
    -- JSON `Vec<SourceMapEntry>`, one per compressed line. NULL on
    -- non-compressed reads.
    source_map              TEXT,
    -- Epoch the baseline was written under. Bumped by
    -- `reset_for_compaction()`; live rows carry the session's current
    -- epoch (the reset wipes earlier rows).
    context_epoch           INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, file_path)
);

CREATE TABLE IF NOT EXISTS sessions (
    session_id   TEXT PRIMARY KEY,
    started_at   INTEGER NOT NULL,
    last_active  INTEGER NOT NULL,
    cwd          TEXT,
    strategy     TEXT,    -- env|git|pid|cwd
    context      TEXT,    -- branch / '(pid N)' / '(env)' / '(cwd)'
    agent        TEXT,    -- claude|codex|gemini|null ($DRIP_AGENT)
    -- Context-compaction ledger. Counters survive
    -- `reset_for_compaction` (which preserves this row).
    -- `compaction_count` is named separately from `context_epoch` to
    -- leave room for future epoch-bumping events that shouldn't count
    -- as compactions. `last_compaction_at` NULL = never.
    context_epoch        INTEGER NOT NULL DEFAULT 0,
    last_compaction_at   INTEGER,
    compaction_count     INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_reads_session ON reads(session_id);
CREATE INDEX IF NOT EXISTS idx_reads_active ON reads(session_id, read_at DESC);

-- Cross-session file registry. `reads` is per-session and TTL-purged;
-- the registry survives until `drip registry gc` and lets the FIRST
-- read in a new session orient the agent against the prior baseline.
CREATE TABLE IF NOT EXISTS file_registry (
    file_path        TEXT PRIMARY KEY,
    content_hash     TEXT NOT NULL,
    content          TEXT NOT NULL DEFAULT '',
    content_storage  TEXT NOT NULL DEFAULT 'inline',
    last_session_id  TEXT NOT NULL,
    last_seen_at     INTEGER NOT NULL,
    last_git_branch  TEXT,
    reads_count      INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_registry_hash ON file_registry(content_hash);

-- Tombstones for recently-purged sessions so that if the same
-- session id reopens (Claude Code reusing a UUID across a long-pause
-- restart), the first read of every file gets a `session expired —
-- fresh baseline started` decoration. 24 h TTL.
CREATE TABLE IF NOT EXISTS expired_sessions (
    session_id  TEXT PRIMARY KEY,
    expired_at  INTEGER NOT NULL
);

-- Cumulative-since-install counters. `reads` is TTL-purged (it stores
-- file content); these aggregates survive forever and feed
-- `drip meter`'s "since install" totals.
--
-- `external_edit_refreshes` tracks reads where the file changed
-- out-of-band (cargo fmt, git pull, a non-hooked editor) between two
-- Claude Code reads — DRIP's Claude hook refreshes its baseline and
-- ships full native content to keep Claude's read-tracker in sync,
-- so these reads contribute 0 savings. Surfacing the count in the
-- meter prevents the "why is my reduction so low?" question on repos
-- the agent edits while it reads.
CREATE TABLE IF NOT EXISTS lifetime_stats (
    id                       INTEGER PRIMARY KEY CHECK (id = 1),
    installed_at             INTEGER NOT NULL,
    total_reads              INTEGER NOT NULL DEFAULT 0,
    tokens_full              INTEGER NOT NULL DEFAULT 0,
    tokens_sent              INTEGER NOT NULL DEFAULT 0,
    external_edit_refreshes  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS lifetime_per_file (
    file_path     TEXT PRIMARY KEY,
    reads         INTEGER NOT NULL DEFAULT 0,
    tokens_full   INTEGER NOT NULL DEFAULT 0,
    tokens_sent   INTEGER NOT NULL DEFAULT 0,
    last_read_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS lifetime_daily (
    day          TEXT PRIMARY KEY,  -- YYYY-MM-DD, UTC
    reads        INTEGER NOT NULL DEFAULT 0,
    tokens_full  INTEGER NOT NULL DEFAULT 0,
    tokens_sent  INTEGER NOT NULL DEFAULT 0
);

-- Edit visibility: separate from reads because edits don't send tokens
-- to the model, but the user wants to see "files I've worked with".
-- Counts every PostToolUse fire (Edit/Write/MultiEdit/NotebookEdit).
CREATE TABLE IF NOT EXISTS lifetime_edited_files (
    file_path    TEXT PRIMARY KEY,
    edits        INTEGER NOT NULL DEFAULT 0,
    last_edit_at INTEGER NOT NULL
);

-- One-shot "let the next Read pass through native" marker. Set by
-- PostToolUse:Edit, consumed by the next Read hook on the same
-- (session, file). Works around Claude Code's "File has not been
-- read yet" check rejecting DRIP's deny-as-substitute Reads after a
-- recent write.
CREATE TABLE IF NOT EXISTS passthrough_pending (
    session_id TEXT NOT NULL,
    file_path  TEXT NOT NULL,
    set_at     INTEGER NOT NULL,
    PRIMARY KEY (session_id, file_path)
);

-- Pre-computed read outcomes populated by `drip watch`. The hook
-- hits this first and skips the fs::read + sha256 + diff path when
-- the cache is fresh (mtime+size match disk AND `baseline_hash`
-- matches the current `reads.content_hash`).
CREATE TABLE IF NOT EXISTS precomputed_reads (
    session_id     TEXT NOT NULL,
    file_path      TEXT NOT NULL,
    file_mtime_ns  INTEGER NOT NULL,
    file_size      INTEGER NOT NULL,
    content_hash   TEXT NOT NULL,
    new_content    TEXT NOT NULL,
    new_tokens     INTEGER NOT NULL,
    delta_tokens   INTEGER NOT NULL,
    diff_text      TEXT,
    outcome_kind   INTEGER NOT NULL,  -- 0=unchanged, 1=delta
    baseline_hash  TEXT NOT NULL,
    computed_at    INTEGER NOT NULL,
    PRIMARY KEY (session_id, file_path)
);
CREATE INDEX IF NOT EXISTS idx_precomputed_path ON precomputed_reads(file_path);

-- Per-read event log used by `drip replay` to reconstruct exactly what
-- the agent received, in order. One row per intercepted read; rolling
-- cap (default 500/session) keeps disk bounded. `rendered` stores the
-- bytes DRIP handed back, capped per row at ~32 KB so big-file deltas
-- don't bloat the DB.
CREATE TABLE IF NOT EXISTS read_events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL,
    file_path       TEXT NOT NULL,
    occurred_at     INTEGER NOT NULL,
    outcome_kind    TEXT NOT NULL,    -- first|unchanged|delta|fallback|deleted|passthrough
    fallback_reason TEXT,
    tokens_full     INTEGER NOT NULL,
    tokens_sent     INTEGER NOT NULL,
    rendered        TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_session ON read_events(session_id, occurred_at DESC);

-- PostToolUse:Edit / Write / MultiEdit captures here. Lets the next
-- Read on the same (session, file) — when the on-disk content still
-- matches `after_hash` — issue an `[DRIP: edit verified ...]`
-- certificate instead of re-shipping the full file via passthrough.
-- One-shot per row: `cert_used = 1` after the certificate fires.
CREATE TABLE IF NOT EXISTS edit_events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL,
    file_path       TEXT NOT NULL,
    before_hash     TEXT NOT NULL,
    after_hash      TEXT NOT NULL,
    patch           TEXT NOT NULL,
    touched_ranges  TEXT NOT NULL,  -- JSON array of {start, end} (1-based, inclusive)
    touched_symbols TEXT,           -- JSON array of names extracted from hunk headers
    edited_at       INTEGER NOT NULL,
    cert_used       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_edit_events_session_file
    ON edit_events(session_id, file_path, edited_at DESC);

INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', '2');
"#;

/// Cross-session view of a file's last-known state. Drives the
/// `↔ unchanged` / `↕ changed` first-read decorations.
#[derive(Debug, Clone)]
pub struct RegistryRecord {
    pub content_hash: String,
    pub content: String,
    pub last_seen_at: i64,
    pub last_git_branch: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EditEventRow {
    pub id: i64,
    pub before_hash: String,
    pub after_hash: String,
    pub patch: String,
    pub touched_ranges: String,
    pub touched_symbols: Option<String>,
    pub edited_at: i64,
}

#[derive(Debug, Clone)]
pub struct ReadRecord {
    pub content_hash: String,
    pub content: String,
    /// `true` iff the agent received a semantic-compressed payload
    /// (bodies elided). PostToolUse:Edit reads this flag plus
    /// `elided_function_names` to warn on edits to elided bodies.
    pub was_semantic_compressed: bool,
    pub elided_function_names: Vec<String>,
    /// 1-indexed inclusive line ranges of `content` that have
    /// actually been delivered to the agent in this session. Sorted
    /// and merged. Empty vec = nothing seen yet (silent baseline).
    /// `[(1, total_lines)]` = full file delivered (Unchanged/Delta
    /// intercepts are safe). Used by `process_partial_read` to refuse
    /// substituting unseen windows, and by `process_read_inner` to
    /// fall back to FullFirst when only partial windows have been
    /// delivered.
    pub seen_ranges: Vec<(usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct Precomputed {
    pub content_hash: String,
    pub new_content: String,
    pub new_tokens: i64,
    pub delta_tokens: i64,
    pub diff_text: Option<String>,
    /// 0 = Unchanged, 1 = Delta — the only cacheable outcomes.
    pub outcome_kind: i64,
    pub baseline_hash: String,
}

pub struct Session {
    pub id: String,
    pub conn: Connection,
    /// Which derivation produced `id`.
    pub strategy: SessionStrategy,
    /// Human-readable companion to `strategy`: branch name (git),
    /// `(pid <ppid>)`, `(env)`, `(cwd)`.
    pub context: String,
    /// `true` when this id was found in `expired_sessions` at open
    /// time. The tracker reads this on the first read, emits a
    /// `session expired — fresh baseline started` decoration, and
    /// clears the flag so the notice fires once per reopened session.
    pub was_expired: std::cell::Cell<bool>,
}

impl Session {
    pub fn open() -> Result<Self> {
        Self::open_inner(None, true)
    }

    pub fn open_with_id(id: String) -> Result<Self> {
        Self::open_inner(Some(id), true)
    }

    /// Like `open()` but does NOT touch the `sessions` table. Used by
    /// read-only commands so inspecting state doesn't pollute the
    /// session listing with empty rows.
    pub fn open_readonly() -> Result<Self> {
        Self::open_inner(None, false)
    }

    /// Read-only counterpart of `open_with_id`.
    pub fn open_with_id_readonly(id: String) -> Result<Self> {
        Self::open_inner(Some(id), false)
    }

    /// Open the per-user SQLite store. `write_session_row = false`
    /// is the read-only path used by inspect-only commands.
    fn open_inner(explicit_id: Option<String>, write_session_row: bool) -> Result<Self> {
        let derivation = match explicit_id {
            Some(id) if !id.is_empty() => SessionDerivation {
                id,
                strategy: SessionStrategy::Env,
                context: "(explicit)".to_string(),
            },
            _ => derive_session(),
        };
        let SessionDerivation {
            id,
            strategy,
            context,
        } = derivation;
        let db_path = data_dir()?.join("sessions.db");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating data dir {parent:?}"))?;
            harden_dir_permissions(parent);
        }
        // Pre-create + chmod the DB file BEFORE SQLite opens it.
        // Chmod-after-open would leave a window where the file lives
        // at umask-derived 0644 and a co-tenant could open it for
        // reading. The post-open `harden_file_permissions` below is a
        // belt for paths where the pre-step partially fails.
        precreate_db_file_secure(&db_path)
            .with_context(|| format!("pre-creating sqlite at {db_path:?}"))?;
        harden_file_permissions(&db_path);
        let conn =
            Connection::open(&db_path).with_context(|| format!("opening sqlite at {db_path:?}"))?;
        harden_file_permissions(&db_path);

        // Set the busy handler FIRST so subsequent contended writes
        // get up to 5 s of retry budget under multi-process contention
        // (Read + Bash hooks racing each other's first DB open).
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // SQLite skips the busy handler for journal-mode changes
        // specifically — `busy_timeout` does NOT cover the WAL flip.
        // Without an explicit retry, a cold-start race between N
        // concurrent first-time openers loses (N-1) with
        // `database is locked`. Once flipped, the mode is persistent
        // and a SELECT short-circuits the pragma on the warm path.
        ensure_wal_mode(&conn)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        // Tolerant additive migrations. ALTER errors with "duplicate
        // column name" when the column already exists; `.ok()`
        // swallows that. Real corruption surfaces on the next query.
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN strategy TEXT", []);
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN context TEXT", []);
        let _ = conn.execute(
            "ALTER TABLE reads ADD COLUMN content_storage TEXT NOT NULL DEFAULT 'inline'",
            [],
        );
        let _ = conn.execute("ALTER TABLE sessions ADD COLUMN agent TEXT", []);
        let _ = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS file_registry (
                file_path        TEXT PRIMARY KEY,
                content_hash     TEXT NOT NULL,
                content          TEXT NOT NULL DEFAULT '',
                content_storage  TEXT NOT NULL DEFAULT 'inline',
                last_session_id  TEXT NOT NULL,
                last_seen_at     INTEGER NOT NULL,
                last_git_branch  TEXT,
                reads_count      INTEGER NOT NULL DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_registry_hash ON file_registry(content_hash);",
        );
        let _ = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS expired_sessions (
                session_id  TEXT PRIMARY KEY,
                expired_at  INTEGER NOT NULL
            );",
        );
        let _ = conn.execute(
            "ALTER TABLE reads ADD COLUMN was_semantic_compressed INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute("ALTER TABLE reads ADD COLUMN elided_functions TEXT", []);
        let _ = conn.execute("ALTER TABLE reads ADD COLUMN source_map TEXT", []);
        // Bash interception was removed; old DBs may carry the four
        // pipeline tables. Drop them so the upgrade is clean — the
        // schema is otherwise additive, but unused tables would still
        // count toward `drip doctor`'s "orphan rows" gauge and confuse
        // anyone inspecting the DB by hand.
        for tbl in [
            "pipeline_results",
            "lifetime_pipeline_stats",
            "lifetime_pipeline_daily",
            "session_pipeline_stats",
        ] {
            let _ = conn.execute(&format!("DROP TABLE IF EXISTS {tbl}"), []);
        }
        // Context-compaction ledger. The renderer decorates
        // post-compaction first reads with `↺ context was compacted
        // (#N)`; the epoch column on `reads` is for replay traces.
        let _ = conn.execute(
            "ALTER TABLE sessions ADD COLUMN context_epoch INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE sessions ADD COLUMN last_compaction_at INTEGER",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE sessions ADD COLUMN compaction_count INTEGER NOT NULL DEFAULT 0",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE reads ADD COLUMN context_epoch INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // v10: track which 1-indexed inclusive line ranges of each
        // baseline have actually been delivered to the agent. JSON
        // array of `[start, end]` pairs (sorted, merged on write).
        // `NULL` / `"[]"` ⇒ nothing delivered yet — no Unchanged or
        // Delta intercept is honest until the requested window is
        // fully covered. A full read writes `[[1, total_lines]]`;
        // partial passthroughs append the window they delivered.
        let _ = conn.execute("ALTER TABLE reads ADD COLUMN seen_ranges TEXT", []);
        // v11: persistent counter for "file changed out-of-band between
        // two reads" events. Surfaced by `drip meter` so users on
        // repos they're actively editing understand why their %
        // reduction is lower than the marketing numbers — those reads
        // ship full content by design to keep Claude's read-tracker in
        // sync with disk.
        let _ = conn.execute(
            "ALTER TABLE lifetime_stats ADD COLUMN external_edit_refreshes INTEGER NOT NULL DEFAULT 0",
            [],
        );
        // Bump persisted schema_version once the columns are in place
        // so future binaries know they don't need to re-run the ALTERs.
        let _ = conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = 'schema_version'
             AND CAST(value AS INTEGER) < ?1",
            params![SCHEMA_VERSION.to_string()],
        );
        check_or_set_schema_version(&conn)?;
        purge_stale_sessions(&conn).ok();
        // Self-heal any drift between `lifetime_stats` (the headline
        // accumulator) and `lifetime_per_file` (the per-file detail
        // table). The two are kept in lock-step by `bump_lifetime`,
        // but rare events — older buggy builds, manual sqlite3
        // intervention, an interrupted prune — can leave them out
        // of sync, which makes `drip meter`'s headline numbers and
        // its ghost-pollution percentages disagree with each other.
        // Cheap (one UPDATE on a single-row table sourced from a
        // small aggregation) and idempotent.
        resync_lifetime_stats(&conn).ok();

        if write_session_row {
            let now = unix_now();
            let cwd = std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from));
            // ON CONFLICT preserves the recorded agent so a follow-up
            // open without $DRIP_AGENT set (e.g. manual `drip sessions`)
            // can't blank it out.
            let agent = agent_from_env();
            conn.execute(
                "INSERT INTO sessions (session_id, started_at, last_active, cwd, strategy, context, agent)
                 VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(session_id) DO UPDATE SET
                    last_active = ?2,
                    strategy    = COALESCE(sessions.strategy, excluded.strategy),
                    context     = COALESCE(sessions.context,  excluded.context),
                    agent       = COALESCE(sessions.agent,    excluded.agent)",
                params![id, now, cwd, strategy.as_str(), context, agent],
            )?;
        }

        // Tombstone consumption — fires the `session expired` notice
        // exactly once per reopen.
        let was_expired = if write_session_row {
            consume_expired_tombstone(&conn, &id).unwrap_or(false)
        } else {
            false
        };

        Ok(Self {
            id,
            conn,
            strategy,
            context,
            was_expired: std::cell::Cell::new(was_expired),
        })
    }

    pub fn get_read(&self, file_path: &str) -> Result<Option<ReadRecord>> {
        type ReadRow = (String, String, String, i64, Option<String>, Option<String>);
        let row: Option<ReadRow> = self
            .conn
            .query_row(
                "SELECT content_hash, content, content_storage,
                        was_semantic_compressed, elided_functions,
                        seen_ranges
                 FROM reads
                 WHERE session_id = ?1 AND file_path = ?2",
                params![self.id, file_path],
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
            .optional()?;
        let Some((content_hash, inline, storage, compressed_flag, elided_json, seen_ranges_json)) =
            row
        else {
            return Ok(None);
        };
        let was_semantic_compressed = compressed_flag != 0;
        let seen_ranges = parse_seen_ranges(seen_ranges_json.as_deref());
        let elided_function_names = elided_json
            .as_deref()
            .and_then(|j| serde_json::from_str::<Vec<String>>(j).ok())
            .unwrap_or_default();
        let mk = |content: String| ReadRecord {
            content_hash: content_hash.clone(),
            content,
            was_semantic_compressed,
            elided_function_names: elided_function_names.clone(),
            seen_ranges: seen_ranges.clone(),
        };
        if storage.as_str() == cache::STORAGE_INLINE || storage.is_empty() {
            return Ok(Some(mk(inline)));
        }
        // Missing blob → treat as stale baseline. Caller falls back to
        // "first read" rather than crashing. `drip cache gc` removes
        // the dangling row.
        if storage.as_str() == cache::STORAGE_FILE {
            let data_dir = data_dir()?;
            match cache::read_blob(&data_dir, &content_hash)? {
                Some(content) => Ok(Some(mk(content))),
                None => {
                    eprintln!(
                        "drip: cache blob missing for {file_path} (hash {content_hash}), \
                         treating as fresh read"
                    );
                    Ok(None)
                }
            }
        } else {
            eprintln!("drip: unknown content_storage='{storage}' for {file_path}");
            Ok(None)
        }
    }

    /// JSON-decoded source map for the most recent read of `file_path`
    /// in this session. `Ok(None)` for rows without a map; decode
    /// errors bubble up so we fail loudly rather than serve a silent
    /// wrong answer.
    pub fn get_source_map(
        &self,
        file_path: &str,
    ) -> Result<Option<crate::core::compress::SourceMap>> {
        let raw: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT source_map FROM reads
                  WHERE session_id = ?1 AND file_path = ?2",
                params![self.id, file_path],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        match raw.flatten() {
            None => Ok(None),
            Some(json) => {
                let parsed: crate::core::compress::SourceMap = serde_json::from_str(&json)
                    .with_context(|| format!("source_map JSON for {file_path} is malformed"))?;
                Ok(Some(parsed))
            }
        }
    }

    pub fn upsert_read(
        &self,
        file_path: &str,
        content_hash: &str,
        content: &str,
        tokens_full: i64,
        tokens_sent: i64,
    ) -> Result<()> {
        self.upsert_read_with_compression(
            file_path,
            content_hash,
            content,
            tokens_full,
            tokens_sent,
            None,
            None,
        )
    }

    /// Records semantic-compression metadata so the post-edit hook can
    /// warn on edits to elided bodies. `compressed = None` ⇒ agent got
    /// full content.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_read_with_compression(
        &self,
        file_path: &str,
        content_hash: &str,
        content: &str,
        tokens_full: i64,
        tokens_sent: i64,
        compressed: Option<(bool, &[String])>,
        source_map: Option<&crate::core::compress::SourceMap>,
    ) -> Result<()> {
        let now = unix_now();
        // Snapshot the old blob hash before overwrite so we can
        // garbage-collect orphans if the new content differs.
        let old_hash = self.capture_blob_hash(file_path);
        let (db_content, storage) = self.materialize_content(content_hash, content)?;
        let (was_compressed, elided_json): (i64, Option<String>) = match compressed {
            Some((true, names)) if !names.is_empty() => (1, serde_json::to_string(&names).ok()),
            Some((true, _)) => (1, None),
            _ => (0, None),
        };
        let source_map_json: Option<String> = source_map
            .filter(|m| !m.is_empty())
            .and_then(|m| serde_json::to_string(m).ok());
        // Full delivery seen_ranges: for an uncompressed payload, the
        // agent has seen every line so seen_ranges = [(1, total_lines)].
        // For a semantic-compressed payload the agent only saw verbatim
        // content for non-elided regions of the original file (visible
        // signatures + non-elided bodies); elided bodies were replaced
        // by stubs. Storing the non-elided original ranges here means a
        // downstream partial Read on an elided region correctly fails
        // the seen_ranges_cover check and passes through to native,
        // instead of falsely claiming Unchanged for content the agent
        // never received. Full re-reads stay on the Delta path because
        // process_read_inner bypasses the seen_ranges guard when
        // was_semantic_compressed is set.
        let total_lines = content.lines().count().max(1);
        let seen_ranges_vec: Vec<(usize, usize)> = match (was_compressed, source_map) {
            (1, Some(map)) if !map.is_empty() => {
                let visible: Vec<(usize, usize)> = map
                    .iter()
                    .filter(|e| !e.elided)
                    .map(|e| (e.original_start, e.original_end))
                    .collect();
                merge_seen_ranges(visible)
            }
            _ => vec![(1, total_lines)],
        };
        let seen_ranges_json = serialize_seen_ranges(&seen_ranges_vec);
        self.conn.execute(
            "INSERT INTO reads
                (session_id, file_path, content_hash, content, read_at,
                 reads_count, tokens_full, tokens_sent, content_storage,
                 was_semantic_compressed, elided_functions, source_map,
                 context_epoch, seen_ranges)
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?8, ?9, ?10, ?11,
                 COALESCE((SELECT context_epoch FROM sessions WHERE session_id = ?1), 0),
                 ?12)
             ON CONFLICT(session_id, file_path) DO UPDATE SET
                content_hash             = excluded.content_hash,
                content                  = excluded.content,
                read_at                  = excluded.read_at,
                reads_count              = reads.reads_count + 1,
                tokens_full              = reads.tokens_full + excluded.tokens_full,
                tokens_sent              = reads.tokens_sent + excluded.tokens_sent,
                content_storage          = excluded.content_storage,
                was_semantic_compressed  = excluded.was_semantic_compressed,
                elided_functions         = excluded.elided_functions,
                source_map               = excluded.source_map,
                context_epoch            = excluded.context_epoch,
                seen_ranges              = excluded.seen_ranges",
            params![
                self.id,
                file_path,
                content_hash,
                db_content,
                now,
                tokens_full,
                tokens_sent,
                storage,
                was_compressed,
                elided_json,
                source_map_json,
                seen_ranges_json,
            ],
        )?;
        self.bump_lifetime(file_path, tokens_full, tokens_sent)?;
        let _ = self.upsert_registry(file_path, content_hash, content);
        let _ = self.invalidate_precomputed(file_path);
        if let Some(prev) = old_hash {
            if prev != content_hash {
                self.maybe_drop_blobs(&[prev]);
            }
        }
        self.touch()?;
        Ok(())
    }

    /// Upsert into `file_registry` so the next session can detect
    /// unchanged/changed status. Same hybrid storage as `reads`.
    /// Honors `DRIP_REGISTRY_DISABLE=1`.
    pub fn upsert_registry(
        &self,
        file_path: &str,
        content_hash: &str,
        content: &str,
    ) -> Result<()> {
        if registry_disabled() {
            return Ok(());
        }
        let (db_content, storage) = self.materialize_content(content_hash, content)?;
        let branch = session_git_branch(self);
        self.conn.execute(
            "INSERT INTO file_registry
                (file_path, content_hash, content, content_storage,
                 last_session_id, last_seen_at, last_git_branch, reads_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)
             ON CONFLICT(file_path) DO UPDATE SET
                content_hash    = excluded.content_hash,
                content         = excluded.content,
                content_storage = excluded.content_storage,
                last_session_id = excluded.last_session_id,
                last_seen_at    = excluded.last_seen_at,
                last_git_branch = excluded.last_git_branch,
                reads_count     = file_registry.reads_count + 1",
            params![
                file_path,
                content_hash,
                db_content,
                storage,
                self.id,
                unix_now(),
                branch,
            ],
        )?;
        Ok(())
    }

    /// Most recently registered state of `file_path`, or `None` on
    /// first-ever encounter.
    pub fn get_registry(&self, file_path: &str) -> Result<Option<RegistryRecord>> {
        if registry_disabled() {
            return Ok(None);
        }
        let row: Option<(String, String, String, i64, Option<String>)> = self
            .conn
            .query_row(
                "SELECT content_hash, content, content_storage,
                        last_seen_at, last_git_branch
                 FROM file_registry
                 WHERE file_path = ?1",
                params![file_path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((hash, inline_content, storage, last_seen_at, branch)) = row else {
            return Ok(None);
        };
        let content = if storage == cache::STORAGE_FILE {
            let data_dir = data_dir()?;
            cache::read_blob(&data_dir, &hash)?.unwrap_or_default()
        } else {
            inline_content
        };
        Ok(Some(RegistryRecord {
            content_hash: hash,
            content,
            last_seen_at,
            last_git_branch: branch,
        }))
    }

    /// Choose between inline storage (return `content` to be written
    /// to the row) and file-cache storage (write the blob, return an
    /// empty string for the `content` column). The choice is purely a
    /// function of payload size and `DRIP_INLINE_MAX_BYTES`; the
    /// hash-addressed cache deduplicates automatically when two files
    /// or two sessions share content.
    fn materialize_content<'a>(
        &self,
        content_hash: &str,
        content: &'a str,
    ) -> Result<(&'a str, &'static str)> {
        let storage = cache::pick_storage(content.len());
        if storage == cache::STORAGE_INLINE {
            return Ok((content, cache::STORAGE_INLINE));
        }
        let data_dir = data_dir()?;
        cache::write_blob(&data_dir, content_hash, content.as_bytes())?;
        Ok(("", cache::STORAGE_FILE))
    }

    /// Increment cumulative-since-install counters. Called from every
    /// model-driven read; NOT from `set_baseline` (a refresh, not a read).
    fn bump_lifetime(&self, file_path: &str, tokens_full: i64, tokens_sent: i64) -> Result<()> {
        let now = unix_now();
        self.conn.execute(
            "INSERT INTO lifetime_stats
                (id, installed_at, total_reads, tokens_full, tokens_sent)
             VALUES (1, ?1, 1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                total_reads = total_reads + 1,
                tokens_full = tokens_full + excluded.tokens_full,
                tokens_sent = tokens_sent + excluded.tokens_sent",
            params![now, tokens_full, tokens_sent],
        )?;
        self.conn.execute(
            "INSERT INTO lifetime_per_file
                (file_path, reads, tokens_full, tokens_sent, last_read_at)
             VALUES (?1, 1, ?2, ?3, ?4)
             ON CONFLICT(file_path) DO UPDATE SET
                reads        = reads + 1,
                tokens_full  = tokens_full + excluded.tokens_full,
                tokens_sent  = tokens_sent + excluded.tokens_sent,
                last_read_at = excluded.last_read_at",
            params![file_path, tokens_full, tokens_sent, now],
        )?;
        self.conn.execute(
            "INSERT INTO lifetime_daily (day, reads, tokens_full, tokens_sent)
             VALUES (date(?1, 'unixepoch'), 1, ?2, ?3)
             ON CONFLICT(day) DO UPDATE SET
                reads = reads + 1,
                tokens_full = tokens_full + excluded.tokens_full,
                tokens_sent = tokens_sent + excluded.tokens_sent",
            params![now, tokens_full, tokens_sent],
        )?;
        Ok(())
    }

    /// Refresh the cached baseline for a file without counting it as a
    /// read by the model. Used by PostToolUse hooks after an Edit/Write so
    /// the next genuine Read returns "unchanged".
    pub fn set_baseline(&self, file_path: &str, content_hash: &str, content: &str) -> Result<()> {
        let now = unix_now();
        // Snapshot before overwrite — if the hash changes, the old
        // blob becomes an orphan once the UPDATE lands.
        let old_hash = self.capture_blob_hash(file_path);
        let (db_content, storage) = self.materialize_content(content_hash, content)?;
        self.conn.execute(
            "INSERT INTO reads
                (session_id, file_path, content_hash, content, read_at,
                 reads_count, tokens_full, tokens_sent, content_storage,
                 context_epoch)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 0, ?6,
                 COALESCE((SELECT context_epoch FROM sessions WHERE session_id = ?1), 0))
             ON CONFLICT(session_id, file_path) DO UPDATE SET
                content_hash    = excluded.content_hash,
                content         = excluded.content,
                read_at         = excluded.read_at,
                content_storage = excluded.content_storage,
                context_epoch   = excluded.context_epoch",
            params![self.id, file_path, content_hash, db_content, now, storage],
        )?;
        // Errors swallowed — registry hiccup can't break a refresh.
        let _ = self.upsert_registry(file_path, content_hash, content);
        let _ = self.invalidate_precomputed(file_path);
        if let Some(prev) = old_hash {
            if prev != content_hash {
                self.maybe_drop_blobs(&[prev]);
            }
        }
        self.touch()?;
        Ok(())
    }

    /// Reset `seen_ranges` to NULL — drop every per-window coverage
    /// claim. Called after an external file change is detected so
    /// the prior coverage (computed against the *old* baseline) can't
    /// leak into intercept decisions made against the new content.
    pub fn reset_seen_ranges(&self, file_path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE reads SET seen_ranges = NULL
             WHERE session_id = ?1 AND file_path = ?2",
            params![self.id, file_path],
        )?;
        Ok(())
    }

    /// Mark the agent as having received `total_lines` of full content
    /// for `file_path`. Used by the post-edit Passthrough handler:
    /// `set_baseline` preserves `seen_ranges` on conflict (because
    /// Edit alone doesn't deliver content), but the next Read after
    /// Passthrough lets Claude Code's native Read run and deliver the
    /// whole file — at that point seen_ranges should reflect full
    /// coverage.
    pub fn mark_full_seen(&self, file_path: &str, total_lines: usize) -> Result<()> {
        let json = serialize_seen_ranges(&[(1, total_lines.max(1))]);
        self.conn.execute(
            "UPDATE reads SET seen_ranges = ?3
             WHERE session_id = ?1 AND file_path = ?2",
            params![self.id, file_path, json],
        )?;
        Ok(())
    }

    /// Append a delivered window `[start, end]` (1-indexed inclusive)
    /// to `seen_ranges`. Read-modify-write under the connection's
    /// implicit transaction; concurrent hooks have separate
    /// connections so the worst case is a write-write race that
    /// loses a single range append (the lost window will be re-added
    /// the next time the same window passes through, so the bug is
    /// self-healing).
    pub fn append_seen_range(&self, file_path: &str, start: usize, end: usize) -> Result<()> {
        if start == 0 || end < start {
            return Ok(());
        }
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT seen_ranges FROM reads
                 WHERE session_id = ?1 AND file_path = ?2",
                params![self.id, file_path],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let mut ranges = parse_seen_ranges(raw.as_deref());
        ranges.push((start, end));
        let merged = merge_seen_ranges(ranges);
        let json = serialize_seen_ranges(&merged);
        self.conn.execute(
            "UPDATE reads SET seen_ranges = ?3
             WHERE session_id = ?1 AND file_path = ?2",
            params![self.id, file_path, json],
        )?;
        Ok(())
    }

    /// Top up `tokens_sent` on an existing reads row by `extra_sent`
    /// without bumping `reads_count`. Used by `render_and_record` to
    /// account for the DRIP header that wraps the substantive body —
    /// `process_read_inner` only sees the body bytes when bumping
    /// lifetime stats; the header is added at render time and would
    /// otherwise be invisible to `drip meter`. Updates the per-session
    /// `reads` row and all three install-wide aggregates so the
    /// meter's headline number, per-file detail, and daily history
    /// stay in lockstep.
    pub fn bump_lifetime_overhead(&self, file_path: &str, extra_sent: i64) -> Result<()> {
        if extra_sent <= 0 {
            return Ok(());
        }
        let now = unix_now();
        self.conn.execute(
            "UPDATE lifetime_stats SET tokens_sent = tokens_sent + ?1 WHERE id = 1",
            params![extra_sent],
        )?;
        self.conn.execute(
            "UPDATE lifetime_per_file SET tokens_sent = tokens_sent + ?2
             WHERE file_path = ?1",
            params![file_path, extra_sent],
        )?;
        self.conn.execute(
            "UPDATE lifetime_daily SET tokens_sent = tokens_sent + ?2
             WHERE day = date(?1, 'unixepoch')",
            params![now, extra_sent],
        )?;
        self.conn.execute(
            "UPDATE reads SET tokens_sent = tokens_sent + ?3
             WHERE session_id = ?1 AND file_path = ?2",
            params![self.id, file_path, extra_sent],
        )?;
        Ok(())
    }

    /// Increment the "file changed out-of-band between two reads"
    /// counter. Called from the ExternalChange path in tracker so
    /// `drip meter` can surface "X of your reads were full-content
    /// refreshes" — the by-design behavior that explains why
    /// reduction is lower on repos the agent edits while it reads.
    pub fn bump_external_edit_refresh(&self) -> Result<()> {
        // INSERT-OR-UPDATE in one statement so a fresh DB without a
        // lifetime_stats row gets the counter as the row's only
        // non-zero value rather than failing the UPDATE silently. The
        // bare INSERT carries `installed_at` so the row stays valid
        // even when bump_external_edit_refresh races bump_lifetime
        // for the first read.
        self.conn.execute(
            "INSERT INTO lifetime_stats
                (id, installed_at, external_edit_refreshes)
             VALUES (1, ?1, 1)
             ON CONFLICT(id) DO UPDATE SET
                external_edit_refreshes = external_edit_refreshes + 1",
            params![unix_now()],
        )?;
        Ok(())
    }

    pub fn record_unchanged(&self, file_path: &str, tokens_full: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE reads
             SET read_at     = ?3,
                 reads_count = reads_count + 1,
                 tokens_full = tokens_full + ?4
             WHERE session_id = ?1 AND file_path = ?2",
            params![self.id, file_path, unix_now(), tokens_full],
        )?;
        self.bump_lifetime(file_path, tokens_full, 0)?;
        self.touch()?;
        Ok(())
    }

    /// Bookkeeping for post-edit responses (cert / passthrough) that
    /// don't touch the stored baseline. Bumps `reads_count` so
    /// `drip meter --session` counts every read the agent issued.
    pub fn record_post_edit_response(
        &self,
        file_path: &str,
        tokens_full: i64,
        tokens_sent: i64,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE reads
             SET read_at     = ?3,
                 reads_count = reads_count + 1,
                 tokens_full = tokens_full + ?4,
                 tokens_sent = tokens_sent + ?5
             WHERE session_id = ?1 AND file_path = ?2",
            params![self.id, file_path, unix_now(), tokens_full, tokens_sent],
        )?;
        if n > 0 {
            self.bump_lifetime(file_path, tokens_full, tokens_sent)?;
            self.touch()?;
        }
        Ok(())
    }

    /// Bookkeeping for a partial-read intercept (`Read` with
    /// `offset`/`limit`). Bumps counters; intentionally does NOT touch
    /// `content_hash` / `content` — the baseline stays pinned to the
    /// full file so the next full read still diffs correctly.
    pub fn record_partial_read(
        &self,
        file_path: &str,
        tokens_full: i64,
        tokens_sent: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE reads
             SET read_at     = ?3,
                 reads_count = reads_count + 1,
                 tokens_full = tokens_full + ?4,
                 tokens_sent = tokens_sent + ?5
             WHERE session_id = ?1 AND file_path = ?2",
            params![self.id, file_path, unix_now(), tokens_full, tokens_sent],
        )?;
        self.bump_lifetime(file_path, tokens_full, tokens_sent)?;
        self.touch()?;
        Ok(())
    }

    /// Record that a model write tool (Edit/Write/MultiEdit/NotebookEdit)
    /// just touched this file. Used by `drip meter` to show
    /// "files edited" / "total edits" alongside read stats.
    pub fn bump_edit(&self, file_path: &str) -> Result<()> {
        let now = unix_now();
        self.conn.execute(
            "INSERT INTO lifetime_edited_files (file_path, edits, last_edit_at)
             VALUES (?1, 1, ?2)
             ON CONFLICT(file_path) DO UPDATE SET
                edits        = edits + 1,
                last_edit_at = excluded.last_edit_at",
            params![file_path, now],
        )?;
        Ok(())
    }

    /// Mark (session, file) so the next Read passes through native
    /// instead of being substituted. Consumed by `take_passthrough`.
    pub fn mark_passthrough(&self, file_path: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO passthrough_pending (session_id, file_path, set_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id, file_path) DO UPDATE SET set_at = excluded.set_at",
            params![self.id, file_path, unix_now()],
        )?;
        Ok(())
    }

    /// Atomic test-and-clear. One-shot: a second Read with no
    /// intervening edit goes through normal DRIP interception.
    pub fn take_passthrough(&self, file_path: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM passthrough_pending
             WHERE session_id = ?1 AND file_path = ?2",
            params![self.id, file_path],
        )?;
        Ok(n > 0)
    }

    pub fn record_edit_event(
        &self,
        file_path: &str,
        before_hash: &str,
        after_hash: &str,
        patch: &str,
        touched_ranges_json: &str,
        touched_symbols_json: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO edit_events
                 (session_id, file_path, before_hash, after_hash, patch,
                  touched_ranges, touched_symbols, edited_at, cert_used)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)",
            params![
                self.id,
                file_path,
                before_hash,
                after_hash,
                patch,
                touched_ranges_json,
                touched_symbols_json,
                unix_now(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Find a recent un-issued edit event for `(session, file)` whose
    /// `after_hash` matches the live file content. Returns `None`
    /// outside the cert window or when no candidate exists.
    pub fn find_edit_cert_candidate(
        &self,
        file_path: &str,
        current_hash: &str,
        window_secs: i64,
    ) -> Result<Option<EditEventRow>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, before_hash, after_hash, patch,
                        touched_ranges, touched_symbols, edited_at
                 FROM edit_events
                 WHERE session_id = ?1
                   AND file_path = ?2
                   AND after_hash = ?3
                   AND cert_used = 0
                   AND edited_at >= ?4
                 ORDER BY edited_at DESC
                 LIMIT 1",
                params![self.id, file_path, current_hash, unix_now() - window_secs,],
                |row| {
                    Ok(EditEventRow {
                        id: row.get(0)?,
                        before_hash: row.get(1)?,
                        after_hash: row.get(2)?,
                        patch: row.get(3)?,
                        touched_ranges: row.get(4)?,
                        touched_symbols: row.get::<_, Option<String>>(5)?,
                        edited_at: row.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Flip `cert_used = 1` so the same edit event can't issue a second
    /// certificate. Called once the read hook commits the certificate
    /// to the agent.
    pub fn mark_edit_cert_used(&self, event_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE edit_events SET cert_used = 1 WHERE id = ?1",
            params![event_id],
        )?;
        Ok(())
    }

    /// Look up a fresh precomputed entry for this (session, file).
    /// Returns `Some` only when (mtime_ns, size) matches the disk state
    /// — i.e., nothing has changed since `drip watch` last ran the diff.
    pub fn get_precomputed(
        &self,
        file_path: &str,
        mtime_ns: i64,
        size: i64,
    ) -> Result<Option<Precomputed>> {
        let row = self
            .conn
            .query_row(
                "SELECT content_hash, new_content, new_tokens, delta_tokens,
                    diff_text, outcome_kind, baseline_hash
             FROM precomputed_reads
             WHERE session_id = ?1 AND file_path = ?2
                AND file_mtime_ns = ?3 AND file_size = ?4",
                params![self.id, file_path, mtime_ns, size],
                |r| {
                    Ok(Precomputed {
                        content_hash: r.get(0)?,
                        new_content: r.get(1)?,
                        new_tokens: r.get(2)?,
                        delta_tokens: r.get(3)?,
                        diff_text: r.get(4)?,
                        outcome_kind: r.get(5)?,
                        baseline_hash: r.get(6)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Drop every precomputed row for this (session, file). Called when
    /// the baseline changes (post-edit, refresh) so a stale diff can't be
    /// served against the new baseline.
    pub fn invalidate_precomputed(&self, file_path: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM precomputed_reads
             WHERE session_id = ?1 AND file_path = ?2",
            params![self.id, file_path],
        )?;
        Ok(())
    }

    /// Append one event to `read_events` and trim the per-session log to
    /// `DRIP_REPLAY_KEEP` rows (default 500). Events are skipped entirely
    /// when `DRIP_REPLAY_LOG=0` so the user can opt out if the extra
    /// SQLite write ever shows up in profiling.
    pub fn record_event(
        &self,
        file_path: &str,
        outcome_kind: &str,
        fallback_reason: Option<&str>,
        tokens_full: i64,
        tokens_sent: i64,
        rendered: &str,
    ) -> Result<()> {
        if std::env::var("DRIP_REPLAY_LOG").as_deref() == Ok("0") {
            return Ok(());
        }
        const MAX_RENDER_BYTES: usize = 32 * 1024;
        let stored = if rendered.len() > MAX_RENDER_BYTES {
            // Walk back to a UTF-8 boundary (`floor_char_boundary` is unstable).
            let mut cut = MAX_RENDER_BYTES;
            while cut > 0 && !rendered.is_char_boundary(cut) {
                cut -= 1;
            }
            format!(
                "{}\n[…truncated, {} more bytes]",
                &rendered[..cut],
                rendered.len() - cut
            )
        } else {
            rendered.to_string()
        };
        self.conn.execute(
            "INSERT INTO read_events
                (session_id, file_path, occurred_at, outcome_kind,
                 fallback_reason, tokens_full, tokens_sent, rendered)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                self.id,
                file_path,
                unix_now(),
                outcome_kind,
                fallback_reason,
                tokens_full,
                tokens_sent,
                stored,
            ],
        )?;
        let keep: i64 = std::env::var("DRIP_REPLAY_KEEP")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|v: &i64| *v > 0)
            .unwrap_or(500);
        self.conn.execute(
            "DELETE FROM read_events
             WHERE session_id = ?1
               AND id NOT IN (
                  SELECT id FROM read_events
                  WHERE session_id = ?1
                  ORDER BY id DESC
                  LIMIT ?2
               )",
            params![self.id, keep],
        )?;
        Ok(())
    }

    pub fn delete_read(&self, file_path: &str) -> Result<()> {
        let old_hash = self.capture_blob_hash(file_path);
        self.conn.execute(
            "DELETE FROM reads WHERE session_id = ?1 AND file_path = ?2",
            params![self.id, file_path],
        )?;
        let _ = self.invalidate_precomputed(file_path);
        // Drop pending passthrough so `drip refresh` returns a real
        // full read instead of falling through.
        self.conn.execute(
            "DELETE FROM passthrough_pending
             WHERE session_id = ?1 AND file_path = ?2",
            params![self.id, file_path],
        )?;
        if let Some(prev) = old_hash {
            self.maybe_drop_blobs(&[prev]);
        }
        Ok(())
    }

    /// Cross-session variant of `delete_read`. An OOB edit (manual
    /// change, `git pull`, …) invalidates every session's baseline
    /// for the affected file, so `drip refresh` drops them all in
    /// one shot — otherwise the user typing the command in their
    /// shell session would silently miss baselines living in the
    /// agent's session and the next agent read would diff against
    /// a stale snapshot.
    ///
    /// Returns the count of sessions that held a baseline so the CLI
    /// can report what was actually cleared.
    pub fn delete_read_all_sessions(&self, file_path: &str) -> Result<usize> {
        // Snapshot every blob hash before deletion so unreferenced
        // blobs get GC'd. We collect across sessions because two
        // sessions could point at the same hash (dedup) and the GC
        // walks `reads.content_hash` post-delete to decide.
        let mut hashes: Vec<String> = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT content_hash FROM reads
                 WHERE file_path = ?1
                   AND content_storage = 'file'
                   AND content_hash IS NOT NULL
                   AND content_hash != ''",
            )?;
            let rows = stmt.query_map(params![file_path], |r| r.get::<_, String>(0))?;
            for r in rows.flatten() {
                hashes.push(r);
            }
        }

        let affected = self
            .conn
            .execute("DELETE FROM reads WHERE file_path = ?1", params![file_path])?;

        // Precomputed cache + pending passthrough are session-keyed
        // tables too — drop every row touching this file path so the
        // next read in any session returns full content.
        let _ = self.conn.execute(
            "DELETE FROM precomputed_reads WHERE file_path = ?1",
            params![file_path],
        );
        let _ = self.conn.execute(
            "DELETE FROM passthrough_pending WHERE file_path = ?1",
            params![file_path],
        );

        self.maybe_drop_blobs(&hashes);
        Ok(affected)
    }

    /// Seconds until this session would expire if idle. `None` when
    /// the row is missing or the TTL has elapsed. Drives the
    /// `session expires in N min` warning in the last 10% of the TTL.
    pub fn seconds_until_expiry(&self) -> Option<i64> {
        let last_active: i64 = self
            .conn
            .query_row(
                "SELECT last_active FROM sessions WHERE session_id = ?1",
                params![self.id],
                |r| r.get(0),
            )
            .ok()?;
        let ttl = session_ttl_secs();
        let elapsed = unix_now() - last_active;
        let remaining = ttl - elapsed;
        if remaining > 0 {
            Some(remaining)
        } else {
            None
        }
    }

    pub fn reset(&self) -> Result<()> {
        // Snapshot file-cache hashes BEFORE the DELETE so we can GC
        // the blobs the reads pointed at — without this, every reset
        // leaks the session's blobs as cache-dir orphans.
        let doomed_hashes: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT content_hash FROM reads
                 WHERE session_id = ?1
                   AND content_storage = 'file'
                   AND content_hash IS NOT NULL
                   AND content_hash != ''",
            )?;
            let rows = stmt
                .query_map(params![self.id], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect::<Vec<String>>();
            rows
        };

        // Purge every session-keyed table. Lifetime tables are
        // install-wide and untouched.
        let tables = [
            "reads",
            "read_events",
            "precomputed_reads",
            "passthrough_pending",
        ];
        for t in tables {
            self.conn.execute(
                &format!("DELETE FROM {t} WHERE session_id = ?1"),
                params![self.id],
            )?;
        }
        // Tombstone for the next reopen.
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO expired_sessions (session_id, expired_at) VALUES (?1, ?2)",
            params![self.id, unix_now()],
        );
        self.conn.execute(
            "DELETE FROM sessions WHERE session_id = ?1",
            params![self.id],
        )?;

        // `delete_blobs_if_unreferenced` re-checks both `reads` and
        // `file_registry` so a registry-only reference keeps the blob.
        self.maybe_drop_blobs(&doomed_hashes);
        Ok(())
    }

    /// Zero every cumulative-since-install counter without touching
    /// active sessions. Backs `drip reset --stats`.
    pub fn reset_lifetime_stats(&self) -> Result<LifetimeResetReport> {
        let report = LifetimeResetReport {
            stats_rows: self.conn.execute("DELETE FROM lifetime_stats", [])? as i64,
            per_file_rows: self.conn.execute("DELETE FROM lifetime_per_file", [])? as i64,
            daily_rows: self.conn.execute("DELETE FROM lifetime_daily", [])? as i64,
            edited_rows: self.conn.execute("DELETE FROM lifetime_edited_files", [])? as i64,
        };
        record_reset_marker(&self.conn).ok();
        Ok(report)
    }

    /// `content_hash` of (this session, `file_path`) iff the row is
    /// in file storage. Snapshotted before every overwrite so
    /// `maybe_drop_blobs` can GC the freshly-orphaned blob.
    fn capture_blob_hash(&self, file_path: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT content_hash FROM reads
                 WHERE session_id = ?1
                   AND file_path  = ?2
                   AND content_storage = 'file'
                   AND content_hash IS NOT NULL
                   AND content_hash != ''",
                params![self.id, file_path],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }

    /// Best-effort GC of unreferenced cache blobs. Swallows IO errors
    /// — orphans get caught by `drip cache gc`.
    fn maybe_drop_blobs(&self, hashes: &[String]) {
        if hashes.is_empty() {
            return;
        }
        if let Ok(dir) = data_dir() {
            let _ = cache::delete_blobs_if_unreferenced(&self.conn, &dir, hashes);
        }
    }

    pub fn touch(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_active = ?2 WHERE session_id = ?1",
            params![self.id, unix_now()],
        )?;
        Ok(())
    }

    pub fn started_at(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row(
                "SELECT started_at FROM sessions WHERE session_id = ?1",
                params![self.id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(unix_now))
    }

    /// Compaction ledger (epoch + last_at + count). `None` when the
    /// row doesn't exist; callers treat that as "no compactions".
    pub fn compaction_state(&self) -> Result<Option<CompactionState>> {
        let row: Option<(i64, Option<i64>, i64)> = self
            .conn
            .query_row(
                "SELECT context_epoch, last_compaction_at, compaction_count
                 FROM sessions WHERE session_id = ?1",
                params![self.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(row.map(|(epoch, last_at, count)| CompactionState {
            epoch,
            last_compaction_at: last_at,
            count,
        }))
    }

    /// Compaction-aware reset. Bumps the ledger, wipes per-session
    /// state, but PRESERVES the `sessions` row so the counters
    /// survive. The renderer surfaces `↺ context was compacted (#N)`
    /// on the next first read of each file. Called from
    /// `SessionStart:compact|clear|resume`.
    pub fn reset_for_compaction(&self) -> Result<()> {
        // Bump the ledger FIRST. A racing read sees the new epoch with
        // old baselines — harmless: the baselines still hash-match the
        // disk content (or don't), so the worst case is the next
        // read sees an unchanged sentinel from a "pre-compaction"
        // baseline that happens to still be correct.
        let now = unix_now();
        let n = self.conn.execute(
            "UPDATE sessions
             SET context_epoch      = context_epoch + 1,
                 last_compaction_at = ?2,
                 compaction_count   = compaction_count + 1
             WHERE session_id = ?1",
            params![self.id, now],
        )?;
        if n == 0 {
            // No row to bump — open_inner skipped write_session_row,
            // or the agent compacted before issuing any read. Insert a
            // minimal row so the counters persist.
            let cwd = std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from));
            self.conn.execute(
                "INSERT INTO sessions
                    (session_id, started_at, last_active, cwd, strategy, context, agent,
                     context_epoch, last_compaction_at, compaction_count)
                 VALUES (?1, ?2, ?2, ?3, ?4, ?5, NULL, 1, ?2, 1)
                 ON CONFLICT(session_id) DO UPDATE SET
                    context_epoch      = sessions.context_epoch + 1,
                    last_compaction_at = ?2,
                    compaction_count   = sessions.compaction_count + 1",
                params![self.id, now, cwd, self.strategy.as_str(), self.context,],
            )?;
        }

        // Mirror reset()'s blob-orphan dance.
        let doomed_hashes: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT content_hash FROM reads
                 WHERE session_id = ?1
                   AND content_storage = 'file'
                   AND content_hash IS NOT NULL
                   AND content_hash != ''",
            )?;
            let rows = stmt
                .query_map(params![self.id], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect::<Vec<String>>();
            rows
        };
        let tables = [
            "reads",
            "read_events",
            "precomputed_reads",
            "passthrough_pending",
        ];
        for t in tables {
            self.conn.execute(
                &format!("DELETE FROM {t} WHERE session_id = ?1"),
                params![self.id],
            )?;
        }
        // No DELETE on `sessions` (the diff vs reset()) and no
        // tombstone — the ↺ notice comes from `compaction_count`.
        self.maybe_drop_blobs(&doomed_hashes);
        Ok(())
    }
}

/// Snapshot of the compaction ledger; carried through to the renderer.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct CompactionState {
    pub epoch: i64,
    pub last_compaction_at: Option<i64>,
    pub count: i64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ReadEvent {
    pub id: i64,
    pub session_id: String,
    pub file_path: String,
    pub occurred_at: i64,
    pub outcome_kind: String,
    pub fallback_reason: Option<String>,
    pub tokens_full: i64,
    pub tokens_sent: i64,
    pub rendered: String,
}

/// Recent events for a session, newest first. `since_ts` is a unix
/// timestamp lower bound; `file_substr` is a case-sensitive substring
/// match.
pub fn recent_events(
    conn: &Connection,
    session_id: &str,
    limit: i64,
    since_ts: Option<i64>,
    file_substr: Option<&str>,
) -> Result<Vec<ReadEvent>> {
    let pattern = file_substr.map(|s| format!("%{s}%"));
    let mut stmt = conn.prepare(
        "SELECT id, session_id, file_path, occurred_at, outcome_kind,
                fallback_reason, tokens_full, tokens_sent, rendered
         FROM read_events
         WHERE session_id = ?1
           AND (?2 IS NULL OR occurred_at >= ?2)
           AND (?3 IS NULL OR file_path LIKE ?3)
         ORDER BY id DESC
         LIMIT ?4",
    )?;
    let rows = stmt
        .query_map(params![session_id, since_ts, pattern, limit], |r| {
            Ok(ReadEvent {
                id: r.get(0)?,
                session_id: r.get(1)?,
                file_path: r.get(2)?,
                occurred_at: r.get(3)?,
                outcome_kind: r.get(4)?,
                fallback_reason: r.get(5)?,
                tokens_full: r.get(6)?,
                tokens_sent: r.get(7)?,
                rendered: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Write a precomputed entry on a borrowed connection (used by `drip
/// watch`).
#[allow(clippy::too_many_arguments)]
pub fn set_precomputed_on(
    conn: &Connection,
    session_id: &str,
    file_path: &str,
    mtime_ns: i64,
    size: i64,
    content_hash: &str,
    new_content: &str,
    new_tokens: i64,
    delta_tokens: i64,
    diff_text: Option<&str>,
    outcome_kind: i64,
    baseline_hash: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO precomputed_reads
            (session_id, file_path, file_mtime_ns, file_size,
             content_hash, new_content, new_tokens, delta_tokens,
             diff_text, outcome_kind, baseline_hash, computed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(session_id, file_path) DO UPDATE SET
            file_mtime_ns = excluded.file_mtime_ns,
            file_size     = excluded.file_size,
            content_hash  = excluded.content_hash,
            new_content   = excluded.new_content,
            new_tokens    = excluded.new_tokens,
            delta_tokens  = excluded.delta_tokens,
            diff_text     = excluded.diff_text,
            outcome_kind  = excluded.outcome_kind,
            baseline_hash = excluded.baseline_hash,
            computed_at   = excluded.computed_at",
        params![
            session_id,
            file_path,
            mtime_ns,
            size,
            content_hash,
            new_content,
            new_tokens,
            delta_tokens,
            diff_text,
            outcome_kind,
            baseline_hash,
            unix_now(),
        ],
    )?;
    Ok(())
}

/// One row per session with a baseline for `file_path`.
#[derive(Debug, Clone)]
pub struct BaselineRow {
    pub session_id: String,
    pub file_path: String,
    pub content_hash: String,
    pub content: String,
}

/// All baselines under `root` (canonical-prefix match). Used by the
/// watcher's initial and periodic scans.
pub fn baselines_under(conn: &Connection, root: &str) -> Result<Vec<BaselineRow>> {
    // Exact match too, in case `drip watch` was pointed at one file.
    let pattern = if root.ends_with('/') {
        format!("{root}%")
    } else {
        format!("{root}/%")
    };
    let mut stmt = conn.prepare(
        "SELECT session_id, file_path, content_hash, content
         FROM reads
         WHERE file_path = ?1 OR file_path LIKE ?2",
    )?;
    let rows = stmt
        .query_map(params![root, pattern], |r| {
            Ok(BaselineRow {
                session_id: r.get(0)?,
                file_path: r.get(1)?,
                content_hash: r.get(2)?,
                content: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One row per session with a baseline for `file_path`.
pub fn baselines_for_file(conn: &Connection, file_path: &str) -> Result<Vec<BaselineRow>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, file_path, content_hash, content
         FROM reads
         WHERE file_path = ?1",
    )?;
    let rows = stmt
        .query_map(params![file_path], |r| {
            Ok(BaselineRow {
                session_id: r.get(0)?,
                file_path: r.get(1)?,
                content_hash: r.get(2)?,
                content: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Per-table delete counts from `reset_lifetime_stats`.
#[derive(Debug, Clone, Default)]
pub struct LifetimeResetReport {
    pub stats_rows: i64,
    pub per_file_rows: i64,
    pub daily_rows: i64,
    pub edited_rows: i64,
}

/// Per-bucket counts from `reset_all_data` — lets the user spot a
/// run against the wrong `DRIP_DATA_DIR` (`0 sessions, 0 reads …`).
#[derive(Debug, Clone, Default)]
pub struct ResetAllReport {
    pub sessions: i64,
    pub reads: i64,
    pub registry: i64,
    pub cache_blobs: i64,
    pub cache_bytes: u64,
    pub lifetime_rows: i64,
}

/// Wipe every row + every blob under `cache/`. Does NOT delete
/// `sessions.db` itself — the next `Session::open` rebuilds the
/// schema. Backs `drip reset --all`.
pub fn reset_all_data() -> Result<ResetAllReport> {
    let data = data_dir()?;
    let db_path = data.join("sessions.db");
    let mut report = ResetAllReport::default();

    if db_path.exists() {
        let conn = Connection::open(&db_path)?;
        report.sessions = count_rows(&conn, "sessions")?;
        report.reads = count_rows(&conn, "reads")?;
        report.registry = count_rows(&conn, "file_registry")?;
        report.lifetime_rows = count_rows(&conn, "lifetime_stats")?
            + count_rows(&conn, "lifetime_per_file")?
            + count_rows(&conn, "lifetime_daily")?
            + count_rows(&conn, "lifetime_edited_files")?;
        for tbl in [
            "reads",
            "read_events",
            "precomputed_reads",
            "passthrough_pending",
            "expired_sessions",
            "file_registry",
            "lifetime_stats",
            "lifetime_per_file",
            "lifetime_daily",
            "lifetime_edited_files",
            "sessions",
        ] {
            // Best-effort: a missing table (very old DB) shouldn't
            // block the wipe.
            if let Err(e) = conn.execute(&format!("DELETE FROM {tbl}"), []) {
                eprintln!("drip reset --all: DELETE FROM {tbl} failed: {e:#}");
            }
        }
        // VACUUM reclaims pages so the file shrinks.
        let _ = conn.execute("VACUUM", []);
    }

    let cache = cache::cache_dir(&data);
    if cache.exists() {
        if let Ok(entries) = std::fs::read_dir(&cache) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        report.cache_blobs += 1;
                        report.cache_bytes += meta.len();
                    }
                }
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    // Mark the reset so `drip meter` swaps "Since install" → "Since reset".
    // The `meta` table is intentionally NOT wiped above (its schema_version
    // row is load-bearing), so the marker survives the next Session::open.
    if db_path.exists() {
        if let Ok(conn) = Connection::open(&db_path) {
            record_reset_marker(&conn).ok();
        }
    }

    Ok(report)
}

/// Stamp `meta('last_reset_at')` with the current unix timestamp. Called
/// from both reset paths (`--stats` and `--all`). `drip meter` reads this
/// to flip the "Since install" label to "Since reset" and re-anchor the
/// elapsed-time counter.
fn record_reset_marker(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES ('last_reset_at', ?1)",
        params![unix_now().to_string()],
    )?;
    Ok(())
}

/// Last `reset --all` / `reset --stats` timestamp, or `None` if no reset
/// has been recorded. Used by `drip meter` to decide between the
/// "Since install" and "Since reset" surface labels.
pub fn last_reset_at(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'last_reset_at'",
        [],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .and_then(|s| s.parse::<i64>().ok())
}

fn count_rows(conn: &Connection, table: &str) -> Result<i64> {
    // Best-effort: an absent table (very old DB) reports 0 rather
    // than blowing up the whole wipe.
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .or_else(|_| Ok::<i64, anyhow::Error>(0))
}

/// Result of `prune_missing_files` — surfaced by `drip meter --prune`.
#[derive(Debug, Clone, Default)]
pub struct PruneReport {
    pub files_pruned: i64,
    pub reads_reclaimed: i64,
    pub tokens_full_reclaimed: i64,
    pub tokens_sent_reclaimed: i64,
    pub paths: Vec<String>,
}

/// Drop rows from `lifetime_per_file` whose path no longer exists,
/// then recompute `lifetime_stats` from the survivors.
/// `lifetime_daily` is per-day aggregate without file_path, so it
/// stays as frozen history.
pub fn prune_missing_files(conn: &Connection) -> Result<PruneReport> {
    let mut stmt =
        conn.prepare("SELECT file_path, reads, tokens_full, tokens_sent FROM lifetime_per_file")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut report = PruneReport::default();
    for (path, reads, tf, ts) in rows {
        if !Path::new(&path).exists() {
            conn.execute(
                "DELETE FROM lifetime_per_file WHERE file_path = ?1",
                params![path],
            )?;
            conn.execute(
                "DELETE FROM lifetime_edited_files WHERE file_path = ?1",
                params![path],
            )?;
            report.files_pruned += 1;
            report.reads_reclaimed += reads;
            report.tokens_full_reclaimed += tf;
            report.tokens_sent_reclaimed += ts;
            report.paths.push(path);
        }
    }

    // Recompute the headline counters from the survivors.
    conn.execute(
        "UPDATE lifetime_stats SET
            total_reads = (SELECT COALESCE(SUM(reads), 0) FROM lifetime_per_file),
            tokens_full = (SELECT COALESCE(SUM(tokens_full), 0) FROM lifetime_per_file),
            tokens_sent = (SELECT COALESCE(SUM(tokens_sent), 0) FROM lifetime_per_file)
         WHERE id = 1",
        [],
    )?;

    Ok(report)
}

/// Self-heal `lifetime_stats` to match SUM over `lifetime_per_file`.
/// Idempotent; one cheap aggregation per process startup keeps the
/// headline counters trustworthy if the two tables drift.
pub fn resync_lifetime_stats(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE lifetime_stats SET
            total_reads = (SELECT COALESCE(SUM(reads), 0) FROM lifetime_per_file),
            tokens_full = (SELECT COALESCE(SUM(tokens_full), 0) FROM lifetime_per_file),
            tokens_sent = (SELECT COALESCE(SUM(tokens_sent), 0) FROM lifetime_per_file)
         WHERE id = 1",
        [],
    )?;
    Ok(())
}

/// Read-only check: share of `tokens_full` from rows whose file no
/// longer exists. Threshold via `DRIP_GHOST_HINT_THRESHOLD` (default
/// 50; set 101 to disable).
pub fn detect_ghost_pollution(conn: &Connection) -> Result<Option<GhostPollution>> {
    let threshold = std::env::var("DRIP_GHOST_HINT_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(50);
    if threshold > 100 {
        return Ok(None);
    }

    let mut stmt = conn.prepare("SELECT file_path, tokens_full FROM lifetime_per_file")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let total_tokens_full: i64 = rows.iter().map(|(_, t)| *t).sum();
    if total_tokens_full <= 0 {
        return Ok(None);
    }

    let mut ghost_files = 0i64;
    let mut ghost_tokens_full = 0i64;
    for (path, tokens) in &rows {
        if !Path::new(path).exists() {
            ghost_files += 1;
            ghost_tokens_full += *tokens;
        }
    }
    if ghost_files == 0 {
        return Ok(None);
    }

    let pct = (((ghost_tokens_full as f64) / (total_tokens_full as f64)) * 100.0).round() as u32;
    if pct < threshold {
        return Ok(None);
    }

    Ok(Some(GhostPollution {
        ghost_files,
        ghost_tokens_full,
        total_tokens_full,
        ghost_pct: pct,
    }))
}

/// Surfaced by `drip meter` when a meaningful share of lifetime
/// totals comes from files no longer on disk (scratch/bench artifacts
/// in `/tmp`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct GhostPollution {
    pub ghost_files: i64,
    pub ghost_tokens_full: i64,
    pub total_tokens_full: i64,
    pub ghost_pct: u32,
}

pub fn data_dir() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("DRIP_DATA_DIR") {
        return Ok(PathBuf::from(custom));
    }
    let base = dirs::data_local_dir()
        .or_else(dirs::data_dir)
        .or_else(dirs::home_dir)
        .context("no data directory available on this platform")?;
    Ok(base.join("drip"))
}

/// Session-id derivation strategies, in priority order:
/// - `Env` — caller-supplied via `DRIP_SESSION_ID`.
/// - `Git` — cwd + branch + worktree; survives crashes, isolates branches.
/// - `Pid` — cwd + ppid + parent start time.
/// - `Cwd` — cwd alone, permanent per directory; opt-in via
///   `DRIP_SESSION_STRATEGY=cwd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStrategy {
    Env,
    Git,
    Pid,
    Cwd,
}

impl SessionStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStrategy::Env => "env",
            SessionStrategy::Git => "git",
            SessionStrategy::Pid => "pid",
            SessionStrategy::Cwd => "cwd",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "env" => Some(SessionStrategy::Env),
            "git" => Some(SessionStrategy::Git),
            "pid" => Some(SessionStrategy::Pid),
            "cwd" => Some(SessionStrategy::Cwd),
            _ => None,
        }
    }
}

/// Session-id derivation result.
#[derive(Debug, Clone)]
pub struct SessionDerivation {
    pub id: String,
    pub strategy: SessionStrategy,
    /// `(env)` / branch name / `(pid <ppid>)` / `(cwd)`. Shows up in
    /// `drip sessions`, keep it short.
    pub context: String,
}

/// Resolve a session id and strategy. Order:
/// 1. `DRIP_SESSION_ID` (non-empty) → `Env`, verbatim.
/// 2. `DRIP_SESSION_STRATEGY` forces `git` / `pid` / `cwd`. `git`
///    falls through to `pid` when no git context is detectable.
/// 3. Default: `git` if reachable from cwd, otherwise `pid`.
pub fn derive_session() -> SessionDerivation {
    if let Ok(id) = std::env::var("DRIP_SESSION_ID") {
        if !id.is_empty() {
            return SessionDerivation {
                id,
                strategy: SessionStrategy::Env,
                context: "(env)".to_string(),
            };
        }
    }

    let cwd_path = std::env::current_dir().unwrap_or_default();
    let cwd = cwd_path.to_string_lossy().into_owned();

    let forced = std::env::var("DRIP_SESSION_STRATEGY")
        .ok()
        .as_deref()
        .and_then(SessionStrategy::parse);

    let try_git = !matches!(forced, Some(SessionStrategy::Pid | SessionStrategy::Cwd));
    if try_git {
        if let Some(ctx) = crate::core::git::detect(&cwd_path) {
            let id = hash_to_short_hex(&[
                b"git:" as &[u8],
                cwd.as_bytes(),
                b":",
                ctx.branch.as_bytes(),
                b":",
                ctx.worktree_id.as_bytes(),
            ]);
            return SessionDerivation {
                id,
                strategy: SessionStrategy::Git,
                context: ctx.branch,
            };
        }
    }

    if matches!(forced, Some(SessionStrategy::Cwd)) {
        let id = hash_to_short_hex(&[b"cwd:" as &[u8], cwd.as_bytes()]);
        return SessionDerivation {
            id,
            strategy: SessionStrategy::Cwd,
            context: "(cwd)".to_string(),
        };
    }

    let ppid = parent_pid().unwrap_or(0);
    let pstart = parent_start_time().unwrap_or(0);
    let id = hash_to_short_hex(&[
        b"pid:" as &[u8],
        cwd.as_bytes(),
        b":",
        &ppid.to_le_bytes(),
        b":",
        &pstart.to_le_bytes(),
    ]);
    SessionDerivation {
        id,
        strategy: SessionStrategy::Pid,
        context: format!("(pid {ppid})"),
    }
}

/// Thin wrapper for call sites that only need the id.
#[allow(dead_code)]
pub fn derive_session_id() -> String {
    derive_session().id
}

fn hash_to_short_hex(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    let digest = h.finalize();
    let mut s = String::with_capacity(16);
    for b in digest.iter().take(8) {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// `DRIP_TEST_PPID` overrides `getppid()` for deterministic tests.
fn parent_pid() -> Option<u32> {
    if let Ok(s) = std::env::var("DRIP_TEST_PPID") {
        if let Ok(n) = s.parse::<u32>() {
            return Some(n);
        }
    }
    parent_pid_real()
}

#[cfg(unix)]
fn parent_pid_real() -> Option<u32> {
    Some(unsafe { libc::getppid() } as u32)
}

#[cfg(not(unix))]
fn parent_pid_real() -> Option<u32> {
    None
}

/// Parent process start time in OS-defined units; entropy ingredient
/// for session id derivation. Failure → 0.
#[cfg(target_os = "macos")]
fn parent_start_time() -> Option<u64> {
    let ppid = unsafe { libc::getppid() };
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let need = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let r = unsafe {
        libc::proc_pidinfo(
            ppid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            need,
        )
    };
    if r >= need {
        Some(info.pbi_start_tvsec.wrapping_mul(1_000_000) + info.pbi_start_tvusec as u64)
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn parent_start_time() -> Option<u64> {
    let ppid = unsafe { libc::getppid() };
    let stat = std::fs::read_to_string(format!("/proc/{ppid}/stat")).ok()?;
    // /proc/<pid>/stat: "<pid> (<comm-with-spaces>) <state> <ppid> ..."
    // The 22nd field (1-indexed) is starttime. The comm field can contain
    // whitespace and parens, so anchor on the LAST ')'.
    let close = stat.rfind(')')?;
    let after = stat.get(close + 2..)?;
    let parts: Vec<&str> = after.split_whitespace().collect();
    // After the closing ')', fields are state(1), ppid(2), pgrp(3), session(4),
    // tty_nr(5), tpgid(6), flags(7), minflt(8), cminflt(9), majflt(10),
    // cmajflt(11), utime(12), stime(13), cutime(14), cstime(15), priority(16),
    // nice(17), num_threads(18), itrealvalue(19), starttime(20).
    parts.get(19).and_then(|s| s.parse().ok())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parent_start_time() -> Option<u64> {
    None
}

/// `chmod 0600` — the DB stores file content, default 0644 would be
/// a disclosure on shared hosts.
#[cfg(unix)]
fn harden_file_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn harden_file_permissions(_path: &Path) {}

/// Pre-create the SQLite file at 0600 so SQLite never materialises it
/// at umask-derived 0644. No-op on non-Unix.
fn precreate_db_file_secure(path: &Path) -> std::io::Result<()> {
    // Touch-only — SQLite owns the bytes.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)?;
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn harden_dir_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn harden_dir_permissions(_path: &Path) {}

/// SCHEMA already inserts `schema_version=1` via `INSERT OR IGNORE`, so
/// this only needs to *read* and refuse forward-incompatible DBs. Pure
/// SELECT — no extra write lock per connection, which matters when many
/// agent hooks open concurrent connections.
/// Idempotently put the database into WAL journal mode, retrying past
/// `SQLITE_BUSY` from concurrent first-time openers.
///
/// `PRAGMA journal_mode = WAL` is the one write SQLite refuses to feed
/// through the busy_handler — a cold-start race between N processes
/// loses (N-1) of them with `database is locked`. We work around it
/// with the cheapest possible dance: read the current mode first
/// (`PRAGMA journal_mode` with no argument is a SELECT — never blocks),
/// no-op if already WAL, otherwise enter a hand-rolled retry loop with
/// exponential backoff capped at 5 s total (matches `busy_timeout`).
/// Once any process has flipped the file to WAL the choice is persisted
/// on disk, so subsequent opens see the SELECT short-circuit and never
/// touch the contended pragma path.
fn ensure_wal_mode(conn: &Connection) -> Result<()> {
    let current: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap_or_default();
    if current.eq_ignore_ascii_case("wal") {
        return Ok(());
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut backoff_ms: u64 = 1;
    loop {
        match conn.pragma_update(None, "journal_mode", "WAL") {
            Ok(()) => return Ok(()),
            Err(e) => {
                let is_busy = matches!(
                    e.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy)
                        | Some(rusqlite::ErrorCode::DatabaseLocked)
                );
                if !is_busy || std::time::Instant::now() >= deadline {
                    return Err(anyhow::Error::from(e).context("setting journal_mode = WAL"));
                }
                // A sibling may have flipped it while we were sleeping.
                let now: String = conn
                    .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                    .unwrap_or_default();
                if now.eq_ignore_ascii_case("wal") {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
                backoff_ms = (backoff_ms * 2).min(100);
            }
        }
    }
}

/// Tombstone TTL — long enough to outlive a lunch break, short enough
/// that the table stays bounded.
const EXPIRED_TOMBSTONE_TTL_SECS: i64 = 24 * 60 * 60;

/// True iff the id was tombstoned, removing the tombstone in the
/// same op. Drives the one-shot "session expired" decoration.
fn consume_expired_tombstone(conn: &Connection, session_id: &str) -> Result<bool> {
    // GC stale tombstones on every check; cheap PK scan.
    let _ = conn.execute(
        "DELETE FROM expired_sessions WHERE expired_at < ?1",
        params![unix_now() - EXPIRED_TOMBSTONE_TTL_SECS],
    );
    let removed = conn.execute(
        "DELETE FROM expired_sessions WHERE session_id = ?1",
        params![session_id],
    )?;
    Ok(removed > 0)
}

/// `DRIP_REGISTRY_DISABLE=1` opts out of cross-session registry use.
fn registry_disabled() -> bool {
    matches!(
        std::env::var("DRIP_REGISTRY_DISABLE").as_deref(),
        Ok("1") | Ok("true")
    )
}

/// Git branch label for inter-session diffs. `None` for non-git
/// strategies and pseudo-context entries (`(env)`, `(pid …)`).
fn session_git_branch(s: &Session) -> Option<String> {
    if matches!(s.strategy, SessionStrategy::Git)
        && !s.context.is_empty()
        && !s.context.starts_with('(')
    {
        Some(s.context.clone())
    } else {
        None
    }
}

/// Canonicalised agent identity from `$DRIP_AGENT`. Unrecognised
/// values return `None` so a shell-rc typo can't poison the column.
/// Recognised: `claude`, `codex`, `gemini` (case-insensitive).
fn agent_from_env() -> Option<String> {
    let raw = std::env::var("DRIP_AGENT").ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude-code" => Some("claude".into()),
        "codex" | "codex-cli" => Some("codex".into()),
        "gemini" | "gemini-cli" => Some("gemini".into()),
        _ => None,
    }
}

fn check_or_set_schema_version(conn: &Connection) -> Result<()> {
    let current: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match current {
        Some(v) if v > SCHEMA_VERSION => anyhow::bail!(
            "drip: sessions.db has schema_version={v}, this build understands up to {SCHEMA_VERSION}. \
             Upgrade drip, or wipe ~/.local/share/drip/sessions.db to start fresh."
        ),
        _ => Ok(()), // None (very old DB) or compatible
    }
}

pub fn hash_content(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d.iter() {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn purge_stale_sessions(conn: &Connection) -> Result<()> {
    let cutoff = unix_now() - session_ttl_secs();
    // Tombstone first so the next reopen of these ids fires the
    // "session expired" decoration.
    let _ = conn.execute(
        "INSERT OR REPLACE INTO expired_sessions (session_id, expired_at)
         SELECT session_id, ?1 FROM sessions WHERE last_active < ?2",
        params![unix_now(), cutoff],
    );
    // Snapshot blob hashes BEFORE the DELETE so we can GC freshly-
    // orphaned blobs. DISTINCT because vendored files dedupe.
    let doomed_hashes: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT content_hash FROM reads
             WHERE content_storage = 'file'
               AND content_hash IS NOT NULL
               AND content_hash != ''
               AND session_id IN (
                 SELECT session_id FROM sessions WHERE last_active < ?1
             )",
        )?;
        let rows = stmt
            .query_map(params![cutoff], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect::<Vec<String>>();
        rows
    };
    conn.execute(
        "DELETE FROM reads WHERE session_id IN (
            SELECT session_id FROM sessions WHERE last_active < ?1
        )",
        params![cutoff],
    )?;
    conn.execute(
        "DELETE FROM passthrough_pending WHERE session_id IN (
            SELECT session_id FROM sessions WHERE last_active < ?1
        )",
        params![cutoff],
    )?;
    // Belt-and-braces: orphan rows from manual edits / crashes.
    conn.execute(
        "DELETE FROM passthrough_pending WHERE session_id NOT IN (
            SELECT session_id FROM sessions
        )",
        [],
    )?;
    conn.execute(
        "DELETE FROM precomputed_reads WHERE session_id IN (
            SELECT session_id FROM sessions WHERE last_active < ?1
        )",
        params![cutoff],
    )?;
    conn.execute(
        "DELETE FROM precomputed_reads WHERE session_id NOT IN (
            SELECT session_id FROM sessions
        )",
        [],
    )?;
    conn.execute(
        "DELETE FROM read_events WHERE session_id IN (
            SELECT session_id FROM sessions WHERE last_active < ?1
        )",
        params![cutoff],
    )?;
    conn.execute(
        "DELETE FROM read_events WHERE session_id NOT IN (
            SELECT session_id FROM sessions
        )",
        [],
    )?;
    conn.execute(
        "DELETE FROM sessions WHERE last_active < ?1",
        params![cutoff],
    )?;

    // GC newly-orphaned blobs. `delete_blobs_if_unreferenced`
    // re-checks `reads` so a dedup'd hash on a surviving session
    // keeps its blob. Best-effort.
    if !doomed_hashes.is_empty() {
        if let Ok(dir) = data_dir() {
            let _ = cache::delete_blobs_if_unreferenced(conn, &dir, &doomed_hashes);
        }
    }
    Ok(())
}

pub fn resolve_path(p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

// ─── seen_ranges helpers ─────────────────────────────────────────────
//
// `reads.seen_ranges` is a JSON array of `[start, end]` pairs (1-indexed
// inclusive line numbers, sorted, merged). Empty / NULL ⇒ nothing has
// been delivered to the agent yet (silent baseline state).

/// Parse the JSON column into a `Vec<(usize, usize)>`. Tolerant of
/// `None`, empty strings, and malformed JSON — DRIP must never crash
/// on a corrupted row, so any decode failure degrades to "agent has
/// seen nothing", which is the safe pessimistic answer.
pub fn parse_seen_ranges(raw: Option<&str>) -> Vec<(usize, usize)> {
    let Some(s) = raw else {
        return Vec::new();
    };
    if s.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<(usize, usize)>>(s).unwrap_or_default()
}

/// Sort and coalesce overlapping or adjacent ranges. Adjacency is
/// `prev.end + 1 == cur.start` — two windows that touch end-to-end
/// merge into one so the agent's view of line N+1 right after seeing
/// N counts as contiguous coverage.
pub fn merge_seen_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.retain(|&(s, e)| s > 0 && e >= s);
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort_by_key(|&(s, _)| s);
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (s, e) in ranges {
        match out.last_mut() {
            Some(last) if s <= last.1.saturating_add(1) => {
                last.1 = last.1.max(e);
            }
            _ => out.push((s, e)),
        }
    }
    out
}

pub fn serialize_seen_ranges(ranges: &[(usize, usize)]) -> String {
    serde_json::to_string(ranges).unwrap_or_else(|_| "[]".to_string())
}

/// `true` iff every line in `[start, end]` (1-indexed inclusive) is
/// covered by some range in `ranges`. The list is assumed sorted &
/// merged (which `parse_seen_ranges` always produces via the writer
/// path; ad-hoc test data should call `merge_seen_ranges` first).
pub fn seen_ranges_cover(ranges: &[(usize, usize)], start: usize, end: usize) -> bool {
    if start == 0 || end < start {
        return false;
    }
    ranges.iter().any(|&(s, e)| s <= start && end <= e)
}

#[cfg(test)]
mod seen_ranges_tests {
    use super::*;

    #[test]
    fn parse_handles_null_and_empty() {
        assert!(parse_seen_ranges(None).is_empty());
        assert!(parse_seen_ranges(Some("")).is_empty());
        assert!(parse_seen_ranges(Some("[]")).is_empty());
    }

    #[test]
    fn parse_tolerates_garbage() {
        assert!(parse_seen_ranges(Some("not json")).is_empty());
        assert!(parse_seen_ranges(Some("{\"x\":1}")).is_empty());
    }

    #[test]
    fn merge_coalesces_overlap() {
        let m = merge_seen_ranges(vec![(1, 10), (5, 20)]);
        assert_eq!(m, vec![(1, 20)]);
    }

    #[test]
    fn merge_coalesces_adjacency() {
        // 10 and 11 are adjacent — merge into one window. This is the
        // behaviour we rely on so that two partial reads of lines 1-10
        // and 11-20 collapse into [(1, 20)] for coverage purposes.
        let m = merge_seen_ranges(vec![(1, 10), (11, 20)]);
        assert_eq!(m, vec![(1, 20)]);
    }

    #[test]
    fn merge_keeps_disjoint_intervals_sorted() {
        let m = merge_seen_ranges(vec![(50, 60), (10, 20)]);
        assert_eq!(m, vec![(10, 20), (50, 60)]);
    }

    #[test]
    fn merge_drops_invalid_ranges() {
        let m = merge_seen_ranges(vec![(0, 5), (10, 8), (1, 1), (5, 5)]);
        // (0, 5) dropped (start=0), (10, 8) dropped (end<start), the
        // singleton (1, 1) and (5, 5) kept.
        assert_eq!(m, vec![(1, 1), (5, 5)]);
    }

    #[test]
    fn cover_requires_inclusion() {
        let r = vec![(1, 50), (100, 200)];
        assert!(seen_ranges_cover(&r, 1, 50));
        assert!(seen_ranges_cover(&r, 100, 150));
        assert!(seen_ranges_cover(&r, 5, 25));
        assert!(!seen_ranges_cover(&r, 49, 51), "straddles a gap");
        assert!(!seen_ranges_cover(&r, 200, 201), "extends past end");
        assert!(!seen_ranges_cover(&r, 0, 5), "start=0 invalid");
        assert!(!seen_ranges_cover(&r, 10, 5), "end<start invalid");
    }

    #[test]
    fn cover_empty_ranges_never_matches() {
        assert!(!seen_ranges_cover(&[], 1, 1));
        assert!(!seen_ranges_cover(&[], 1, 100));
    }

    #[test]
    fn serialize_round_trips() {
        let r = vec![(1, 10), (50, 60)];
        let s = serialize_seen_ranges(&r);
        let parsed = parse_seen_ranges(Some(&s));
        assert_eq!(r, parsed);
    }
}
