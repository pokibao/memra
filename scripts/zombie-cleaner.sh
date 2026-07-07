#!/bin/bash
# Clean stale Memra serve processes.
#
# Intended for launchd on the always-on MA host. The MCP wrapper is the hard
# guard; this script is the gentle janitor that removes old serve processes
# after they have clearly outlived an interactive session. It must not kill the
# always-on CLI forwarding daemon (`memra serve --daemon`), which is intentionally
# long-lived under launchd.
set -euo pipefail

MAX_AGE_SECONDS="${MA_ZOMBIE_MAX_AGE_SECONDS:-3600}"
SIGNAL="${MA_ZOMBIE_CLEANER_SIGNAL:-TERM}"
DRY_RUN="${MA_ZOMBIE_CLEANER_DRY_RUN:-0}"

if ! [[ "$MAX_AGE_SECONDS" =~ ^[0-9]+$ ]]; then
  echo "[zombie-cleaner] MA_ZOMBIE_MAX_AGE_SECONDS must be an integer" >&2
  exit 2
fi

elapsed_to_seconds() {
  local raw="$1"
  local days=0
  local clock="$raw"
  if [[ "$clock" == *-* ]]; then
    days="${clock%%-*}"
    clock="${clock#*-}"
  fi

  IFS=: read -r a b c <<<"$clock"
  if [[ -n "${c:-}" ]]; then
    echo $((days * 86400 + 10#$a * 3600 + 10#$b * 60 + 10#$c))
  elif [[ -n "${b:-}" ]]; then
    echo $((days * 86400 + 10#$a * 60 + 10#$b))
  else
    echo $((days * 86400 + 10#$a))
  fi
}

case "$DRY_RUN" in
  1|true|yes|on|TRUE|YES|ON|True|Yes|On) DRY_RUN=1 ;;
  *) DRY_RUN=0 ;;
esac

if ! command -v pgrep >/dev/null 2>&1; then
  echo "[zombie-cleaner] pgrep unavailable" >&2
  exit 2
fi

pids=()
while IFS= read -r pid; do
  pids+=("$pid")
done < <(pgrep -f '(^|/)(ma|memra) serve' 2>/dev/null || true)
if (( ${#pids[@]} == 0 )); then
  echo "[zombie-cleaner] no MA serve processes found"
  exit 0
fi

killed=0
kept=0
for pid in "${pids[@]}"; do
  [[ "$pid" =~ ^[0-9]+$ ]] || continue
  line="$(ps -p "$pid" -o etime= -o command= 2>/dev/null || true)"
  [[ -n "$line" ]] || continue
  read -r etime command <<<"$line"
  if [[ "$command" == *" serve --daemon"* || "$command" == *" --daemon"* ]]; then
    echo "[zombie-cleaner] keeping protected daemon pid=$pid command=$command"
    kept=$((kept + 1))
    continue
  fi
  age_seconds="$(elapsed_to_seconds "$etime")"
  if (( age_seconds <= MAX_AGE_SECONDS )); then
    kept=$((kept + 1))
    continue
  fi

  if (( DRY_RUN )); then
    echo "[zombie-cleaner] would kill pid=$pid age=${age_seconds}s command=$command"
  else
    echo "[zombie-cleaner] killing pid=$pid age=${age_seconds}s signal=$SIGNAL command=$command"
    kill "-$SIGNAL" "$pid" 2>/dev/null || true
  fi
  killed=$((killed + 1))
done

echo "[zombie-cleaner] done killed=$killed kept=$kept max_age_seconds=$MAX_AGE_SECONDS dry_run=$DRY_RUN"
