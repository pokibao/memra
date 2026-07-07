#!/bin/bash
# MA Truth Banner: print the real Memra database state on wrapper start.
# This protects agents from treating 0-byte wrong-path files as data loss.

set -euo pipefail

TRUE_DB="$HOME/.memra/projects/memra/.storage/memory_anchor.sqlite3"
AUDIT_LOG="$HOME/.memra/logs/wrapper-audit.log"

mkdir -p "$(dirname "$AUDIT_LOG")"

timestamp() {
    date -u +%FT%TZ
}

file_size_bytes() {
    stat -f%z "$1" 2>/dev/null || stat -c%s "$1"
}

if [ ! -f "$TRUE_DB" ]; then
    echo "[MA-TRUTH] ALERT: true database missing: $TRUE_DB" >&2
    echo "$(timestamp) MISSING $TRUE_DB" >> "$AUDIT_LOG"
    exit 0
fi

SIZE=$(file_size_bytes "$TRUE_DB")
SIZE_MB=$(( SIZE / 1000 / 1000 ))
MTIME=$(stat -f%Sm -t '%FT%TZ' "$TRUE_DB" 2>/dev/null || stat -c%y "$TRUE_DB")
BACKEND=$(awk '/^backend:[[:space:]]*/ {print $2; exit}' "$HOME/.memra/config.yaml" 2>/dev/null || true)
BACKEND="${BACKEND:-?}"

LATEST_BACKUP=$(ls -t "$HOME/.memra/backups/"/memory_anchor_*.sqlite3 2>/dev/null | head -1 || true)
if [ -n "$LATEST_BACKUP" ]; then
    BACKUP_SIZE=$(file_size_bytes "$LATEST_BACKUP")
    BACKUP_MB=$(( BACKUP_SIZE / 1000 / 1000 ))
    if [ "$BACKUP_MB" -gt 0 ] && [ $(( SIZE_MB * 100 / BACKUP_MB )) -lt 50 ]; then
        echo "[MA-TRUTH] ALERT: true database ${SIZE_MB}MB << latest backup ${BACKUP_MB}MB (< 50%)" >&2
        echo "$(timestamp) SHRINK_ALERT db=${SIZE_MB}MB backup=${BACKUP_MB}MB" >> "$AUDIT_LOG"
    fi
fi

echo "[MA-TRUTH] backend=$BACKEND db=${SIZE_MB}MB path=$TRUE_DB mtime=$MTIME" >&2
echo "$(timestamp) WAKE backend=$BACKEND db=${SIZE_MB}MB" >> "$AUDIT_LOG"
