#!/usr/bin/env bash
# DRIP non-read hook benchmarks — Glob filtering and Grep filtering.
#
# Methodology
# -----------
# Both hooks rely on `.dripignore` to remove paths / matches the agent
# doesn't need. We measure the byte-level reduction honestly:
#
#   - Glob:  count and byte-size of paths returned by `find` over a fixture
#            tree, with vs. without `.dripignore` exclusion rules applied.
#            DRIP's Glob hook applies these rules to the agent's tool
#            response before forwarding, so the savings here are a tight
#            proxy for what the agent receives on a real Glob call.
#
#   - Grep:  count and byte-size of lines returned by `rg <pattern>` over
#            the same tree, with vs. without `--glob '!…'` exclusions
#            built from the same `.dripignore`. Identical proxy
#            relationship to the Glob bench.
#
# (Bash pipeline interception was dropped in commit 97a2204 — the
# `drip hook claude-bash` subcommand no longer exists. Earlier
# revisions of this script also benched a `Bash` row; that section
# has been removed since the underlying feature is gone.)
#
# We do NOT measure: long-form multi-file agent sessions, cross-session
# behaviour, or anything that would require running a real agent
# end-to-end. Those are tracked under "future benchmarks" in
# BENCHMARKS.md section 7.
#
# Output: scripts/bench_output/non_read_hooks.json. Re-run with:
#   bash scripts/bench_non_read_hooks.sh
#   python3 scripts/generate_benchmarks_md.py > BENCHMARKS.md

set -euo pipefail

# Sample sizing — see scripts/bench_multilang.sh for rationale.
LATENCY_SAMPLES="${LATENCY_SAMPLES:-50}"
LATENCY_WARMUP="${LATENCY_WARMUP:-5}"

# Cross-platform contract:
#   - Linux (any distro):     direct.
#   - macOS:                  direct (BSD coreutils suffice; see notes
#                             about `zcat` in the gzip section below).
#   - WSL / WSL2 on Windows:  direct.
#   - Git Bash / MSYS2:       direct as long as `gzip`, `python3`,
#                             `ripgrep` are on PATH. `ln -s` is auto-
#                             detected and falls back to `cp -f`.
#   - cmd.exe / PowerShell:   not supported. Run from one of the bash
#                             environments above.

# Friendly required-tool check before the bench burns time building
# the project tree.
missing=()
for cmd in python3 gzip awk seq head tail wc cat tr; do
  command -v "$cmd" >/dev/null 2>&1 || missing+=("$cmd")
done
if (( ${#missing[@]} > 0 )); then
  echo "fatal: required tools missing from PATH: ${missing[*]}" >&2
  echo "  install via your platform's package manager (apt / brew /" >&2
  echo "  winget / pacman) or run this script from inside WSL." >&2
  exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE_DIR="$ROOT/scripts/bench_fixtures"
OUTPUT_DIR="$ROOT/scripts/bench_output"
# DRIP binary path. Honour `$DRIP` env override (CI uses this) and
# tolerate the Windows `.exe` suffix on Git Bash so the script doesn't
# try to rebuild a binary that's already there under a different name.
DRIP="${DRIP:-$ROOT/target/release/drip}"
if [[ ! -x "$DRIP" && -x "${DRIP}.exe" ]]; then
  DRIP="${DRIP}.exe"
fi

if [[ ! -x "$DRIP" ]]; then
  echo "Building release binary..." >&2
  (cd "$ROOT" && cargo build --release >/dev/null)
fi

# Isolate the bench's DRIP state from the user's real data dir.
# Without this, every run leaves dozens of synthetic session ids in
# `~/.local/share/drip/sessions.db` until the 2 h TTL expires, which
# pollutes `drip sessions` and `drip meter` until then. Honour an
# explicit override so CI can pin the dir if needed. The cleanup
# trap is set further down once `$WORK` also exists.
export DRIP_DATA_DIR="${DRIP_DATA_DIR:-$(mktemp -d)}"

# Some environments alias `rg` to grep / a wrapper that doesn't accept
# ripgrep flags. Resolve to the on-disk binary so the bench is
# unaffected.
RG=$(command -v rg 2>/dev/null || true)
if [[ -z "$RG" || ! -x "$RG" ]]; then
  echo "rg (ripgrep) is required for the Grep benchmark — please install it." >&2
  exit 1
fi
# Sanity-check that we're really invoking ripgrep, not grep.
if ! "$RG" --version 2>/dev/null | head -1 | grep -q '^ripgrep'; then
  echo "warning: 'rg' on PATH ($RG) doesn't look like ripgrep — searching common install locations" >&2
  # Mac/Linux/WSL paths first; Git Bash / MSYS2 paths last so the
  # script also resolves rg from a `winget install BurntSushi.ripgrep`
  # default install or a Chocolatey one without the user having to
  # tweak PATH.
  for cand in \
      /opt/homebrew/bin/rg \
      /usr/local/bin/rg \
      /usr/bin/rg \
      "/c/Program Files/ripgrep/rg.exe" \
      "$HOME/scoop/apps/ripgrep/current/rg.exe" \
      "$LOCALAPPDATA/Microsoft/WinGet/Packages/BurntSushi.ripgrep.MSVC_Microsoft.Winget.Source_8wekyb3d8bbwe/rg.exe"; do
    if [[ -x "$cand" ]] && "$cand" --version 2>/dev/null | head -1 | grep -q '^ripgrep'; then
      RG="$cand"
      break
    fi
  done
  if ! "$RG" --version 2>/dev/null | head -1 | grep -q '^ripgrep'; then
    echo "fatal: cannot locate the real ripgrep binary." >&2
    echo "  install via:  brew install ripgrep   (Mac)" >&2
    echo "                apt install ripgrep    (Debian/Ubuntu)" >&2
    echo "                winget install BurntSushi.ripgrep.MSVC   (Windows)" >&2
    exit 1
  fi
fi

mkdir -p "$OUTPUT_DIR"

# ---------------------------------------------------------------------------
# Build a synthetic project tree
# ---------------------------------------------------------------------------
# Layout (everything under $WORK):
#   src/                 — symlinks to all bench_fixtures source files
#   .git/                — fake git directory (always ignored)
#   target/              — fake Rust build dir (ignored)
#   node_modules/        — fake JS deps (ignored)
#   build/               — fake build artefacts (ignored)
#   Cargo.lock           — lock file (ignored)
#   package-lock.json    — lock file (ignored)
#   pnpm-lock.yaml       — lock file (ignored)
#
# The .dripignore at the root of the work tree contains the standard
# ignore patterns: lock files, .git, target, node_modules, build.

WORK=$(mktemp -d)
# Combined cleanup: both the bench's transient WORK tree and the
# isolated DRIP data dir created above. Bash's `trap … EXIT` only
# keeps the most recent handler, so we re-set it here once both
# paths exist.
trap 'rm -rf "$WORK" "$DRIP_DATA_DIR"' EXIT

mkdir -p "$WORK/src" "$WORK/.git/objects" "$WORK/target/debug" \
         "$WORK/target/release" "$WORK/node_modules/lodash" \
         "$WORK/node_modules/react/dist" "$WORK/build"

# Real source files (the bench fixtures). On Mac/Linux/WSL we
# symlink so byte counts reflect the real fixture sizes; on Git
# Bash / MSYS2 / Windows-without-developer-mode `ln -s` either
# silently copies or refuses, so we fall back to `cp` and the
# numbers stay equally honest. The bench measures DRIP's behaviour
# on a project tree, not the filesystem — both shapes are valid.
have_symlink_support=1
if ! ln -sf "$FIXTURE_DIR/$(ls "$FIXTURE_DIR" | head -1)" "$WORK/.symlink-probe" 2>/dev/null; then
  have_symlink_support=0
fi
rm -f "$WORK/.symlink-probe" 2>/dev/null || true

for f in "$FIXTURE_DIR"/*; do
  base=$(basename "$f")
  case "$base" in
    *.dripignore|.*) continue ;;
  esac
  if (( have_symlink_support )); then
    ln -sf "$f" "$WORK/src/$base"
  else
    cp -f "$f" "$WORK/src/$base"
  fi
done

# Fake noise — a few KB each, enough to dominate the byte total for
# unfiltered Glob/Grep results.
seq 1 200 | awk '{print "blob " NR " " $0 $0 $0 $0}' > "$WORK/.git/objects/pack-deadbeef.idx"
seq 1 500 | awk '{print "DEBUG line " NR ": " $0 $0 $0}' > "$WORK/target/debug/build.log"
seq 1 500 | awk '{print "RELEASE artefact " NR ": " $0 $0 $0}' > "$WORK/target/release/build.log"
seq 1 800 | awk '{print "function noop_" NR "() { return " NR "; }"}' > "$WORK/node_modules/lodash/index.js"
seq 1 500 | awk '{print "var X" NR " = " NR ";"}' > "$WORK/node_modules/react/dist/react.production.min.js"
seq 1 300 | awk '{print "BUILD step " NR " complete"}' > "$WORK/build/manifest.txt"

# Lock files at the project root.
seq 1 400 | awk '{print "[[package]]\nname = \"crate" NR "\"\nversion = \"0.1." NR "\""}' > "$WORK/Cargo.lock"
seq 1 600 | awk '{print "  \"dep" NR "\": { \"version\": \"1.0." NR "\", \"resolved\": \"https://example.com/" NR "\" }"}' > "$WORK/package-lock.json"
seq 1 400 | awk '{print "/" NR "@1.0." NR ":\n  resolution: hash" NR}' > "$WORK/pnpm-lock.yaml"

# Bench `.dripignore` — same patterns DRIP installs by default, written
# inline so the bench is self-contained.
cat > "$WORK/.dripignore" <<'EOF'
.git/
target/
node_modules/
build/
*.lock
package-lock.json
pnpm-lock.yaml
EOF

# Build the equivalent rg --glob exclusion list. ripgrep's syntax differs
# slightly from globset, but for these patterns the result is identical.
RG_EXCLUDES=(
  --glob '!**/.git/**'
  --glob '!**/target/**'
  --glob '!**/node_modules/**'
  --glob '!**/build/**'
  --glob '!*.lock'
  --glob '!**/*.lock'
  --glob '!**/package-lock.json'
  --glob '!**/pnpm-lock.yaml'
)

# `find` exclusion via -prune for the unignored count. -L follows
# symlinks so the bench_fixtures sources we linked into src/ are
# counted as files (otherwise -type f skips them).
find_filtered() {
  find -L "$WORK" \
    \( -path '*/.git' -o -path '*/target' -o -path '*/node_modules' -o -path '*/build' \) -prune \
    -o -type f \
    \( ! -name '*.lock' ! -name 'package-lock.json' ! -name 'pnpm-lock.yaml' \) \
    -print
}

# ---------------------------------------------------------------------------
# Glob benchmark — `find . -type f` over the synthetic tree
# ---------------------------------------------------------------------------

glob_raw=$(find -L "$WORK" -type f)
glob_filtered=$(find_filtered)

glob_raw_count=$(printf '%s\n' "$glob_raw" | wc -l | tr -d ' ')
glob_filtered_count=$(printf '%s\n' "$glob_filtered" | wc -l | tr -d ' ')
glob_raw_bytes=$(printf '%s\n' "$glob_raw" | wc -c | tr -d ' ')
glob_filtered_bytes=$(printf '%s\n' "$glob_filtered" | wc -c | tr -d ' ')
glob_filtered_paths=$((glob_raw_count - glob_filtered_count))

# Latency: 10 runs of each variant.
measure_glob_latency() {
  local mode="$1"
  python3 - "$WORK" "$mode" <<'PY'
import os, subprocess, sys, time
work, mode = sys.argv[1], sys.argv[2]
def run():
    if mode == "raw":
        return subprocess.run(["find", "-L", work, "-type", "f"],
                              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                              check=False).stdout
    else:
        return subprocess.run(
            ["find", "-L", work,
             "(", "-path", "*/.git", "-o", "-path", "*/target",
             "-o", "-path", "*/node_modules", "-o", "-path", "*/build", ")",
             "-prune", "-o", "-type", "f",
             "(", "!", "-name", "*.lock",
                  "!", "-name", "package-lock.json",
                  "!", "-name", "pnpm-lock.yaml", ")", "-print"],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            check=False).stdout
import os
N = int(os.environ.get("LATENCY_SAMPLES", "50"))
W = int(os.environ.get("LATENCY_WARMUP", "5"))
samples = []
for _ in range(N):
    t0 = time.perf_counter()
    run()
    samples.append((time.perf_counter() - t0) * 1000)
samples = samples[W:] if len(samples) > W else samples
samples.sort()
def pick(p):
    n = len(samples)
    return samples[max(0, min(n - 1, int(round(p * (n - 1)))))]
print(f"{pick(0.5):.2f} {pick(0.95):.2f} {pick(0.99):.2f}")
PY
}

read glob_p50 glob_p95 glob_p99 <<<"$(measure_glob_latency filtered)"

# ---------------------------------------------------------------------------
# Grep benchmark — `rg "fn|def|func|function|public " -l`
# ---------------------------------------------------------------------------
# Pattern intentionally matches lines in BOTH ignored and unignored files
# (lock files and node_modules/lodash will hit on `function`).

GREP_PATTERN='\b(fn|def|func|function|public)\b'

# Counterfactual ("raw") baseline: ripgrep with every ignore source
# disabled, so we measure what an unfiltered tool-call result looks
# like. The "filtered" run keeps ripgrep's normal behaviour but adds
# DRIP's `.dripignore`-derived `--glob '!…'` rules; the comparison
# isolates the savings DRIP attributes to its own filter step.
grep_raw=$("$RG" --no-heading --line-number --no-ignore --hidden --follow \
              --glob '!**/.git/**' \
              "$GREP_PATTERN" "$WORK" 2>/dev/null || true)
grep_filtered=$("$RG" --no-heading --line-number --no-ignore --hidden --follow \
                   "${RG_EXCLUDES[@]}" \
                   "$GREP_PATTERN" "$WORK" 2>/dev/null || true)

grep_raw_count=$(printf '%s' "$grep_raw" | wc -l | tr -d ' ')
grep_filtered_count=$(printf '%s' "$grep_filtered" | wc -l | tr -d ' ')
grep_raw_bytes=$(printf '%s' "$grep_raw" | wc -c | tr -d ' ')
grep_filtered_bytes=$(printf '%s' "$grep_filtered" | wc -c | tr -d ' ')
grep_filtered_matches=$((grep_raw_count - grep_filtered_count))

# Latency: 10 runs.
read grep_p50 grep_p95 grep_p99 <<<"$(python3 - "$WORK" "$GREP_PATTERN" "$RG" <<'PY'
import subprocess, sys, time
work, pat, rg = sys.argv[1], sys.argv[2], sys.argv[3]
excludes = ["--no-ignore", "--hidden", "--follow"]
for p in ["**/.git/**","**/target/**","**/node_modules/**","**/build/**",
          "*.lock","**/*.lock","**/package-lock.json","**/pnpm-lock.yaml"]:
    excludes += ["--glob", "!" + p]
import os
N = int(os.environ.get("LATENCY_SAMPLES", "50"))
W = int(os.environ.get("LATENCY_WARMUP", "5"))
samples = []
for _ in range(N):
    t0 = time.perf_counter()
    subprocess.run([rg, "--no-heading", "--line-number", *excludes, pat, work],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    samples.append((time.perf_counter() - t0) * 1000)
samples = samples[W:] if len(samples) > W else samples
samples.sort()
def pick(p):
    n = len(samples)
    return samples[max(0, min(n - 1, int(round(p * (n - 1)))))]
print(f"{pick(0.5):.2f} {pick(0.95):.2f} {pick(0.99):.2f}")
PY
)"

# (Bash pipeline interception was dropped in 97a2204; the bench section
# that exercised `drip hook claude-bash` has been removed alongside it.)

# ---------------------------------------------------------------------------
# Emit one combined JSON document
# ---------------------------------------------------------------------------
python3 - "$OUTPUT_DIR" \
  "$glob_raw_count" "$glob_filtered_count" "$glob_filtered_paths" \
  "$glob_raw_bytes" "$glob_filtered_bytes" \
  "$glob_p50" "$glob_p95" "$glob_p99" \
  "$grep_raw_count" "$grep_filtered_count" "$grep_filtered_matches" \
  "$grep_raw_bytes" "$grep_filtered_bytes" \
  "$grep_p50" "$grep_p95" "$grep_p99" <<'PY'
import json, os, sys
out_dir = sys.argv[1]
(grcnt, gfcnt, gfpath, grbyt, gfbyt, gp50, gp95, gp99,
 rcnt, fcnt, fmatch, rbyt, fbyt, rp50, rp95, rp99) = sys.argv[2:]
def saved_pct(big, small):
    big = int(big); small = int(small)
    return int(round(100 * (1 - small / big))) if big else 0
result = {
    "glob": {
        "scenario": "find -type f over a synthetic project tree (sources + .git/ + target/ + node_modules/ + build/ + 3 lock files)",
        "raw_paths": int(grcnt),
        "filtered_paths": int(gfcnt),
        "paths_dropped": int(gfpath),
        "raw_bytes": int(grbyt),
        "filtered_bytes": int(gfbyt),
        "reduction_pct": saved_pct(grbyt, gfbyt),
        "latency_ms": {"p50": float(gp50), "p95": float(gp95), "p99": float(gp99)},
    },
    "grep": {
        "scenario": "rg '\\b(fn|def|func|function|public)\\b' over the same tree",
        "raw_matches": int(rcnt),
        "filtered_matches": int(fcnt),
        "matches_dropped": int(fmatch),
        "raw_bytes": int(rbyt),
        "filtered_bytes": int(fbyt),
        "reduction_pct": saved_pct(rbyt, fbyt),
        "latency_ms": {"p50": float(rp50), "p95": float(rp95), "p99": float(rp99)},
    },
}
path = os.path.join(out_dir, "non_read_hooks.json")
with open(path, "w") as f:
    json.dump(result, f, indent=2)
print(f"Wrote {path}")
PY

echo
echo "Done. Combined non-read benchmark output: $OUTPUT_DIR/non_read_hooks.json"
