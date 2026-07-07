#!/bin/bash
# MA offsite backup: copy the latest local daily backup to iCloud Drive.

set -euo pipefail

LOCAL_BACKUP_DIR="$HOME/.memra/backups"
ICLOUD_DIR="$HOME/Library/Mobile Documents/com~apple~CloudDocs/Memra-Backups"
AUDIT_LOG="$HOME/.memra/logs/offsite-backup.log"

mkdir -p "$ICLOUD_DIR" "$(dirname "$AUDIT_LOG")"

timestamp() {
    date -u +%FT%TZ
}

file_size_bytes() {
    stat -f%z "$1" 2>/dev/null || stat -c%s "$1"
}

LATEST=$(ls -t "$LOCAL_BACKUP_DIR"/memory_anchor_*.sqlite3 2>/dev/null | head -1 || true)
if [ -z "$LATEST" ]; then
    echo "$(timestamp) NO_BACKUP_FOUND" >> "$AUDIT_LOG"
    exit 1
fi

BASENAME=$(basename "$LATEST")
DEST="$ICLOUD_DIR/$BASENAME"

if [ -f "$DEST" ]; then
    echo "$(timestamp) SKIP $BASENAME already exists" >> "$AUDIT_LOG"
else
    cp "$LATEST" "$DEST"
    SIZE_MB=$(( $(file_size_bytes "$DEST") / 1000 / 1000 ))
    echo "$(timestamp) COPIED $BASENAME ${SIZE_MB}MB" >> "$AUDIT_LOG"
fi

find "$ICLOUD_DIR" -name "memory_anchor_*.sqlite3" -mtime +30 -delete
COUNT=$(find "$ICLOUD_DIR" -maxdepth 1 -name "memory_anchor_*.sqlite3" -type f | wc -l | tr -d ' ')
echo "$(timestamp) ROTATION_DONE count=$COUNT" >> "$AUDIT_LOG"
