#!/usr/bin/env sh
# DRIP installer — detects OS/arch and drops a `drip` binary into ~/.local/bin.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/drip-cli/drip/main/install.sh | sh
#
# Env overrides:
#   DRIP_VERSION   Pin a release tag (default: latest)
#   DRIP_PREFIX    Install dir (default: $HOME/.local/bin)
#   DRIP_REPO      GitHub repo  (default: drip-cli/drip)

set -eu

REPO="${DRIP_REPO:-drip-cli/drip}"
VERSION="${DRIP_VERSION:-latest}"
PREFIX="${DRIP_PREFIX:-$HOME/.local/bin}"

say() { printf "  %s\n" "$1"; }
err() { printf "error: %s\n" "$1" >&2; exit 1; }

detect_target() {
  os=$(uname -s 2>/dev/null || echo unknown)
  arch=$(uname -m 2>/dev/null || echo unknown)
  case "$os" in
    Linux)
      # Linux releases are statically linked against musl so the
      # same binary runs on every glibc / Alpine variant. Matches
      # the targets produced by .github/workflows/release.yml.
      case "$arch" in
        x86_64|amd64) echo "x86_64-unknown-linux-musl" ;;
        aarch64|arm64) echo "aarch64-unknown-linux-musl" ;;
        *) err "unsupported linux arch: $arch" ;;
      esac
      ;;
    Darwin)
      case "$arch" in
        x86_64) echo "x86_64-apple-darwin" ;;
        arm64)  echo "aarch64-apple-darwin" ;;
        *) err "unsupported macos arch: $arch" ;;
      esac
      ;;
    MINGW*|MSYS*|CYGWIN*)
      echo "x86_64-pc-windows-msvc"
      ;;
    *) err "unsupported OS: $os (try: cargo install --git https://github.com/$REPO)" ;;
  esac
}

resolve_url() {
  target="$1"
  ext="tar.gz"
  case "$target" in *windows*) ext="zip" ;; esac
  if [ "$VERSION" = "latest" ]; then
    echo "https://github.com/$REPO/releases/latest/download/drip-$target.$ext"
  else
    echo "https://github.com/$REPO/releases/download/$VERSION/drip-$target.$ext"
  fi
}

download() {
  url="$1"
  out="$2"
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL "$url" -o "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$url" -O "$out"
  else
    err "need curl or wget"
  fi
}

main() {
  target=$(detect_target)
  url=$(resolve_url "$target")
  say "DRIP target  : $target"
  say "Release URL  : $url"
  say "Install dir  : $PREFIX"

  mkdir -p "$PREFIX"
  tmp=$(mktemp -d 2>/dev/null || mktemp -d -t drip)
  trap 'rm -rf "$tmp"' EXIT

  case "$url" in
    *.zip)
      pkg="$tmp/drip.zip"
      download "$url" "$pkg"
      command -v unzip >/dev/null 2>&1 || err "unzip required"
      unzip -q "$pkg" -d "$tmp"
      ;;
    *.tar.gz)
      pkg="$tmp/drip.tar.gz"
      download "$url" "$pkg"
      tar -xzf "$pkg" -C "$tmp"
      ;;
  esac

  bin=$(find "$tmp" -type f \( -name 'drip' -o -name 'drip.exe' \) | head -n1)
  [ -n "$bin" ] || err "binary not found in archive"
  dest="$PREFIX/$(basename "$bin")"
  if ! install -m 755 "$bin" "$dest" 2>/dev/null; then
    cp "$bin" "$dest"
    chmod +x "$dest"
  fi

  say "Installed    : $PREFIX/$(basename "$bin")"
  case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *)
      printf "\n  add this to your shell rc:\n    export PATH=\"%s:\$PATH\"\n\n" "$PREFIX"
      ;;
  esac

  printf "\nNext: run \`drip init -g\` and restart your agent.\n"
}

# Skip main() when sourced for testing (`DRIP_INSTALL_NO_MAIN=1`).
if [ "${DRIP_INSTALL_NO_MAIN:-0}" != "1" ]; then
  main "$@"
fi
