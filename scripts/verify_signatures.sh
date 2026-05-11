#!/usr/bin/env bash
# Signature-preservation verifier for the multi-language bench.
#
# For each fixture, count how many "structurally important" lines exist in the
# original (function/method signatures, type declarations, import lines), then
# capture the rendered first-read output that DRIP returns and confirm every
# one of those lines is still present.
#
# Output: scripts/bench_output/signatures.json — { lang: { ok: bool, original: {...}, rendered: {...} } }
# Exit 0 if every language preserves every signature; exit 1 otherwise.
#
# Usage:
#   bash scripts/verify_signatures.sh                  # verify all
#   LANGS=python bash scripts/verify_signatures.sh     # subset

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE_DIR="$ROOT/scripts/bench_fixtures"
OUTPUT_DIR="$ROOT/scripts/bench_output"
DRIP="${DRIP:-$ROOT/target/release/drip}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

if [[ ! -x "$DRIP" ]]; then
  (cd "$ROOT" && cargo build --release >/dev/null)
fi

mkdir -p "$OUTPUT_DIR"

# Isolate from the user's real data dir. The verifier creates one
# session id per language to capture a fresh first-read; without
# this, those ids accumulate in `~/.local/share/drip/sessions.db`
# until the 2 h TTL expires.
export DRIP_DATA_DIR="${DRIP_DATA_DIR:-$(mktemp -d)}"
# The script already declares its own $TMP cleanup — chain ours so
# both directories disappear on EXIT.
trap 'rm -rf "$TMP" "$DRIP_DATA_DIR"' EXIT

# Per-language detection rules — returned by `lang_config` as 4 lines:
#   <fixture-basename>
#   <signature regex>
#   <type-declaration regex>
#   <import regex>
lang_config() {
  case "$1" in
    python)
      printf '%s\n' \
        'pricing_engine.py' \
        '^[[:space:]]*(def |async def )' \
        '^class ' \
        '^(import |from )'
      ;;
    rust)
      printf '%s\n' \
        'session_manager.rs' \
        '^[[:space:]]*(pub )?(async )?fn ' \
        '^(pub )?(struct |enum |trait |impl )' \
        '^use '
      ;;
    typescript)
      printf '%s\n' \
        'api_client.ts' \
        '^[[:space:]]+(public |private |protected |static |async |readonly |get |set )+[a-zA-Z_][a-zA-Z0-9_]*[[:space:]]*[<(]|^[[:space:]]+[a-zA-Z_][a-zA-Z0-9_]*\([^)]*\)[[:space:]]*:[[:space:]]|^(export )?(async )?function[[:space:]]+[a-zA-Z_]' \
        '^(export )?(class |interface |enum |type )' \
        '^import '
      ;;
    java)
      printf '%s\n' \
        'UserRepository.java' \
        '^[[:space:]]*(public |private |protected )?(static )?(final )?[A-Za-z][A-Za-z0-9_<>?, []*[[:space:]]+[a-zA-Z_][a-zA-Z0-9_]*[[:space:]]*\(' \
        '^(public |private )?(final )?(abstract )?(class |interface |enum )' \
        '^import '
      ;;
    go)
      printf '%s\n' \
        'http_handler.go' \
        '^func ' \
        '^type ' \
        '^import '
      ;;
    cpp)
      printf '%s\n' \
        'json_parser.cpp' \
        '^[[:space:]]*[A-Za-z_][A-Za-z0-9_<>:&* ]*[[:space:]]+[a-zA-Z_][a-zA-Z0-9_]*[[:space:]]*\(' \
        '^(class |struct |enum |namespace )' \
        '^#include'
      ;;
    csharp)
      printf '%s\n' \
        'OrderService.cs' \
        '^[[:space:]]+(public |private |protected |internal )(static |async |virtual |override |sealed |abstract )*[A-Za-z_][^=()]*[[:space:]]+[a-zA-Z_][a-zA-Z0-9_]*[[:space:]]*\(' \
        '^[[:space:]]*(public |internal )?(sealed )?(abstract )?(class |interface |record |enum |struct )' \
        '^using '
      ;;
    kotlin)
      printf '%s\n' \
        'DataRepository.kt' \
        '^[[:space:]]*(public |private |internal |protected )?(suspend )?(inline )?(override )?(fun )' \
        '^(public |internal )?(sealed )?(data )?(open )?(abstract )?(class |interface |object |enum class )' \
        '^import '
      ;;
    *) return 1 ;;
  esac
}

ALL_LANGS=(python rust typescript java go cpp csharp kotlin)
SELECTED=("${ALL_LANGS[@]}")
if [[ -n "${LANGS:-}" ]]; then
  SELECTED=()
  for want in $LANGS; do
    for known in "${ALL_LANGS[@]}"; do
      [[ "$want" == "$known" ]] && SELECTED+=("$known")
    done
  done
fi

ALL_OK=1
RESULTS_JSON='[]'

# Capture a fresh first-read for one language.
capture_first_read() {
  local lang="$1" file="$2"
  local sid="verify-${lang}-$$-$RANDOM"
  DRIP_SESSION_ID="$sid" "$DRIP" reset >/dev/null 2>&1 || true
  DRIP_SESSION_ID="$sid" "$DRIP" read "$file"
}

for lang in "${SELECTED[@]}"; do
  cfg=$(lang_config "$lang") || { echo "  SKIP $lang — unknown" >&2; continue; }
  fixture=$(printf '%s\n' "$cfg" | sed -n '1p')
  sig_re=$(printf '%s\n' "$cfg" | sed -n '2p')
  type_re=$(printf '%s\n' "$cfg" | sed -n '3p')
  imp_re=$(printf '%s\n' "$cfg" | sed -n '4p')
  src="$FIXTURE_DIR/$fixture"
  if [[ ! -f "$src" ]]; then
    echo "  SKIP $lang — $fixture not present" >&2
    continue
  fi

  rendered=$(capture_first_read "$lang" "$src")
  rendered_file="$TMP/${lang}.rendered"
  printf '%s' "$rendered" > "$rendered_file"

  # The honest test: every line that looks like a signature/type/import in the
  # source must appear (verbatim, modulo leading whitespace) in the rendered
  # output. A regex match-count comparison catches false positives inside
  # elided bodies and inflates "lost" counts, so we do exact line-presence
  # instead — DRIP's compressor preserves signature lines byte-for-byte.
  count_present() {
    local pattern="$1" original="$2" rendered="$3"
    local total=0 missing=0
    while IFS= read -r line; do
      # Strip leading whitespace; that's all the compressor may change.
      local stripped
      stripped=$(printf '%s' "$line" | sed 's/^[[:space:]]*//')
      [[ -z "$stripped" ]] && continue
      # Skip statement-keyword noise that some greedy regexes pick up
      # inside elided bodies (e.g. `throw new Foo(...)` matches generic
      # `Type Name(...)`-style signatures even though it isn't one).
      case "$stripped" in
        throw\ *|return\ *|if\ *|for\ *|while\ *|switch\ *|case\ *|else\ *|catch\ *|do\ *|new\ *|"this."*|"super."*) continue ;;
      esac
      # Stream-operator lines (`os << foo(...)`) match generic
      # `Type name(...)` regexes but aren't signatures.
      case "$stripped" in
        *' << '*|*' >> '*) continue ;;
      esac
      total=$((total + 1))
      if ! grep -qF -- "$stripped" "$rendered"; then
        missing=$((missing + 1))
      fi
    done < <(grep -E "$pattern" "$original" || true)
    printf '%d %d' "$total" "$missing"
  }

  read sig_orig sig_miss <<<"$(count_present "$sig_re" "$src" "$rendered_file")"
  read type_orig type_miss <<<"$(count_present "$type_re" "$src" "$rendered_file")"
  read imp_orig imp_miss <<<"$(count_present "$imp_re" "$src" "$rendered_file")"
  sig_rend=$((sig_orig - sig_miss))
  type_rend=$((type_orig - type_miss))
  imp_rend=$((imp_orig - imp_miss))

  ok=1
  (( sig_rend  >= sig_orig  )) || ok=0
  (( type_rend >= type_orig )) || ok=0
  (( imp_rend  >= imp_orig  )) || ok=0

  if (( ok )); then
    status="ok"
    printf "  ✓ %-12s sigs=%3d/%-3d types=%2d/%-2d imports=%2d/%-2d\n" \
      "$lang" "$sig_rend" "$sig_orig" "$type_rend" "$type_orig" "$imp_rend" "$imp_orig"
  else
    status="FAIL"
    ALL_OK=0
    printf "  ✗ %-12s sigs=%3d/%-3d types=%2d/%-2d imports=%2d/%-2d  (some signatures lost!)\n" \
      "$lang" "$sig_rend" "$sig_orig" "$type_rend" "$type_orig" "$imp_rend" "$imp_orig"
  fi

  RESULTS_JSON=$(python3 - "$RESULTS_JSON" "$lang" "$status" \
                  "$sig_orig" "$sig_rend" "$type_orig" "$type_rend" "$imp_orig" "$imp_rend" <<'PY'
import json, sys
acc = json.loads(sys.argv[1])
lang, status, so, sr, to, tr, io_, ir = sys.argv[2:]
acc.append({
    "language": lang,
    "ok": status == "ok",
    "signatures":  {"original": int(so), "rendered": int(sr)},
    "type_decls":  {"original": int(to), "rendered": int(tr)},
    "imports":     {"original": int(io_), "rendered": int(ir)},
})
print(json.dumps(acc))
PY
  )
done

printf '%s' "$RESULTS_JSON" | python3 -m json.tool > "$OUTPUT_DIR/signatures.json"

if (( ALL_OK )); then
  echo
  echo "All languages: 100% signature preservation."
  exit 0
else
  echo
  echo "FAIL: at least one language lost signatures during compression." >&2
  exit 1
fi
