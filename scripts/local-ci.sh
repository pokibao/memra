#!/usr/bin/env bash
# local-ci.sh - R4 cargo-only local gate.
#
# Usage:
#   ./scripts/local-ci.sh          # fmt + clippy + tests + release build
#   ./scripts/local-ci.sh --fast   # fmt + clippy + tests, skip release build
#   ./scripts/local-ci.sh --rust-only
#
# Legacy language gates were retired in R4. Archived v6 surfaces live under
# _archive_python_v6/ and must not be required for v7.0.0.

set -euo pipefail

MODE="full"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --fast)
            MODE="fast"
            shift
            ;;
        --rust-only|--rust)
            MODE="full"
            shift
            ;;
        --py-only|--py)
            echo "error: Python gates were archived in R4; use cargo-only local-ci" >&2
            exit 2
            ;;
        -h|--help)
            awk 'NR==1{next} /^$/{exit} {sub(/^# ?/,""); print}' "$0"
            exit 0
            ;;
        *)
            echo "error: unknown flag: $1" >&2
            echo "usage: $(basename "$0") [--fast | --rust-only]" >&2
            exit 2
            ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

if [[ -t 1 ]]; then
    RED=$'\e[31m'; GREEN=$'\e[32m'; YELLOW=$'\e[33m'; BOLD=$'\e[1m'; RESET=$'\e[0m'
else
    RED=""; GREEN=""; YELLOW=""; BOLD=""; RESET=""
fi

step() { printf '\n%s\n' "${BOLD}> $*${RESET}"; }
pass() { printf '%s\n' "${GREEN}ok: $*${RESET}"; }
fail() { printf '%s\n' "${RED}fail: $*${RESET}"; }
skip() { printf '%s\n' "${YELLOW}skip: $*${RESET}"; }

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        fail "$1 not found in PATH"
        return 1
    fi
}

run_rust() {
    require_cmd cargo

    step "Rust: cargo fmt --all --check"
    cargo fmt --all --check
    pass "fmt clean"

    step "Rust: cargo clippy --workspace --all-targets -- -D warnings"
    cargo clippy --workspace --all-targets -- -D warnings
    pass "clippy clean"

    step "Rust: cargo test --workspace"
    cargo test --workspace
    pass "workspace tests green"

    if [[ "$MODE" == "fast" ]]; then
        skip "release build (--fast mode)"
    else
        step "Rust: cargo build --release --bin memra"
        cargo build --release --bin memra
        pass "release build ok"
    fi
}

run_shell() {
    step "Shell: bash -n active shell entrypoints"
    local failed=0
    while IFS= read -r shell; do
        [[ -n "$shell" ]] || continue
        [[ "$shell" == _archive_python_v6/* ]] && continue
        [[ -f "$shell" ]] || continue
        bash -n "$shell" || failed=1
    done < <(
        {
            git ls-files '*.sh'
            git ls-files 'scripts/git-hooks/*'
        } | sort -u
    )
    if [[ "$failed" != "0" ]]; then
        fail "bash -n failed"
        return 1
    fi
    pass "shell syntax clean"
}

run_python_sunset_gate() {
    step "R4: active Python file sunset gate"
    local active_py
    active_py="$(
        find . -name '*.py' \
            -not -path './_archive_python_v6/*' \
            -not -path './plugins/memra/*' \
            -not -path './.venv/*' \
            -not -path './target/*' \
            -not -path './.git/*' \
            | sort
    )"
    if [[ -n "$active_py" ]]; then
        printf '%s\n' "$active_py" >&2
        fail "active Python files remain outside the approved plugin adapter"
        return 1
    fi
    pass "no active Python files outside plugin adapter"
}

run_diff_check() {
    step "Diff: git diff --check"
    git diff --check
    pass "diff clean"
}

start_time="$(date +%s)"
run_python_sunset_gate
run_rust
run_shell
run_diff_check
elapsed="$(( $(date +%s) - start_time ))"

printf '\n%s\n' "${GREEN}${BOLD}ok: all cargo-only checks passed in ${elapsed}s (mode=$MODE)${RESET}"
