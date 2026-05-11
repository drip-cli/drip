#!/usr/bin/env bash
# DRIP multi-language benchmark — measures token savings on production-grade
# fixtures across 4 realistic agent workflows per language.
#
# Workflows per file:
#   A. Explore   — first read + 1 unchanged re-read           (2 reads)
#   B. Debug     — first read + 4 unchanged re-reads          (5 reads)
#   C. Edit      — first read + edit + delta + unchanged      (4 reads)
#   D. Multi-edit— first read + 3 (edit + delta) cycles       (7 reads)
#
# Output: per-language JSON in scripts/bench_output/, plus a combined
# all_results.json. No averages are written here — averaging happens in
# generate_benchmarks_md.py so methodology stays in one place.
#
# Usage:
#   bash scripts/bench_multilang.sh                  # all languages
#   LANGS=python bash scripts/bench_multilang.sh     # just Python
#   LANGS="python rust" bash scripts/bench_multilang.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE_DIR="$ROOT/scripts/bench_fixtures"
OUTPUT_DIR="$ROOT/scripts/bench_output"
DRIP="${DRIP:-$ROOT/target/release/drip}"

# Number of latency samples per outcome and warmup discards. Defaults
# chosen so a single-run bench is statistically meaningful: at 10
# samples the median's stdev across runs is ~equal to the median itself
# (system noise dominates); at 50 with 5 warmup discards it drops
# below ~1 ms.
LATENCY_SAMPLES="${LATENCY_SAMPLES:-50}"
LATENCY_WARMUP="${LATENCY_WARMUP:-5}"

# Always rebuild — cargo no-ops if target/release is in sync with src/,
# and catches the case where the user benches against a stale binary
# after an unrelated `git checkout`.
echo "Ensuring release binary is up to date..." >&2
(cd "$ROOT" && cargo build --release >/dev/null)

mkdir -p "$OUTPUT_DIR"

# Isolate the bench's DRIP state. Without this, every run leaves
# 80+ synthetic session ids (10 latency-sample sessions × 8
# languages, plus per-workflow ids) in the user's real
# `~/.local/share/drip/sessions.db`. They expire after the 2 h TTL,
# but in the meantime `drip sessions` becomes useless and `drip
# meter` mixes bench artefacts with real usage. Honour an explicit
# DRIP_DATA_DIR if the caller passed one (CI / release pipelines).
export DRIP_DATA_DIR="${DRIP_DATA_DIR:-$(mktemp -d)}"
trap 'rm -rf "$DRIP_DATA_DIR"' EXIT

# Language registry: lang|extension|fixture_basename
ALL_LANGS=(
  "python|py|pricing_engine"
  "rust|rs|session_manager"
  "typescript|ts|api_client"
  "java|java|UserRepository"
  "go|go|http_handler"
  "cpp|cpp|json_parser"
  "csharp|cs|OrderService"
  "kotlin|kt|DataRepository"
)

# Filter via $LANGS
SELECTED=("${ALL_LANGS[@]}")
if [[ -n "${LANGS:-}" ]]; then
  SELECTED=()
  for entry in "${ALL_LANGS[@]}"; do
    IFS='|' read -r lang ext base <<<"$entry"
    for want in $LANGS; do
      [[ "$lang" == "$want" ]] && SELECTED+=("$entry")
    done
  done
fi

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# Run drip in a fresh isolated session, return only the stdout (the rendered
# read result — exactly what the agent would receive).
run_read() {
  local sid="$1" file="$2"
  DRIP_SESSION_ID="$sid" "$DRIP" read "$file"
}

# Pull the meter JSON for a session.
meter_json() {
  local sid="$1"
  DRIP_SESSION_ID="$sid" "$DRIP" meter --session --json
}

# Reset a session's tracked reads.
reset_session() {
  local sid="$1"
  DRIP_SESSION_ID="$sid" "$DRIP" reset >/dev/null 2>&1 || true
}

# Fire the PostToolUse:Edit hook for (sid, file). Used by workflow C
# (Edit) so the next read exercises DRIP's edit-certificate path —
# without this call, a raw `cp` on disk doesn't notify DRIP and the
# read falls through delta instead of the cert.
run_post_edit() {
  local sid="$1" file="$2"
  local payload
  payload=$(python3 -c '
import json,sys
print(json.dumps({"session_id":sys.argv[1],"tool_name":"Edit",
                  "tool_input":{"file_path":sys.argv[2]}}))' "$sid" "$file")
  printf '%s' "$payload" | DRIP_SESSION_ID="$sid" "$DRIP" hook claude-post-edit >/dev/null
}

# Median of integers/floats from stdin (one per line).
median() {
  python3 -c '
import sys
xs = sorted(float(l) for l in sys.stdin if l.strip())
n = len(xs)
if n == 0:
    print(0); sys.exit(0)
mid = xs[n // 2] if n % 2 else (xs[n // 2 - 1] + xs[n // 2]) / 2
print(f"{mid:.2f}")
'
}

# Latency (ms) of a single drip read invocation. stderr is silenced.
time_one_read() {
  local sid="$1" file="$2"
  python3 -c '
import subprocess, sys, time
sid, drip, fp = sys.argv[1], sys.argv[2], sys.argv[3]
t0 = time.perf_counter()
subprocess.run([drip, "read", fp], env={**__import__("os").environ, "DRIP_SESSION_ID": sid},
               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
print(f"{(time.perf_counter() - t0) * 1000:.3f}")
' "$sid" "$DRIP" "$file"
}

# ---------------------------------------------------------------------------
# Per-language benchmark
# ---------------------------------------------------------------------------

bench_language() {
  local lang="$1" ext="$2" base="$3"
  local v1="$FIXTURE_DIR/${base}.${ext}"
  local v2="$FIXTURE_DIR/${base}_v2.${ext}"
  local out="$OUTPUT_DIR/${lang}.json"

  if [[ ! -f "$v1" ]] || [[ ! -f "$v2" ]]; then
    echo "  skip $lang — fixture missing ($v1 or $v2)" >&2
    return 0
  fi

  local size_v1 lines_v1
  size_v1=$(wc -c < "$v1" | tr -d ' ')
  lines_v1=$(wc -l < "$v1" | tr -d ' ')
  local size_v2 lines_v2
  size_v2=$(wc -c < "$v2" | tr -d ' ')
  lines_v2=$(wc -l < "$v2" | tr -d ' ')

  echo "  $lang: $base.$ext ($lines_v1 lines, $size_v1 B)" >&2

  local nonce ts
  nonce=$$-$RANDOM
  ts=$(date +%s)

  # Working copy so we can swap v1↔v2 without touching the fixture
  local work
  work=$(mktemp -d)
  trap 'rm -rf "$work"' RETURN
  local file="$work/${base}.${ext}"
  cp "$v1" "$file"

  # ----- Workflow A : Explore (2 reads) ------------------------------------
  local sid_a="bench-${lang}-explore-${nonce}"
  reset_session "$sid_a"
  run_read "$sid_a" "$file" >/dev/null
  run_read "$sid_a" "$file" >/dev/null
  local stats_a
  stats_a=$(meter_json "$sid_a")

  # ----- Workflow B : Debug (5 reads, all unchanged) -----------------------
  local sid_b="bench-${lang}-debug-${nonce}"
  reset_session "$sid_b"
  for _ in 1 2 3 4 5; do
    run_read "$sid_b" "$file" >/dev/null
  done
  local stats_b
  stats_b=$(meter_json "$sid_b")

  # ----- Workflow C : Edit (4 reads with one edit) -------------------------
  # Each `cp` simulates an Edit landing on disk; we now fire the
  # PostToolUse:Edit hook immediately after so the next read goes
  # through DRIP's certificate path instead of the bare delta path.
  local sid_c="bench-${lang}-edit-${nonce}"
  reset_session "$sid_c"
  cp "$v1" "$file"
  run_read "$sid_c" "$file" >/dev/null    # 1: full
  cp "$v2" "$file"
  run_post_edit "$sid_c" "$file"
  run_read "$sid_c" "$file" >/dev/null    # 2: edit-cert
  run_read "$sid_c" "$file" >/dev/null    # 3: unchanged (after cert)
  cp "$v1" "$file"
  run_post_edit "$sid_c" "$file"
  run_read "$sid_c" "$file" >/dev/null    # 4: edit-cert back
  local stats_c
  stats_c=$(meter_json "$sid_c")

  # ----- Workflow D : Multi-edit (7 reads, 3 edit cycles) ------------------
  local sid_d="bench-${lang}-multiedit-${nonce}"
  reset_session "$sid_d"
  cp "$v1" "$file"
  run_read "$sid_d" "$file" >/dev/null
  for cycle in 1 2 3; do
    if (( cycle % 2 )); then cp "$v2" "$file"; else cp "$v1" "$file"; fi
    run_read "$sid_d" "$file" >/dev/null
    run_read "$sid_d" "$file" >/dev/null
  done
  local stats_d
  stats_d=$(meter_json "$sid_d")

  # ----- Latency: warmup-discarded LATENCY_SAMPLES per outcome -------------
  # We take LATENCY_SAMPLES samples and discard the first LATENCY_WARMUP.
  # Warmup catches the cold-cache + page-fault tax on the first 1-3 reads;
  # without it, p50 jumps when the bench runs sandwiched between other CPU
  # work. Defaults: 50 samples, drop 5.
  cp "$v1" "$file"
  local sid_lat="bench-${lang}-latency-${nonce}"
  reset_session "$sid_lat"
  local first_samples=()
  for ((i=1; i<=LATENCY_SAMPLES; i++)); do
    local s="bench-${lang}-lat-first-${nonce}-${i}"
    reset_session "$s"
    first_samples+=( "$(time_one_read "$s" "$file")" )
  done
  reset_session "$sid_lat"
  run_read "$sid_lat" "$file" >/dev/null  # prime
  local unchanged_samples=()
  for ((i=1; i<=LATENCY_SAMPLES; i++)); do
    unchanged_samples+=( "$(time_one_read "$sid_lat" "$file")" )
  done
  local delta_samples=()
  local sid_delta="bench-${lang}-lat-delta-${nonce}"
  reset_session "$sid_delta"
  cp "$v1" "$file"
  run_read "$sid_delta" "$file" >/dev/null  # prime
  for ((i=1; i<=LATENCY_SAMPLES; i++)); do
    if (( i % 2 )); then cp "$v2" "$file"; else cp "$v1" "$file"; fi
    delta_samples+=( "$(time_one_read "$sid_delta" "$file")" )
  done

  # Centralised stats helper: drops the first LATENCY_WARMUP samples,
  # emits "p50 p95 p99" on one line. Computing all three in one Python
  # invocation cuts the per-language Python startup tax from 9× to 3×.
  pcts() {
    LATENCY_WARMUP="$LATENCY_WARMUP" python3 -c '
import sys, os
warmup = int(os.environ["LATENCY_WARMUP"])
xs = [float(l) for l in sys.stdin if l.strip()]
xs = xs[warmup:] if len(xs) > warmup else xs
xs.sort()
n = len(xs)
def pick(p):
    idx = max(0, min(n - 1, int(round(p * (n - 1)))))
    return xs[idx]
print(f"{pick(0.50):.2f} {pick(0.95):.2f} {pick(0.99):.2f}")
'
  }

  local first_p50 first_p95 first_p99 unc_p50 unc_p95 unc_p99 d_p50 d_p95 d_p99
  read -r first_p50 first_p95 first_p99 < <(printf "%s\n" "${first_samples[@]}" | pcts)
  read -r unc_p50   unc_p95   unc_p99   < <(printf "%s\n" "${unchanged_samples[@]}" | pcts)
  read -r d_p50     d_p95     d_p99     < <(printf "%s\n" "${delta_samples[@]}" | pcts)

  # ----- First-read compression: capture the actual rendered output -------
  cp "$v1" "$file"
  local sid_comp="bench-${lang}-comp-${nonce}"
  reset_session "$sid_comp"
  local rendered
  rendered=$(run_read "$sid_comp" "$file")
  local rendered_bytes
  rendered_bytes=$(printf '%s' "$rendered" | wc -c | tr -d ' ')

  # Save the rendered first-read so the signature verifier can re-check
  printf '%s' "$rendered" > "$OUTPUT_DIR/${lang}_first_read.txt"

  # Combine into one JSON document
  python3 - "$lang" "$ext" "$base" "$lines_v1" "$size_v1" "$rendered_bytes" \
    "$first_p50" "$first_p95" "$first_p99" \
    "$unc_p50" "$unc_p95" "$unc_p99" \
    "$d_p50" "$d_p95" "$d_p99" \
    "$LATENCY_SAMPLES" "$LATENCY_WARMUP" \
    "$stats_a" "$stats_b" "$stats_c" "$stats_d" <<'PY' > "$out"
import json, sys
(lang, ext, base, lines_v1, size_v1, rendered_bytes,
 first_p50, first_p95, first_p99,
 unc_p50, unc_p95, unc_p99,
 d_p50, d_p95, d_p99,
 lat_samples, lat_warmup,
 a, b, c, d) = sys.argv[1:]
out = {
    "language": lang,
    "fixture": f"{base}.{ext}",
    "lines": int(lines_v1),
    "bytes": int(size_v1),
    "first_read_rendered_bytes": int(rendered_bytes),
    "workflows": {
        "explore":     json.loads(a),
        "debug":       json.loads(b),
        "edit":        json.loads(c),
        "multi_edit":  json.loads(d),
    },
    "latency_ms": {
        "first_read":  {"p50": float(first_p50), "p95": float(first_p95), "p99": float(first_p99)},
        "unchanged":   {"p50": float(unc_p50),   "p95": float(unc_p95),   "p99": float(unc_p99)},
        "delta":       {"p50": float(d_p50),     "p95": float(d_p95),     "p99": float(d_p99)},
        # Effective sample count after warmup discard. Consumers (e.g.
        # generate_benchmarks_md.py) read this so the published prose
        # stays in sync with the actual methodology.
        "samples_per_outcome": max(int(lat_samples) - int(lat_warmup), 0),
        "raw_samples":         int(lat_samples),
        "warmup_dropped":      int(lat_warmup),
    },
}
print(json.dumps(out, indent=2))
PY

  echo "    -> $out" >&2
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

echo "DRIP multi-language benchmark"
echo "  binary:   $DRIP ($("$DRIP" --version))"
echo "  fixtures: $FIXTURE_DIR"
echo "  output:   $OUTPUT_DIR"
echo

if [[ ${#SELECTED[@]} -eq 0 ]]; then
  echo "no languages selected (LANGS=...)" >&2
  exit 1
fi

for entry in "${SELECTED[@]}"; do
  IFS='|' read -r lang ext base <<<"$entry"
  bench_language "$lang" "$ext" "$base"
done

# Combine into one all_results.json
python3 - "$OUTPUT_DIR" <<'PY' > "$OUTPUT_DIR/all_results.json"
import glob, json, os, sys
out_dir = sys.argv[1]
results = []
for path in sorted(glob.glob(os.path.join(out_dir, "*.json"))):
    if os.path.basename(path) == "all_results.json":
        continue
    with open(path) as f:
        results.append(json.load(f))
print(json.dumps(results, indent=2))
PY

echo
echo "Done. Combined output: $OUTPUT_DIR/all_results.json"
