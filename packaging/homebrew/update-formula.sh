#!/usr/bin/env bash
set -euo pipefail

# Generate Homebrew formula with per-platform release asset checksums.
# Usage: update-formula.sh <version> <asset-dir> <formula-out>

VERSION="${1:?version required}"
ASSET_DIR="${2:?asset directory required}"
OUT="${3:?formula output path required}"

sha_for() {
  local file="${ASSET_DIR}/${1}"
  if [[ ! -f "${file}" ]]; then
    echo "missing asset: ${file}" >&2
    exit 1
  fi
  shasum -a 256 "${file}" | awk '{print $1}'
}

MAC_ARM_SHA="$(sha_for "plete-v${VERSION}-aarch64-apple-darwin.tar.gz")"
MAC_INTEL_SHA="$(sha_for "plete-v${VERSION}-x86_64-apple-darwin.tar.gz")"
LINUX_ARM_SHA="$(sha_for "plete-v${VERSION}-aarch64-unknown-linux-gnu.tar.gz")"
LINUX_INTEL_SHA="$(sha_for "plete-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz")"

cat > "${OUT}" <<RUBY
# typed: false
# frozen_string_literal: true

class Plete < Formula
  desc "Global git repo index and worktree manager"
  homepage "https://github.com/peterddod/plete"
  version "${VERSION}"
  license "MIT"
  head "https://github.com/peterddod/plete.git", branch: "main"

  on_macos do
    on_arm do
      url "https://github.com/peterddod/plete/releases/download/v${VERSION}/plete-v${VERSION}-aarch64-apple-darwin.tar.gz"
      sha256 "${MAC_ARM_SHA}"
    end
    on_intel do
      url "https://github.com/peterddod/plete/releases/download/v${VERSION}/plete-v${VERSION}-x86_64-apple-darwin.tar.gz"
      sha256 "${MAC_INTEL_SHA}"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/peterddod/plete/releases/download/v${VERSION}/plete-v${VERSION}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "${LINUX_ARM_SHA}"
    end
    on_intel do
      url "https://github.com/peterddod/plete/releases/download/v${VERSION}/plete-v${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "${LINUX_INTEL_SHA}"
    end
  end

  depends_on "git"

  def install
    bin.install "plete"
    # Renamed from jeet: running agents' hooks and launchd jobs call
    # bin/jeet, so it stays as a link. pt is never installed here; see
    # \`plete alias install\`.
    bin.install_symlink bin/"plete" => "jeet"
    generate_completions_from_executable(bin/"plete", "completions")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/plete --version")
    assert_match version.to_s, shell_output("#{bin}/jeet --version")
  end
end
RUBY

echo "wrote ${OUT}"
