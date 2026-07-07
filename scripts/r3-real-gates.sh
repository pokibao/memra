#!/usr/bin/env bash
# R3 real gate runner.
#
# Purpose: make the blocked R3 gate reproducible once provider credentials and
# Gemini CLI auth are available on a Pro-class lab host. This script intentionally refuses to
# run by default on the MacBook Air and refuses to rebuild ma implicitly.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

OUT_DIR=""
MA_BIN="${MA_BIN:-}"
GEMINI_HOME="${R3_GEMINI_HOME:-$HOME}"
GEMINI_MODEL="${R3_GEMINI_MODEL:-gemini-3.1-pro-preview}"
ENV_FILE="${R3_ENV_FILE:-}"
BASELINE_AGGREGATE=""
CANDIDATE_AGGREGATE=""
THRESHOLD_PERCENT="5.0"
ALLOW_NON_LAB_HOST="${MA_R3_ALLOW_NON_LAB_HOST:-0}"
ALLOW_MISSING_METRIC_DIFF=0

usage() {
    cat <<'USAGE'
Usage:
  scripts/r3-real-gates.sh [options]

Run the R3 real gate preflight on a Pro-class lab host:
  - strict 4-provider LLM smoke
  - Gemini cached-auth check for unattended autoresearch
  - optional aggregate metric diff when baseline/candidate JSONL files exist

Options:
  --out-dir PATH                 Evidence output dir.
  --ma-bin PATH                  Existing memra binary to run. No implicit rebuild.
  --env-file PATH                Load KEY=value secrets without shell eval.
  --gemini-home PATH             Home dir containing .gemini cached auth.
  --gemini-model MODEL           Gemini CLI probe/model label.
  --baseline-aggregate PATH      Python baseline aggregate JSONL.
  --candidate-aggregate PATH     Rust candidate aggregate JSONL.
  --threshold-percent N          Metric diff threshold, default 5.0.
  --allow-non-lab-host           Bypass host guard for emergency/manual use.
  --allow-missing-metric-diff    Do not fail if aggregate files are omitted.
  -h, --help                     Show this help.

Exit code:
  0 only when selected gates pass.
  1 when any selected gate blocks or when metric-diff inputs are missing.
  2 for usage/configuration errors.

Heavy validation policy:
  Run this on lab-m1 / lab-pro / M1 MacBook Pro. Do not run real provider gates,
  long autoresearch, CI, or soak on the MacBook Air.
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --out-dir)
            OUT_DIR="${2:?missing value for --out-dir}"
            shift 2
            ;;
        --ma-bin)
            MA_BIN="${2:?missing value for --ma-bin}"
            shift 2
            ;;
        --env-file)
            ENV_FILE="${2:?missing value for --env-file}"
            shift 2
            ;;
        --gemini-home)
            GEMINI_HOME="${2:?missing value for --gemini-home}"
            shift 2
            ;;
        --gemini-model)
            GEMINI_MODEL="${2:?missing value for --gemini-model}"
            shift 2
            ;;
        --baseline-aggregate)
            BASELINE_AGGREGATE="${2:?missing value for --baseline-aggregate}"
            shift 2
            ;;
        --candidate-aggregate)
            CANDIDATE_AGGREGATE="${2:?missing value for --candidate-aggregate}"
            shift 2
            ;;
        --threshold-percent)
            THRESHOLD_PERCENT="${2:?missing value for --threshold-percent}"
            shift 2
            ;;
        --allow-non-lab-host)
            ALLOW_NON_LAB_HOST=1
            shift
            ;;
        --allow-missing-metric-diff)
            ALLOW_MISSING_METRIC_DIFF=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ -n "$BASELINE_AGGREGATE" || -n "$CANDIDATE_AGGREGATE" ]]; then
    if [[ -z "$BASELINE_AGGREGATE" || -z "$CANDIDATE_AGGREGATE" ]]; then
        echo "[r3-real-gates] both --baseline-aggregate and --candidate-aggregate are required." >&2
        exit 2
    fi
    if [[ ! -f "$BASELINE_AGGREGATE" ]]; then
        echo "[r3-real-gates] baseline aggregate does not exist: $BASELINE_AGGREGATE" >&2
        exit 2
    fi
    if [[ ! -f "$CANDIDATE_AGGREGATE" ]]; then
        echo "[r3-real-gates] candidate aggregate does not exist: $CANDIDATE_AGGREGATE" >&2
        exit 2
    fi
fi

if [[ -n "$ENV_FILE" && ! -f "$ENV_FILE" ]]; then
    echo "[r3-real-gates] env file does not exist: $ENV_FILE" >&2
    exit 2
fi

HOSTNAME_VALUE="$(hostname)"
ARCH_VALUE="$(uname -m)"
MEM_BYTES="$(sysctl -n hw.memsize 2>/dev/null || echo 0)"

is_lab_m1_host() {
    case "$1" in
        lab-m1|lab-m1.local|lab-pro|lab-pro.local|mbp-lan|mbp-lan.local)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

load_env_file() {
    local line_no=0
    local raw
    local line

    while IFS= read -r raw || [[ -n "$raw" ]]; do
        line_no=$((line_no + 1))
        line="${raw%$'\r'}"

        if [[ -z "$line" || "$line" =~ ^[[:space:]]*# ]]; then
            continue
        fi
        if [[ "$line" == export\ * ]]; then
            line="${line#export }"
        fi
        if [[ ! "$line" =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; then
            echo "[r3-real-gates] invalid env-file line $line_no; expected KEY=value." >&2
            exit 2
        fi

        export "$line"
    done < "$ENV_FILE"
}

if [[ "$ALLOW_NON_LAB_HOST" != "1" ]]; then
    if ! is_lab_m1_host "$HOSTNAME_VALUE"; then
        echo "[r3-real-gates] refusing non-lab host: $HOSTNAME_VALUE" >&2
        echo "[r3-real-gates] pass --allow-non-lab-host only for deliberate manual use." >&2
        exit 2
    fi
    if [[ "$ARCH_VALUE" != "arm64" ]]; then
        echo "[r3-real-gates] refusing non-arm64 host: $ARCH_VALUE" >&2
        exit 2
    fi
    if [[ "$MEM_BYTES" -lt 32000000000 ]]; then
        echo "[r3-real-gates] refusing low-memory host: hw.memsize=$MEM_BYTES" >&2
        exit 2
    fi
fi

if [[ -n "$ENV_FILE" ]]; then
    load_env_file
    echo "[r3-real-gates] env_file=$ENV_FILE"
fi

if [[ -z "$MA_BIN" ]]; then
    if [[ -x "$REPO_ROOT/target/release/memra" ]]; then
        MA_BIN="$REPO_ROOT/target/release/memra"
    elif [[ -x "$REPO_ROOT/target/debug/memra" ]]; then
        MA_BIN="$REPO_ROOT/target/debug/memra"
    else
        echo "[r3-real-gates] no existing memra binary found; set --ma-bin PATH." >&2
        echo "[r3-real-gates] this script will not cargo build implicitly." >&2
        exit 2
    fi
fi

if [[ ! -x "$MA_BIN" ]]; then
    echo "[r3-real-gates] memra binary is not executable: $MA_BIN" >&2
    exit 2
fi

if [[ -z "$OUT_DIR" ]]; then
    stamp="$(date +%Y-%m-%dT%H-%M-%S)"
    OUT_DIR="$REPO_ROOT/docs/dogfood-results/${stamp}-r3-real-gates"
fi
mkdir -p "$OUT_DIR"

run_rc=0
run_capture() {
    local name="$1"
    shift
    local stdout_path="$OUT_DIR/${name}.stdout"
    local stderr_path="$OUT_DIR/${name}.stderr"
    local command_path="$OUT_DIR/${name}.command.txt"
    local sep=""
    local arg

    : > "$command_path"
    for arg in "$@"; do
        printf '%s%q' "$sep" "$arg" >> "$command_path"
        sep=" "
    done
    printf '\n' >> "$command_path"

    set +e
    "$@" >"$stdout_path" 2>"$stderr_path"
    run_rc=$?
    set -e

    echo "$run_rc" > "$OUT_DIR/${name}.exitcode"
    echo "[r3-real-gates] $name exit=$run_rc"
}

json_string() {
    local value="${1-}"
    value="${value//\\/\\\\}"
    value="${value//\"/\\\"}"
    value="${value//$'\n'/\\n}"
    value="${value//$'\r'/\\r}"
    value="${value//$'\t'/\\t}"
    printf '"%s"' "$value"
}

json_nullable_string() {
    if [[ -z "${1-}" ]]; then
        printf 'null'
    else
        json_string "$1"
    fi
}

echo "[r3-real-gates] host=$HOSTNAME_VALUE arch=$ARCH_VALUE mem=$MEM_BYTES"
echo "[r3-real-gates] ma_bin=$MA_BIN"
echo "[r3-real-gates] out_dir=$OUT_DIR"

head_value="$(git rev-parse --short HEAD)"
branch_value="$(git rev-parse --abbrev-ref HEAD)"
echo "$head_value" > "$OUT_DIR/head.txt"
echo "$branch_value" > "$OUT_DIR/branch.txt"

run_capture "llm-smoke-strict" \
    "$MA_BIN" llm smoke --provider all --strict --json
llm_rc="$run_rc"

run_capture "gemini-auth-check" \
    "$MA_BIN" research gemini-auth \
    --home "$GEMINI_HOME" \
    --model "$GEMINI_MODEL" \
    --check \
    --json
gemini_rc="$run_rc"

metric_rc=0
metric_status="not_run"
if [[ -n "$BASELINE_AGGREGATE" || -n "$CANDIDATE_AGGREGATE" ]]; then
    run_capture "autoresearch-metric-diff" \
        "$MA_BIN" research metric-diff \
        --baseline "$BASELINE_AGGREGATE" \
        --candidate "$CANDIDATE_AGGREGATE" \
        --threshold-percent "$THRESHOLD_PERCENT" \
        --json
    metric_rc="$run_rc"
    metric_status="ran"
elif [[ "$ALLOW_MISSING_METRIC_DIFF" != "1" ]]; then
    metric_rc=1
    metric_status="missing_inputs"
fi

run_status="blocked"
if [[ "$llm_rc" -eq 0 && "$gemini_rc" -eq 0 && "$metric_rc" -eq 0 ]]; then
    run_status="passed"
fi
run_date="$(date '+%Y-%m-%d %H:%M:%S %Z')"

cat > "$OUT_DIR/SUMMARY.json" <<EOF
{
  "status": $(json_string "$run_status"),
  "generated_at": $(json_string "$run_date"),
  "host": {
    "hostname": $(json_string "$HOSTNAME_VALUE"),
    "arch": $(json_string "$ARCH_VALUE"),
    "hw_memsize": $MEM_BYTES
  },
  "ma_binary": $(json_string "$MA_BIN"),
  "git": {
    "branch": $(json_string "$branch_value"),
    "head": $(json_string "$head_value")
  },
  "inputs": {
    "env_file": $(json_nullable_string "$ENV_FILE"),
    "gemini_home": $(json_string "$GEMINI_HOME"),
    "gemini_model": $(json_string "$GEMINI_MODEL"),
    "baseline_aggregate": $(json_nullable_string "$BASELINE_AGGREGATE"),
    "candidate_aggregate": $(json_nullable_string "$CANDIDATE_AGGREGATE"),
    "threshold_percent": $(json_string "$THRESHOLD_PERCENT"),
    "allow_missing_metric_diff": $([[ "$ALLOW_MISSING_METRIC_DIFF" == "1" ]] && printf 'true' || printf 'false')
  },
  "gates": {
    "provider_strict_smoke": {
      "exit": $llm_rc,
      "stdout": "llm-smoke-strict.stdout",
      "stderr": "llm-smoke-strict.stderr"
    },
    "gemini_cached_auth": {
      "exit": $gemini_rc,
      "stdout": "gemini-auth-check.stdout",
      "stderr": "gemini-auth-check.stderr"
    },
    "autoresearch_metric_diff": {
      "exit": $metric_rc,
      "status": $(json_string "$metric_status"),
      "stdout": $(if [[ "$metric_status" == "ran" ]]; then json_string "autoresearch-metric-diff.stdout"; else printf 'null'; fi),
      "stderr": $(if [[ "$metric_status" == "ran" ]]; then json_string "autoresearch-metric-diff.stderr"; else printf 'null'; fi)
    }
  },
  "completion_rule": "R3 is complete only when provider strict smoke, Gemini auth, real autoresearch metric diff, and required soak evidence are green or alice signs the required exception."
}
EOF

cat > "$OUT_DIR/SUMMARY.md" <<EOF
# R3 Real Gates Run

Date: $run_date

## Host

- hostname: \`$HOSTNAME_VALUE\`
- arch: \`$ARCH_VALUE\`
- hw.memsize: \`$MEM_BYTES\`
- memra binary: \`$MA_BIN\`
- branch: \`$branch_value\`
- head: \`$head_value\`

## Results

| Gate | Exit | Evidence |
|---|---:|---|
| Provider strict smoke | $llm_rc | \`llm-smoke-strict.stdout\`, \`llm-smoke-strict.stderr\` |
| Gemini cached auth check | $gemini_rc | \`gemini-auth-check.stdout\`, \`gemini-auth-check.stderr\` |
| Autoresearch metric diff | $metric_rc | status: \`$metric_status\` |

Machine-readable summary: \`SUMMARY.json\`

## Interpretation

- R3 is complete only when provider strict smoke exits 0, Gemini auth exits 0,
  real autoresearch metric diff is present and exits 0, and the required soak
  evidence is collected.
- If any provider cannot pass after the approved retry window, do not write a
  \`D-008.R3.<provider>.exception\` without alice sign-off.
EOF

if [[ "$run_status" == "passed" ]]; then
    echo "[r3-real-gates] PASS"
    exit 0
fi

echo "[r3-real-gates] BLOCKED (see $OUT_DIR/SUMMARY.md)" >&2
exit 1
