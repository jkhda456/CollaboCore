#!/usr/bin/env bash
# Copy the source tree — what belongs in a commit — to another folder.
#
#   python3 build.py export /path/to/CollaboCore          copy (and delete what no longer exists)
#   KEEP=1 python3 build.py export /path/to/CollaboCore   copy, delete nothing
#   DRY=1  python3 build.py export /path/to/CollaboCore   list what would be copied
#
# Everything .gitignore covers is left behind: build output, downloaded sources, .tools, and the
# third-party clones (kernel/linux, third_party/*), which carry their own git history and are
# fetched by `scripts/bootstrap-tools.sh` and the CI workflow. The result is a few MB and builds
# from scratch with `python3 build.py`.
#
# A shared folder keeps no file modes, so the copy has no executable bits. That is fine: build.py
# runs every script through bash and puts the bits back; `python3 build.py perms --git` in the
# repository records them for the commit.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${1:-}"
[[ -n "$DEST" ]] || { echo "usage: scripts/export-source.sh DESTINATION" >&2; exit 2; }
command -v rsync >/dev/null || { echo "rsync is needed" >&2; exit 1; }

mkdir -p "$DEST"
DEST="$(cd "$DEST" && pwd)"
[[ "$DEST" != "$ROOT" ]] || { echo "the destination is the source" >&2; exit 2; }

# The .gitignore files below the root (flutter/collabo_core_demo, tests) count too, as for git.
options=(-a --human-readable --itemize-changes --exclude-from="$ROOT/.gitignore"
         --filter='dir-merge,- .gitignore' --exclude=.git/
         # dist/ at the destination is the build output copied there on purpose; never delete it.
         --filter='P /dist/')
# A shared folder (vboxsf, exFAT, SMB) has no unix owners or permissions to copy.
# (stat names vboxsf only by its magic number, 0x786f4256.)
case "$(stat -f -c %T "$DEST" 2>/dev/null || echo unknown)" in
  # Content, not size: an edit that keeps a file's size would otherwise be skipped.
  vboxsf|*0x786f4256*|msdos|exfat|fuseblk|cifs|smb2) options+=(--no-perms --no-owner --no-group --no-times --checksum);;
esac
[[ -n "${KEEP:-}" ]] || options+=(--delete --delete-excluded)
[[ -z "${DRY:-}" ]] || options+=(--dry-run)

echo "== $ROOT  ->  $DEST"
rsync "${options[@]}" "$ROOT/" "$DEST/" | grep -vE '^\.[df]\.\.\.\.\.\.\.\.' || true

if [[ -z "${DRY:-}" ]]; then
  echo "== exported: $(du -sh "$DEST" | cut -f1), $(find "$DEST" -type f | wc -l) files"
  echo "   build it there with: python3 build.py   (it fetches the clones and .tools itself)"
fi
