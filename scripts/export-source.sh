#!/usr/bin/env bash
# Copy the source tree — what belongs in a commit — to another folder.
#
#   scripts/export-source.sh /path/to/CollaboCore          copy (and delete what no longer exists)
#   KEEP=1 scripts/export-source.sh /path/to/CollaboCore   copy, delete nothing
#   DRY=1  scripts/export-source.sh /path/to/CollaboCore   list what would be copied
#
# Everything .gitignore covers is left behind: build output, downloaded sources, .tools, and the
# third-party clones (kernel/linux, third_party/*), which carry their own git history and are
# fetched by `scripts/bootstrap-tools.sh` and the CI workflow. The result is a few MB and builds
# from scratch with `./build.sh`.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${1:-}"
[[ -n "$DEST" ]] || { echo "usage: scripts/export-source.sh DESTINATION" >&2; exit 2; }
command -v rsync >/dev/null || { echo "rsync is needed" >&2; exit 1; }

mkdir -p "$DEST"
DEST="$(cd "$DEST" && pwd)"
[[ "$DEST" != "$ROOT" ]] || { echo "the destination is the source" >&2; exit 2; }

options=(-a --human-readable --itemize-changes --exclude-from="$ROOT/.gitignore"
         --exclude=.git/ --exclude='*.log' --exclude=.DS_Store)
# A shared folder (vboxsf, exFAT, SMB) has no unix owners or permissions to copy.
case "$(stat -f -c %T "$DEST" 2>/dev/null || echo unknown)" in
  vboxsf|msdos|exfat|fuseblk|cifs|smb2) options+=(--no-perms --no-owner --no-group --no-times --size-only);;
esac
[[ -n "${KEEP:-}" ]] || options+=(--delete --delete-excluded)
[[ -z "${DRY:-}" ]] || options+=(--dry-run)

echo "== $ROOT  ->  $DEST"
rsync "${options[@]}" "$ROOT/" "$DEST/" | grep -vE '^\.[df]\.\.\.\.\.\.\.\.' || true

if [[ -z "${DRY:-}" ]]; then
  echo "== exported: $(du -sh "$DEST" | cut -f1), $(find "$DEST" -type f | wc -l) files"
  echo "   build it there with: ./build.sh   (it fetches the clones and .tools itself)"
fi
