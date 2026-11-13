#!/usr/bin/env bash
set -euo pipefail

# Generate shell completion scripts for packaging (deb, local install).
# Usage: generate.sh <path-to-plete-binary> [output-dir]

PLETE="${1:?plete binary path required}"
OUT="${2:-$(cd "$(dirname "$0")" && pwd)}"

mkdir -p "${OUT}"

"${PLETE}" completions bash >"${OUT}/plete.bash"
"${PLETE}" completions zsh >"${OUT}/_plete"
"${PLETE}" completions fish >"${OUT}/plete.fish"

for f in plete.bash _plete plete.fish; do
  if [[ ! -s "${OUT}/${f}" ]]; then
    echo "empty or missing completion file: ${OUT}/${f}" >&2
    exit 1
  fi
done

echo "wrote completion scripts to ${OUT}"
