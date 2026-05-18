#!/usr/bin/env bash
set -euo pipefail

# Install jeet apt source and package on Debian/Ubuntu.
# Usage: curl -fsSL https://peterddod.github.io/jeet/deb-install.sh | bash

if ! command -v apt-get >/dev/null 2>&1; then
  echo "This installer requires apt (Debian/Ubuntu)." >&2
  exit 1
fi

if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
  SUDO="sudo"
else
  SUDO=""
fi

list_file="/etc/apt/sources.list.d/jeet.list"
repo_line='deb [trusted=yes] https://peterddod.github.io/jeet stable main'

if [[ -f "${list_file}" ]] && grep -qF "${repo_line}" "${list_file}"; then
  echo "jeet apt source already configured"
else
  echo "Adding jeet apt source..."
  echo "${repo_line}" | ${SUDO} tee "${list_file}" >/dev/null
fi

${SUDO} apt-get update
${SUDO} apt-get install -y jeet

echo "Installed $(jeet --version 2>/dev/null || echo jeet)"
