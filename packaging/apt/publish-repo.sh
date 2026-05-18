#!/usr/bin/env bash
set -euo pipefail

# Publish a static Debian repository layout to OUT_DIR.
# Usage: publish-repo.sh <out-dir> <deb-file> [<deb-file> ...]

OUT_DIR="${1:?output directory required}"
shift

mkdir -p "${OUT_DIR}/pool/main/j/jeet"

declare -A ARCH_DIRS
ARCH_DIRS[amd64]="dists/stable/main/binary-amd64"
ARCH_DIRS[arm64]="dists/stable/main/binary-arm64"

for deb in "$@"; do
  cp "${deb}" "${OUT_DIR}/pool/main/j/jeet/"
done

for arch in amd64 arm64; do
  mkdir -p "${OUT_DIR}/${ARCH_DIRS[$arch]}"
  : > "${OUT_DIR}/${ARCH_DIRS[$arch]}/Packages"
done

shopt -s nullglob
for deb in "${OUT_DIR}/pool/main/j/jeet/"*.deb; do
  arch="$(dpkg-deb -f "${deb}" Architecture)"
  case "${arch}" in
    amd64)
      dpkg-scanpackages --arch amd64 pool/main/j/jeet >> "${OUT_DIR}/dists/stable/main/binary-amd64/Packages"
      ;;
    arm64)
      dpkg-scanpackages --arch arm64 pool/main/j/jeet >> "${OUT_DIR}/dists/stable/main/binary-arm64/Packages"
      ;;
    *)
      echo "unsupported architecture in ${deb}: ${arch}" >&2
      exit 1
      ;;
  esac
done

for arch in amd64 arm64; do
  gzip -9 -k -f "${OUT_DIR}/${ARCH_DIRS[$arch]}/Packages"
done

cd "${OUT_DIR}"
apt-ftparchive release dists/stable > dists/stable/Release

echo "apt repo published to ${OUT_DIR}"
