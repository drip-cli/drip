#!/usr/bin/env bash
# DRIP realistic-workload benchmark.
#
# Simulates a 5-iteration coding session against 10 files: each iteration
# tweaks 5 of them then re-reads all 10. With baseline behaviour every
# read sends the full file; with DRIP, only the first read is full and
# subsequent reads are deltas (or "unchanged").
#
# Outputs a Reddit-ready summary at the end:
#   - tokens-without-DRIP (counterfactual: every read = full)
#   - tokens-with-DRIP    (what DRIP actually emitted)
#   - reduction %
#   - p50/p99 latency for full and delta paths
#
# Usage: bash scripts/bench.sh [--release|--debug]

set -euo pipefail

cd "$(dirname "$0")/.."

MODE="${1:-}"
case "$MODE" in
  --debug)
    cargo build --quiet
    BIN="$(pwd)/target/debug/drip"
    ;;
  ""|--release)
    cargo build --release --quiet
    BIN="$(pwd)/target/release/drip"
    ;;
  *)
    echo "usage: $0 [--release|--debug]" >&2
    exit 2
    ;;
esac

echo
echo "DRIP realistic-workload benchmark"
echo "  binary       : $BIN"
echo "  files        : 10"
echo "  iterations   : 5  (1 baseline + 4 mutate-and-reread cycles)"
echo "  reads total  : 50"
echo

WORKDIR="$(mktemp -d -t drip-bench.XXXXXX)"
DB="$WORKDIR/db"
SRC="$WORKDIR/src"
mkdir -p "$DB" "$SRC"
trap 'rm -rf "$WORKDIR"' EXIT

export DRIP_DATA_DIR="$DB"
export DRIP_SESSION_ID="bench-$$"

# Make 10 plausible code-shaped files.
for i in $(seq 0 9); do
  python3 - "$SRC/file_$i.rs" "$i" <<'PY'
import sys
path, idx = sys.argv[1], int(sys.argv[2])
N = 100 + idx*10
with open(path, "w") as f:
    for k in range(N):
        f.write(f"fn module_{idx}_handler_{k:03}(req: Request) -> Response {{ /* {k} */ inner({k}) }}\n")
PY
done

# Run reads via Python so we can collect timings cleanly.
python3 - "$BIN" "$SRC" <<'PY'
import os, sys, subprocess, time, json, statistics, pathlib, random
random.seed(42)

bin_ = sys.argv[1]
src  = pathlib.Path(sys.argv[2])
files = sorted(src.glob("file_*.rs"))
env = os.environ.copy()

def read(path):
    t0 = time.perf_counter()
    r = subprocess.run([bin_,"read",str(path)], env=env, capture_output=True)
    dt_ms = (time.perf_counter()-t0)*1000
    if r.returncode != 0:
        sys.stderr.write(r.stderr.decode())
        sys.exit(1)
    out = r.stdout.decode()
    head = out.splitlines()[0] if out else ""
    return dt_ms, head

def kind(head):
    if "[DRIP: full read"   in head: return "full"
    if "[DRIP: unchanged"   in head: return "unchanged"
    if "[DRIP: delta only"  in head: return "delta"
    return "other"

def parse_tokens(head):
    # forms:
    #  "[DRIP: full read | NNN tokens | path]"
    #  "[DRIP: unchanged since last read | 0 tokens sent (NNN saved) | path]"
    #  "[DRIP: delta only | PP% token reduction (sent/full) | path]"
    import re
    m = re.search(r"full read \| (\d+) tokens", head)
    if m: return int(m.group(1)), int(m.group(1))
    m = re.search(r"unchanged.*?\((\d+) saved\)", head)
    if m: return 0, int(m.group(1))  # sent, full
    m = re.search(r"delta only \| (\d+)% token reduction \((\d+)/(\d+)\)", head)
    if m: return int(m.group(2)), int(m.group(3))  # sent, full
    return 0, 0

reads = []   # (kind, sent_tokens, full_tokens, dt_ms)

# iteration 0: cold reads of all 10 files
for f in files:
    dt, head = read(f)
    s, fl = parse_tokens(head)
    reads.append((kind(head), s, fl, dt))

# iterations 1..4: mutate 5 random files then re-read all 10
for it in range(1, 5):
    for f in random.sample(files, 5):
        text = f.read_text()
        # change two lines deterministically
        new = text.replace(f"handler_010", f"handler_X{it}0", 1)
        new = new.replace(f"handler_050", f"handler_X{it}5", 1)
        if new == text:
            new = text + f"// touched-{it}\n"
        f.write_text(new)
    for f in files:
        dt, head = read(f)
        s, fl = parse_tokens(head)
        reads.append((kind(head), s, fl, dt))

# Summarize.
def quantile(xs, q):
    if not xs: return 0.0
    xs = sorted(xs)
    return xs[min(len(xs)-1, int(len(xs)*q))]

by_kind = {"full":0,"delta":0,"unchanged":0,"other":0}
sent_total = 0
full_total = 0
lat_full = []
lat_delta = []
lat_unchanged = []
for k, s, fl, dt in reads:
    by_kind[k] += 1
    sent_total += s
    full_total += fl
    if k == "full":      lat_full.append(dt)
    elif k == "delta":   lat_delta.append(dt)
    elif k == "unchanged": lat_unchanged.append(dt)

reduction = (1 - sent_total / max(1, full_total)) * 100

print()
print("Read outcomes")
print(f"  full      : {by_kind['full']}")
print(f"  delta     : {by_kind['delta']}")
print(f"  unchanged : {by_kind['unchanged']}")
print()
print("Token accounting")
print(f"  full reads (counterfactual)  : {full_total}")
print(f"  what DRIP actually sent      : {sent_total}")
print(f"  saved                        : {full_total - sent_total}")
print(f"  reduction                    : {reduction:.1f}%")
print()
def stats(label, xs):
    if not xs:
        print(f"  {label:<25} n=0")
        return
    xs.sort()
    p50 = xs[len(xs)//2]
    p95 = xs[int(len(xs)*0.95)]
    p99 = xs[int(len(xs)*0.99)] if len(xs) >= 100 else xs[-1]
    print(f"  {label:<25} n={len(xs):<3} min={min(xs):.2f}ms p50={p50:.2f}ms p95={p95:.2f}ms p99={p99:.2f}ms max={max(xs):.2f}ms")
print("Latency per outcome")
stats("first read (full)",   lat_full)
stats("delta read",          lat_delta)
stats("unchanged read",      lat_unchanged)

print()
print("Reddit-ready summary")
print("--------------------")
print(f"50 reads across 10 files. DRIP cut total tokens from {full_total} to {sent_total}")
print(f"({reduction:.1f}% reduction). p50 overhead per read:")
print(f"  full: {quantile(lat_full,0.5):.2f}ms | delta: {quantile(lat_delta,0.5):.2f}ms | unchanged: {quantile(lat_unchanged,0.5):.2f}ms")
PY
