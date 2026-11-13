#!/usr/bin/env bash
set -euo pipefail

# Install plete apt source and package on Debian/Ubuntu.
# Usage: curl -fsSL https://peterddod.github.io/plete/deb-install.sh | bash

if ! command -v apt-get >/dev/null 2>&1; then
  echo "This installer requires apt (Debian/Ubuntu)." >&2
  exit 1
fi

if [[ "${EUID:-$(id -u)}" -ne 0 ]]; then
  SUDO="sudo"
else
  SUDO=""
fi

list_file="/etc/apt/sources.list.d/plete.list"
repo_line='deb [trusted=yes] https://peterddod.github.io/plete stable main'

# Renamed from jeet: the old source moved with the repository and no longer
# answers, so it goes.
old_list="/etc/apt/sources.list.d/jeet.list"
if [[ -f "${old_list}" ]]; then
  echo "Removing the old jeet apt source..."
  ${SUDO} rm -f "${old_list}"
fi

if [[ -f "${list_file}" ]] && grep -qF "${repo_line}" "${list_file}"; then
  echo "plete apt source already configured"
else
  echo "Adding plete apt source..."
  echo "${repo_line}" | ${SUDO} tee "${list_file}" >/dev/null
fi

${SUDO} apt-get update
${SUDO} apt-get install -y plete

echo "Installed $(plete --version 2>/dev/null || echo plete)"
