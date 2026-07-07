#!/usr/bin/env bash
# install_phase1_hooks.sh — Install Phase 1 Stop + PostToolUse hooks into ~/.claude/settings.json
#
# Usage:
#   ./install_phase1_hooks.sh                # DRY_RUN mode (safe default, logs only, no writes)
#   ./install_phase1_hooks.sh --enable        # Real-write mode (unsets DRY_RUN)
#   ./install_phase1_hooks.sh --uninstall     # Remove Phase 1 hook entries
#   ./install_phase1_hooks.sh --dry-run       # Alias for default (explicit DRY_RUN mode)
#
# Safety:
#   - Backs up ~/.claude/settings.json before any modification
#   - Validates JSON before overwriting
#   - Rolls back to backup on error
#   - Never writes to settings.json directly (always tmp + validate + rename)
#   - Requires jq for JSON rewriting; does not shell out to Python
#
# Design contract: docs/phase1-stop-hook-design.md §1.2 §8.3 §9

set -euo pipefail

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------
SETTINGS_FILE="${HOME}/.claude/settings.json"
TIMESTAMP=$(date +"%Y%m%d-%H%M%S")
BACKUP_FILE="${SETTINGS_FILE}.bak.${TIMESTAMP}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOOK_SCRIPT="${REPO_DIR}/hooks/ma-hook-rust.sh"

MODE="dry_run"  # default: safe

# ---------------------------------------------------------------------------
# Parse args
# ---------------------------------------------------------------------------
for arg in "$@"; do
    case "$arg" in
        --enable)
            MODE="enable"
            ;;
        --uninstall)
            MODE="uninstall"
            ;;
        --dry-run)
            MODE="dry_run"
            ;;
        -h|--help)
            grep '^#' "$0" | sed 's/^# \?//'
            exit 0
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            echo "Use --enable, --uninstall, --dry-run, or --help" >&2
            exit 1
            ;;
    esac
done

echo "[phase1-hooks] Mode: ${MODE}"
echo "[phase1-hooks] Repo:  ${REPO_DIR}"
echo "[phase1-hooks] Script: ${HOOK_SCRIPT}"

# ---------------------------------------------------------------------------
# Verify hook script exists
# ---------------------------------------------------------------------------
if [[ ! -f "${HOOK_SCRIPT}" ]]; then
    echo "[ERROR] Hook script not found: ${HOOK_SCRIPT}" >&2
    exit 1
fi

if ! command -v jq &>/dev/null; then
    echo "[ERROR] jq is required for Phase 1 hook settings updates" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Ensure settings.json exists
# ---------------------------------------------------------------------------
if [[ ! -f "${SETTINGS_FILE}" ]]; then
    echo "[phase1-hooks] settings.json not found — creating minimal skeleton"
    mkdir -p "$(dirname "${SETTINGS_FILE}")"
    echo '{}' > "${SETTINGS_FILE}"
fi

# ---------------------------------------------------------------------------
# Backup
# ---------------------------------------------------------------------------
cp "${SETTINGS_FILE}" "${BACKUP_FILE}"
echo "[phase1-hooks] Backup: ${BACKUP_FILE}"

# ---------------------------------------------------------------------------
# Rollback helper
# ---------------------------------------------------------------------------
_rollback() {
    echo "[ERROR] Rolling back to backup..." >&2
    cp "${BACKUP_FILE}" "${SETTINGS_FILE}" || true
    exit 1
}
trap _rollback ERR

# ---------------------------------------------------------------------------
# JSON validation
# ---------------------------------------------------------------------------
_validate_json() {
    local file="$1"
    jq empty "$file" 2>/dev/null
}

_validate_json "${SETTINGS_FILE}" || {
    echo "[ERROR] Current settings.json is not valid JSON — aborting" >&2
    rm -f "${BACKUP_FILE}"
    trap - ERR
    exit 1
}

# ---------------------------------------------------------------------------
# Build hook command strings
# ---------------------------------------------------------------------------
STOP_CMD="cd ${REPO_DIR} && /bin/bash ${HOOK_SCRIPT} stop"
POSTTOOL_CMD="cd ${REPO_DIR} && /bin/bash ${HOOK_SCRIPT} posttool"

# ---------------------------------------------------------------------------
# UNINSTALL path
# ---------------------------------------------------------------------------
if [[ "${MODE}" == "uninstall" ]]; then
    echo "[phase1-hooks] Removing Phase 1 hook entries..."

    TMP_FILE=$(mktemp)
    # shellcheck disable=SC2064  # TMP_FILE must expand at trap install time, not at signal time
    trap "_rollback; rm -f '${TMP_FILE}'" ERR

    # Remove old direct-Python and current Rust-wrapper Phase 1 hook entries.
    jq '
        if .hooks then
            .hooks |= (
                if .Stop then
                    .Stop |= map(
                        .hooks |= map(select(.command | test("stop_hook_entry.py|ma-hook-rust.sh (stop|posttool)") | not))
                    ) | .Stop |= map(select(.hooks | length > 0))
                else . end |
                if .PostToolUse then
                    .PostToolUse |= map(
                        .hooks |= map(select(.command | test("stop_hook_entry.py|ma-hook-rust.sh (stop|posttool)") | not))
                    ) | .PostToolUse |= map(select(.hooks | length > 0))
                else . end
            )
        else .
        end
    ' "${SETTINGS_FILE}" > "${TMP_FILE}"

    _validate_json "${TMP_FILE}" || {
        echo "[ERROR] Output is not valid JSON after uninstall — rolling back" >&2
        rm -f "${TMP_FILE}"
        _rollback
    }

    mv "${TMP_FILE}" "${SETTINGS_FILE}"
    trap - ERR
    echo "[phase1-hooks] Uninstall complete. Backup preserved: ${BACKUP_FILE}"
    exit 0
fi

# ---------------------------------------------------------------------------
# INSTALL path (dry_run or enable)
# ---------------------------------------------------------------------------
if [[ "${MODE}" == "dry_run" ]]; then
    ENV_VARS='{"MA_PHASE1_DRY_RUN": "1"}'
    MODE_LABEL="DRY_RUN (logging only, no writes)"
else
    ENV_VARS='{}'
    MODE_LABEL="REAL WRITE MODE"
fi

echo "[phase1-hooks] Installing hooks in mode: ${MODE_LABEL}"

TMP_FILE=$(mktemp)
# shellcheck disable=SC2064  # TMP_FILE must expand at trap install time, not at signal time
trap "_rollback; rm -f '${TMP_FILE}'" ERR

STOP_CMD_ESC="${STOP_CMD}"
POSTTOOL_CMD_ESC="${POSTTOOL_CMD}"

jq --arg stop_cmd "${STOP_CMD_ESC}" \
   --arg posttool_cmd "${POSTTOOL_CMD_ESC}" \
   --argjson env_vars "${ENV_VARS}" \
   '
   .hooks //= {} |

   # Inject Stop hook (append if not present, avoiding duplicates)
   .hooks.Stop //= [] |
   .hooks.Stop |= (
       map(select(
           .hooks | map(.command | test("stop_hook_entry.py|ma-hook-rust.sh (stop|posttool)")) | any | not
       )) + [{
           "matcher": "*",
           "hooks": [{
               "type": "command",
               "command": $stop_cmd,
               "timeout": 30000,
               "env": $env_vars
           }]
       }]
   ) |

   # Inject PostToolUse hook
   .hooks.PostToolUse //= [] |
   .hooks.PostToolUse |= (
       map(select(
           .hooks | map(.command | test("stop_hook_entry.py|ma-hook-rust.sh (stop|posttool)")) | any | not
       )) + [{
           "matcher": "Edit|Write|mcp__memra__add_rule|mcp__memra__save_checkpoint|mcp__memra__report_outcome",
           "hooks": [{
               "type": "command",
               "command": $posttool_cmd,
               "timeout": 15000,
               "env": $env_vars
           }]
       }]
   )
   ' "${SETTINGS_FILE}" > "${TMP_FILE}"

_validate_json "${TMP_FILE}" || {
    echo "[ERROR] Output is not valid JSON — rolling back" >&2
    rm -f "${TMP_FILE}"
    _rollback
}

mv "${TMP_FILE}" "${SETTINGS_FILE}"
trap - ERR

echo ""
echo "[phase1-hooks] Install complete!"
echo "[phase1-hooks] Mode:   ${MODE_LABEL}"
echo "[phase1-hooks] Backup: ${BACKUP_FILE}"
echo ""
if [[ "${MODE}" == "dry_run" ]]; then
    echo "  Hooks installed in DRY_RUN mode."
    echo "  Facts will be logged to ~/.memra/logs/stop_hook_dry_run.jsonl"
    echo "  No memories will be written to MA."
    echo ""
    echo "  To enable real writes after reviewing dry-run logs:"
    echo "    ${REPO_DIR}/scripts/install_phase1_hooks.sh --enable"
    echo ""
    echo "  To disable hooks entirely:"
    echo "    ${REPO_DIR}/scripts/install_phase1_hooks.sh --uninstall"
else
    echo "  Hooks installed in REAL WRITE mode."
    echo "  High-confidence facts (>=0.85) will be automatically added to Memra."
    echo ""
    echo "  Emergency kill switch (no reinstall needed):"
    echo "    export MA_PHASE1_DISABLED=1"
    echo ""
    echo "  To revert to DRY_RUN mode:"
    echo "    ${REPO_DIR}/scripts/install_phase1_hooks.sh --dry-run"
fi
