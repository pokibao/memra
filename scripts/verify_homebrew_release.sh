#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_SLUG="${MA_RELEASE_REPO:-memra/memra}"
FORMULA_REF="${MA_HOMEBREW_FORMULA:-memra/tap/memra}"
FORMULA_PATH="${ROOT_DIR}/packaging/homebrew/memra.rb"

usage() {
  cat >&2 <<'EOF'
Usage:
  scripts/verify_homebrew_release.sh <tag>

Examples:
  scripts/verify_homebrew_release.sh v7.0.0
  scripts/verify_homebrew_release.sh 7.0.0

Checks:
  1. required release assets exist on GitHub
  2. each browser_download_url is reachable anonymously
  3. run brew fetch against either the matching local formula or the public tap
     formula to catch false-green Homebrew claims
EOF
}

if [[ $# -ne 1 ]]; then
  usage
  exit 1
fi

RAW_TAG="$1"
TAG="${RAW_TAG#v}"
TAG="v${TAG}"
VERSION="${TAG#v}"

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "[verify-homebrew-release] missing required command: ${cmd}" >&2
    exit 1
  fi
}

require_cmd gh
require_cmd curl

ASSETS=(
  "memra-aarch64-apple-darwin.tar.gz"
  "memra-x86_64-unknown-linux-gnu.tar.gz"
)

echo "[verify-homebrew-release] repo=${REPO_SLUG} tag=${TAG}"

for asset in "${ASSETS[@]}"; do
  asset_tsv="$(
    gh api "repos/${REPO_SLUG}/releases/tags/${TAG}" \
      --jq ".assets[] | select(.name == \"${asset}\") | [.browser_download_url, (.digest // \"\")] | @tsv"
  )"

  if [[ -z "${asset_tsv}" ]]; then
    echo "[verify-homebrew-release] missing release asset: ${asset}" >&2
    exit 1
  fi

  asset_url="${asset_tsv%%$'\t'*}"
  asset_digest="${asset_tsv#*$'\t'}"

  echo "[verify-homebrew-release] checking asset=${asset}"
  echo "  url: ${asset_url}"
  if [[ -n "${asset_digest}" ]]; then
    echo "  digest: ${asset_digest}"
  fi

  curl -fsSIL "${asset_url}" >/dev/null
done

if command -v brew >/dev/null 2>&1; then
  formula_version=""
  if [[ -f "${FORMULA_PATH}" ]]; then
    formula_version="$(sed -n 's/^[[:space:]]*version \"\\([^\"]*\\)\"$/\\1/p' "${FORMULA_PATH}" | head -n 1)"
  fi
  if [[ -f "${FORMULA_PATH}" && "${formula_version}" == "${VERSION}" ]]; then
    echo "[verify-homebrew-release] brew fetch local formula"
    HOMEBREW_NO_INSTALL_FROM_API=1 brew fetch --force --formula "${FORMULA_PATH}"
  else
    echo "[verify-homebrew-release] brew fetch tap formula=${FORMULA_REF}"
    HOMEBREW_NO_INSTALL_FROM_API=1 brew fetch --force --formula "${FORMULA_REF}"
  fi
fi

echo "[verify-homebrew-release] ok"
