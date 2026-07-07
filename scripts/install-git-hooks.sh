#!/usr/bin/env bash
# install-git-hooks.sh — copy scripts/git-hooks/* into .git/hooks/.
#
# Reason: git doesn't version-control hooks by default (they live in .git/hooks/,
# which is gitignored implicitly). We keep the canonical versions in
# scripts/git-hooks/ and install them via this script.
#
# Usage:
#   ./scripts/install-git-hooks.sh         # install all hooks (overwrites existing)
#   ./scripts/install-git-hooks.sh --check # report install state without changing
#
# Uninstall: delete the hook file from .git/hooks/ (or `git push --no-verify` to bypass once).

set -euo pipefail

MODE="install"
if [[ "${1:-}" == "--check" ]]; then
    MODE="check"
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC_DIR="$REPO_ROOT/scripts/git-hooks"

# Worktree-aware: in a linked worktree ".git" is a FILE pointing to the real
# gitdir, so "$REPO_ROOT/.git/hooks" does not exist. `git rev-parse --git-path
# hooks` resolves to the correct hooks dir in main repo AND in worktrees.
if ! DST_DIR="$(git -C "$REPO_ROOT" rev-parse --git-path hooks 2>/dev/null)"; then
    echo "ERROR: not inside a git repository (or git not installed)" >&2
    exit 1
fi
# rev-parse --git-path returns relative path in main repo; make it absolute.
case "$DST_DIR" in
    /*) ;;
    *)  DST_DIR="$REPO_ROOT/$DST_DIR" ;;
esac
mkdir -p "$DST_DIR"

if [[ ! -d "$SRC_DIR" ]]; then
    echo "ERROR: $SRC_DIR not found" >&2
    exit 1
fi

count=0
for src in "$SRC_DIR"/*; do
    [[ -f "$src" ]] || continue
    name="$(basename "$src")"
    dst="$DST_DIR/$name"

    if [[ "$MODE" == "check" ]]; then
        if [[ -f "$dst" ]] && cmp -s "$src" "$dst"; then
            echo "✓ $name: installed and up-to-date"
        elif [[ -f "$dst" ]]; then
            echo "⚠ $name: installed but DIFFERENT from $SRC_DIR/$name — run without --check to refresh"
        else
            echo "✗ $name: NOT installed — run without --check to install"
        fi
        continue
    fi

    # C2 safety: back up any existing hook that differs before overwriting,
    # so user-authored hooks aren't silently clobbered.
    if [[ -f "$dst" ]] && ! cmp -s "$src" "$dst"; then
        backup="$dst.bak-$(date +%Y%m%d-%H%M%S)"
        cp "$dst" "$backup"
        echo "  ↳ backed up existing $name → $(basename "$backup")"
    fi
    cp "$src" "$dst"
    chmod +x "$dst"
    echo "✓ installed $name → $dst"
    count=$((count + 1))
done

if [[ "$MODE" == "install" ]]; then
    echo
    echo "Installed $count hook(s)."
    echo "Bypass once with: git push --no-verify (use sparingly)"
fi
