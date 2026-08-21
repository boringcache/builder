#!/bin/sh
set -eu

sccache_version="0.17.0"
case "$(uname -m)" in
  x86_64)
    sccache_target="x86_64-unknown-linux-musl"
    sccache_sha256="67c4a96dd237c1f518f6b36083f270f9976d516f1e57fce891755ea782e50006"
    ;;
  aarch64 | arm64)
    sccache_target="aarch64-unknown-linux-musl"
    sccache_sha256="821a86343191aa1cbab74bd42f9e93c9a63bf85e4742945f40d3ae84193c1c77"
    ;;
  *)
    echo "unsupported sccache architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

sccache_archive="sccache-v${sccache_version}-${sccache_target}.tar.gz"
sccache_release="https://github.com/mozilla/sccache/releases/download/v${sccache_version}/${sccache_archive}"
sccache_tmp="$(mktemp -d)"
trap 'rm -rf "$sccache_tmp"' 0 HUP INT TERM

curl --fail --silent --show-error --location \
  --proto '=https' --proto-redir '=https' --retry 3 \
  --output "${sccache_tmp}/${sccache_archive}" \
  "$sccache_release"
printf '%s  %s\n' "$sccache_sha256" "${sccache_tmp}/${sccache_archive}" \
  | sha256sum --check --status
tar -xzf "${sccache_tmp}/${sccache_archive}" -C "$sccache_tmp"
install -m 0755 \
  "${sccache_tmp}/sccache-v${sccache_version}-${sccache_target}/sccache" \
  /usr/local/bin/sccache
sccache --version
