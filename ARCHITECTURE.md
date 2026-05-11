# Architecture

This document explains *why* DRIP is shaped the way it is. The
"what" lives in the code and `README.md`; this is the design
rationale you'd want before changing anything load-bearing.

---

## 1. Goals and non-goals

**Goals**

- Cut LLM token usage on file re-reads by 60–95 % with zero
  behavioural change for the agent.
- Be invisible: same input shape (`Read` tool, `read` syscall
  surface), same output semantics.
- Stay under a 5 MB binary, sub-10 ms cold start, single-file
  SQLite store.
- Work with any agent that supports a pre-tool hook or speaks
  MCP.

**Non-goals**

- Editing files. DRIP is read-only with respect to user code.
- Cross-machine sync. The store is local.
- Reconstructing the model's actual context window. DRIP tracks
  what *it returned*, not what the model retained.

---

## 2. Why a per-session ledger, not a global cache

A "global most-recent-content" cache would corrupt agents that
switch between sessions or branches: the second session would
receive a diff against a baseline it never saw, leaving the model
confused. The cost of keying by `(session_id, file_path)` is one
extra column and zero ambiguity.

The session id resolves through a 4-strategy ladder in priority
order:

1. **`env`** — `DRIP_SESSION_ID` set verbatim.
2. **`git`** — `sha256("git:" + cwd + ":" + branch + ":" + worktree_id)`
   truncated to 16 hex. DRIP reads `.git/HEAD` directly (handles
   real `.git` directories *and* worktree gitlinks), no
   subprocess. **Survives crashes** (same branch ⇒ same id) and
   **isolates branches**.
3. **`pid`** — `sha256(cwd + parent_pid + parent_start_time)`.
   Used outside a repo or when the git probe can't determine a
   branch.
4. **`cwd`** — `sha256(cwd)` only. Permanent per directory; opt-in
   via `DRIP_SESSION_STRATEGY=cwd`.

Malformed `.git`, missing `HEAD`, broken gitlinks, garbage HEAD
content — all bail silently to `pid`. False negatives are fine; a
*confidently-wrong* branch name would silently misroute reads, so
every parse step bails on the slightest ambiguity.

Sessions auto-expire on a heartbeat-driven TTL (`DRIP_SESSION_TTL_SECS`,
default 7200 s). A purged session lands in `expired_sessions` (24 h
tombstone) so when the agent reopens the same id, the next first-read
emits a one-shot `ℹ session expired — fresh baseline started` notice.

---

## 3. Why SQLite (and hybrid storage)

A single file at `~/.local/share/drip/sessions.db` is trivial to
inspect, back up, and delete. WAL mode handles concurrent hook
invocations from parallel agent tools without corruption. The
`top savings` and history queries reduce to a `GROUP BY` instead of
a directory walk. Bundled `rusqlite/bundled` means no system
dependency — `cargo install` Just Works on a fresh machine.

**Hybrid storage** routes files by size:

- Files at or below `DRIP_INLINE_MAX_BYTES` (default 32 KB) live
  inline in the `reads.content` column.
- Larger files are written to `<DRIP_DATA_DIR>/cache/<sha256>.bin`
  (atomic tmp + `rename(2)`, `0700` dir, `0600` file). The row
  carries only the hash + a `content_storage='file'` marker.

Hash-addressed naming gives **automatic deduplication**: two
sessions reading the same vendored library or generated artefact
share one blob. Cache GC walks both `reads` and `file_registry`
when computing the active set, so a registry-only reference always
keeps its blob alive.

The schema is intentionally narrow — see
`src/core/session.rs::SCHEMA`. Migrations are additive (`ALTER
TABLE … ADD COLUMN` with tolerant `OR IGNORE`) and a `meta(schema_version)`
row guards against running an older `drip` against a future DB.

---

## 4. Cross-session file registry

Per-session `reads` rows are purged on TTL expiry. To stop the agent
from restarting blind on every new session, DRIP also writes a
single `file_registry` row per absolute path on every `set_baseline`,
holding `(content_hash, content, branch, last_seen_at)`.

On the **first read in a new session** the registry decorates the
header:

| State on first read in new session | Header decoration                                                |
|------------------------------------|------------------------------------------------------------------|
| Never seen before                  | (none)                                                           |
| Seen, byte-identical hash          | ` \| ↔ unchanged since last session (3h ago)`                    |
| Seen, hash differs                 | ` \| ↕ changed since last session: +23 lines, -5 lines` + diff trailer |

The full file content is **always** sent on the first read — at
session start the agent has nothing to diff against. The decoration
is purely orientation. The diff trailer is capped at 200 lines so a
wholesale rewrite can't blow the agent's context budget.

---

## 5. Why `similar` for diffing

`similar` produces git-compatible unified diffs and is what
`cargo` / `insta` use under the hood. Pure Rust, no `unsafe`,
configurable context radius. DRIP picks 3 lines of context — enough
for the model to anchor each hunk, small enough that single-line
edits stay tiny.

DRIP deliberately does **not** use binary diffs (bsdiff, vcdiff)
because the consumer is an LLM that only meaningfully understands
text. A unified diff in the model's training distribution beats a
smaller-but-opaque binary patch every time.

### Diff complexity gate

A unified diff scattered across many functions can cost more tokens
than the file itself *and* is harder for the agent to reason about
than a clean re-read. `differ::analyze_complexity` measures hunk
count, changed-line ratio, and max hunk distance; when any threshold
trips (`DRIP_MAX_HUNKS=6`, `DRIP_MAX_CHANGED_PCT=0.40`, or > 3
hunks with > 200-line span) DRIP ships a clean full re-read with a
`[DRIP: diff complexity: …]` header instead.

Multi-hunk diffs that don't trip the gate gain a language-aware
hunk summary (`| 3 hunks: calculate_subtotal (ln 42), main (ln 156)`)
so the agent can spot the touched regions at a glance.

---

## 6. Hook contracts

### Claude Code — five PreToolUse / PostToolUse hooks

DRIP installs:

- `PreToolUse:Read` — allows native first reads so Claude's
  read-before-edit tracker is populated, then substitutes
  diff/unchanged on re-reads, applies `.dripignore`. For partial reads (`Read(file, offset=N,
  limit=M)`) on a file that already has a baseline, the same
  diff/unchanged logic is scoped to the requested window
  (`[DRIP: unchanged (lines X-Y)]` or `[DRIP: delta only (lines
  X-Y)]`); on a file DRIP has never seen, the partial read passes
  through to native since DRIP has no prior content to compare
  the slice against. Partial reads never mutate the baseline — the
  agent saw a slice, not the file, so the next genuine full read
  still serves the whole content.
- `PreToolUse:Glob` — re-runs the glob, filters via `.dripignore`,
  returns at most 1,000 matches sorted newest-first.
- `PreToolUse:Grep` — when `rg` is on `PATH`, re-issues the search
  with `.dripignore` excludes. Streams output with a 4 MiB ceiling.
- `PostToolUse:Edit|Write|MultiEdit|NotebookEdit` — refreshes
  baseline, marks one-shot passthrough, surfaces a warning via
  `hookSpecificOutput.additionalContext` when the edit overlaps a
  function whose body was elided.

Substitution mechanism: `permissionDecision: "deny"` +
`permissionDecisionReason` carrying the rendered output. This is
currently the only stable way for a hook to *substitute* what the
model sees in place of a tool result. If Claude Code ships a
richer "rewrite tool result" hook, we'll switch to it; the rest of
the system is unaffected.

### MCP — Codex and Gemini

Codex / Gemini don't expose Claude-style PreToolUse hooks. The
portable interception surface is the **Model Context Protocol**:
`drip mcp` runs as a stdio JSON-RPC server advertising a single
`read_file` tool. Each agent picks it up via its config
(`~/.codex/config.toml` `[mcp_servers.drip]`,
`~/.gemini/settings.json`) and is steered toward it by an
instruction appended to the agent's system-prompt file.

Cursor (and other IDEs whose agent has a built-in `read_file` tool
alongside MCP) is deliberately not on this list: even with our MCP
server registered, the agent's native read tool wins by default and
DRIP only sees a fraction of the reads. Rather than ship a
half-working integration, we focus on agents whose only read path
is interceptable.

The MCP server (`src/mcp.rs`) implements only what's needed:
`initialize`, `tools/list`, `tools/call`, `ping`. ~150 lines of
code, pure `serde_json` — no MCP SDK dependency. The same
`read::run()` function powers the hook path and the MCP path, so
behaviour is identical and tested once.

---

## 7. Semantic compression

`core::compress` walks supported source files on DRIP-substituted
first reads, finds function/method bodies, and replaces each one with
a one-line stub — keeping signatures visible so the agent knows
what's there, hiding bodies unless asked. Claude Code's native
`Read` first pass is not compressed because the native tool must run
to populate Claude's read-before-edit tracker. The full file is still
stored as the SQLite baseline, so subsequent diffs are computed
against the real content.

Two scanners share one helper layer:

- **Indent-based** (`Python`) — track the function's decl indent,
  collect lines whose indent is strictly greater, replace with
  `... # [DRIP-elided]`.
- **Brace-balancing** (Rust, JS/TS, Go, Java, C, C++, C#, Kotlin,
  Swift, Scala, PHP) — track string/comment state so a literal `}`
  in a string never truncates the body, exclude control-flow
  keywords (`if` / `else` / `while` / `for` / `switch` / `try` /
  …) and structural keywords (`class` / `struct` / `interface` /
  `namespace` / `impl` / …) so method bodies elide while their
  signatures and the surrounding class stay visible.

DRIP deliberately doesn't pull a real parser (tree-sitter, syn) —
the line scanner is good enough on real code (> 95 % on the test
corpus) and degrades gracefully (false negatives mean
"uncompressed", never mangled output).

Bodies shorter than `DRIP_COMPRESS_MIN_BODY` lines (default 15,
floor 4) stay inline — eliding a short helper costs more tokens
than the function itself, and the body is usually more
informative than the stub.

When an edit lands on a function whose body was elided, the
post-edit hook reads `reads.was_semantic_compressed` +
`reads.elided_functions` (JSON list), runs three detection
heuristics (Edit/MultiEdit text scan, Write-tool diff fallback,
edit-position-inside-function-span), and emits a warning via
`hookSpecificOutput.additionalContext` so the model knows to
re-read before reasoning further.

---

## 8. Token estimation

DRIP uses `bytes / 4` (rounded up). The real tokenizer is
BPE-specific and would require pulling in `tiktoken-rs` or similar
— adding ~3 MB and an init cost we don't want on every hook for a
reporting number.

This is an estimate, not a billing-grade tokenizer. The meter tracks
agent-facing payload size: full file bodies, diff bodies, edit
certificates, and registry diff trailers. Small DRIP control headers
are intentionally excluded from totals so the metric stays comparable
across outcomes (`unchanged` means zero file payload resent, even
though the one-line notice itself has a few tokens).

---

## 9. Edge cases — design decisions

| Case                                | Decision                                   | Why                                                                            |
|-------------------------------------|--------------------------------------------|--------------------------------------------------------------------------------|
| Binary file (NUL byte or non-UTF-8) | Always full read, no diff                  | Diffing binary text is meaningless and risks corrupting the model's view       |
| File > 50 MB                        | `metadata().len()` short-circuit pre-read  | OOM hazard if an agent points DRIP at `/dev/zero` or a huge log                |
| File > 100 KB                       | Full read with `[DRIP: large file]` header | Diff CPU + token cost stops winning; full re-reads on huge files are rare      |
| Truncation > 50 %                   | Full read with `[DRIP: truncated]`         | Often signals destructive intent; a fresh baseline is safer than a giant diff  |
| Diff would cost more than the file  | Full read fallback                         | DRIP must never send a *bigger* payload than the original                      |
| Diff has > `DRIP_MAX_HUNKS` hunks   | Full read with `[DRIP: diff complexity]`   | Sprawling diffs hurt agent reasoning; a clean re-read is friendlier            |
| Deleted file                        | `[DRIP: file deleted]` + drop the row      | Stale baselines could mislead a later read of a recreated file                 |
| SQLite locked                       | WAL + `busy_timeout = 500 ms`              | Hook calls are short and parallel; WAL avoids reader/writer contention         |

---

## 10. Why not a daemon

A long-running daemon would let us skip SQLite open + schema check
on every call. We prototyped it and the saving is ~3–4 ms — not
worth the extra failure mode (stale daemon, port collision,
restart-on-upgrade). The current cold-path cost is dominated by
`clap` parsing and rusqlite WAL setup; both are bounded and
predictable.

If we later want to push under 1 ms per call, the right move is
`mmap` the SQLite db with `PRAGMA mmap_size`, not a daemon.

`drip watch` is the one exception: a long-lived watcher pre-computes
diffs for already-tracked files so the hook can skip fs::read, sha256,
and diff on the next read. The hook validates `(mtime, size)` against a
`precomputed_reads` table and consumes the cached diff on hit. A
1-second polling fallback (`DRIP_WATCH_RESCAN_MS`) covers watcher
backends that miss events during editor rename bursts or special-file
transitions. For typical small code files the inline hook is already in
the noise — `drip watch` is opt-in.

---

## 11. What changes if you fork this

The interesting axes to vary:

- **Diff format.** Switch `similar`'s formatter for a custom JSON
  patch if your downstream consumer is structured rather than an
  LLM.
- **Storage.** Replace SQLite with `sled` if you want zero
  `unsafe` and pure-Rust embeddability — at the cost of less-mature
  tooling.
- **Session keying.** The 4-strategy ladder is in
  `src/core/session.rs::derive_session`. Add a strategy or change
  the hash inputs there.
- **Hook target.** Extend `src/hooks/` with a new module for any
  agent — the public contract is stdin JSON → stdout text.

The core invariant the rest of the system depends on:

> For a given `(session_id, file_path)`, `reads.content` always
> reflects the most recent version DRIP has *returned to the
> agent*.

Anything that violates that — e.g., updating `reads.content` on
read but not actually emitting a diff — will produce wrong
baselines and silently confuse the model. Tests in
`tests/integration/diff_accuracy.rs` exist to catch exactly that.
