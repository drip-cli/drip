# Benchmarks

This document collects measured numbers for DRIP: token savings on
realistic agent workloads, per-language compression rates, latency
budgets, and cost projections.

**All numbers are from real measurements** on production-grade source
files (roughly 500–850 lines each, 8 languages). Nothing is hand-tuned or
rounded favourably — if a workflow saves 31 %, the table prints 31 %.
Re-run any time:

```bash
cargo build --release
bash scripts/bench_multilang.sh                   # token savings + latency
bash scripts/verify_signatures.sh                 # signature preservation
python3 scripts/generate_benchmarks_md.py > BENCHMARKS.md
```

The fixtures live in `scripts/bench_fixtures/`; the JSON dumps in
`scripts/bench_output/`. Both are tracked in git so you can verify
the numbers without rerunning.

---
## Summary

Measured on 8 production-grade fixtures (roughly 500–850 lines each), 4
single-file agent workflows per language, 45 effective latency samples
(50 raw, 5 warmup discarded) per operation. Workflow rows show the **aggregate-by-tokens** ratio (the
sum-of-all-fixtures ratio that the cost projection in section 5
also uses); the per-fixture range is shown in parentheses.

| Metric                                                       | Value                                                            |
|--------------------------------------------------------------|------------------------------------------------------------------|
| Languages tested                                             | 8 of 8                                                            |
| First-read semantic compression                              | **42% simple average across fixtures** (35–62 % per fixture) — **41% aggregate by tokens** |
| Explore workflow (1 read + 1 same-session unchanged re-read) | **70% aggregate** (67–80 % per fixture)                                |
| Debug workflow (1 read + 4 same-session unchanged re-reads)  | **88% aggregate** (86–92 % per fixture)                                |
| Edit workflow (read + edit-cert + unchanged + edit-cert)     | **83% aggregate** (80–89 % per fixture)                                |
| Multi-edit workflow (3 edit cycles, 7 reads)                 | **48% aggregate** (47–51 % per fixture)                                |
| Glob hook (`.dripignore` on a synthetic noisy tree)         | **33% saved** (paths filtered, see section 3)                 |
| Grep hook (`.dripignore` on a synthetic noisy tree)         | **67% saved** (matches filtered, see section 3)               |
| Post-edit verification certificate (see section 4)         | **99% saved aggregate** (209,001 → 3,098 B across all 8 fixtures) |
| Latency tail (worst p99 across 45 samples per outcome)       | **59.6 ms**                                              |
| Memory (max RSS, 1 MB file)                                  | ~10 MB (constant)                                                |
| Signature / import / type preservation                       | **100 %** (all 8 fixtures, see section 7)                              |

DRIP's largest win is **avoiding repeated context reinjection of
files the agent has already seen**. First-read semantic compression
saves about **42%** on this fixture set; workflows that re-read
the same file save substantially more (Debug aggregates
**88%**) because subsequent reads return a minimal
unchanged sentinel rather than the full file content. The Edit row
measures DRIP's post-edit verification certificate path: when the
agent immediately re-reads a file after an edit, DRIP returns a
compact attestation (hash + touched ranges, ~390 B) instead of
reinjecting the full file. The Multi-edit row captures a broader
refactor loop where repeated edits + reads still cost more than a
pure unchanged sentinel, but each post-edit re-read still rides the
cert path. Both numbers are published as-measured rather than
massaged to inflate the headline.

---
## 1. Semantic compression on first reads

Reduction comes from signature-preserving elision: function bodies
are replaced with `{ ... }` while every signature, doc-comment,
import, and type/class declaration stays visible. The compressor is
conservative — when in doubt it keeps the body inline rather than
risk mangling output.

| Language   | Fixture                | Lines | Bytes   | Tokens (full → sent) | Reduction | Elided   | Hidden    |
|------------|------------------------|------:|--------:|---------------------:|----------:|---------:|----------:|
| Python     | `pricing_engine.py` |   731 |  27,096 B |  6,774 →  2,575 | ** 62 %** |   15 fns |  426 lines |
| Rust       | `session_manager.rs` |   582 |  22,915 B |  5,729 →  3,727 | ** 35 %** |    6 fns |  218 lines |
| TypeScript | `api_client.ts` |   646 |  26,109 B |  6,528 →  4,057 | ** 38 %** |    6 fns |  278 lines |
| Java       | `UserRepository.java` |   744 |  30,106 B |  7,527 →  4,666 | ** 38 %** |    8 fns |  292 lines |
| Go         | `http_handler.go` |   569 |  20,595 B |  5,149 →  2,822 | ** 45 %** |    9 fns |  285 lines |
| C++        | `json_parser.cpp` |   691 |  23,984 B |  5,996 →  3,549 | ** 41 %** |    8 fns |  250 lines |
| C#         | `OrderService.cs` |   659 |  27,728 B |  6,932 →  4,340 | ** 37 %** |    7 fns |  248 lines |
| Kotlin     | `DataRepository.kt` |   840 |  30,260 B |  7,565 →  4,860 | ** 36 %** |    8 fns |  297 lines |

Variance across languages is real and expected: short bodies are kept
inline, dense docstring/JSDoc files compress harder, languages with
heavier ceremony (Java annotations, C# attributes) reduce less in
percentage terms because the headers are themselves verbose. None of
that is hidden — every value above is read directly from the
rendered first-read output that the agent would receive.

---
## 2. Token savings — four agent workflows

Each workflow runs independently in its own DRIP session, with the
same fixture file as the only file the agent touches. Tokens are
DRIP's `bytes / 4` estimator (see section 10); the percentages are derived
quantities from that estimator and should be read as **trends on
this fixture set**, not as a guarantee about any specific
tokenizer's exact savings.

### Workflow A — Explore (2 reads)

First read + 1 unchanged re-read **in the same DRIP session**. Tests the same-session unchanged path: DRIP recognises that the second read sees byte-identical content to the first and responds with a minimal unchanged sentinel — no file content is reinjected. Cross-session behaviour is *not* exercised here — both reads share `DRIP_SESSION_ID`.

| Language   | Reads | Without DRIP | With DRIP | Saved |
|------------|------:|-------------:|----------:|------:|
| Python     |     2 |     13,548 |      2,684 | ** 80 %** |
| Rust       |     2 |     11,458 |      3,836 | ** 67 %** |
| TypeScript |     2 |     13,056 |      4,163 | ** 68 %** |
| Java       |     2 |     15,054 |      4,776 | ** 68 %** |
| Go         |     2 |     10,298 |      2,929 | ** 72 %** |
| C++        |     2 |     11,992 |      3,656 | ** 70 %** |
| C#         |     2 |     13,864 |      4,447 | ** 68 %** |
| Kotlin     |     2 |     15,130 |      4,968 | ** 67 %** |

### Workflow B — Debug (5 reads)

First read + 4 unchanged re-reads in the same session. Simulates the agent re-reading a single module while debugging.

| Language   | Reads | Without DRIP | With DRIP | Saved |
|------------|------:|-------------:|----------:|------:|
| Python     |     5 |     33,870 |      2,806 | ** 92 %** |
| Rust       |     5 |     28,645 |      3,958 | ** 86 %** |
| TypeScript |     5 |     32,640 |      4,282 | ** 87 %** |
| Java       |     5 |     37,635 |      4,901 | ** 87 %** |
| Go         |     5 |     25,745 |      3,051 | ** 88 %** |
| C++        |     5 |     29,980 |      3,778 | ** 87 %** |
| C#         |     5 |     34,660 |      4,569 | ** 87 %** |
| Kotlin     |     5 |     37,825 |      5,090 | ** 87 %** |

### Workflow C — Edit cycle (4 reads, 1 edit)

Read → edit (swap to v2, fire PostToolUse:Edit hook) → re-read (edit certificate) → re-read (unchanged) → revert (swap back to v1, fire hook again) → re-read (edit certificate). The cert path replaces what would otherwise be a native full-file shipment every time the post-edit re-read fires; `DRIP_CERT_DISABLE=1` reverts the workflow to the legacy passthrough path (each post-edit re-read then ships the full file natively, with `tokens_sent = tokens_full` accounted to match what the agent sees).

| Language   | Reads | Without DRIP | With DRIP | Saved |
|------------|------:|-------------:|----------:|------:|
| Python     |     4 |     27,500 |      2,938 | ** 89 %** |
| Rust       |     4 |     23,182 |      4,113 | ** 82 %** |
| TypeScript |     4 |     27,858 |      4,553 | ** 84 %** |
| Java       |     4 |     28,306 |      5,206 | ** 82 %** |
| Go         |     4 |     20,768 |      3,329 | ** 84 %** |
| C++        |     4 |     24,626 |      3,981 | ** 84 %** |
| C#         |     4 |     28,366 |      4,742 | ** 83 %** |
| Kotlin     |     4 |     27,742 |      5,545 | ** 80 %** |

### Workflow D — Multi-edit (7 reads, 3 edit cycles)

First read + 3 (edit + 2 re-reads) cycles. Simulates a refactor session where the agent reads, modifies, and re-reads the same file repeatedly.

| Language   | Reads | Without DRIP | With DRIP | Saved |
|------------|------:|-------------:|----------:|------:|
| Python     |     7 |     48,226 |     23,495 | ** 51 %** |
| Rust       |     7 |     40,635 |     21,374 | ** 47 %** |
| TypeScript |     7 |     49,188 |     25,576 | ** 48 %** |
| Java       |     7 |     49,085 |     25,642 | ** 48 %** |
| Go         |     7 |     36,387 |     18,633 | ** 49 %** |
| C++        |     7 |     43,256 |     22,371 | ** 48 %** |
| C#         |     7 |     49,800 |     25,966 | ** 48 %** |
| Kotlin     |     7 |     47,919 |     25,230 | ** 47 %** |

---
## 3. Non-read hooks: Glob, Grep

DRIP doesn't only intercept `Read`. Its `.dripignore`-aware Glob and
Grep hooks filter the agent's tool-call output. Both are measured
on a **synthetic noisy project tree** built by the bench (sources +
`.git/` + `target/` + `node_modules/` + `build/` + lock files).
Treat these as **representative scenarios** — Glob/Grep savings
depend on how much of your real repo matches the `.dripignore`
patterns.

| Hook  | Scenario                                            | Without DRIP | With DRIP | Detail | Saved |
|-------|-----------------------------------------------------|-------------:|----------:|--------|------:|
| Glob  | find -type f over a synthetic project tree (sources + .git/ + target/ + node_modules/ + build/ + 3 lock files) | 2,427 B (paths 28) | 1,632 B (paths 19) | 9 paths dropped | **33%** |
| Grep  | rg '\b(fn\|def\|func\|function\|public)\b' over the same tree | 157,877 B (1,146 matches) | 51,801 B (346 matches) | 800 matches dropped | **67%** |

Glob and Grep rows are a single tool-call result. Every DRIP byte
count is read live from the binary — no modeling.

**Latency** (45 effective samples per operation — 50 raw, 5 warmup
discarded; same methodology as section 6):

| Hook | Scenario | p50 (ms) | p95 (ms) | p99 (ms) |
|------|----------|---------:|---------:|---------:|
| Glob | filtered `find`             | 1.74 | 2.23 | 2.68 |
| Grep | filtered `rg` over the tree | 4.25 | 5.52 | 5.57 |

p50 / p95 were stable in local reruns; p99 has more variance and
is reported as a tail indicator, not a guarantee.

**Reading the numbers honestly.** Glob and Grep are real token-level
savings via `.dripignore` filtering. Their magnitude depends almost
entirely on how much of your repo lives under ignored paths
(`node_modules`, `target`, `.git`, lock files). The synthetic tree
above is loaded with that noise on purpose; an "all-source" repo
with a near-empty `.dripignore` would save much less.

Re-run with `bash scripts/bench_non_read_hooks.sh`. Raw output:
`scripts/bench_output/non_read_hooks.json`.

---
## 4. Post-edit certificates

When an agent edits a file and then immediately reads it back to verify
the change (a "must Read before Edit" pattern enforced by some tool
harnesses), DRIP returns a compact `[DRIP: edit verified | hash: …]`
certificate instead of letting the read fall through to a native
full-file shipment. The certificate carries the file hash, the touched
line ranges parsed from the diff hunks, and a refresh hint.

Without the certificate path, the post-edit re-read sends the entire
file contents (the harness ignores DRIP's deny-as-substitute responses
in this narrow window). With it on, the agent gets a few hundred bytes
of attestation. Disable with `DRIP_CERT_DISABLE=1`; window via
`DRIP_CERT_WINDOW_SECS` (default 300).

| Language   | Fixture | Without DRIP | With cert | Saved |
|------------|---------|-------------:|----------:|------:|
| Python     | `pricing_engine.py` | 27,122 B | 389 B | **99%** |
| Rust       | `session_manager.rs` | 22,941 B | 391 B | **98%** |
| TypeScript | `api_client.ts` | 26,135 B | 381 B | **99%** |
| Java       | `UserRepository.java` | 30,132 B | 393 B | **99%** |
| Go         | `http_handler.go` | 20,621 B | 385 B | **98%** |
| C++        | `json_parser.cpp` | 24,010 B | 385 B | **98%** |
| C#         | `OrderService.cs` | 27,754 B | 385 B | **99%** |
| Kotlin     | `DataRepository.kt` | 30,286 B | 389 B | **99%** |
| **Aggregate** | — | 209,001 B | 3,098 B | **99%** |

The bench drives the actual hooks (`drip hook claude-post-edit` then
`drip hook claude`) so the numbers reflect what the agent really
sees. Re-run with `bash scripts/bench_post_edit.sh`. Raw output:
`scripts/bench_output/post_edit_cert.json`.

---
## 5. Cost projection

> ⚠ **This is a linear projection for the measured file-read workload,
> not a prediction of total invoice savings.** In real sessions,
> tokens outside the measured file-read workload — system prompt,
> assistant output, test logs, lint output, build chatter, and other
> unmeasured tool output — dilute the percentage saved on the
> overall bill. Treat the figures below as a back-of-envelope sense
> of scale on file-read traffic only.

Extrapolating the **multi-edit** workflow (364,496 tokens
without DRIP, 188,287 tokens with DRIP — 48 % saved
across all 8 languages combined) to a solo developer
running 5 sessions / day, 22 work-days / month:

| Model                  | Price         | Without DRIP | With DRIP | Saved / month |
|------------------------|---------------|-------------:|----------:|--------------:|
| Claude Sonnet 4.6      | \$ 3.00/Mtok | \$ 120.28 | \$  62.13 | **\$  58.15** |
| Claude Opus 4.6        | \$15.00/Mtok | \$ 601.42 | \$ 310.67 | **\$ 290.74** |
| Claude Haiku 4.5       | \$ 1.00/Mtok | \$  40.09 | \$  20.71 | **\$  19.38** |
| GPT-5 (Codex)          | \$10.00/Mtok | \$ 400.95 | \$ 207.12 | **\$ 193.83** |
| Gemini 2.5 Pro         | \$ 2.50/Mtok | \$ 100.24 | \$  51.78 | **\$  48.46** |

To estimate your own case, point `drip meter --history` at a real
session — it surfaces dollar savings using whatever
`DRIP_PRICE_PER_MTOK` you configure, against your actual mix of
unchanged / delta / first-read traffic. The table above only
exists to give a back-of-envelope sense of scale.

---
## 6. Latency

End-to-end wall time including Rust process startup. The internal
DRIP work (DB lookup + diff) measures < 1 ms; the rest is the
roughly-flat ~5 ms cost of spawning the binary.

> **Sampling note.** Each cell is computed from **45 samples** (50 raw, first 5 discarded as warmup) per language × operation. p50 and p95 were stable in local reruns; p99 over 45 samples still has noticeable variance and should be read as a tail indicator, not a guarantee.

| Language   | First read (p50 / p95 / p99) | Unchanged (p50 / p95 / p99) | Delta (p50 / p95 / p99) |
|------------|-----------------------------:|----------------------------:|------------------------:|
| Python     |  5.37 /  6.72 /  7.25 |  5.08 /  5.84 /  6.27 |  5.41 /  5.96 /  9.75 |
| Rust       |  5.64 /  6.81 /  7.25 |  5.73 /  7.89 / 13.29 |  6.01 / 11.31 / 59.63 |
| TypeScript |  5.83 /  8.70 / 10.60 |  5.43 /  6.26 /  6.36 |  5.77 /  6.43 /  6.45 |
| Java       |  6.02 /  6.76 / 10.42 |  5.46 /  8.39 / 15.11 |  6.14 /  6.85 /  7.13 |
| Go         |  5.91 /  6.49 /  8.16 |  5.53 /  6.27 /  7.11 |  6.09 /  6.69 /  7.06 |
| C++        |  6.30 /  6.98 / 12.43 |  5.77 /  6.54 /  6.61 |  6.06 /  8.02 /  8.42 |
| C#         |  6.44 /  7.99 /  8.93 |  5.89 /  6.74 / 10.60 |  6.18 /  6.89 /  7.50 |
| Kotlin     |  7.03 / 10.47 / 23.58 |  5.91 /  6.65 /  8.32 |  6.88 /  7.82 /  8.28 |

All values in milliseconds. **Worst observed tail across every
language and operation: 59.63 ms** (out of 1080
samples total — 45 per outcome × 3 outcomes × 8 languages).
Medians cluster around 6–7 ms, p95s around 7–11 ms — the regime
that actually matters for the perceived hook latency. Numbers
were taken on macOS arm64 (Apple Silicon); we have not run an
equivalent multi-language bench on Linux x86_64, so cross-platform
deltas are not claimed here.

---
## 7. Signature / import / type preservation — 100 %

For every fixture the verifier extracts signature lines, type
declarations (`class`/`struct`/`enum`/`interface`/`trait`/`record`),
and imports from the original file, then asserts each one appears
**verbatim** (modulo leading whitespace) in DRIP's rendered
first-read output.

| Language   | Signatures | Type decls | Imports | Result |
|------------|-----------:|-----------:|--------:|:------:|
| Python     | 20/20 | 10/10 | 9/9 | ✅ |
| Rust       | 19/19 | 8/8 | 12/12 | ✅ |
| TypeScript | 18/18 | 10/10 | 4/4 | ✅ |
| Java       | 52/52 | 4/4 | 54/54 | ✅ |
| Go         | 16/16 | 9/9 | 1/1 | ✅ |
| C++        | 36/36 | 8/8 | 14/14 | ✅ |
| C#         | 12/12 | 14/14 | 8/8 | ✅ |
| Kotlin     | 37/37 | 11/11 | 13/13 | ✅ |

**All languages pass: every function/method signature, every type declaration, and every import line in the original file appears verbatim in the rendered first-read output.**

**What this check does and doesn't claim.** It shows that the
agent's *structural surface* — every callable name, every type
declaration, every import — appears verbatim in the rendered
output. It does **not** claim that no information was lost:
function bodies are deliberately elided behind a `[DRIP-elided:
N lines, run drip refresh for full]` placeholder. If the agent
needs the elided code, it asks for it — `drip refresh <file>`
re-serves the full content on the next read. Run the verifier
yourself with `bash scripts/verify_signatures.sh`.

---
## 8. Current benchmark scope

This document deliberately stays small and reproducible. Here is
what the numbers above actually cover, and — explicitly — what
they don't.

**What this benchmark measures.**
- Single-file, same-session agent workflows (Explore / Debug /
  Edit / Multi-edit) on 8 production-grade fixtures.
- First-read semantic compression (raw bytes → rendered bytes).
- Same-session unchanged re-reads, where DRIP returns a minimal
  unchanged sentinel and **does not reinject file contents**.
- Same-session delta size after a real edit (v1 ↔ v2 swaps).
- Hook + process latency, 45 samples per operation (50 raw, 5 warmup discarded).
- Verbatim preservation of every signature / type declaration /
  import line (section 7).
- Non-read hooks: Glob and Grep `.dripignore` filtering (section 3).
- Post-edit verification certificates, where an immediate read after an edit is replaced by a compact hash + range attestation (section 4).

**What it does *not* measure yet.**
- Real-world sessions: long-running, multi-file, multi-day workflows
  on a real agent (Claude Code / Codex / Gemini). DRIP is too
  young — we want to ship the tool first, then publish those
  numbers when they exist.
- Cross-session behaviour: the cross-session registry is exercised
  by integration tests but is *not* benchmarked here (every
  workflow uses a single, isolated `DRIP_SESSION_ID`).
- Compression of agent-side outputs (test runner output, build
  logs, lint output, other non-read tool results) — not covered by
  this benchmark.
- Multi-agent / parallel workflows.
- Task success rate (does the agent reach the goal faster, with
  fewer wrong turns?). This is the most interesting question to
  answer next; it requires running a real benchmark like SWE-bench
  with and without DRIP, which is on the roadmap and not done yet.
- Statistically robust latency (p99 from 1,000+ samples) — see
  the sampling note in section 6.

These aren't fatal gaps; they're the next benchmarks we want to
publish, in priority order. **None of the numbers in this file
should be read as a claim about any of the items in the second
list.**

---
## 9. Reproducibility

Everything in this file is generated from raw measurements committed
to the repo. To re-run from scratch:

```bash
git clone https://github.com/drip-cli/drip
cd drip
cargo build --release

# Token savings + latency  (~30 s on Apple Silicon)
bash scripts/bench_multilang.sh

# Signature preservation
bash scripts/verify_signatures.sh

# Non-read hooks: Glob / Grep filtering  (~15 s)
bash scripts/bench_non_read_hooks.sh

# Post-edit verification certificates  (~10 s)
bash scripts/bench_post_edit.sh

# Regenerate this file
python3 scripts/generate_benchmarks_md.py > BENCHMARKS.md
```

To rerun a single language:

```bash
LANGS=python bash scripts/bench_multilang.sh
LANGS=python bash scripts/verify_signatures.sh
```

Per-language raw output lives in `scripts/bench_output/<lang>.json`
(with `<lang>_first_read.txt` showing the literal bytes DRIP returns
on a first read).

---
## 10. Methodology notes

**Token estimator.** DRIP uses `bytes / 4` (rounded up) so the
benchmark is portable and doesn't pin to any one model's
tokenizer. The headline percentages should be read as **relative
trends on this fixture set**, not as guaranteed savings against a
specific tokenizer. Real BPE tokenizers (`cl100k_base`, GPT-5,
Claude, Gemini) vary in how they split punctuation, identifiers,
and whitespace, which can move the absolute numbers by
single-digit percentages in either direction. A future version
may add per-model tokenizer plug-ins for tighter accounting; we
haven't shipped that yet.

**DB state.** Each workflow runs in a fresh, named DRIP session
(`DRIP_SESSION_ID=bench-...`) so cross-session registry effects
don't bleed between benchmarks. The session is reset between
workflows; latency samples use independent sessions for first-read
to avoid interference from cached baselines.

**Process startup overhead.** A non-trivial fraction of the measured
wall-time is Rust binary startup (~5 ms). The internal diff path is
~1 ms. Future work: optional daemon mode behind a feature flag.

**Hardware.** Latency was taken on macOS arm64 (Apple Silicon).
We have not yet run an equivalent multi-language bench on Linux
x86_64; the older single-language `scripts/bench.sh` tracked
within ±15 % on the same operations, but that's not a measurement
of *this* fixture set and shouldn't be quoted as one.

**Where DRIP doesn't help much.**
- *Tiny files* (< 50 lines): function bodies are too short to elide;
  a re-read still hits the unchanged-path savings, but first-read
  compression is near zero.
- *Files in constant churn*: every read produces a new diff. DRIP
  still wins over the no-cache counterfactual but the gap narrows.
- *Languages where bodies are dominated by signatures themselves*
  (heavy Java annotations, dense XML doc): the percentage reduction
  on first read is lower than for languages with longer bodies.

**Why first-read reduction varies by language.**
Even after compressor tuning, languages with verbose structural
surface — annotations, generics, doc comments, imports, or
compact bodies — have less removable body text as a percentage of
the file. *Rust*, *TypeScript*, *Java*, *C#*, and *Kotlin* all
cluster in the mid-30s % range because their *signatures* consume
a large fraction of the total bytes: even when bodies elide
cleanly the maximum achievable reduction is lower than for *Python*
or *Go* where docstring-heavy bodies dominate. Re-read savings
(workflows B–D) are unaffected — the unchanged path is independent
of first-read elision.

**Workload limits.** The 4 workflows here are micro-benchmarks
constructed to be reproducible in seconds. Real agent sessions
combine all four patterns and add Edit / Write / Glob / Grep
traffic that DRIP's other hooks intercept (`drip meter --history`
shows the live mix on your machine). We do **not** claim that
real-session numbers will be equal to or better than what's
tabulated here — that claim requires running DRIP on long-form
agent traces, which is on the roadmap (see section 8) and not done yet.

---

*Generated by `scripts/generate_benchmarks_md.py` from JSON dumps in
`scripts/bench_output/`. Last run: see git log.*
