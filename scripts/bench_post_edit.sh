#!/usr/bin/env bash
# DRIP post-edit verification bench — measures the bytes DRIP returns
# when an agent reads a file IMMEDIATELY after editing it.
#
# Without DRIP, the post-edit re-read ships the full file (the harness
# requires "must read before edit" to see real content).
#
# With DRIP's edit-certificate path, the read returns a compact
# `[DRIP: edit verified ...]` attestation containing the file hash and
# the touched line ranges. The bench drives the actual hooks
# (`drip hook claude-post-edit` then `drip hook claude`) so the
# numbers reflect what the agent really sees.
#
# Output: scripts/bench_output/post_edit_cert.json

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE_DIR="$ROOT/scripts/bench_fixtures"
OUTPUT_DIR="$ROOT/scripts/bench_output"
DRIP="${DRIP:-$ROOT/target/release/drip}"

if [[ ! -x "$DRIP" ]]; then
  (cd "$ROOT" && cargo build --release >/dev/null)
fi

mkdir -p "$OUTPUT_DIR"

# Isolate from the user's real data dir — same rationale as
# bench_multilang.sh: many synthetic session ids per run.
export DRIP_DATA_DIR="${DRIP_DATA_DIR:-$(mktemp -d)}"
trap 'rm -rf "$DRIP_DATA_DIR"' EXIT

# Languages and their fixtures (parallel arrays — bash 3.2 has no
# associative arrays, and macOS ships 3.2 by default).
LANG_NAMES=(python rust typescript java go cpp csharp kotlin)
LANG_FIXTURES=(
  pricing_engine.py
  session_manager.rs
  api_client.ts
  UserRepository.java
  http_handler.go
  json_parser.cpp
  OrderService.cs
  DataRepository.kt
)

fixture_for() {
  local want="$1" i=0
  for name in "${LANG_NAMES[@]}"; do
    if [[ "$name" == "$want" ]]; then
      echo "${LANG_FIXTURES[$i]}"
      return 0
    fi
    i=$((i + 1))
  done
  return 1
}

# Helper: run the read hook in deny/allow mode and emit the bytes the
# agent sees. For `deny` the agent sees `permissionDecisionReason`; for
# `allow` the agent runs native Read → it sees the full file.
agent_visible_bytes() {
  local response_file="$1" file_path="$2"
  python3 - "$response_file" "$file_path" <<'PY'
import json, sys, os
resp_path, file_path = sys.argv[1:]
with open(resp_path, "rb") as f:
    payload = f.read().decode("utf-8", errors="replace")
try:
    obj = json.loads(payload)
    h = obj.get("hookSpecificOutput", {})
    if h.get("permissionDecision") == "deny":
        print(len(h.get("permissionDecisionReason", "").encode("utf-8")))
        sys.exit(0)
except Exception:
    pass
# allow → agent runs native Read → sees full file content.
print(os.path.getsize(file_path))
PY
}

bench_one_language() {
  local lang="$1" fixture="$2" cert_mode="$3"
  local file="$FIXTURE_DIR/$fixture"
  local sid="bench-cert-${lang}-${cert_mode}-$$-$RANDOM"
  local tmp_workdir
  tmp_workdir=$(mktemp -d)
  trap "rm -rf '$tmp_workdir'" RETURN
  local copy="$tmp_workdir/$fixture"
  cp "$file" "$copy"

  # 1. Cold read primes the baseline.
  DRIP_SESSION_ID="$sid" "$DRIP" reset >/dev/null 2>&1 || true
  DRIP_SESSION_ID="$sid" "$DRIP" read "$copy" >/dev/null

  # 2. Mutate the fixture (insert a tiny line near the top so the
  #    diff has a recognisable hunk).
  python3 - "$copy" <<'PY'
import sys
p = sys.argv[1]
with open(p) as f:
    lines = f.readlines()
inject_line = "// DRIP_CERT_BENCH_INJECT\n"
out = []
done = False
for i, l in enumerate(lines):
    out.append(l)
    if not done and i > 5 and l.strip() == "":
        out.append(inject_line)
        done = True
if not done:
    out.append(inject_line)
with open(p, "w") as f:
    f.writelines(out)
PY

  # 3. PostToolUse:Edit fires — DRIP records the edit_event.
  local post_payload
  post_payload=$(python3 -c '
import json,sys
print(json.dumps({"session_id":sys.argv[1],"tool_name":"Edit",
                  "tool_input":{"file_path":sys.argv[2]}}))' "$sid" "$copy")
  printf '%s' "$post_payload" | DRIP_SESSION_ID="$sid" "$DRIP" hook claude-post-edit >/dev/null

  # 4. The model re-reads. Measure bytes the agent actually sees,
  #    with cert ON or OFF depending on $cert_mode.
  local read_payload
  read_payload=$(python3 -c '
import json,sys
print(json.dumps({"session_id":sys.argv[1],"tool_name":"Read",
                  "tool_input":{"file_path":sys.argv[2]}}))' "$sid" "$copy")
  local resp_file
  resp_file=$(mktemp)
  if [[ "$cert_mode" == "off" ]]; then
    DRIP_CERT_DISABLE=1 DRIP_SESSION_ID="$sid" "$DRIP" hook claude < <(printf '%s' "$read_payload") > "$resp_file"
  else
    DRIP_SESSION_ID="$sid" "$DRIP" hook claude < <(printf '%s' "$read_payload") > "$resp_file"
  fi
  local bytes
  bytes=$(agent_visible_bytes "$resp_file" "$copy")
  rm -f "$resp_file"
  printf '%s' "$bytes"
}

emit() {
  local lang="$1"
  local fixture; fixture=$(fixture_for "$lang")
  local without="$2"
  local with="$3"
  local pct
  pct=$(python3 -c '
import sys
big, small = int(sys.argv[1]), int(sys.argv[2])
print(int(round(100 * (1 - small / big))) if big else 0)' "$without" "$with")
  python3 - "$lang" "$fixture" "$without" "$with" "$pct" <<'PY'
import json, sys
lang, fixture, without, with_drip, pct = sys.argv[1:]
print(json.dumps({
    "language": lang,
    "fixture": fixture,
    "without_drip_bytes": int(without),
    "with_cert_bytes": int(with_drip),
    "reduction_pct": int(pct),
}))
PY
}

results=()
for lang in "${LANG_NAMES[@]}"; do
  fixture=$(fixture_for "$lang")
  if [[ ! -f "$FIXTURE_DIR/$fixture" ]]; then
    echo "  SKIP $lang — $fixture not present" >&2
    continue
  fi
  echo "  measuring $lang ($fixture) …" >&2
  without=$(bench_one_language "$lang" "$fixture" off)
  with=$(bench_one_language "$lang" "$fixture" on)
  results+=( "$(emit "$lang" "$without" "$with")" )
done

# Aggregate + write JSON.
python3 - "$OUTPUT_DIR" "${results[@]}" <<'PY'
import json, os, sys
out_dir = sys.argv[1]
rows = [json.loads(r) for r in sys.argv[2:]]
total_without = sum(r["without_drip_bytes"] for r in rows)
total_with = sum(r["with_cert_bytes"] for r in rows)
agg = int(round(100 * (1 - total_with / total_without))) if total_without else 0
result = {
    "scenario": "post-edit verification re-read: agent edits, then immediately reads the file",
    "languages": rows,
    "aggregate": {
        "without_drip_bytes": total_without,
        "with_cert_bytes": total_with,
        "reduction_pct": agg,
    },
}
path = os.path.join(out_dir, "post_edit_cert.json")
with open(path, "w") as f:
    json.dump(result, f, indent=2)
print(f"Wrote {path}")
PY

echo
echo "Done. Output: $OUTPUT_DIR/post_edit_cert.json"
