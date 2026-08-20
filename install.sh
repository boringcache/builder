#!/usr/bin/env sh
set -eu

REPO="${BORINGBUILDER_REPO:-boringcache/builder}"
VERSION="${BORINGBUILDER_VERSION:-latest}"
INSTALL_DIR="${BORINGBUILDER_INSTALL_DIR:-$HOME/.local/bin}"

usage() {
  printf '%s\n' \
    'Install boringbuilder from GitHub Releases.' \
    '' \
    'Usage: install.sh [--version vX.Y.Z] [--install-dir PATH]'
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      VERSION="$2"
      shift 2
      ;;
    --install-dir)
      INSTALL_DIR="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'error: unknown option %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) ASSET=boringbuilder-linux-amd64 ;;
  Linux/aarch64|Linux/arm64) ASSET=boringbuilder-linux-arm64 ;;
  Darwin/arm64) ASSET=boringbuilder-macos-arm64 ;;
  *)
    printf 'error: unsupported platform %s/%s\n' "$(uname -s)" "$(uname -m)" >&2
    exit 1
    ;;
esac

if [ "$VERSION" = latest ]; then
  BASE_URL="https://github.com/$REPO/releases/latest/download"
else
  case "$VERSION" in v*) ;; *) VERSION="v$VERSION" ;; esac
  BASE_URL="https://github.com/$REPO/releases/download/$VERSION"
fi

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/boringbuilder-install.XXXXXX")"
trap 'rm -rf "$WORK_DIR"' EXIT HUP INT TERM

download_with_curl() {
  curl -fsSL --retry 3 -o "$WORK_DIR/$ASSET" "$BASE_URL/$ASSET" &&
    curl -fsSL --retry 3 -o "$WORK_DIR/SHA256SUMS" "$BASE_URL/SHA256SUMS"
}

download_with_gh() {
  command -v gh >/dev/null 2>&1 || return 1
  if [ "$VERSION" = latest ]; then
    gh release download \
      --repo "$REPO" \
      --pattern "$ASSET" \
      --pattern SHA256SUMS \
      --dir "$WORK_DIR" \
      --clobber
  else
    gh release download "$VERSION" \
      --repo "$REPO" \
      --pattern "$ASSET" \
      --pattern SHA256SUMS \
      --dir "$WORK_DIR" \
      --clobber
  fi
}

if ! download_with_curl; then
  rm -f "$WORK_DIR/$ASSET" "$WORK_DIR/SHA256SUMS"
  if ! download_with_gh; then
    printf '%s\n' \
      'error: release download failed' \
      'for a private repository, install and authenticate GitHub CLI first: gh auth login' >&2
    exit 1
  fi
fi

EXPECTED="$(awk -v asset="$ASSET" '$2 == asset { print $1 }' "$WORK_DIR/SHA256SUMS")"
if [ -z "$EXPECTED" ]; then
  printf 'error: checksum for %s is missing\n' "$ASSET" >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "$WORK_DIR/$ASSET" | awk '{ print $1 }')"
else
  ACTUAL="$(shasum -a 256 "$WORK_DIR/$ASSET" | awk '{ print $1 }')"
fi
if [ "$EXPECTED" != "$ACTUAL" ]; then
  printf 'error: checksum mismatch for %s\n' "$ASSET" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
install -m 755 "$WORK_DIR/$ASSET" "$INSTALL_DIR/boringbuilder"
printf 'installed boringbuilder to %s/boringbuilder\n' "$INSTALL_DIR"
