#!/usr/bin/env bash
# Collect a release: the runtime for each platform that is built here, the browser build, and
# the checksums to publish with them.
#
#   dist/release/
#     collabo-core-<platform>.tar.gz | .zip   the runtime an app ships and spawns
#     collabo-core-web.tar.gz                 the browser page (serve it with COOP/COEP headers)
#     SHA256SUMS                              of every file above
#     VERSION                                 what went in: versions, sizes, dates
#
#   scripts/release.sh                 everything already in dist/
#   PLATFORMS="linux-x64" scripts/release.sh    only those runtimes
#   scripts/release.sh --build         build first (this platform's runtime and the web page)
#
# Cross-platform releases are assembled by collecting each platform's own dist/runtime folder
# here (CI publishes them as artifacts): the engine is native code and is built on its own OS.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/dist/release"

if [[ "${1:-}" == "--build" ]]; then
  python3 "$ROOT/build.py" engine web runtime
fi

[[ -d "$ROOT/dist/runtime" ]] || { echo "no dist/runtime; run: python3 build.py runtime" >&2; exit 1; }
rm -rf "$OUT"; mkdir -p "$OUT"

found=0
for dir in "$ROOT"/dist/runtime/collabo-core-*/; do
  [[ -f "$dir/manifest.json" ]] || continue
  platform="$(basename "$dir")"; platform="${platform#collabo-core-}"
  if [[ -n "${PLATFORMS:-}" && " $PLATFORMS " != *" $platform "* ]]; then continue; fi
  echo "== $platform"
  case "$platform" in
    win-*) (cd "$ROOT/dist/runtime" && python3 -c "
import os, sys, zipfile
root = sys.argv[1]
with zipfile.ZipFile(sys.argv[2], 'w', zipfile.ZIP_DEFLATED) as z:
    for base, _, names in os.walk(root):
        for name in names:
            z.write(os.path.join(base, name))
" "collabo-core-$platform" "$OUT/collabo-core-$platform.zip") ;;
    *) tar -czf "$OUT/collabo-core-$platform.tar.gz" -C "$ROOT/dist/runtime" "collabo-core-$platform" ;;
  esac
  found=$((found + 1))
done
[[ $found -gt 0 ]] || { echo "no runtime packages matched" >&2; exit 1; }

if [[ -d "$ROOT/dist/web" ]]; then
  echo "== web"
  tar -czf "$OUT/collabo-core-web.tar.gz" -C "$ROOT/dist" web
fi

# What is inside, so a published archive can be traced back to its sources.
{
  echo "collaboCore release"
  echo "built:   $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  # Read straight from the image: a pipe here would trip `pipefail` when grep stops early.
  echo "kernel:  $(grep -a -m1 -oE 'Linux version [0-9][^ ]*' "$ROOT/dist/engine/kernel/vmlinux.wasm" || echo unknown)"
  echo "engine:  $(grep -m1 '^version' "$ROOT/engine/Cargo.toml" | cut -d'"' -f2) (rust, wasmtime $(grep -m1 '^wasmtime' "$ROOT/engine/Cargo.toml" | cut -d'"' -f2))"
  echo "python:  $(basename "$(ls -d "$ROOT"/python/src/Python-* 2>/dev/null | head -1)" 2>/dev/null || echo 'CPython 3.13')"
  echo "protocol: $(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['protocol'])" "$(ls -d "$ROOT"/dist/runtime/collabo-core-*/ | head -1)manifest.json")"
  echo
  echo "files:"
  (cd "$OUT" && ls -lh | awk 'NR>1 {printf "  %-40s %s\n", $9, $5}')
} > "$OUT/VERSION"

(cd "$OUT" && sha256sum ./* > SHA256SUMS.tmp && mv SHA256SUMS.tmp SHA256SUMS)
echo
cat "$OUT/VERSION"
echo "== release ready: $OUT"
