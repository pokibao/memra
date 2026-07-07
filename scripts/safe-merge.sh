#!/usr/bin/env bash
# safe-merge.sh — gate `gh pr merge` on PR review state.
#
# Addresses the silent-bypass merge anti-pattern observed on 2026-04-24
# and 2026-04-25: 4 agent-batch PRs and 4 sweep PRs were merged without
# checking `gh api repos/.../pulls/N/reviews`, missing 7+ Codex P1/P2
# line-level findings that required follow-up PRs (#203, #205, #206).
#
# `gh pr view --json mergeStateStatus` returns CLEAN even when Codex Bot
# left a COMMENTED review with P1 findings — because COMMENTED is not
# CHANGES_REQUESTED. Trusting `gh pr view` alone is exactly how the
# bypass happens. This script forces a real review-state read before
# delegating to `gh pr merge`.
#
# Lifecycle:
#   1. Resolve PR number from positional arg or current branch.
#   2. Fetch reviews via `gh api repos/.../pulls/N/reviews`.
#   3. Fetch review comments (line-level findings) for any COMMENTED
#      reviews via `gh api repos/.../pulls/N/comments`.
#   4. Fetch issue-level comments to detect Codex's silent-approval
#      "no findings" reply (👍 / "looks good" without inline comments).
#   5. Classify:
#        - CHANGES_REQUESTED  → block, exit 1
#        - COMMENTED with line-level findings → block unless --ack
#        - APPROVED or silent-approval-only → green, proceed
#   6. Call `gh pr merge PR_NUM <gh-merge-args...>`.
#
# Exit codes:
#   0 — review state clean, merge invoked
#   1 — block (CHANGES_REQUESTED or unaddressed P1/P2 findings without --ack)
#   2 — gh / API error
#   3 — usage / preflight error
#
# Usage:
#   scripts/safe-merge.sh <PR_NUM> [-- gh-merge-args...]
#   scripts/safe-merge.sh <PR_NUM> --ack [-- gh-merge-args...]
#   scripts/safe-merge.sh --dry-run <PR_NUM> [-- gh-merge-args...]
#
# Examples:
#   scripts/safe-merge.sh 207 -- --squash --delete-branch
#   scripts/safe-merge.sh --ack 207 -- --squash      # acknowledge findings handled in follow-up
#   scripts/safe-merge.sh --dry-run                  # check current branch's PR without merging
#
# Codex bot login defaults to chatgpt-codex-connector (matches pr-ship.sh).
# Override with BOT_LOGIN env var.

set -euo pipefail

# ─── Defaults ──────────────────────────────────────────────────────────────
BOT_LOGIN="${BOT_LOGIN:-chatgpt-codex-connector}"
ACK=0
DRY_RUN=0
PR_NUM=""
GH_MERGE_ARGS=()

# ─── Helpers ──────────────────────────────────────────────────────────────
usage() {
    awk '
        NR == 1 { next }
        /^#/ { sub(/^# ?/, ""); print; next }
        /^$/ { print ""; next }
        { exit }
    ' "$0"
    exit 3
}

err() {
    echo "safe-merge: $*" >&2
}

# ─── Argv ──────────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --help|-h)
            usage
            ;;
        --ack)
            ACK=1
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --)
            shift
            GH_MERGE_ARGS=("$@")
            break
            ;;
        --*)
            err "unknown flag: $1"
            exit 3
            ;;
        *)
            if [[ -z "$PR_NUM" ]]; then
                PR_NUM="$1"
                shift
            else
                err "unexpected positional argument: $1"
                exit 3
            fi
            ;;
    esac
done

# ─── Resolve PR number ────────────────────────────────────────────────────
if [[ -z "$PR_NUM" ]]; then
    if ! PR_NUM="$(gh pr view --json number --jq '.number' 2>/dev/null)"; then
        err "no PR number given and current branch has no associated PR"
        exit 3
    fi
fi
if ! [[ "$PR_NUM" =~ ^[0-9]+$ ]]; then
    err "invalid PR number: $PR_NUM"
    exit 3
fi

REPO_SLUG="$(gh repo view --json nameWithOwner --jq '.nameWithOwner' 2>/dev/null)"
if [[ -z "$REPO_SLUG" ]]; then
    err "gh repo view failed — not in a GitHub-tracked repo?"
    exit 2
fi

echo "▶ safe-merge: PR #$PR_NUM in $REPO_SLUG"

# ─── Pull review state ────────────────────────────────────────────────────
# `gh api --paginate` emits one JSON document per page (newline-separated)
# rather than a single concatenated array, so we slurp+flatten with
# `jq -s 'add // []'` before downstream queries can use `length` etc.
# Without this, multi-page PRs would yield multi-line counts that break
# numeric comparisons under `set -e` (Codex P2 on PR #207).
slurp_pages() {
    jq -s 'add // []'
}

# All reviews on the PR (not issue comments). Note: this endpoint
# returns a HISTORY of review events, not the current verdict per
# reviewer — collapsed below to "latest review per reviewer" before
# state counting (Codex P1 on PR #207).
REVIEWS_RAW="$(gh api "repos/$REPO_SLUG/pulls/$PR_NUM/reviews" --paginate 2>/dev/null)" || {
    err "gh api reviews failed"
    exit 2
}
REVIEWS_JSON="$(slurp_pages <<<"$REVIEWS_RAW")"

# Per-reviewer latest review only — collapse the history to the current
# verdict each reviewer holds. A reviewer who left CHANGES_REQUESTED
# and later APPROVED contributes only the APPROVED entry here.
REVIEWS_LATEST_JSON="$(jq '
    [group_by(.user.login)[] | sort_by(.submitted_at) | last]
' <<<"$REVIEWS_JSON")"

# Line-level review comments (the things that contain P1/P2 markers).
REVIEW_COMMENTS_RAW="$(gh api "repos/$REPO_SLUG/pulls/$PR_NUM/comments" --paginate 2>/dev/null)" || {
    err "gh api review-comments failed"
    exit 2
}
REVIEW_COMMENTS_JSON="$(slurp_pages <<<"$REVIEW_COMMENTS_RAW")"

# Issue-level comments (where Codex sometimes drops a silent-approval
# "no findings" reply with 👍 instead of an actual review).
ISSUE_COMMENTS_RAW="$(gh api "repos/$REPO_SLUG/issues/$PR_NUM/comments" --paginate 2>/dev/null)" || {
    err "gh api issue-comments failed"
    exit 2
}
ISSUE_COMMENTS_JSON="$(slurp_pages <<<"$ISSUE_COMMENTS_RAW")"

# ─── Classify ─────────────────────────────────────────────────────────────
# Per-reviewer-latest verdict, NOT raw history counts.
CHANGES_REQUESTED_COUNT="$(jq '[.[] | select(.state == "CHANGES_REQUESTED")] | length' <<<"$REVIEWS_LATEST_JSON")"
APPROVED_COUNT="$(jq '[.[] | select(.state == "APPROVED")] | length' <<<"$REVIEWS_LATEST_JSON")"
COMMENTED_COUNT="$(jq '[.[] | select(.state == "COMMENTED")] | length' <<<"$REVIEWS_LATEST_JSON")"
TOTAL_REVIEWERS="$(jq 'length' <<<"$REVIEWS_LATEST_JSON")"
TOTAL_REVIEW_EVENTS="$(jq 'length' <<<"$REVIEWS_JSON")"

# Bot's most recent review (if any). GitHub appends "[bot]" to bot
# logins on review/comment payloads ("chatgpt-codex-connector[bot]")
# while leaving the BOT_LOGIN env value bare — match the prefix so
# both forms hit.
BOT_LATEST_STATE="$(jq -r --arg bot "$BOT_LOGIN" '
    [.[] | select(.user.login | startswith($bot))] | sort_by(.submitted_at) | last | .state // ""
' <<<"$REVIEWS_JSON")"

# Line-level review comments — these carry P1/P2 markers in their body.
P1_FINDINGS="$(jq '[.[] | select((.body | test("(?i)\\bP1\\b|priority[: ]?1|severity[: ]?(critical|high)|🔴|🟠")) ) ] | length' <<<"$REVIEW_COMMENTS_JSON")"
P2_FINDINGS="$(jq '[.[] | select((.body | test("(?i)\\bP2\\b|priority[: ]?2|severity[: ]?medium|🟡")) ) ] | length' <<<"$REVIEW_COMMENTS_JSON")"
TOTAL_REVIEW_COMMENTS="$(jq 'length' <<<"$REVIEW_COMMENTS_JSON")"

# Bot's silent-approval issue comment — body matches "no findings" / 👍 /
# "looks good" pattern. This is how Codex sometimes signals "clean review"
# WITHOUT submitting a formal APPROVED review.
BOT_SILENT_APPROVAL="$(jq -r --arg bot "$BOT_LOGIN" '
    [.[]
     | select(.user.login | startswith($bot))
     | select(.body | test("(?i)no findings|nothing to flag|looks good|👍|lgtm"))
    ] | length
' <<<"$ISSUE_COMMENTS_JSON")"

cat <<-EOF
  Reviews:        $TOTAL_REVIEWERS reviewer(s), $TOTAL_REVIEW_EVENTS event(s)
                    APPROVED:           $APPROVED_COUNT  (per-reviewer latest)
                    CHANGES_REQUESTED:  $CHANGES_REQUESTED_COUNT  (per-reviewer latest)
                    COMMENTED:          $COMMENTED_COUNT  (per-reviewer latest)
  Bot ($BOT_LOGIN):
                    latest review:      ${BOT_LATEST_STATE:-(none)}
                    silent approvals:   $BOT_SILENT_APPROVAL
  Line-level findings: $TOTAL_REVIEW_COMMENTS total
                    P1-marked:          $P1_FINDINGS
                    P2-marked:          $P2_FINDINGS
EOF

# ─── Decide ───────────────────────────────────────────────────────────────
BLOCK=0
REASONS=()

if [[ "$CHANGES_REQUESTED_COUNT" -gt 0 ]]; then
    BLOCK=1
    REASONS+=("$CHANGES_REQUESTED_COUNT review(s) in CHANGES_REQUESTED state")
fi

if [[ "$P1_FINDINGS" -gt 0 ]]; then
    BLOCK=1
    REASONS+=("$P1_FINDINGS P1-marked line-level finding(s)")
fi

if [[ "$P2_FINDINGS" -gt 0 ]]; then
    BLOCK=1
    REASONS+=("$P2_FINDINGS P2-marked line-level finding(s)")
fi

# COMMENTED review without any line-level finding markers is suspicious
# (Codex usually inlines findings) — flag it but don't auto-block;
# the user has eyes on the summary above.
if [[ "$BLOCK" -eq 0 && "$COMMENTED_COUNT" -gt 0 && "$TOTAL_REVIEW_COMMENTS" -gt 0 ]]; then
    # Has line-level comments but they don't match our P1/P2 regex —
    # could be discussion or unmarked findings. Flag for human review.
    BLOCK=1
    REASONS+=("$TOTAL_REVIEW_COMMENTS line-level comment(s) without P1/P2 marker — review manually before merge")
fi

if [[ "$BLOCK" -eq 1 ]]; then
    if [[ "$ACK" -eq 1 ]]; then
        echo
        echo "⚠️  --ack passed; proceeding despite blockers:"
        for r in "${REASONS[@]}"; do
            echo "    - $r"
        done
    else
        echo
        echo "✗ safe-merge: BLOCKED. Reasons:"
        for r in "${REASONS[@]}"; do
            echo "    - $r"
        done
        echo
        echo "  Inspect findings:"
        echo "    gh api repos/$REPO_SLUG/pulls/$PR_NUM/reviews --jq '.[] | {user:.user.login, state, body}'"
        echo "    gh api repos/$REPO_SLUG/pulls/$PR_NUM/comments --jq '.[] | {user:.user.login, path, line, body}'"
        echo
        echo "  Override (after addressing or acknowledging):"
        echo "    scripts/safe-merge.sh --ack $PR_NUM -- ${GH_MERGE_ARGS[*]:-}"
        exit 1
    fi
else
    echo
    echo "✓ safe-merge: review state clean."
fi

# ─── Merge ────────────────────────────────────────────────────────────────
if [[ "$DRY_RUN" -eq 1 ]]; then
    echo
    echo "(dry-run) would invoke: gh pr merge $PR_NUM ${GH_MERGE_ARGS[*]:-}"
    exit 0
fi

if [[ ${#GH_MERGE_ARGS[@]} -eq 0 ]]; then
    echo
    err "no merge args after --; refusing to invoke 'gh pr merge' without an explicit strategy."
    err "example: scripts/safe-merge.sh $PR_NUM -- --squash --delete-branch"
    exit 3
fi

echo
echo "▶ gh pr merge $PR_NUM ${GH_MERGE_ARGS[*]}"
exec gh pr merge "$PR_NUM" "${GH_MERGE_ARGS[@]}"
