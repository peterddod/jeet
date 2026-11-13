#!/usr/bin/env bash
set -euo pipefail

# Push a generated formula to the homebrew-plete tap.
# Requires HOMEBREW_TAP_TOKEN with Contents read+write on peterddod/homebrew-plete.
# Usage: push-formula.sh <formula-file> <version-tag e.g. v0.2.4>

FORMULA="${1:?formula file required}"
VERSION="${2:?version tag required}"
TAP_REPO="${TAP_REPO:-peterddod/homebrew-plete}"

if [[ -z "${HOMEBREW_TAP_TOKEN:-}" ]]; then
  echo "HOMEBREW_TAP_TOKEN is not set." >&2
  echo "Create a fine-grained PAT with Contents read+write on ${TAP_REPO} only," >&2
  echo "then add it as a secret on peterddod/plete." >&2
  exit 1
fi

TAP_URL="https://x-access-token:${HOMEBREW_TAP_TOKEN}@github.com/${TAP_REPO}.git"

git config --global user.name "github-actions[bot]"
git config --global user.email "41898282+github-actions[bot]@users.noreply.github.com"

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

git clone "${TAP_URL}" "${work}/tap"
cp "${FORMULA}" "${work}/tap/Formula/plete.rb"
cd "${work}/tap"
git add Formula/plete.rb
# Renamed from jeet: `brew upgrade` moves an installed jeet to plete, which
# Homebrew only does once no jeet formula is left in the tap.
if [[ -f Formula/jeet.rb ]]; then
  git rm -q Formula/jeet.rb
fi
echo '{ "jeet": "plete" }' > formula_renames.json
git add formula_renames.json
if git diff --staged --quiet; then
  echo "formula unchanged"
  exit 0
fi
git commit -m "chore: update plete formula to ${VERSION}"
git push origin HEAD

echo "pushed formula ${VERSION} to ${TAP_REPO}"
