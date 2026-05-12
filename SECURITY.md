# Security Policy

## Supported Versions

DRIP is pre-1.0 and ships a single release line. Only the latest
released tag receives security fixes. If you depend on an older
tag, please pin to it explicitly and upgrade promptly when a
security advisory is published.

| Version | Supported          |
| ------- | ------------------ |
| `main` + latest release tag | ✅ |
| Older tags | ❌ no backports |

## Reporting a Vulnerability

**Please do NOT open a public GitHub issue for security problems.**

If you believe you have found a vulnerability in DRIP, report it
privately through one of the following channels:

- **Preferred** — GitHub Private Vulnerability Reporting:
  open [a security advisory](https://github.com/drip-cli/drip/security/advisories/new)
  on the repository. GitHub keeps the report confidential until a
  fix is published.
- **Alternative** — email **contact@drip-ai.app** with the
  subject `[DRIP security]`. PGP not currently required; mark the
  thread as confidential and we'll coordinate from there.

Please include:

- Affected DRIP version (`drip --version`) and platform.
- A description of the issue and the impact you've assessed.
- Reproduction steps or a minimal proof-of-concept if you have one.
- Any mitigation you've already tried.

## Response Timeline

DRIP is maintained by a small team; the project is hobby-time
serious. The targets below are best-effort, not contractual:

| Stage                                        | Target |
| -------------------------------------------- | ------ |
| Acknowledge receipt                          | 72 h   |
| Initial triage (severity + reproducibility)  | 1 week |
| Fix shipped or mitigation published          | 30 days (high/critical), 90 days (low/medium) |
| Public disclosure (after fix or by mutual agreement) | coordinated |

If the issue affects users on an actively-supported release we
will issue a patched version, update `CHANGELOG.md`, and credit
the reporter (unless they prefer anonymity).

## Scope

In scope:

- The `drip` binary (CLI, hook handlers, MCP server).
- The on-disk state (`sessions.db`, file-cache blobs, dripignore).
- Hook integrations (Claude Code, Codex, Gemini): how DRIP parses
  payloads and writes responses.
- The release artifacts (Homebrew formula, install script, GitHub
  release binaries) and the supply chain to produce them.

Out of scope:

- Vulnerabilities in upstream dependencies that we can only mitigate
  by waiting for the upstream patch — report those directly to the
  upstream project (we'll bump our pin once a fix lands). We track
  these via `cargo audit` in CI.
- Theoretical attacks that require an attacker already running with
  the user's privileges on the same machine where DRIP runs (DRIP's
  threat model assumes process-level trust on the local box).
- Issues that only affect setups bypassing DRIP's own safety guards
  (`DRIP_DISABLE=1`, hand-edited `sessions.db`).

## Hardening Already in Place

For context on the threat model and the choices we've already made:

- SQLite store and cache directory chmodded `0700` / `0600` to
  prevent co-tenant reads on shared boxes.
- Hash-addressed cache blobs (SHA-256), atomic tmp + rename writes.
- Symlink guard on the cache directory (`run_gc` skips and removes
  symlinks before counting bytes, blocking confused-deputy reads
  of arbitrary files via planted links).
- `.dripignore` boundary applied identically on read AND post-edit
  paths so secrets in ignored files (`.env`, credentials) are never
  persisted to `reads.content` or `file_registry`.
- FIFO / character-device guards in the read path (no DoS via
  blocking reads against `/dev/zero` and friends).
- Hard file-size cap (`HARD_SIZE_CAP_BYTES`) to refuse pathological
  inputs.
- No network calls. No telemetry. Single-process, no daemon.
- Conventional Commits enforced in CI — commit-message tampering is
  visible in `git log` and surfaces in the release CHANGELOG.

Thank you for helping keep DRIP and its users safe.
