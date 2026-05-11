# Contributing to DRIP

Thanks for your interest. DRIP is small and pragmatic — most contributions land in under 200 lines. This guide tells you how to set up, what conventions to follow, and how to get a PR merged quickly.

## Code of conduct

This project follows the [Contributor Covenant v2.1](https://www.contributor-covenant.org/version/2/1/code_of_conduct/). By participating you agree to abide by its terms.

---

## Setup

You need a stable Rust toolchain (`>= 1.74`) and a Unix shell. Windows users: WSL2 works.

```bash
git clone https://github.com/drip-cli/drip
cd DRIP
cargo build
cargo test
```

`rusqlite` is pulled with the `bundled` feature, so you do **not** need a system SQLite. First build is ~45 s; incremental builds are sub-second.

### Run a single test

```bash
cargo test diff_accuracy::                 # by module
cargo test modified_file_returns_unified   # by name
```

### Try your local build against a real read

```bash
cargo build --release
DRIP_DATA_DIR=/tmp/drip-dev DRIP_SESSION_ID=dev \
  ./target/release/drip read README.md
```

---

## Benchmarks

Run on the **release** binary, never debug.

```bash
bash scripts/bench_reddit.sh               # realistic 4-language workload
cargo test --release diff_perf::           # 50 KB / 99 KB diff perf assertions
cargo test concurrency::                   # 15-process SQLite contention
```

`bench_reddit.sh` builds a synthetic Python / Rust / Java / TypeScript project, simulates a typical 4-read-per-file agent loop, and emits a markdown table. Those numbers feed [`BENCHMARKS.md`](./BENCHMARKS.md) — re-run them when you change the diff path or session storage.

---

## Commit conventions

DRIP uses [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/). CI runs `commitlint` on every PR — non-conforming commits block the merge.

Subject line in imperative mood, ≤ 100 chars, no trailing period.

```
feat:    a new user-facing feature
fix:     a bug fix
perf:    a performance improvement (back it with bench numbers)
refactor: a non-behavioural code change
docs:    documentation only
test:    test-only changes
chore:   build, CI, tooling
```

Examples:

```
feat: add --json output to drip meter
fix: respect offset/limit on Claude Read hook
perf: use SHA-256 streaming for files > 1 MB
docs: clarify Codex AGENTS.md instruction
```

The CHANGELOG is generated from these — sloppy subjects make sloppy release notes.

---

## Pull request flow

1. Fork and branch from `develop`. Branch name: `feat/<short-name>` or `fix/<short-name>`.
2. Add or update tests. Every behavioural change needs a test — no exceptions.
3. Run the full suite: `cargo test`. Verify formatting + lints: `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`. CI enforces all three.
4. If you changed perf-sensitive code, paste the relevant numbers from `scripts/bench_reddit.sh` in the PR description.
5. Open the PR against `develop`. Use the template.
6. Sign the [CLA](./CLA.md) on your first PR — the bot posts instructions. You only sign once.

`CHANGELOG.md` is generated automatically by `release-please` from your commit messages — don't edit it by hand.

PRs are reviewed within a few days. We'll either merge, request changes, or explain why we can't take it.

---

## Adding support for a new agent

Most agents fit one of three contracts. Pick the one that matches yours:

### A. The agent supports pre-tool hooks (Claude Code style)

1. Add a module under `src/hooks/<agent>.rs` that:
   - Parses the agent's hook payload from stdin.
   - Extracts the file path from whichever field the agent uses.
   - Calls `tracker::process_read(&session, &file_path)`.
   - Returns the agent's accept/reject envelope on stdout.
2. Add a `HookAgent::<Name>` variant in `src/commands/hook.rs` and route it.
3. Wire `drip init --agent <name>` in `src/commands/init.rs` to write the agent's settings file (idempotently).
4. Add an integration test under `tests/integration/<agent>_hook.rs` that drives the binary against a fake payload.

### B. The agent supports MCP

There's nothing to add — DRIP already exposes a `read_file` MCP tool via `drip mcp`. Just teach `drip init --agent <name>` to register the server in the agent's config and append a usage instruction to its system-prompt file.

### C. The agent supports neither

Fall back to the generic stdin proxy: pipe `{"file_path": "..."}` into `drip hook gemini` (or add a sibling). Document the wiring in the README.

In all three cases: bias toward zero typing for the user. If `drip init --agent <name>` doesn't make the agent transparent, the integration isn't done.

---

## What we won't merge

- Code with no tests (with one exception: pure documentation PRs).
- Refactors that don't change behaviour and don't make the next change easier.
- New dependencies without a clear performance or correctness reason. The whole binary is < 5 MB; we'd like to keep it that way.
- Features for hypothetical future use cases. Build for the agent in front of you.

If you're unsure whether something fits, open an issue first — we'll tell you in a sentence whether it's worth your time.

---

## Releasing (maintainers only)

Releases are driven by [release-please](.github/workflows/release-please.yml). Day-to-day, you don't tag manually — the workflow opens a release PR every time `main` accumulates new Conventional Commits, and merging that PR creates the matching `vX.Y.Z` tag. The tag fans out through three downstream jobs:

1. `release.yml` builds the 5-platform binary matrix (Linux x86_64/arm64 musl, macOS x86_64/arm64, Windows x86_64), attaches archives + `SHA256SUMS` to the GitHub Release.
2. `release.yml`'s `update-homebrew` job sha256s the freshly-published assets and pushes a refreshed `Formula/drip.rb` to [`drip-cli/homebrew-drip`](https://github.com/drip-cli/homebrew-drip) — `brew upgrade` is then live within minutes.
3. `release.yml`'s `publish-crates` job runs `cargo publish --locked` so the new version lands on [crates.io](https://crates.io/crates/drip-cli) for `cargo install drip-cli` users. The binary is named `drip` via `[[bin]]` (the crate is published as `drip-cli` because the bare `drip` name is squatted by an unrelated v0.0.0 placeholder).

### Required repository secrets

| Secret | Scope | Purpose |
|---|---|---|
| `HOMEBREW_TAP_TOKEN` | `Contents: read & write` on `drip-cli/homebrew-drip` | Lets `update-homebrew` push the regenerated formula. Skipped (with a CI warning) when unset, so forks build cleanly. |
| `CARGO_REGISTRY_TOKEN` | `publish-new` + `publish-update` on `drip-cli` (do NOT include `change-owners`, `yank`, or `trusted-publishing`) | Lets `publish-crates` run `cargo publish` on tagged releases. Skipped (with a CI warning) when unset, so forks build cleanly. |

`HOMEBREW_TAP_TOKEN` — generate at <https://github.com/settings/tokens?type=beta>, scope it to a single repo (the tap), set Contents: read+write, and add under **Repo → Settings → Secrets and variables → Actions**.

`CARGO_REGISTRY_TOKEN` — create at <https://crates.io/settings/tokens>, scope to crate `drip-cli` with only the two `publish-*` permissions, set a sensible expiry (e.g. 365 days), and add as the same Actions secret name.
