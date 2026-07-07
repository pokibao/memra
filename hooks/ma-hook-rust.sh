#!/usr/bin/env bash
# Thin Claude Code hook wrapper for Rust-owned Memra hooks.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="${CLAUDE_PLUGIN_ROOT:-$(cd "$SCRIPT_DIR/.." && pwd)}"

if [[ $# -lt 1 ]]; then
    echo "usage: ma-hook-rust.sh <hook-subcommand> [args...]" >&2
    exit 2
fi

HOOK_SUBCOMMAND="$1"
shift

MA_HOOK_MA_BIN="${MA_HOOK_MA_BIN:-$PROJECT_DIR/target/release/memra}"

ensure_ma_hook_binary() {
    if [[ -x "$MA_HOOK_MA_BIN" ]] \
        && "$MA_HOOK_MA_BIN" hook "$HOOK_SUBCOMMAND" --help >/dev/null 2>&1
    then
        return 0
    fi
    if [[ "$MA_HOOK_MA_BIN" != "$PROJECT_DIR/target/release/memra" ]]; then
        echo "[ma-hook-rust] MA_HOOK_MA_BIN is not executable or lacks hook $HOOK_SUBCOMMAND: $MA_HOOK_MA_BIN" \
            >&2
        return 127
    fi
    echo "[ma-hook-rust] Rust memra binary missing or stale; building release binary for hook $HOOK_SUBCOMMAND..." \
        >&2
    (cd "$PROJECT_DIR" && cargo build --release -p memra-server --bin memra >&2)
}

ensure_ma_hook_binary
export MA_HOOK_PROJECT_DIR="$PROJECT_DIR"
exec "$MA_HOOK_MA_BIN" hook "$HOOK_SUBCOMMAND" "$@"
