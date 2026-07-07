#!/usr/bin/env bash
# check_no_plaintext_secrets.sh — SEC-02 pre-commit / CI gate
#
# Scans tracked source files for plaintext secret patterns.
# Exits 1 if any real secrets are detected (not examples / noqa-annotated lines).
#
# Patterns checked:
#   sk_live_           Stripe live secret keys
#   ghp_               GitHub Personal Access Tokens
#   eyJ[A-Za-z0-9]    JWT headers (base64-encoded `{`)
#   AKIA               AWS Access Key IDs
#   bot[0-9]+:AA       Telegram Bot tokens
#   AKL                Alibaba Cloud / ByteDance-style access keys (AKLT…)
#
# Exclusions:
#   - Lines annotated with # noqa: secrets-example
#   - Archived v6 sources under _archive_python_v6/
#   - This script itself
#   - .venv/, node_modules/ (third-party / local environments)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT_NAME="$(basename "${BASH_SOURCE[0]}")"

# Patterns that indicate a real secret assignment (value present after = or :)
PATTERNS=(
    'sk_live_[A-Za-z0-9]{16,}'
    'ghp_[A-Za-z0-9]{36}'
    'AKIA[A-Z0-9]{16}'
    'AKLT[A-Za-z0-9+/_-]{20,}'
    'bot[0-9]{8,10}:AA[A-Za-z0-9_-]{30,}'
    'eyJ[A-Za-z0-9+/_-]{20,}\.[A-Za-z0-9+/_-]{10,}'
)

# Paths to exclude from scanning
EXCLUDE_PATHS=(
    ".venv"
    "node_modules"
    "_archive_python_v6"
    "$SCRIPT_NAME"                     # skip self
)

FOUND=0

# Build exclude regex for grep
EXCLUDE_GREP=""
for excl in "${EXCLUDE_PATHS[@]}"; do
    EXCLUDE_GREP="${EXCLUDE_GREP}${excl}|"
done
EXCLUDE_GREP="${EXCLUDE_GREP%|}"  # strip trailing pipe

echo "[SEC-02] Scanning for plaintext secrets in $REPO_ROOT ..."

for PATTERN in "${PATTERNS[@]}"; do
    # grep tracked files only; skip excluded paths and noqa-annotated lines.
    # NB: use -E (POSIX extended). macOS BSD grep silently errors on -P and
    # with 2>/dev/null + pipefail+|| true the whole pipe returns empty —
    # which would make this gate a fake pass. All our patterns are -E safe.
    MATCHES=$(git -C "$REPO_ROOT" ls-files -- \
        '*.py' '*.rs' '*.sh' '*.yaml' '*.yml' '*.toml' '*.json' '*.md' '.env' \
        2>/dev/null \
        | grep -vE "$EXCLUDE_GREP" \
        | (cd "$REPO_ROOT" && xargs grep -lE "$PATTERN" 2>/dev/null) \
        | (cd "$REPO_ROOT" && xargs grep -E "$PATTERN" 2>/dev/null) \
        | grep -v "# noqa: secrets-example" \
        | grep -v "r'" \
        | grep -v '^\s*#' \
        || true)

    if [[ -n "$MATCHES" ]]; then
        echo ""
        echo "[FAIL] Pattern detected: $PATTERN"
        echo "$MATCHES" | head -5
        FOUND=1
    fi
done

if [[ $FOUND -eq 0 ]]; then
    echo "[PASS] No plaintext secrets detected in tracked files."
    exit 0
else
    echo ""
    echo "[FAIL] Plaintext secrets found. Migrate them with: akm add <KEY_NAME>"
    echo "       Then replace the value with: akm get <KEY_NAME>"
    echo "       See docs/sec-migration-plan-2026-04-15.md for the full inventory."
    exit 1
fi
