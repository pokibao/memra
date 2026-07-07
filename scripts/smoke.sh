#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ORIGINAL_HOME="${HOME}"
PROJECT="${1:-phase4-smoke}"
PORT="${MA_SMOKE_PORT:-17331}"
KEEP_HOME="${MA_SMOKE_KEEP_HOME:-0}"
TMP_HOME="${MA_SMOKE_HOME:-$(mktemp -d "${TMPDIR:-/tmp}/ma-smoke.XXXXXX")}"
SERVER_PID=""

cleanup() {
  if [[ -n "${SERVER_PID}" ]]; then
    kill -TERM "${SERVER_PID}" >/dev/null 2>&1 || true
    wait "${SERVER_PID}" >/dev/null 2>&1 || true
  fi
  if [[ "${KEEP_HOME}" != "1" && -z "${MA_SMOKE_HOME:-}" ]]; then
    rm -rf "${TMP_HOME}"
  fi
}
trap cleanup EXIT

run_ma() {
  if [[ -n "${MA_BIN:-}" ]]; then
    # MA_BIN is set — refuse to fall back to cargo run, or a broken CI artifact
    # would silently pass by rebuilding from source (false green release).
    if [[ ! -x "${MA_BIN}" ]]; then
      echo "[smoke] MA_BIN='${MA_BIN}' is set but not executable" >&2
      exit 3
    fi
    "${MA_BIN}" "$@"
  else
    cargo run -q -p memra-server --bin memra -- "$@"
  fi
}

wait_for_http() {
  local url="$1"
  for _ in $(seq 1 80); do
    if curl -fsS "${url}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

export HOME="${TMP_HOME}"
export CARGO_HOME="${CARGO_HOME:-${ORIGINAL_HOME}/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-${ORIGINAL_HOME}/.rustup}"
mkdir -p "${HOME}"

cd "${ROOT_DIR}"

echo "[smoke] HOME=${HOME}"
echo "[smoke] init"
run_ma init --project "${PROJECT}" --force >/tmp/ma-smoke-init.log

echo "[smoke] demo"
run_ma demo --project "${PROJECT}" --force >/tmp/ma-smoke-demo.log

echo "[smoke] doctor"
run_ma doctor --project "${PROJECT}" --json > /tmp/ma-smoke-doctor.json

echo "[smoke] stats"
run_ma stats --project "${PROJECT}" --json > /tmp/ma-smoke-stats.json

echo "[smoke] HTTP server"
START_TS="$(date +%s)"
RUST_LOG=error run_ma serve --http --project "${PROJECT}" --port "${PORT}" \
  >/tmp/ma-smoke-http.out 2>/tmp/ma-smoke-http.err &
SERVER_PID="$!"

wait_for_http "http://127.0.0.1:${PORT}/health"
READY_TS="$(date +%s)"
STARTUP_SECONDS="$((READY_TS - START_TS))"

HEALTH_BODY="$(curl -fsS "http://127.0.0.1:${PORT}/health")"
METRICS_BODY="$(curl -fsS "http://127.0.0.1:${PORT}/metrics")"
MCP_STATUS="$(curl -s -o /dev/null -w '%{http_code}' \
  -X POST "http://127.0.0.1:${PORT}/mcp" \
  -H 'host: 127.0.0.1' \
  -d '{}')"

if [[ "${HEALTH_BODY}" != "ok" ]]; then
  echo "[smoke] unexpected /health body: ${HEALTH_BODY}" >&2
  exit 1
fi

if ! grep -q 'ma_http_up 1' <<<"${METRICS_BODY}"; then
  echo "[smoke] /metrics missing ma_http_up" >&2
  exit 1
fi

if [[ "${MCP_STATUS}" != "401" ]]; then
  echo "[smoke] expected unauthenticated /mcp to return 401, got ${MCP_STATUS}" >&2
  exit 1
fi

STOP_START="$(date +%s)"
kill -TERM "${SERVER_PID}"
wait "${SERVER_PID}" || true
SERVER_PID=""
STOP_END="$(date +%s)"
STOP_SECONDS="$((STOP_END - STOP_START))"

echo "[smoke] summary"
echo "  project: ${PROJECT}"
echo "  startup_seconds: ${STARTUP_SECONDS}"
echo "  shutdown_seconds: ${STOP_SECONDS}"
echo "  doctor_json: /tmp/ma-smoke-doctor.json"
echo "  stats_json: /tmp/ma-smoke-stats.json"
echo "  http_no_auth_status: ${MCP_STATUS}"
