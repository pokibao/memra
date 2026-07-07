#!/usr/bin/env bash
# 记忆巩固夜间任务 — launchd 每晚 3:00am 运行
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR" || exit 1

LOG_DIR="${MA_CONSOLIDATE_LOG_DIR:-$HOME/.memra/logs/consolidation}"
mkdir -p "$LOG_DIR"
TIMESTAMP="$(date '+%Y%m%d_%H%M%S')"
SUMMARY_TSV="$LOG_DIR/orchestration_${TIMESTAMP}.tsv"
SUMMARY_JSON="$LOG_DIR/orchestration_${TIMESTAMP}.json"
STAGE6_SUMMARY_JSON="$LOG_DIR/consolidation_runtime_${TIMESTAMP}.json"
export MA_CONSOLIDATE_STAGE6_SUMMARY_PATH="$STAGE6_SUMMARY_JSON"
OVERALL_EXIT_CODE=0
MA_CONSOLIDATE_MA_BIN="${MA_CONSOLIDATE_MA_BIN:-$PROJECT_DIR/target/release/memra}"

# --- TODO-OPS-02 hardenings (2026-04-24) ----------------------------------
# A. auto_dream exit-code visibility — see Stage 7 rewrite below.
# B. MA_CONSOLIDATE_SKIP_LLM_ON_UPSTREAM_FAIL=1 — when set AND any upstream
#    stage failed, Stage 7 (reality_check) and Stage 9 (dream) are skipped
#    rather than running LLMs against a known-broken graph. Default: 0
#    (preserve current behavior — run everything, rely on SQLite isolation).
# C. MA_CONSOLIDATE_DRY_RUN=1 — run all --apply-gated stages (1-5, 7 inner,
#    9/10) as dry-run. Stages 6/6B/8 always write (no dry-run flag upstream).
APPLY_FLAG="--apply"
AUTO_DREAM_DRY_RUN_FLAG=""
if [[ "${MA_CONSOLIDATE_DRY_RUN:-0}" == "1" ]]; then
    APPLY_FLAG=""
    AUTO_DREAM_DRY_RUN_FLAG="--dry-run"
    echo "[$(date)] MA_CONSOLIDATE_DRY_RUN=1 — stages 1-5/7/9 will run without --apply (auto_dream uses --dry-run)" >&2
fi
SKIP_LLM_ON_UPSTREAM_FAIL="${MA_CONSOLIDATE_SKIP_LLM_ON_UPSTREAM_FAIL:-0}"

now_ms() {
    "$MA_CONSOLIDATE_MA_BIN" consolidate now-ms
}

ensure_ma_consolidate_binary() {
    if [[ -x "$MA_CONSOLIDATE_MA_BIN" ]] \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate build-summary --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate now-ms >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate strengthen-prune --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate connect --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate chain --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate accuracy --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate synapse --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate runtime --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate reality-check --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate refresh-playbooks --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate refresh-experience --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate dream --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate dream-evolve --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" consolidate candidate-ttl --help >/dev/null 2>&1 \
        && "$MA_CONSOLIDATE_MA_BIN" dream run-pending --help >/dev/null 2>&1
    then
        return 0
    fi
    if [[ "$MA_CONSOLIDATE_MA_BIN" != "$PROJECT_DIR/target/release/memra" ]]; then
        echo "[$(date)] MA_CONSOLIDATE_MA_BIN is not executable or lacks consolidate helpers: $MA_CONSOLIDATE_MA_BIN" \
            >&2
        return 127
    fi
    echo "[$(date)] Rust memra binary missing or stale; building release binary for consolidation helpers..." \
        >&2
    (cd "$PROJECT_DIR" && cargo build --release -p memra-server --bin memra >&2)
}

build_summary_json() {
    if ! ensure_ma_consolidate_binary; then
        OVERALL_EXIT_CODE=1
        echo "[$(date)] Failed to prepare Rust consolidation summary builder. Summary: $SUMMARY_JSON" >&2
        return
    fi
    if ! "$MA_CONSOLIDATE_MA_BIN" consolidate build-summary \
        --summary-tsv "$SUMMARY_TSV" \
        --summary-json "$SUMMARY_JSON" \
        --stage6-summary "$STAGE6_SUMMARY_JSON"
    then
        OVERALL_EXIT_CODE=1
        echo "[$(date)] Phase 6 observability threshold breached. Summary: $SUMMARY_JSON" >&2
    fi
}

run_stage() {
    local stage_key="$1"
    local display_name="$2"
    local log_path="$3"
    local command="$4"

    local started_ms ended_ms duration_ms exit_code status
    started_ms="$(now_ms)"

    echo "[$(date)] Running ${display_name}..."
    bash -lc "$command" >"$log_path" 2>&1
    exit_code=$?
    case "$exit_code" in
        0)
            status="passed"
            ;;
        2)
            # Convention: stage exit 2 = degraded (infra missing, pipeline ran but
            # produced no real work). Distinct from full failure (1) so dashboards
            # can flag silent no-ops without crashing the orchestrator.
            status="degraded"
            OVERALL_EXIT_CODE=1
            echo "[$(date)] Stage ${stage_key} DEGRADED (exit ${exit_code}), continuing..."
            ;;
        *)
            status="failed"
            OVERALL_EXIT_CODE=1
            echo "[$(date)] Stage ${stage_key} failed with exit ${exit_code}, continuing..."
            ;;
    esac

    ended_ms="$(now_ms)"
    duration_ms=$((ended_ms - started_ms))
    printf '%s\t%s\t%s\t%s\t%s\n' \
        "$stage_key" "$status" "$exit_code" "$duration_ms" "$log_path" \
        >> "$SUMMARY_TSV"
}

skip_stage() {
    local stage_key="$1"
    local display_name="$2"
    local log_path="$3"
    local reason="$4"
    local msg
    msg="[$(date)] SKIPPED ${display_name}: ${reason}"
    echo "$msg" >&2
    # Write a real log file so downstream consumers can still open log_path.
    echo "$msg" > "$log_path"
    printf '%s\t%s\t%s\t%s\t%s\n' \
        "$stage_key" "skipped" "0" "0" "$log_path" \
        >> "$SUMMARY_TSV"
}

should_run_llm_stage() {
    if [[ "$SKIP_LLM_ON_UPSTREAM_FAIL" == "1" ]] && [[ "$OVERALL_EXIT_CODE" -ne 0 ]]; then
        return 1
    fi
    return 0
}

STAGE1_CMD="${MA_CONSOLIDATE_STAGE1_CMD:-\"$MA_CONSOLIDATE_MA_BIN\" consolidate strengthen-prune${APPLY_FLAG:+ $APPLY_FLAG} --max-per-phase 100}"
STAGE2_CMD="${MA_CONSOLIDATE_STAGE2_CMD:-\"$MA_CONSOLIDATE_MA_BIN\" consolidate connect${APPLY_FLAG:+ $APPLY_FLAG} --max-links 500}"
STAGE3_CMD="${MA_CONSOLIDATE_STAGE3_CMD:-\"$MA_CONSOLIDATE_MA_BIN\" consolidate chain${APPLY_FLAG:+ $APPLY_FLAG} --max-chains 200}"
STAGE4_CMD="${MA_CONSOLIDATE_STAGE4_CMD:-\"$MA_CONSOLIDATE_MA_BIN\" consolidate accuracy${APPLY_FLAG:+ $APPLY_FLAG} --max-per-phase 50}"
STAGE5_CMD="${MA_CONSOLIDATE_STAGE5_CMD:-\"$MA_CONSOLIDATE_MA_BIN\" consolidate synapse${APPLY_FLAG:+ $APPLY_FLAG} --max-hebbian 10 --max-pruned 50 --max-bridges 10}"
STAGE6_CMD="${MA_CONSOLIDATE_STAGE6_CMD:-\"$MA_CONSOLIDATE_MA_BIN\" consolidate runtime --rust-writer --assert-runtime-truth --summary-output \"$MA_CONSOLIDATE_STAGE6_SUMMARY_PATH\"}"
STAGE6B_CMD="${MA_CONSOLIDATE_STAGE6B_CMD:-\"$MA_CONSOLIDATE_MA_BIN\" consolidate refresh-playbooks --refresh-evidence}"
# Stage 7 rewrite (TODO-OPS-02 hardening A):
# - Capture auto_dream exit code (was `|| true` silently swallowed).
# - Emit exit code to DREAM_LOG header for postmortem.
# - Propagate non-zero auto_dream exit as stage failure so TSV reflects truth.
# - Reality check only runs when auto_dream=0 AND status=no_pending_job.
STAGE7_CMD="${MA_CONSOLIDATE_STAGE7_CMD:-DREAM_LOG=\"$LOG_DIR/auto_dream_${TIMESTAMP}.json\"; \"$MA_CONSOLIDATE_MA_BIN\" dream run-pending --project memra${AUTO_DREAM_DRY_RUN_FLAG:+ $AUTO_DREAM_DRY_RUN_FLAG} --json > \"\$DREAM_LOG\" 2>&1; AUTO_DREAM_EXIT=\$?; echo \"[nightly] auto_dream exit=\$AUTO_DREAM_EXIT\" >&2; if [ \$AUTO_DREAM_EXIT -eq 0 ] && grep -q '\"status\": \"no_pending_job\"' \"\$DREAM_LOG\"; then \"$MA_CONSOLIDATE_MA_BIN\" consolidate reality-check${APPLY_FLAG:+ $APPLY_FLAG} --max-per-phase 30 --llm-max 10; elif [ \$AUTO_DREAM_EXIT -ne 0 ]; then echo \"[nightly] auto_dream crashed, skipping reality_check\" >&2; exit \$AUTO_DREAM_EXIT; fi}"
STAGE8_CMD="${MA_CONSOLIDATE_STAGE8_CMD:-\"$MA_CONSOLIDATE_MA_BIN\" consolidate refresh-experience --limit 12}"
STAGE9_CMD="${MA_CONSOLIDATE_STAGE9_CMD:-\"$MA_CONSOLIDATE_MA_BIN\" consolidate dream${APPLY_FLAG:+ $APPLY_FLAG} --max-candidates 10}"
STAGE10_CMD="${MA_CONSOLIDATE_STAGE10_CMD:-\"$MA_CONSOLIDATE_MA_BIN\" consolidate dream-evolve --project memra${APPLY_FLAG:+ $APPLY_FLAG} --lookback-days 7}"
STAGE11_CMD="${MA_CONSOLIDATE_STAGE11_CMD:-\"$MA_CONSOLIDATE_MA_BIN\" consolidate candidate-ttl --project memra}"

if ! ensure_ma_consolidate_binary; then
    echo "[$(date)] Failed to prepare Rust consolidation helper: $MA_CONSOLIDATE_MA_BIN" >&2
    exit 1
fi

echo "[$(date)] Starting nightly consolidation..."

run_stage "phase1_strengthen_prune" \
    "strengthen + prune" \
    "$LOG_DIR/strengthen_${TIMESTAMP}.json" \
    "$STAGE1_CMD"

run_stage "phase2_connect" \
    "connect" \
    "$LOG_DIR/connect_${TIMESTAMP}.json" \
    "$STAGE2_CMD"

run_stage "phase3_chain" \
    "version chain detection" \
    "$LOG_DIR/chain_${TIMESTAMP}.json" \
    "$STAGE3_CMD"

run_stage "phase4_accuracy" \
    "accuracy detection" \
    "$LOG_DIR/accuracy_${TIMESTAMP}.json" \
    "$STAGE4_CMD"

run_stage "phase5_synapse" \
    "synaptic strengthening" \
    "$LOG_DIR/synapse_${TIMESTAMP}.json" \
    "$STAGE5_CMD"

run_stage "phase6_consolidation" \
    "memory consolidation (event->fact promotion)" \
    "$LOG_DIR/consolidation_${TIMESTAMP}.json" \
    "$STAGE6_CMD"

run_stage "phase6b_curated_playbooks" \
    "curated high-signal playbook refresh" \
    "$LOG_DIR/curated_playbooks_${TIMESTAMP}.json" \
    "$STAGE6B_CMD"

if should_run_llm_stage; then
    run_stage "phase7_reality_check" \
        "reality check (autoDream pending_job drain)" \
        "$LOG_DIR/reality_check_${TIMESTAMP}.json" \
        "$STAGE7_CMD"
else
    skip_stage "phase7_reality_check" \
        "reality check (autoDream pending_job drain)" \
        "$LOG_DIR/reality_check_${TIMESTAMP}.json" \
        "MA_CONSOLIDATE_SKIP_LLM_ON_UPSTREAM_FAIL=1 and upstream failed"
fi

run_stage "phase8_experience_refresh" \
    "experience substrate refresh" \
    "$LOG_DIR/experience_${TIMESTAMP}.json" \
    "$STAGE8_CMD"

if should_run_llm_stage; then
    run_stage "phase9_dream" \
        "dreaming cognitive lifecycle" \
        "$LOG_DIR/dream_${TIMESTAMP}.json" \
        "$STAGE9_CMD"
else
    skip_stage "phase9_dream" \
        "dreaming cognitive lifecycle" \
        "$LOG_DIR/dream_${TIMESTAMP}.json" \
        "MA_CONSOLIDATE_SKIP_LLM_ON_UPSTREAM_FAIL=1 and upstream failed"
fi

if should_run_llm_stage; then
    run_stage "phase10_dream_evolve" \
        "dream feedback evolution" \
        "$LOG_DIR/dream_evolve_${TIMESTAMP}.json" \
        "$STAGE10_CMD"
else
    skip_stage "phase10_dream_evolve" \
        "dream feedback evolution" \
        "$LOG_DIR/dream_evolve_${TIMESTAMP}.json" \
        "MA_CONSOLIDATE_SKIP_LLM_ON_UPSTREAM_FAIL=1 and upstream failed"
fi

# Stage 11 (candidate-ttl) mutates data (is_active=0) unconditionally — the
# Rust subcommand has no --apply / --dry-run flag. Respect the script's
# dry-run contract by skipping the stage when APPLY_FLAG is empty; operators
# expect dry-run executions never to mutate production rows.
# Codex bot P2 finding on PR #246 (scripts/consolidate_nightly.sh:161).
if [ -n "${APPLY_FLAG}" ]; then
    run_stage "phase11_candidate_ttl" \
        "candidate TTL expiry" \
        "$LOG_DIR/candidate_ttl_${TIMESTAMP}.json" \
        "$STAGE11_CMD"
else
    skip_stage "phase11_candidate_ttl" \
        "candidate TTL expiry" \
        "$LOG_DIR/candidate_ttl_${TIMESTAMP}.json" \
        "dry-run mode (APPLY_FLAG unset): TTL would mutate is_active=0"
fi

build_summary_json

# OUTCOME.md grader (借 Anthropic Managed Agents Outcomes 抽象，本地 deterministic).
# Non-fatal: grader 自身 bug 不能拖垮 nightly overall_status.
# Skipped via MA_OUTCOME_GRADER_SKIP=1 (debug only).
if [[ "${MA_OUTCOME_GRADER_SKIP:-0}" != "1" ]]; then
    GRADER_LOG="$LOG_DIR/outcome_grade_${TIMESTAMP}.log"
    echo "[$(date)] Running OUTCOME grader (skip-d=${MA_OUTCOME_GRADER_SKIP_D:-0})..."
    GRADER_CMD=(
        "$MA_CONSOLIDATE_MA_BIN"
        grade
        --apply
        --orchestration
        "$SUMMARY_JSON"
    )
    if [[ "${MA_OUTCOME_GRADER_SKIP_D:-0}" == "1" ]]; then
        GRADER_CMD+=(--skip-d)
    fi
    if [[ -n "${MA_OUTCOME_GRADER_DOGFOOD_DIR:-}" ]]; then
        GRADER_CMD+=(--dogfood-dir "$MA_OUTCOME_GRADER_DOGFOOD_DIR")
    fi
    if [[ -n "${MA_OUTCOME_GRADER_PENDING_DIR:-}" ]]; then
        GRADER_CMD+=(--pending-dir "$MA_OUTCOME_GRADER_PENDING_DIR")
    fi
    "${GRADER_CMD[@]}" > "$GRADER_LOG" 2>&1 || true
    GRADER_RESULT=$(grep -m1 '"result"' "$GRADER_LOG" 2>/dev/null | sed -E 's/.*"result": *"([^"]+)".*/\1/' || echo "unknown")
    echo "[$(date)] OUTCOME grader result=$GRADER_RESULT (log: $GRADER_LOG)"
fi

if [[ "$OVERALL_EXIT_CODE" -eq 0 ]]; then
    echo "[$(date)] Consolidation complete. Summary: $SUMMARY_JSON"
else
    echo "[$(date)] Consolidation completed with failed stages. Summary: $SUMMARY_JSON" >&2
fi

exit "$OVERALL_EXIT_CODE"
