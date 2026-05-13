<!-- markdownlint-disable MD033 MD041 -->

<div align="center">

# DRIP

**Delta Read Interception Proxy — sends only file diffs to your LLM agent.**

[Website](https://drip-ai.app/) · [Install](#installation) · [Benchmarks](./BENCHMARKS.md) · [Architecture](./ARCHITECTURE.md)

[![CI](https://img.shields.io/github/actions/workflow/status/drip-cli/drip/ci.yml?branch=main&label=build&logo=github)](https://github.com/drip-cli/drip/actions)
[![Crates.io](https://img.shields.io/crates/v/drip-cli.svg?logo=rust)](https://crates.io/crates/drip-cli)
[![Homebrew](https://img.shields.io/badge/homebrew-tap-orange.svg?logo=homebrew)](https://github.com/drip-cli/homebrew-drip)
[![Website](https://img.shields.io/badge/website-drip--ai.app-18E299.svg?logo=vercel&logoColor=white)](https://drip-ai.app/)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](./LICENSE)
[![Tests](https://img.shields.io/badge/tests-431%20passing-brightgreen.svg)](./tests)
[![Benchmarks](https://img.shields.io/badge/benchmarks-8%20langs%2C%204%20workflows-blue.svg)](./BENCHMARKS.md)

</div>

A single Rust binary that sits between your coding agent (Claude
Code, Codex, Gemini) and the filesystem. It records a baseline on
the first read, then returns just a unified diff or `[unchanged]` on
re-reads. MCP/manual first reads can be semantically compressed
for code; Claude Code's native `Read` first pass stays native full
content so Claude's read-before-edit tracker remains correct. Same
agent, same workflow, **~60–80 % fewer tokens** spent on file reads.

---

## Table of contents

- [The problem](#the-problem)
- [How it works](#how-it-works)
- [Install](#install)
- [Quick start](#quick-start)
- [Commands](#commands)
- [Configuration](#configuration)
- [Compatibility](#compatibility)
- [Privacy & security](#privacy--security)
- [Documentation](#documentation)
- [Contributing](#contributing)
- [License](#license)

---

## The problem

Your agent codes a feature on `app.py` (400 lines):

```text
read app.py     →  400 lines
edit 3 lines    →  …
read app.py     →  400 lines  ← already saw 397 of them
edit 2 lines    →  …
read app.py     →  400 lines  ← still re-reading the same content
read app.py     →  400 lines
read app.py     →  400 lines
                    ─────────
                    2,000 lines sent to the model
```

With DRIP the same five reads look like this:

```text
read app.py     →  ~40 lines (semantic-compressed first read)
read app.py     →  ~6 lines  (unified diff)
read app.py     →  ~4 lines
read app.py     →  unchanged — 0 lines
read app.py     →  ~2 lines
                    ─────────
                    ~52 lines sent to the model
```

Same agent. Same workflow. **~97 % fewer tokens** on this loop —
that example is illustrative; on a measured 8-language fixture
set the workflow means land at **34 % – 88 %** depending on the
mix of unchanged re-reads (high) vs. live edits (lower) — see
[`BENCHMARKS.md`](./BENCHMARKS.md) for the full breakdown across
**8 languages, 4 workflow styles, and verified
signature/import/type preservation** on every fixture.

---

## How it works

DRIP intercepts file-read calls before they reach the agent's tool
result and replaces them with the smallest payload that brings the
agent up to date.

```text
                ┌─────────────────────────────────┐
   Agent ────►  │  DRIP hook (Read / MCP)         │  ────► smallest payload
                └────────────┬────────────────────┘            │
                             │ SHA-256(content)                │
                             ▼                                 │
                ┌─────────────────────────────────┐             │
                │  ~/.local/share/drip/sessions.db │ ◄──────────┘
                │   (session_id, file) → baseline │
                └─────────────────────────────────┘
```

Per `(session_id, file_path)` DRIP picks one outcome:

| Outcome                              | What the agent receives                                |
|--------------------------------------|--------------------------------------------------------|
| First read                           | Full content. MCP/manual substitutions are semantically compressed when applicable; Claude `Read` first pass stays native full content |
| Same hash as last read               | `[DRIP: unchanged]` (zero-byte body)                   |
| Different hash, small diff           | Unified diff via [`similar`](https://docs.rs/similar)  |
| Diff would cost more than file       | Auto-fallback to full content                          |
| Partial read with baseline, same     | `[DRIP: unchanged (lines X-Y)]` — window-scoped 0-byte |
| Partial read with baseline, drifted  | Unified diff scoped to the requested window only       |
| Partial read on unknown file         | Native passthrough (no baseline to compare against)    |
| File deleted                         | `[DRIP: file deleted since last read]`                 |
| Read after your own edit             | `[DRIP: edit verified \| hash: …]` certificate (file hash + touched ranges, ~390 B) |
| First read past Claude's `Read` 25 k-token limit | Semantic-compressed view substituted via `deny` (signatures + `[DRIP-elided]` stubs) — the agent sees the file's shape instead of the native `exceeds maximum allowed tokens` error |
| Path matches `.dripignore`           | `<ignored>` placeholder                                |

**Tokens, in practice.** Every row below is **measured**, not
estimated, on a real fixture in this repo. Re-run with
`bash scripts/bench_multilang.sh` (~30 s on Apple Silicon).

| Scenario                                                     | Without DRIP   | With DRIP        | Saved        |
|--------------------------------------------------------------|---------------:|-----------------:|-------------:|
| First read of a 731-line Python file (semantic-compressed)   |    6,774 tok   |    2,575 tok     |   **62 %**   |
| 5 reads of the same Python file — 1 first + 4 unchanged      |   33,870 tok   |    2,806 tok     |   **92 %**   |
| Edit cycle on a 744-line Java file (4 reads, 1 edit + cert)  |   28,306 tok   |    5,206 tok     |   **82 %**   |
| 7-read refactor session, 3 edit cycles (8 langs combined)    |  364,496 tok   |  188,287 tok     |   **48 %**   |

> **Bonus — DRIP reads files Claude's `Read` tool can't.** Claude
> refuses anything past ~25 000 tokens with `File content (X tokens)
> exceeds maximum allowed tokens (25000)`. DRIP detects the threshold,
> runs semantic compression even on the native-passthrough path, and
> substitutes the compressed view. **Live numbers: a 130 KB,
> 1 980-line Python module that native `Read` flat-out refuses comes
> back as 1 781 tokens of structured signatures + `[DRIP-elided]`
> stubs — a 95 % reduction *on a file the agent otherwise couldn't
> open at all*.** The agent navigates the file's shape, then uses
> partial `Read(offset, limit)` to drill into specific bodies for
> editing. Threshold tunable via `DRIP_CLAUDE_READ_TOKEN_BUDGET`.

Live numbers from your own session are always one command away:

```bash
drip meter                            # cumulative since install
drip meter --session                  # current Claude/Codex/Gemini session only
drip meter --history                  # per-command savings over time
```

**Concrete dollar impact** at typical solo-dev usage (5 sessions/day,
22 work-days/month), **linearly extrapolated** from the multi-edit
refactor workload above (read the caveat in
[`BENCHMARKS.md section 3`](./BENCHMARKS.md#3-cost-projection) — this is
*not* a prediction of your real monthly invoice, just a back-of-envelope
sense of scale on file-read traffic):
**~\$50/month on Sonnet 4.6**, **~\$249/month on Opus 4.6**,
**~\$166/month on GPT-5/Codex**. To estimate your own case, run
`drip meter --history` against a real session and override the price
with `DRIP_PRICE_PER_MTOK=N`.

> ⚠ **Caveat — prompt caching cuts the headline.** The figures above
> assume the **full per-token price** for every read. If your agent
> uses Anthropic's [prompt caching](https://docs.claude.com/en/docs/build-with-claude/prompt-caching)
> (or any provider equivalent), repeated reads of the same file hit
> the cache at **~10% of the price**. In that world, DRIP's
> unchanged/delta savings stack on top of caching for a smaller
> additional gain — order **1/3 to 1/2 of the headline $ figure** in
> the typical case. DRIP still wins on first-read compression (large
> files past the 25k Read budget that the agent couldn't open at all)
> and on cross-session orientation, but the $ projection above is the
> **no-caching upper bound**, not the cache-aware one.

Full reproducible benchmarks (per-language compression, latency,
8-language workload, signature-preservation audit) live in
[`BENCHMARKS.md`](./BENCHMARKS.md).

State lives in a single SQLite file. There is no daemon, no network
call, no telemetry. For the design rationale (why per-session, why
SQLite, why `similar`, edge cases) see
[`ARCHITECTURE.md`](./ARCHITECTURE.md).

### What DRIP also does well

1. **Reads files Claude's `Read` tool can't.** Claude's `Read` refuses
   anything past ~25 000 tokens with `File content (X tokens) exceeds
   maximum allowed tokens (25000)` — files of that size are simply
   unreadable from the agent's side. DRIP detects the threshold before
   `process_read` even runs, routes through the `DripRendered` entry
   point so semantic compression executes (skipped on the native
   passthrough path by default), and substitutes the compressed view
   via `permissionDecision: deny`. The agent sees the file's full
   structure — signatures, class declarations, imports, plus
   `[DRIP-elided]` stubs for bodies — and then drills into specific
   regions with partial `Read(offset, limit)` reads (which populate
   the harness's edit-tracker for the window). Live numbers from a
   130 KB / 1 980-line Python fixture: native errored out; DRIP
   returns **1 781 tokens of structure (95 % reduction)** on a file
   that's otherwise totally unreadable. Threshold tunable via
   `DRIP_CLAUDE_READ_TOKEN_BUDGET` (default 10 000 DRIP tokens ≈
   24-26 k Claude tokens, since Claude's tokenizer runs ~2.5× tighter
   than DRIP's `bytes/4` heuristic). When compression isn't available
   (non-code file, raw data, `DRIP_NO_COMPRESS=1`) the hook falls back
   to `allow` so the native error still points the agent at
   `offset`/`limit`.

2. **Semantic compression on first reads.** 13 languages recognised
   — Python, Rust, JS/TS, Go, Java, C, C++, C#, Kotlin, Swift, Scala,
   PHP. Function bodies are elided, signatures + imports + class
   declarations preserved (verified zero structural loss on the 8
   benchmarked fixtures). Compression ratio varies by file density:
   measured 60 % on Python, 44 % on Go, 43 % on C++, 42 % on
   TypeScript, 38 % on Java, 36 % on Rust, 36 % on Kotlin, 33 % on
   C# — see
   [`BENCHMARKS.md section 1`](./BENCHMARKS.md#1-semantic-compression-on-first-reads)
   for the full per-language table. The C-family parser handles
   both K&R (`signature {`) and Allman (`signature` + lone-`{` line)
   brace styles plus attributes / primary constructors / records.
3. **Javadoc / KDoc / JSDoc compression.** Long doc blocks (≥ 6
   lines) collapse to summary + `@param` / `@return` / `@throws`
   tags, with a `[DRIP-javadoc-elided: N lines]` marker for the
   prose / examples that were removed. Disable with
   `DRIP_COMPRESS_JAVADOC=0`.
4. **`.dripignore`** — gitignore-style. Filters reads, glob results,
   and grep results at the source. Built-in defaults for lock files,
   `node_modules`, build artefacts, binaries, fonts, video.
5. **Edit certificates.** Read a file immediately after editing it
   and DRIP returns a compact `[DRIP: edit verified | hash: …]`
   attestation (file hash + touched line ranges + symbol names
   parsed from the diff, ~390 B) instead of letting the harness
   ship the full file. Disable with `DRIP_CERT_DISABLE=1`.
6. **Session keying — crash-resistant, branch-isolated.** Session id
   derives from `(cwd, git branch, worktree)` so a relaunched agent
   on the same branch reuses its prior baselines, and a branch
   switch isolates them.
7. **Cross-session file registry.** First reads in a new session
   carry an `↔ unchanged since last session` or `↕ changed since
   last session` orientation header so the agent doesn't restart
   blind.

---

## Install

### macOS

```bash
# Homebrew (recommended) — Apple Silicon + Intel
brew install drip-cli/drip/drip

# Or via the install script
curl -fsSL https://raw.githubusercontent.com/drip-cli/drip/main/install.sh | sh
```

### Linux

```bash
# Install script (recommended) — drops the binary into ~/.local/bin.
# Pulls the static musl build, which runs unmodified on every distro
# (glibc, Alpine, NixOS) without an interpreter mismatch.
curl -fsSL https://raw.githubusercontent.com/drip-cli/drip/main/install.sh | sh

# Or via Homebrew (linuxbrew)
brew install drip-cli/drip/drip
```

If `~/.local/bin` is not on your `PATH`, the script prints the
exact line to add to your shell rc.

### Windows

```powershell
# 1. Download the latest archive from
#    https://github.com/drip-cli/drip/releases/latest
#    (file: drip-x86_64-pc-windows-msvc.zip)
# 2. Extract `drip.exe` somewhere on your PATH, e.g. C:\Users\<you>\bin
# 3. Verify
drip --version
```

A native PowerShell installer is on the roadmap. WSL users can
follow the Linux instructions instead.

### Cross-platform (any OS with a Rust toolchain ≥ 1.74)

```bash
# From crates.io — installs the `drip` binary
cargo install drip-cli

# Or from source
git clone https://github.com/drip-cli/drip
cd drip
cargo install --path .
```

### Updating

```bash
drip update                          # auto-detects install method (brew / cargo / script)
drip update --dry-run                # show what would happen, don't run anything
```

`drip update` detects how DRIP is installed by inspecting the
binary path (`/opt/homebrew/`, `~/.cargo/bin/`, `~/.local/bin/`)
and runs the matching upgrade command. Already up-to-date is a
clean no-op.

### Uninstalling

First, remove DRIP's hooks from your agent — this preserves any
hand-edited content in `CLAUDE.md` / `AGENTS.md`:

```bash
drip uninstall                       # default: --agent claude (project-level)
drip uninstall -g                    # remove the global Claude hooks (~/.claude/)
drip uninstall --agent codex         # remove Codex CLI integration
drip uninstall --agent gemini        # remove Gemini CLI integration
```

Then remove the binary itself, depending on how you installed it:

```bash
brew uninstall drip                  # Homebrew install
brew untap drip-cli/drip             # also drop the tap if you're done with it
cargo uninstall drip-cli             # `cargo install drip-cli` install
rm ~/.local/bin/drip                 # install-script install
# Windows: delete drip.exe from wherever you put it on PATH
```

To purge cached state too (SQLite DB + on-disk file cache):

```bash
rm -rf ~/.local/share/drip                       # Linux
rm -rf "~/Library/Application Support/drip"      # macOS
# Windows (PowerShell): Remove-Item -Recurse -Force "$env:LOCALAPPDATA\drip"
```

(or set `DRIP_DATA_DIR=...` to relocate state — `drip doctor`
shows the current path).

---

## Quick start

Wire DRIP into your agent — one command, idempotent, non-destructive:

```bash
drip init                            # Claude Code, project-level   (./.claude/)
drip init -g                         # Claude Code, global          (~/.claude/)
drip init --agent codex              # Codex CLI                    (~/.codex/, always global — no project-level)
drip init --agent gemini             # Gemini CLI, project-level    (./.gemini/)
drip init -g --agent gemini          # Gemini CLI, global           (~/.gemini/)
```

That's it. The agent now goes through DRIP for every file read. You
never call `drip read` yourself.

Verify the install:

```bash
drip doctor                          # ✅ / ⚠️ / ❌ report on every component
drip meter                            # token savings since install
```

Remove cleanly:

```bash
drip uninstall                       # local
drip uninstall --global              # global
drip uninstall --agent codex         # one specific agent
```

`uninstall` only removes the bytes DRIP wrote — hand-edited config,
pre-existing hooks, and unrelated MCP servers are left intact.

---

## Commands

```bash
drip init [--agent <name>] [-g]      # Wire DRIP into an agent
drip uninstall [--agent <name>] [-g] # Remove DRIP from an agent
drip update [--dry-run]              # Upgrade DRIP via brew / cargo / script
drip doctor [--json] [--quiet]       # Audit the install
drip read <file>                     # Manual read (you'll rarely call this)
drip refresh <file>                  # Drop one file's baseline
drip reset                           # Wipe the current session (cheap, reversible)
drip reset --stats                   # Zero lifetime counters, keep sessions/baselines
drip reset --all [--force]           # Nuke EVERYTHING (sessions + lifetime + cache blobs)
drip sessions                        # List sessions with strategy + savings
drip meter [--session [id]] [--json]  # Token-savings report
drip meter --history | --graph        # Time-series view
drip meter --prune                    # Drop rows for files no longer on disk
drip replay [--full] [--since 5m]    # Chronological read log
drip watch [path]                    # Pre-compute diffs in the background
drip cache stats | gc | compact      # Storage hygiene
drip registry stats | gc             # Cross-session file registry
drip mcp                             # Run as MCP server (stdio)
drip completions <shell>             # Print shell completions
```

Detailed flag reference: `drip <command> --help`.

### What each command is for

| Command                    | When to reach for it                                                          |
|----------------------------|-------------------------------------------------------------------------------|
| `drip meter`                | Sanity-check that DRIP is paying off; share the % with your team              |
| `drip replay`              | Debug a "why did the agent see X?" moment — exact bytes the agent received    |
| `drip refresh <file>`      | A teammate's `git pull` landed; force the next read to be a clean full read   |
| `drip reset`               | Start a new task with zero memory of the previous one                         |
| `drip reset --stats`       | A bench polluted lifetime counters; zero them but keep working               |
| `drip reset --all`         | Wipe every session, baseline, blob, and lifetime counter (asks `yes` first) |
| `drip doctor`              | Something feels off. One paste = full triage                                  |
| `drip watch`               | Want changed tracked files diffed in the background before the next read      |
| `drip cache compact`       | DB size is growing — hoist large inline rows to the file cache + VACUUM       |

`drip meter` reports estimated file-read payload savings:

```text
DRIP Token Savings (Since Install)
══════════════════════════════════════════════════════════════════
Files tracked:     47
Total reads:       312
Files edited:      18  (62 edits)
Tokens full:       133.3K
Tokens sent:       30.5K
Tokens saved:      102.7K  (77 %)
$ saved:           $0.31  (@ $3.00 / Mtok)
CO₂ avoided:       41 g    (@ 0.40 g / Ktok)
Efficiency meter:  ████████████▒▒▒▒  77 %

Top Files
──────────────────────────────────────────────────────────────────
  1.  src/app.py            34 reads    8.20K saved   94 %  ▓▓▓▓▓▓▓▓▓▓
  2.  src/utils.py          12 reads    3.10K saved   81 %  ▓▓▓▓░░░░░░
  3.  config.toml            8 reads    1.20K saved  100 %  ▓▓░░░░░░░░
```

Token totals use DRIP's lightweight `bytes / 4` estimator and count the
payload DRIP substitutes (file bodies, diffs, certificates, registry
diff trailers). Tiny DRIP control headers are not included, so
`unchanged` means "zero file payload resent", not "zero literal
tokenizer cost".

---

## Configuration

Per-environment-variable. Defaults are sensible — most users never
touch these.

| Variable                       | Default                  | Effect                                                                  |
|--------------------------------|--------------------------|-------------------------------------------------------------------------|
| `DRIP_DISABLE`                 | unset                    | `=1` makes every hook a no-op (emergency bypass)                        |
| `DRIP_DATA_DIR`                | `~/.local/share/drip`    | Where the SQLite store lives                                            |
| `DRIP_SESSION_ID`              | derived                  | Pin a session id verbatim (top priority)                                |
| `DRIP_SESSION_STRATEGY`        | `git` in repo, else `pid`| Force `git` / `pid` / `cwd`                                             |
| `DRIP_SESSION_TTL_SECS`        | `7200`                   | Heartbeat-based session lifetime (floor `1800`)                         |
| `DRIP_NO_COMPRESS`             | unset                    | `=1` disables semantic compression on first reads                       |
| `DRIP_COMPRESS_MIN_BYTES`      | `1024`                   | Skip compression below this file size                                   |
| `DRIP_COMPRESS_MIN_BODY`       | `15`                     | Minimum function-body line count to elide (floor `4`)                   |
| `DRIP_MAX_HUNKS`               | `6`                      | Diff with more hunks falls back to a full re-read                       |
| `DRIP_MAX_CHANGED_PCT`         | `0.40`                   | Diff changing more than this fraction falls back to full                |
| `DRIP_INLINE_MAX_BYTES`        | `32768`                  | Above this, content goes to `cache/<hash>.bin` (`0` = always cache)     |
| `DRIP_IGNORE_FILE`             | unset                    | Explicit `.dripignore` path (overrides cwd / `~/`)                      |
| `DRIP_WATCH_RESCAN_MS`         | `1000`                   | Polling fallback interval for `drip watch` when FS events are missed    |
| `DRIP_REJECT_SYMLINKS`         | unset                    | `=1` refuses to follow symlinks                                         |
| `DRIP_WORKSPACE_ROOT`          | unset                    | MCP `read_file` refuses paths outside this directory                    |
| `DRIP_REGISTRY_DISABLE`        | unset                    | `=1` opts out of cross-session orientation                              |
| `DRIP_REPLAY_LOG`              | enabled                  | `=0` disables the `read_events` log                                     |
| `DRIP_REPLAY_KEEP`             | `500`                    | Per-session rolling cap on replay events                                |
| `DRIP_CHECK_UPDATES`           | unset                    | `=1` makes `drip doctor` check the GitHub Releases API for a newer tag  |
| `DRIP_UPDATE_FAKE_LATEST`      | unset                    | `=X.Y.Z` short-circuits the update check (used by tests / live demo)    |
| `DRIP_PRICE_PER_MTOK`          | `3.00`                   | USD per million input tokens (default = Sonnet 4.6)                     |
| `DRIP_CO2_G_PER_KTOK`          | `0.40`                   | Grams CO₂e per Ktok of input                                            |
| `NO_COLOR`                     | unset                    | Disables ANSI color codes                                               |
| `FORCE_COLOR`                  | unset                    | Forces color output even when piped                                     |

### `.dripignore`

Like `.gitignore`, but for things you never want fed to your agent
in full — lock files, `node_modules`, build artefacts, secrets.
DRIP applies it to file reads, glob results, and grep results.

Lookup order (highest priority first):

1. `$DRIP_IGNORE_FILE` — explicit path override
2. `./.dripignore` — project-level rules
3. `~/.dripignore` — user-level rules
4. Built-in defaults (always applied unless explicitly negated)

Built-in defaults cover `.git/**`, `node_modules/**`, `target/**`,
`dist/**`, `__pycache__/**`, all common lock files, and binary
artefacts (images, archives, fonts, video). Use `!pattern` to
re-include a default.

> **Pattern note.** Agents pass *absolute* paths to DRIP, so anchor
> patterns with `**/` when you want them to match anywhere
> (`**/secrets/**`). Bare basename patterns like `*.lock` already
> work everywhere.

---

## Compatibility

### Platforms

| Platform              | Status            |
|-----------------------|-------------------|
| macOS arm64           | ✅ first-class    |
| macOS x86_64          | ✅ first-class    |
| Linux x86_64          | ✅ first-class    |
| Linux arm64           | ✅ first-class    |
| WSL (any distro)      | ✅ first-class    |
| Windows x86_64        | ⚠️  best-effort   |

Windows specifics: `%APPDATA%\drip\sessions.db` for the store,
`%USERPROFILE%\.claude\settings.json` for hooks, brief retry loop on
`ERROR_SHARING_VIOLATION` during atomic writes. Branch isolation via
the `git` keying strategy works the same; the `pid` strategy
gracefully falls through to `cwd`.

### Agents

| Agent                  | Mode                                                  | Scope (default → `-g`)              | Auto-install              |
|------------------------|-------------------------------------------------------|-------------------------------------|---------------------------|
| Claude Code            | `PreToolUse` (Read, Glob, Grep) + `PostToolUse`       | `./.claude/` → `~/.claude/`         | `drip init`               |
| Codex CLI (OpenAI)     | MCP server (`read_file` tool)                         | always `~/.codex/` (no project-level) | `drip init --agent codex` |
| Gemini CLI             | MCP server (`read_file` tool)                         | `./.gemini/` → `~/.gemini/`         | `drip init --agent gemini`|

> **Codex specifics.** Codex CLI reads its config exclusively from
> `~/.codex/config.toml` — there's no project-level override path
> in the agent itself, so `drip init --agent codex` is always
> global. Passing `-g` is a no-op for this agent (silently
> accepted; ignored). Same for `drip uninstall --agent codex`.
>
> **Codex: compaction visibility.** Codex CLI does not yet expose a
> before-compress / on-compact hook upstream
> ([openai/codex#16098](https://github.com/openai/codex/issues/16098)).
> DRIP baselines therefore persist across Codex's context
> compactions — the agent will re-issue Reads and DRIP will
> short-circuit them with `[unchanged]` against a baseline the
> agent's tracker has lost. Workaround: run `drip reset` manually
> after a Codex compaction, or wait for the upstream hook to land.
> Claude Code (v9+) and Gemini CLI both wire this automatically via
> their respective hooks.

> **Why no Cursor?** Cursor is a VS Code-based IDE whose agent has a
> built-in `read_file` tool that bypasses MCP. Even with DRIP's MCP
> server registered, the agent reaches for its native read by
> default — DRIP would only see the calls the model explicitly
> routed through it, which is not the contract the rest of the
> agents get. We'd rather not advertise a half-working integration.

For Claude Code, `drip init` writes:

- 5 hooks in `~/.claude/settings.json` (or `./.claude/`)
- `drip.md` — the read-hint contract for the agent
- `@drip.md` line in `CLAUDE.md` so the agent loads that contract
  every session

For MCP-based agents, `drip init` registers the `drip mcp` server in
the agent's config (`~/.codex/config.toml` or
`~/.gemini/settings.json`) and appends a usage hint to the agent's
system-prompt file. Every write is idempotent and atomic
(tmp-file + `rename(2)`).

> **A note on Claude Code's "blocking error" wording.** When DRIP
> intercepts a re-read, the substituted output may show up prefixed
> with `PreToolUse:Read hook blocking error from command: …`. This
> is cosmetic — DRIP uses Claude Code's only currently-stable hook
> mechanism (`permissionDecision: "deny"` + `permissionDecisionReason`)
> to feed the delta back to the model. The model receives the diff
> as the tool result; the "blocking error" framing is the renderer,
> not an actual failure.

### Shell completions

`drip init` auto-installs completions for the detected `$SHELL`
(zsh / bash / fish). PowerShell and Elvish are emitted on stdout:

```bash
drip completions zsh   > ~/.zsh/completions/_drip
drip completions bash  > ~/.bash_completion.d/drip.bash
drip completions fish  > ~/.config/fish/completions/drip.fish
drip completions powershell    # add to $PROFILE
drip completions elvish        # add to ~/.config/elvish/rc.elv
```

---

## Privacy & security

DRIP runs entirely on your machine. **No network calls, no
telemetry, no third-party services** — verified on every release
by [`cargo machete`](https://crates.io/crates/cargo-machete) +
manual audit of the dependency graph. Everything tracked lives in
a single SQLite file under your platform's data dir:

| OS        | Path                                            |
|-----------|-------------------------------------------------|
| Linux     | `~/.local/share/drip/sessions.db`                |
| macOS     | `~/Library/Application Support/drip/sessions.db` |
| Windows   | `%LOCALAPPDATA%\drip\sessions.db`                |

Override with `DRIP_DATA_DIR=/your/path`. `drip doctor` prints the
current location.

| Concern                              | What DRIP does                                                                  |
|--------------------------------------|---------------------------------------------------------------------------------|
| File contents at rest                | Stored only in `sessions.db`, `chmod 0600` on Unix (parent dir `0700`)          |
| Secrets in files                     | Add patterns to `.dripignore` — DRIP returns a placeholder, never the content   |
| Symlink-redirect reads               | `DRIP_REJECT_SYMLINKS=1` refuses any symlinked path                             |
| MCP tool reaching outside a workspace| `DRIP_WORKSPACE_ROOT=/path/to/repo` refuses anything outside that root          |
| Memory / OOM on huge files           | Files > 50 MB short-circuited via `metadata().len()` *before* loading           |
| Atomic config edits                  | `drip init` writes settings via tmp-file + `rename(2)`                          |
| Existing file mode preservation      | `atomic_write` copies the target's Unix mode bits                               |
| Replay log secrets                   | Per-event content capped at 32 KB; `DRIP_REPLAY_LOG=0` disables it entirely     |
| Emergency bypass                     | `DRIP_DISABLE=1` short-circuits every hook to a no-op                           |

To wipe state: `drip reset` clears the current session;
`drip reset --all` (with a `yes` confirmation, or `--force` for
scripts) clears every session, baseline, cache blob, and lifetime
counter — equivalent to deleting the data dir from the table above
but safer because the schema gets rebuilt on the next call.

### Threat model: shared data dirs

DRIP's threat model assumes a **single trusted user owns the data dir**
(`sessions.db` is `chmod 0600`, parent `0700`). The `DRIP_SESSION_ID`
environment variable is treated as authoritative — anyone who can set
it can read or pollute the baselines of any session whose id they
guess.

That's fine for a normal developer workstation. It is **not** safe in
shared dev containers, multi-tenant Linux hosts where users share a
home directory, or CI runners that re-use a writable cache between
unrelated jobs. In those environments:

- give each tenant their own `DRIP_DATA_DIR`, **or**
- set `DRIP_DISABLE=1` for jobs that shouldn't see another tenant's
  reads, **or**
- accept the risk and treat session ids as untrusted (don't run DRIP
  against secrets-bearing files in such jobs).

DRIP does not authenticate session ids on purpose: every supported
agent passes one in as plain text and adding HMAC/signing would break
the "drop-in, zero-config" contract that makes the tool worth running.

### Replay log retains first-read content

`drip replay` captures the rendered output of every read so you can
reconstruct what the agent saw. For *first* reads of a file
(`FullFirst`), that rendered output **is the file's full content**,
which means a secret-bearing file that slips past `.dripignore` will
have its bytes sit in `read_events.rendered` until the keep-window
rolls them off (default 500 events) or you run `drip reset --all`.
The built-in dripignore patterns now cover the common shapes
(`.env*`, `*.pem`, `id_rsa*`, `~/.ssh/**`, `~/.aws/credentials`,
`.netrc`, `.npmrc`, `kubeconfig`, …); for project-specific secret
filenames (`secret.toml`, `creds.json`, …) add patterns to a project
`.dripignore`. To opt out of the replay capture entirely set
`DRIP_REPLAY_LOG=0`.

---

## Documentation

- [`BENCHMARKS.md`](./BENCHMARKS.md) — measured numbers, per-language
  compression rates, latency budgets, cost projections.
- [`ARCHITECTURE.md`](./ARCHITECTURE.md) — design rationale: why
  per-session, why SQLite, hook contract, edge-case decisions.
- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — setup, conventions, how
  to add support for a new agent or compression language.
- [`CHANGELOG.md`](./CHANGELOG.md) — release-by-release notes.
- [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md) — Contributor
  Covenant v2.1.

---

## Contributing

Issues and PRs welcome. Read [`CONTRIBUTING.md`](./CONTRIBUTING.md)
first — it covers setup, commit conventions, and how to add a new
agent or compression language.

Every PR needs:

- Tests for any behavioural change (`cargo test` must pass).
- Commit messages in [Conventional Commits](https://www.conventionalcommits.org/)
  format — CI lints them, and `release-please` parses them to bump
  the version automatically.
- A signed [Contributor License Agreement](./CLA.md) — the CLA-bot
  posts the sign-off instructions on your first PR. You only sign
  once.

---

## Team

Built by **[Perform Code SAS](https://drip-ai.app/en/legal/notice)** in
Lille, France.

|     | Founder          | Focus            | GitHub                                       |
| --- | ---------------- | ---------------- | -------------------------------------------- |
| 🎨  | Maxence Bombeeck | Designer & Swift | [@MaxenceB59](https://github.com/MaxenceB59) |
| 🦀  | Hugo Barbosa     | Core DRIP        | [@Hugobrbs](https://github.com/Hugobrbs)     |
| 🛠️  | Hugo Ponthieux   | DevOps & Infra   | [@Hugoy8](https://github.com/Hugoy8)         |

---

## License

[Apache-2.0](./LICENSE) © Perform Code SAS
