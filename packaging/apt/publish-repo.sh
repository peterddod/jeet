#!/usr/bin/env bash
set -euo pipefail

# Publish a static Debian repository layout to OUT_DIR.
# Usage: publish-repo.sh <out-dir> <deb-file> [<deb-file> ...]

OUT_DIR="${1:?output directory required}"
shift

mkdir -p "${OUT_DIR}/pool/main/j/jeet"
mkdir -p "${OUT_DIR}/dists/stable/main/binary-amd64"
mkdir -p "${OUT_DIR}/dists/stable/main/binary-arm64"

for deb in "$@"; do
  cp "${deb}" "${OUT_DIR}/pool/main/j/jeet/"
done

cd "${OUT_DIR}"

: > dists/stable/main/binary-amd64/Packages
: > dists/stable/main/binary-arm64/Packages

shopt -s nullglob
for deb in pool/main/j/jeet/*.deb; do
  arch="$(dpkg-deb -f "${deb}" Architecture)"
  case "${arch}" in
    amd64)
      dpkg-scanpackages --arch amd64 pool/main/j/jeet >> dists/stable/main/binary-amd64/Packages
      ;;
    arm64)
      dpkg-scanpackages --arch arm64 pool/main/j/jeet >> dists/stable/main/binary-arm64/Packages
      ;;
    *)
      echo "unsupported architecture in ${deb}: ${arch}" >&2
      exit 1
      ;;
  esac
done

gzip -9 -k -f dists/stable/main/binary-amd64/Packages
gzip -9 -k -f dists/stable/main/binary-arm64/Packages

apt-ftparchive release dists/stable > dists/stable/Release

echo "apt repo published to ${OUT_DIR}"
