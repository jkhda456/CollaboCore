#!/usr/bin/env bash
# Publish dist/web to the VirtualBox shared folder so it is visible from the
# Windows host:  collaboCore/dist/web  ->  /media/sf_share/workspace/minimiWS/Release
#
# This is a copy (rsync), not a mount: a bind mount over a vboxsf directory is
# only visible inside this guest, never on the host.
#
# Usage:
#   ./sync-release.sh            # copy dist/web/ into the share (mirror: deletes extras there)
#   ./sync-release.sh --dry-run  # show what would change
#   ./sync-release.sh --check    # verify the share matches dist/web/ (exit 1 if not)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/dist/web/"
DST="${RELEASE_DST:-/media/sf_share/workspace/minimiWS/Release}/"

[[ -d "$SRC" ]] || { echo "missing $SRC" >&2; exit 1; }
[[ -d "$DST" ]] || { echo "missing $DST (is the vboxsf share mounted?)" >&2; exit 1; }
mountpoint -q /media/sf_share || { echo "/media/sf_share is not a mount point; refusing to write" >&2; exit 1; }

# vboxsf keeps no unix perms/owners and no symlinks: copy content and mtimes only.
# --size-only-ish safety: compare by checksum so identical rebuilds are no-ops.
OPTS=(-rt --no-perms --no-owner --no-group --delete --checksum --itemize-changes)

case "${1:-}" in
  "")          rsync "${OPTS[@]}" "$SRC" "$DST" ;;
  --dry-run)   rsync "${OPTS[@]}" --dry-run "$SRC" "$DST" ;;
  --check)
    out="$(rsync "${OPTS[@]}" --dry-run "$SRC" "$DST")"
    if [[ -n "$out" ]]; then echo "$out"; echo "share differs from dist/web/" >&2; exit 1; fi
    echo "share matches dist/web/" ;;
  *) echo "unknown option: $1" >&2; exit 2 ;;
esac

[[ "${1:-}" == "" ]] && echo "synced $SRC -> $DST"
exit 0
