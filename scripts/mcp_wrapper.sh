#!/bin/bash
# Memra MCP wrapper
#
# DEFAULT: Rust backend only — the production path after the R4 sunset.
# The former v6 runtime was archived under _archive_python_v6/ and is no
# longer a supported runtime path for this checkout.
#
# SINGLE-WRITER RULE (H4): this checkout starts one Rust writer only. WAL mode
# tolerates concurrent readers, but two writers can corrupt the database.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Zombie-guard (Issue #63 defensive fix)
# OpenClaw gateway's cron (*/15) re-spawns `memra serve` when its
# memory-sync path fails; accumulated failed spawns reach storm territory and
# crash autoresearch workers (see docs/known_issues.md §6). The real fix
# belongs on the OpenClaw client (terminal-error classification); this guard
# is cheap insurance so re-enabling the gateway can't nuke MA again.
#
# Bypass: MA_ZOMBIE_GUARD_DISABLE=1 (or true/yes/on) for manual debugging.
case "${MA_ZOMBIE_GUARD_DISABLE:-0}" in
    1|true|yes|on|TRUE|YES|ON|True|Yes|On)
        ;;
    *)
        if ! command -v pgrep >/dev/null 2>&1; then
            echo "[mcp_wrapper] pgrep unavailable; zombie-guard DISABLED" >&2
        else
            # Pattern `(^|/)(ma|memra) serve` matches the actual MA
            # backend binaries, not incidental strings in other processes'
            # argv (e.g. zsh shells that embed the phrase in a Codex task
            # prompt). The wrapper script cmdline is `bash mcp_wrapper.sh`
            # so self-exclusion is automatic.
            # Default 8 leaves headroom above the intended steady-state
            # (1-2 live `memra serve` processes) while failing before process
            # buildup hides a client-side respawn bug. Tune via
            # MA_MAX_SIBLINGS=N for controlled debugging.
            MA_MAX_SIBLINGS="${MA_MAX_SIBLINGS:-8}"
            # Portability: macOS/BSD pgrep does NOT support -c, so count via pipe.
            # Wrap pgrep in `{ ...; true; }` group so no-match (exit 1) does not
            # trip `set -euo pipefail` and kill the wrapper on a clean box.
            SIBLING_COUNT=$({ pgrep -f '(^|/)(ma|memra) serve' 2>/dev/null || true; } | wc -l | tr -d ' ')
            SIBLING_COUNT=${SIBLING_COUNT:-0}
            [[ "$SIBLING_COUNT" =~ ^[0-9]+$ ]] || SIBLING_COUNT=0
            if (( SIBLING_COUNT >= MA_MAX_SIBLINGS )); then
                echo "[mcp_wrapper] ${SIBLING_COUNT} MA serve processes already alive (limit ${MA_MAX_SIBLINGS}); refusing to start (Issue #63 zombie-guard)" >&2
                echo "[mcp_wrapper] set MA_ZOMBIE_GUARD_DISABLE=1 to override, or kill stale processes first" >&2
                # exit 75 = EX_TEMPFAIL; signals temporary refusal to MCP clients
                # so they surface the error instead of showing "connected but
                # no tools." No effect on OpenClaw's cron-based retry cadence.
                exit 75
            fi
        fi
        ;;
esac

# Truth banner: print the real database state before backend startup so agents
# do not mistake wrong-path 0-byte files for data loss.
"$SCRIPT_DIR/ma-truth-banner.sh" || true

# Backend selector priority: MA_BACKEND env > ~/.memra/config.yaml > rust.
# R4 accepts only rust. A stale config value of python is surfaced loudly so it
# can be corrected instead of silently reviving the archived runtime.
MA_CONFIG_YAML="${HOME}/.memra/config.yaml"
BACKEND="${MA_BACKEND:-}"
if [[ -z "$BACKEND" ]] && [[ -f "$MA_CONFIG_YAML" ]]; then
  # Minimal YAML reader: match top-level "backend:" key (zero indent only),
  # strip quotes/whitespace; only accepts rust|python so stale python configs
  # can emit a precise R4 error below.
  # Anchored at column 0 so nested keys (e.g. "  backend: rust" inside a
  # section) are rejected — matching the Python reader's top-level-only contract.
  CFG_BACKEND="$(
    awk '
      /^backend:[[:space:]]*/ {
        sub(/^backend:[[:space:]]*/, "", $0)
        sub(/[[:space:]]*#.*$/, "", $0)
        gsub(/["'"'"']/, "", $0)
        gsub(/[[:space:]]/, "", $0)
        if ($0 == "rust" || $0 == "python") {
          print $0
        }
        exit
      }
    ' "$MA_CONFIG_YAML" 2>/dev/null
  )"
  if [[ "$CFG_BACKEND" == "rust" || "$CFG_BACKEND" == "python" ]]; then
    BACKEND="$CFG_BACKEND"
  fi
fi
BACKEND="${BACKEND:-rust}"

case "${MA_BACKEND_PROBE:-0}" in
  1|true|yes|on|TRUE|YES|ON|True|Yes|On)
    if [[ "$BACKEND" != "rust" ]]; then
      echo "MA_BACKEND must be rust after R4 Python sunset (got: '$BACKEND')" >&2
      exit 2
    fi
    echo "{\"backend\":\"$BACKEND\",\"project\":\"${MCP_MEMORY_PROJECT_ID:-memra}\",\"single_writer_rule\":\"Rust is the only active writer in the R4 checkout\"}"
    exit 0
    ;;
esac

if [[ "$BACKEND" != "rust" ]]; then
  echo "MA_BACKEND must be rust after R4 Python sunset (got: '$BACKEND')" >&2
  echo "Remove or update backend: python in $MA_CONFIG_YAML, or use an archived v6 checkout for rollback archaeology." >&2
  exit 2
fi

BIN="$PROJECT_DIR/target/release/memra"
BUILD_LOCK="$PROJECT_DIR/target/.ma-build.lock"
if [[ ! -x "$BIN" ]]; then
  echo "[mcp_wrapper] Rust binary not found; building/waiting for build lock..." >&2
  mkdir -p "$PROJECT_DIR/target"
  if mkdir "$BUILD_LOCK" 2>/dev/null; then
    cleanup_build_lock() { rmdir "$BUILD_LOCK" 2>/dev/null || true; }
    trap cleanup_build_lock EXIT INT TERM
    echo "[mcp_wrapper] acquired build lock; running cargo build --release --bin memra" >&2
    (cd "$PROJECT_DIR" && cargo build --release --bin memra >&2)
    cleanup_build_lock
    trap - EXIT INT TERM
  else
    echo "[mcp_wrapper] another process is building ma; waiting for $BIN" >&2
    for _ in $(seq 1 180); do
      [[ -x "$BIN" ]] && break
      sleep 1
    done
  fi
fi

if [[ ! -x "$BIN" ]]; then
  echo "[mcp_wrapper] Rust binary still missing after build wait; refusing to spawn parallel cargo build" >&2
  exit 75
fi

exec "$BIN" serve --project "${MCP_MEMORY_PROJECT_ID:-memra}"
