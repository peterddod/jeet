#!/usr/bin/env bash
set -euo pipefail

# Generate shell completion scripts for packaging (deb, local install).
# Usage: generate.sh <path-to-jeet-binary> [output-dir]

JEET="${1:?jeet binary path required}"
OUT="${2:-$(cd "$(dirname "$0")" && pwd)}"

mkdir -p "${OUT}"

"${JEET}" completions bash >"${OUT}/jeet.bash"
"${JEET}" completions zsh >"${OUT}/_jeet"
"${JEET}" completions fish >"${OUT}/jeet.fish"

for f in jeet.bash _jeet jeet.fish; do
  if [[ ! -s "${OUT}/${f}" ]]; then
    echo "empty or missing completion file: ${OUT}/${f}" >&2
    exit 1
  fi
done

echo "wrote completion scripts to ${OUT}"
