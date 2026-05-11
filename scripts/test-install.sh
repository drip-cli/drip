#!/usr/bin/env bash
# Smoke tests for install.sh's arch/OS detection.
#
# Approach: shadow `uname` with a tiny shim per scenario so install.sh
# resolves a different (os, arch) combination. We stop install.sh right
# after URL resolution by short-circuiting `download`, then assert on
# the printed target.
#
# Run: bash scripts/test-install.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INSTALLER="$ROOT/install.sh"

PASS=0; FAIL=0

case_under_test() {
  local name="$1" os="$2" arch="$3" expected_target="$4"

  local tmp; tmp=$(mktemp -d)
  cat > "$tmp/uname" <<EOF
#!/usr/bin/env bash
case "\$1" in
  -s) echo "$os" ;;
  -m) echo "$arch" ;;
  *)  echo "$os" ;;
esac
EOF
  chmod +x "$tmp/uname"

  # Force install.sh to terminate after target resolution.
  # We capture the "DRIP target" line and stop before any download.
  local out
  out=$(PATH="$tmp:$PATH" \
        DRIP_REPO="drip-cli/drip" \
        DRIP_VERSION="v0.0.0" \
        DRIP_PREFIX="$tmp/install" \
        DRIP_INSTALL_NO_MAIN=1 \
        bash -c '
          set -e
          source "$0"
          target=$(detect_target)
          url=$(resolve_url "$target")
          echo "TARGET=$target"
          echo "URL=$url"
        ' "$INSTALLER")

  rm -rf "$tmp"

  local actual_target
  actual_target=$(printf "%s\n" "$out" | sed -n 's/^TARGET=//p')
  local actual_url
  actual_url=$(printf "%s\n" "$out" | sed -n 's/^URL=//p')

  local ext="tar.gz"
  case "$expected_target" in *windows*) ext="zip" ;; esac
  local expected_url="https://github.com/drip-cli/drip/releases/download/v0.0.0/drip-$expected_target.$ext"

  if [ "$actual_target" = "$expected_target" ] && [ "$actual_url" = "$expected_url" ]; then
    printf "  ok   %-30s -> %s\n" "$name" "$expected_target"
    PASS=$((PASS+1))
  else
    printf "  FAIL %-30s\n" "$name"
    printf "       expected target=%s url=%s\n" "$expected_target" "$expected_url"
    printf "       actual   target=%s url=%s\n" "$actual_target"   "$actual_url"
    FAIL=$((FAIL+1))
  fi
}

echo "install.sh detection cases"
case_under_test "linux x86_64"      "Linux"  "x86_64"  "x86_64-unknown-linux-musl"
case_under_test "linux amd64 alias" "Linux"  "amd64"   "x86_64-unknown-linux-musl"
case_under_test "linux aarch64"     "Linux"  "aarch64" "aarch64-unknown-linux-musl"
case_under_test "linux arm64 alias" "Linux"  "arm64"   "aarch64-unknown-linux-musl"
case_under_test "macos intel"       "Darwin" "x86_64"  "x86_64-apple-darwin"
case_under_test "macos arm"         "Darwin" "arm64"   "aarch64-apple-darwin"
case_under_test "windows mingw"     "MINGW64_NT-10.0" "x86_64" "x86_64-pc-windows-msvc"
case_under_test "windows msys"      "MSYS_NT-10.0"    "x86_64" "x86_64-pc-windows-msvc"
case_under_test "windows cygwin"    "CYGWIN_NT-10.0"  "x86_64" "x86_64-pc-windows-msvc"

echo
echo "result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
