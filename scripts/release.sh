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
#   scripts/release.sh --index DIR     only rewrite DIR's VERSION file list and SHA256SUMS
#
# Releases accumulate: an archive is replaced only when its platform is packed again, so the
# runtimes built on other machines (Windows, macOS) stay next to this one's, and VERSION and
# SHA256SUMS always cover every archive in the folder. The engine is native code and is built
# on its own OS; each machine adds its platform (CI collects them as artifacts).
set -euo pipefail
PYTHON="${PYTHON:-python3}"   # build.py passes its own interpreter
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$ROOT/dist/release"

# VERSION's file list and SHA256SUMS for everything in a release folder. VERSION's header (what
# went in) is kept when there is one: --index runs on a folder another machine released into.
index() {
  local out=$1 header
  header="$(sed '/^files:$/,$d' "$out/VERSION" 2>/dev/null || true)"
  {
    echo "${header:-collaboCore release}"
    echo
    echo "files:"
    (cd "$out" && for f in collabo-core-*; do
       [[ -f "$f" ]] || continue
       printf '  %-40s %6s  %s\n' "$f" "$(du -h "$f" | cut -f1)" "$(date -u -r "$f" +%Y-%m-%dT%H:%MZ)"
     done)
  } > "$out/VERSION.tmp" && mv "$out/VERSION.tmp" "$out/VERSION"
  (cd "$out" && sha256sum VERSION collabo-core-* > SHA256SUMS.tmp && mv SHA256SUMS.tmp SHA256SUMS)
}

case "${1:-}" in
  "") ;;
  --index)
    [[ -d "${2:-}" ]] || { echo "--index needs a release folder" >&2; exit 2; }
    index "$2"
    exit 0 ;;
  --build)
    # Off Linux only the runtime is built here; the kernel, images and web page come from Linux.
    case "$(uname -s)" in
      Linux) "$PYTHON" "$ROOT/build.py" engine web runtime ;;
      *) "$PYTHON" "$ROOT/build.py" runtime ;;
    esac ;;
  *) echo "unknown argument: $1 (release collects what is built; build first: build.py runtime release, or release --build)" >&2
     exit 2 ;;
esac

[[ -d "$ROOT/dist/runtime" ]] || { echo "no dist/runtime; run: python3 build.py runtime" >&2; exit 1; }
mkdir -p "$OUT"

found=0
for dir in "$ROOT"/dist/runtime/collabo-core-*/; do
  [[ -f "$dir/manifest.json" ]] || continue
  platform="$(basename "$dir")"; platform="${platform#collabo-core-}"
  if [[ -n "${PLATFORMS:-}" && " $PLATFORMS " != *" $platform "* ]]; then continue; fi
  echo "== $platform"
  rm -f "$OUT/collabo-core-$platform.zip" "$OUT/collabo-core-$platform.tar.gz"
  case "$platform" in
    win-*) (cd "$ROOT/dist/runtime" && "$PYTHON" -c "
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
  echo "built:   $(date -u +%Y-%m-%dT%H:%M:%SZ) (the last archives packed; each file's own date below)"
  # Read straight from the image: a pipe here would trip `pipefail` when grep stops early.
  echo "kernel:  $(grep -a -m1 -oE 'Linux version [0-9][^ ]*' "$ROOT/dist/engine/kernel/vmlinux.wasm" || echo unknown)"
  echo "engine:  $(grep -m1 '^version' "$ROOT/engine/Cargo.toml" | cut -d'"' -f2) (rust, wasmtime $(grep -m1 '^wasmtime' "$ROOT/engine/Cargo.toml" | cut -d'"' -f2))"
  echo "python:  $(awk '$1 == "python" {n = split($2, a, "/"); sub(/\.tar\..*$/, "", a[n]); print a[n]}' "$ROOT/python/sources.lock")"
  echo "protocol: $("$PYTHON" -c "import json,sys; print(json.load(open(sys.argv[1]))['protocol'])" "$(ls -d "$ROOT"/dist/runtime/collabo-core-*/ | head -1)manifest.json")"
  echo
} > "$OUT/VERSION"
index "$OUT"
echo
cat "$OUT/VERSION"
echo "== release ready: $OUT"
