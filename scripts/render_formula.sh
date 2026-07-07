#!/usr/bin/env bash
# render_formula.sh — render packaging/homebrew/*.rb with real values
#
# Usage:
#   scripts/render_formula.sh [--template packaging/homebrew/memra@next.rb] \
#     <version> <macos-arm64.tar.gz> <linux-x64.tar.gz>
#
# Outputs the rendered formula to stdout. Redirect to a file if needed.
#
# Example:
#   scripts/render_formula.sh 0.6.0 \
#     dist/memra-aarch64-apple-darwin.tar.gz \
#     dist/memra-x86_64-unknown-linux-gnu.tar.gz \
#     > packaging/homebrew/memra.rb
#
#   scripts/render_formula.sh --template packaging/homebrew/memra@next.rb 0.6.0 \
#     dist/memra-aarch64-apple-darwin.tar.gz \
#     dist/memra-x86_64-unknown-linux-gnu.tar.gz \
#     > packaging/homebrew/memra@next.rb
#
# Supported targets match .github/workflows/rust-release.yml. Intel Mac and
# Linux ARM64 are blocked by ort-sys prebuilt binary availability (T9).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TEMPLATE="${REPO_ROOT}/packaging/homebrew/memra.rb"

print_usage() {
  cat >&2 <<'EOF'
Usage:
  scripts/render_formula.sh [--template PATH] <version> <macos-arm64.tar.gz> <linux-x64.tar.gz>

Examples:
  scripts/render_formula.sh 0.6.0 dist/macos-arm64.tar.gz dist/linux-x64.tar.gz > packaging/homebrew/memra.rb
  scripts/render_formula.sh --template packaging/homebrew/memra@next.rb 0.6.0 dist/macos-arm64.tar.gz dist/linux-x64.tar.gz > packaging/homebrew/memra@next.rb
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --template)
      if [[ $# -lt 2 ]]; then
        print_usage
        exit 1
      fi
      TEMPLATE="$2"
      shift 2
      ;;
    --help|-h)
      print_usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    -*)
      echo "error: unknown option: $1" >&2
      print_usage
      exit 1
      ;;
    *)
      break
      ;;
  esac
done

if [[ $# -ne 3 ]]; then
  print_usage
  exit 1
fi

VERSION="$1"
MACOS_ARM64_FILE="$2"
LINUX_X64_FILE="$3"

if [[ ! -f "${TEMPLATE}" ]]; then
  if [[ -f "${REPO_ROOT}/${TEMPLATE}" ]]; then
    TEMPLATE="${REPO_ROOT}/${TEMPLATE}"
  else
    echo "error: template not found: ${TEMPLATE}" >&2
    exit 1
  fi
fi

sha256_of() {
  local file="$1"
  if [[ ! -f "${file}" ]]; then
    echo "error: file not found: ${file}" >&2
    exit 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${file}" | awk '{print $1}'
  else
    shasum -a 256 "${file}" | awk '{print $1}'
  fi
}

MACOS_ARM64_SHA="$(sha256_of "${MACOS_ARM64_FILE}")"
LINUX_X64_SHA="$(sha256_of "${LINUX_X64_FILE}")"

sed \
  -e "s/__VERSION__/${VERSION}/g" \
  -e "s/__MACOS_ARM64_SHA__/${MACOS_ARM64_SHA}/g" \
  -e "s/__LINUX_X64_SHA__/${LINUX_X64_SHA}/g" \
  "${TEMPLATE}"
