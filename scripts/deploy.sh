#!/usr/bin/env bash
# Deploy to a folder: the source tree (as `build.py export` copies it) and the build output
# (dist/: runtime, web, release archives), then check what arrived.
#
#   python3 build.py deploy [DIR]              source + dist/
#   python3 build.py deploy [DIR] --release    rebuild dist/release (archives, checksums) first
#   python3 build.py deploy [DIR] --dist       only dist/        --source   only the source
#   python3 build.py deploy [DIR] --check      only compare; exit 1 if DIR differs
#   DRY=1 python3 build.py deploy [DIR]        list what would change
#
# DIR: the argument, else $COLLABO_DEPLOY_DIR, else the first line of .deploy-target (a local file,
# not committed: each machine has its own target, e.g. a VirtualBox shared folder).
#
# The source copy deletes what no longer exists in the tree (dist/ at the target excepted);
# dist/ is mirrored exactly. Files are compared by content, not size or time, because a shared
# folder keeps neither modes nor reliable times. Afterwards dist/release/SHA256SUMS and every
# runtime's manifest.json are checked against the files at the target.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

DEST="" SOURCE=1 DIST=1 RELEASE="" CHECK=""
for arg in "$@"; do
  case "$arg" in
    --release) RELEASE=1 ;;
    --dist) SOURCE="" ;;
    --source) DIST="" ;;
    --check) CHECK=1 ;;
    -*) echo "unknown option: $arg" >&2; exit 2 ;;
    *) DEST="$arg" ;;
  esac
done
if [[ -z "$DEST" ]]; then DEST="${COLLABO_DEPLOY_DIR:-}"; fi
if [[ -z "$DEST" && -f "$ROOT/.deploy-target" ]]; then DEST="$(head -1 "$ROOT/.deploy-target" | tr -d '\r')"; fi
[[ -n "$DEST" ]] || { echo "no target: python3 build.py deploy DIR, COLLABO_DEPLOY_DIR=DIR, or put DIR in .deploy-target" >&2; exit 2; }
DEST="${DEST/#\~/$HOME}"
[[ -d "$DEST" ]] || { echo "the target $DEST does not exist (is the shared folder mounted?)" >&2; exit 1; }
DEST="$(cd "$DEST" && pwd)"
[[ "$DEST" != "$ROOT" ]] || { echo "the target is this tree" >&2; exit 2; }
command -v rsync >/dev/null || { echo "rsync is needed" >&2; exit 1; }

if [[ -n "$RELEASE" && -z "$CHECK" && -z "${DRY:-}" ]]; then
  bash "$ROOT/scripts/release.sh"
fi
if [[ -n "$DIST" ]]; then
  [[ -d "$ROOT/dist/runtime" || -d "$ROOT/dist/web" ]] || { echo "nothing built in dist/ (python3 build.py)" >&2; exit 1; }
  # An archive older than what it packs was not rebuilt after the last build.
  if [[ -f "$ROOT/dist/release/VERSION" ]] \
     && [[ -n "$(find "$ROOT/dist/runtime" "$ROOT/dist/web" -type f -newer "$ROOT/dist/release/VERSION" -print -quit 2>/dev/null)" ]]; then
    echo "note: dist/release is older than dist/runtime or dist/web; --release rebuilds it" >&2
  fi
fi

echo "== deploy $ROOT  ->  $DEST"
changes=0
if [[ -n "$SOURCE" ]]; then
  if [[ -n "$CHECK" ]]; then
    out="$(DRY=1 bash "$ROOT/scripts/export-source.sh" "$DEST" | grep -E '^(>|\*|c)' || true)"
    [[ -z "$out" ]] || { echo "$out"; changes=1; }
  else
    bash "$ROOT/scripts/export-source.sh" "$DEST"
  fi
fi
if [[ -n "$DIST" ]]; then
  options=(-rt --no-perms --no-owner --no-group --checksum --delete --itemize-changes)
  [[ -z "${DRY:-}" && -z "$CHECK" ]] || options+=(--dry-run)
  echo "== dist/"
  mkdir -p "$DEST/dist"
  out="$(rsync "${options[@]}" "$ROOT/dist/" "$DEST/dist/" | grep -vE '^\.[df]' || true)"
  [[ -z "$out" ]] || { echo "$out"; [[ -z "$CHECK" ]] || changes=1; }
fi

if [[ -n "$CHECK" ]]; then
  [[ $changes == 0 ]] || { echo "the target differs from this tree" >&2; exit 1; }
  echo "== the target matches"
  exit 0
fi
[[ -z "${DRY:-}" ]] || exit 0

# What arrived is what was built.
if [[ -n "$DIST" ]]; then
  if [[ -f "$DEST/dist/release/SHA256SUMS" ]]; then
    (cd "$DEST/dist/release" && sha256sum --quiet -c SHA256SUMS) && echo "== dist/release: checksums ok"
  fi
  python3 - "$DEST/dist/runtime" <<'PY'
import hashlib, json, os, sys
base = sys.argv[1]
bad = 0
for name in sorted(os.listdir(base)) if os.path.isdir(base) else []:
    manifest = os.path.join(base, name, "manifest.json")
    if not os.path.isfile(manifest):
        continue
    files = json.load(open(manifest))["files"]
    for rel, digest in files.items():
        path = os.path.join(base, name, *rel.split("/"))
        if not os.path.isfile(path) or hashlib.sha256(open(path, "rb").read()).hexdigest() != digest:
            print(f"  mismatch: {name}/{rel}")
            bad += 1
    print(f"== dist/runtime/{name}: {len(files)} files {'ok' if not bad else 'NOT ok'}")
sys.exit(1 if bad else 0)
PY
fi
echo "== deployed to $DEST"
